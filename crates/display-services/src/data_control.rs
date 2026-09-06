//! wlroots' clipboard, through the protocol a clipboard manager is meant to
//! use. This module is the data-control implementation of
//! [`crate::guest_clipboard::GuestClipboard`].
//!
//! Two protocols, not one: `ext-data-control-v1` is the standardised successor
//! and `zwlr-data-control-unstable-v1` the original, and they are the same
//! protocol under two sets of names. Hyprland offers both, older wlroots
//! compositors only the second, so both are spoken and `ext` is preferred. The
//! difference is four enums wide, which is why there is one module rather than
//! two.
//!
//! Three things separate this from Mutter's side of the seam, and all three are
//! why [`crate::guest_clipboard`] is a trait rather than a table of functions:
//!
//!   * **there are no serials.** Mutter announces a transfer and then hands
//!     over a descriptor when asked for one by serial; here the descriptor
//!     arrives *with* the announcement, in the source's `send` event. So the
//!     serials in [`crate::guest_clipboard::Event`] are minted here, and the
//!     descriptors wait in [`Transfers`] until the daemon answers;
//!   * **owning the selection does not blind this side.** Mutter refuses to let
//!     a client read a selection it owns, so the daemon has to stay a listener;
//!     data-control has no such rule, and `listen` is therefore nothing at all.
//!     What it does have is the echo: a client that sets the selection is told
//!     about it like anybody else, which is what `owned` below suppresses;
//!   * **reading is a pipe this side makes.** `receive` takes a descriptor
//!     rather than returning one, so `read_mime` creates the pipe, hands over
//!     the writing end and drains the reading end -- with the same loop the
//!     other implementation uses, from [`crate::clipboard_pipe`].
//!
//! Nothing here is policy. What may cross and how large it may be is
//! [`vmlord_display_protocol::clipboard`].

use std::{
    collections::HashMap,
    fmt,
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use vmlord_display_protocol::clipboard::Kind;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client as ext;
use wayland_protocols_wlr::data_control::v1::client as wlr;

// The macros below name a protocol's modules by a single identifier, because a
// path cannot be pasted into a `use` or a pattern from a macro argument. These
// aliases are what makes that single identifier exist for both protocols.
use ext::{
    ext_data_control_device_v1 as ext_device, ext_data_control_offer_v1 as ext_offer,
    ext_data_control_source_v1 as ext_source,
};
use wlr::{
    zwlr_data_control_device_v1 as wlr_device, zwlr_data_control_offer_v1 as wlr_offer,
    zwlr_data_control_source_v1 as wlr_source,
};

use crate::{
    clipboard_pipe::{drain, fill, pipe},
    guest_clipboard::{
        ClipboardError, Event, GNOME_COPIED_MIME, GuestClipboard, URI_LIST_MIME, kinds_of,
        offers_files,
    },
};

/// How long one transfer may take before it is abandoned.
///
/// The same five seconds the Mutter side uses, for the same reason: a guest
/// application that never answers a selection request must not hold this
/// process.
const DEADLINE: Duration = Duration::from_secs(5);

/// The version of either data-control protocol this speaks.
///
/// One, which is all of `ext-data-control-v1` and the half of the wlroots
/// protocol that is not the primary selection -- and the primary selection has
/// no counterpart on a Windows host, so there is nothing to gain by asking for
/// two.
const VERSION: u32 = 1;

/// The wlroots clipboard, over whichever of the two protocols the session
/// offers.
pub struct Clipboard {
    /// Kept for its `flush`: a request sent from the daemon's thread has to
    /// reach the compositor without waiting for the reading thread to loop.
    connection: Connection,
    queue: QueueHandle<State>,
    /// Kept alive because it is what makes a source: a selection is set as
    /// often as the host copies, and the manager is the only thing that can
    /// make one.
    manager: Manager,
    device: Device,
    shared: Arc<Mutex<Shared>>,
}

/// What the reading thread and the daemon's thread both touch.
#[derive(Default)]
struct Shared {
    /// The peer's current selection, which is what `read_mime` reads.
    offer: Option<Offer>,
    /// This side's selection, while it owns one.
    source: Option<Source>,
    /// Whether this side owns the selection, which is how its own offer is
    /// told from the guest's.
    owned: bool,
    /// Descriptors announced but not yet answered.
    transfers: Transfers,
}

impl GuestClipboard for Clipboard {
    /// Connects to the compositor, binds whichever data-control protocol it
    /// offers, and begins turning its events into [`Event`]s.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if there is no Wayland socket, no seat on
    /// it, or neither data-control protocol -- which is what a GNOME session
    /// looks like, and what sends the daemon to the other implementation.
    fn open() -> Result<(Self, Receiver<Event>), ClipboardError> {
        let connection = Connection::connect_to_env().map_err(compositor)?;
        let mut queue = connection.new_event_queue::<State>();
        let handle = queue.handle();
        let registry = connection.display().get_registry(&handle, ());

        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut state = State {
            events: sender,
            shared: Arc::clone(&shared),
            offers: HashMap::new(),
            globals: Globals {
                registry: Some(registry),
                ..Globals::default()
            },
        };
        // The first round trip is what the registry's advertisements arrive
        // in; nothing below this line can be decided before it returns.
        queue.roundtrip(&mut state).map_err(compositor)?;

        let (manager, device) = state.globals.bind(&handle)?;
        // The second carries the `get_data_device` above to the compositor and
        // brings back the selection that already exists, if there is one.
        queue.roundtrip(&mut state).map_err(compositor)?;

        thread::spawn(move || pump_events(&mut queue, &mut state));

        Ok((
            Self {
                connection,
                queue: handle,
                manager,
                device,
                shared,
            },
            receiver,
        ))
    }

    /// Nothing: a data-control device reports selections from the moment it
    /// exists, and unlike Mutter this side is not blinded by owning one.
    ///
    /// # Errors
    ///
    /// None. The signature is the seam's.
    fn listen(&self) -> Result<(), ClipboardError> {
        Ok(())
    }

    /// A fresh source offering these formats, made the selection.
    ///
    /// A source is single-use -- the compositor cancels it as soon as anything
    /// else is copied -- so each call builds a new one and drops the last.
    ///
    /// `files` adds [`URI_LIST_MIME`] and [`GNOME_COPIED_MIME`]: the first is
    /// what most of a wlroots desktop reads a file selection as, the second
    /// what a GTK file manager living on one looks for.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the connection cannot be flushed.
    fn own(&self, kinds: &[Kind], files: bool) -> Result<(), ClipboardError> {
        let mut mimes: Vec<&str> = kinds.iter().map(|kind| kind.mime()).collect();
        if files {
            mimes.push(URI_LIST_MIME);
            mimes.push(GNOME_COPIED_MIME);
        }

        let source = self.manager.source(&self.queue);
        for mime in mimes {
            source.offer(mime);
        }
        self.device.set_selection(&source);

        {
            let mut shared = self.shared.lock().expect("the clipboard's state");
            // Set before the flush: the compositor's answer to `set_selection`
            // is an offer of this very selection, and it must not be forwarded
            // to the host that just sent it.
            shared.owned = true;
            if let Some(previous) = shared.source.replace(source) {
                previous.destroy();
            }
        }

        self.connection.flush().map_err(compositor)
    }

    /// Hands the compositor the writing end of a pipe and drains the reading
    /// end, which is what `receive` is.
    ///
    /// # Errors
    ///
    /// The same as [`GuestClipboard::read`], plus
    /// [`ClipboardError::Compositor`] when there is no selection to read --
    /// which is what a selection replaced between the offer and the read looks
    /// like.
    fn read_mime(&self, mime: &str, cap: usize) -> Result<Vec<u8>, ClipboardError> {
        let (reader, writer) = pipe()?;

        {
            let shared = self.shared.lock().expect("the clipboard's state");
            let offer = shared
                .offer
                .as_ref()
                .ok_or_else(|| ClipboardError::Compositor("there is no selection".to_owned()))?;
            offer.receive(mime, &writer);
        }
        self.connection.flush().map_err(compositor)?;

        // Before the read and not after it: while this end of the pipe is open
        // the reader never sees the close that ends a selection, however
        // promptly the compositor finishes writing.
        drop(writer);

        drain(&reader, cap, DEADLINE)
    }

    /// Fills the descriptor the compositor sent with the transfer, then closes
    /// it -- the close is what tells the reader the selection is whole.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the serial names no waiting transfer
    /// and [`ClipboardError::Transfer`] if the descriptor cannot be written.
    fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), ClipboardError> {
        let descriptor = self.take(serial)?;

        fill(&descriptor, bytes)
    }

    /// Closes the descriptor with nothing written, which is what refusing is
    /// here: the reader gets an empty selection rather than a wait that never
    /// ends.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the serial names no waiting transfer.
    fn refuse(&self, serial: u32) -> Result<(), ClipboardError> {
        drop(self.take(serial)?);

        Ok(())
    }
}

impl Clipboard {
    /// The descriptor a serial was minted for, taken out of the table.
    fn take(&self, serial: u32) -> Result<OwnedFd, ClipboardError> {
        self.shared
            .lock()
            .expect("the clipboard's state")
            .transfers
            .take(serial)
            .ok_or_else(|| ClipboardError::Compositor(format!("no transfer {serial} is waiting")))
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            if let Some(source) = shared.source.take() {
                source.destroy();
            }
            if let Some(offer) = shared.offer.take() {
                offer.destroy();
            }
        }
        self.device.destroy();
        self.manager.destroy();
        let _ = self.connection.flush();
    }
}

/// The descriptors announced to the daemon and not yet answered.
///
/// The serials in [`Event`] are this table's keys, and they exist because the
/// seam has them: the other implementation's compositor mints them, and here
/// there is nothing else to name a descriptor by.
#[derive(Default)]
struct Transfers {
    /// The serial the next transfer gets. Starts at one, so that zero never
    /// names a live transfer.
    next: u32,
    open: HashMap<u32, OwnedFd>,
}

impl Transfers {
    /// Files a descriptor away and returns the serial that fetches it back.
    fn hand_out(&mut self, descriptor: OwnedFd) -> u32 {
        self.next = self.next.wrapping_add(1).max(1);
        let serial = self.next;
        self.open.insert(serial, descriptor);

        serial
    }

    /// Takes a descriptor back out, if that serial still names one.
    fn take(&mut self, serial: u32) -> Option<OwnedFd> {
        self.open.remove(&serial)
    }
}

/// What the registry advertised, before anything is bound.
#[derive(Default)]
struct Globals {
    registry: Option<wl_registry::WlRegistry>,
    seat: Option<u32>,
    ext: Option<u32>,
    wlr: Option<u32>,
}

impl Globals {
    /// Binds a seat and the better of the two protocols, and asks for that
    /// seat's data device.
    ///
    /// `ext` in preference to `wlr`: it is the standardised one, and a
    /// compositor that offers both is telling this side which it would rather
    /// speak.
    fn bind(&self, queue: &QueueHandle<State>) -> Result<(Manager, Device), ClipboardError> {
        let registry = self.registry.as_ref().ok_or_else(|| {
            ClipboardError::Compositor("the compositor announced no registry".to_owned())
        })?;
        let name = self.ext.or(self.wlr).ok_or_else(|| {
            ClipboardError::Compositor("the compositor offers no data-control protocol".to_owned())
        })?;
        let seat_name = self.seat.ok_or_else(|| {
            ClipboardError::Compositor("the compositor announced no seat".to_owned())
        })?;
        let seat: wl_seat::WlSeat = registry.bind(seat_name, VERSION, queue, ());

        if self.ext.is_some() {
            let manager: ext::ext_data_control_manager_v1::ExtDataControlManagerV1 =
                registry.bind(name, VERSION, queue, ());
            let device = Device::Ext(manager.get_data_device(&seat, queue, ()));

            return Ok((Manager::Ext(manager), device));
        }

        let manager: wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 =
            registry.bind(name, VERSION, queue, ());
        let device = Device::Wlr(manager.get_data_device(&seat, queue, ()));

        Ok((Manager::Wlr(manager), device))
    }
}

/// What the reading thread keeps.
struct State {
    events: Sender<Event>,
    shared: Arc<Mutex<Shared>>,
    /// The mime types each offer has named so far. An offer arrives empty and
    /// is filled by a run of `offer` events before the `selection` that uses
    /// it, so there is nowhere else to keep them.
    offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    globals: Globals,
}

/// Dispatches until the compositor goes away, then says so.
fn pump_events(queue: &mut wayland_client::EventQueue<State>, state: &mut State) {
    while queue.blocking_dispatch(state).is_ok() {}

    let _ = state.events.send(Event::Closed);
}

impl State {
    /// A selection somebody else owns became current.
    fn selection(&mut self, offer: Offer) {
        let mimes = self.offers.remove(&offer.id()).unwrap_or_default();
        let previous = {
            let mut shared = self.shared.lock().expect("the clipboard's state");
            if shared.owned {
                // This side's own selection, coming back. Forwarding it would
                // put what the host just sent straight back on the wire --
                // the same echo `session-is-owner` suppresses on the other
                // implementation. The flag is reliable because the compositor
                // cancels the previous source before it announces the new
                // selection, and both events travel this one connection in
                // order.
                offer.destroy();

                return;
            }

            shared.offer.replace(offer)
        };
        if let Some(previous) = previous {
            previous.destroy();
        }

        let kinds = kinds_of(&mimes);
        let files = offers_files(&mimes);
        if kinds.is_empty() && !files {
            // Ordinary -- a guest copying a spreadsheet cell offers a dozen
            // formats this build carries none of. The count and never the
            // names: a mime type can carry a file name.
            eprintln!(
                "vmlord-display-clipboard: the desktop changed to {} format(s), none carried",
                mimes.len()
            );

            return;
        }

        let _ = self.events.send(Event::PeerOffer { kinds, files });
    }

    /// Something in the guest is asking for the selection this side owns, and
    /// has been given a descriptor to be answered on.
    fn transfer(&mut self, mime: &str, descriptor: OwnedFd) {
        // The mime first, because a descriptor this side cannot answer should
        // never get a serial: dropping it closes it, which is an empty
        // selection rather than an application left waiting on a read.
        let files = mime == URI_LIST_MIME || mime == GNOME_COPIED_MIME;
        let kind = Kind::from_mime(mime);
        if kind.is_none() && !files {
            return;
        }

        let serial = self
            .shared
            .lock()
            .expect("the clipboard's state")
            .transfers
            .hand_out(descriptor);
        let event = match kind {
            Some(kind) => Event::Transfer { kind, serial },
            None => Event::TransferFiles {
                mime: mime.to_owned(),
                serial,
            },
        };

        let _ = self.events.send(event);
    }

    /// The clipboard was emptied by somebody else.
    fn cleared(&mut self) {
        let previous = self
            .shared
            .lock()
            .expect("the clipboard's state")
            .offer
            .take();
        if let Some(previous) = previous {
            previous.destroy();
        }
    }

    /// The selection this side owned was replaced by somebody else's.
    fn cancelled(&mut self) {
        let mut shared = self.shared.lock().expect("the clipboard's state");
        shared.owned = false;
        if let Some(source) = shared.source.take() {
            source.destroy();
        }
    }
}

/// The global that makes sources and devices, over either protocol.
enum Manager {
    Ext(ext::ext_data_control_manager_v1::ExtDataControlManagerV1),
    Wlr(wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1),
}

impl Manager {
    /// A new selection source, on the same protocol as this manager.
    fn source(&self, queue: &QueueHandle<State>) -> Source {
        match self {
            Self::Ext(manager) => Source::Ext(manager.create_data_source(queue, ())),
            Self::Wlr(manager) => Source::Wlr(manager.create_data_source(queue, ())),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(manager) => manager.destroy(),
            Self::Wlr(manager) => manager.destroy(),
        }
    }
}

/// A data device, over either protocol.
#[derive(Clone)]
enum Device {
    Ext(ext::ext_data_control_device_v1::ExtDataControlDeviceV1),
    Wlr(wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
}

impl Device {
    fn set_selection(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(device), Source::Ext(source)) => device.set_selection(Some(source)),
            (Self::Wlr(device), Source::Wlr(source)) => device.set_selection(Some(source)),
            _ => unreachable!("a source is made by the device that sets it"),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

/// A selection somebody else owns, over either protocol.
enum Offer {
    Ext(ext::ext_data_control_offer_v1::ExtDataControlOfferV1),
    Wlr(wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1),
}

impl Offer {
    fn id(&self) -> wayland_client::backend::ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    fn receive(&self, mime: &str, descriptor: &OwnedFd) {
        use std::os::fd::AsFd;

        match self {
            Self::Ext(offer) => offer.receive(mime.to_owned(), descriptor.as_fd()),
            Self::Wlr(offer) => offer.receive(mime.to_owned(), descriptor.as_fd()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

/// A selection this side owns, over either protocol.
enum Source {
    Ext(ext::ext_data_control_source_v1::ExtDataControlSourceV1),
    Wlr(wlr::zwlr_data_control_source_v1::ZwlrDataControlSourceV1),
}

impl Source {
    fn offer(&self, mime: &str) {
        match self {
            Self::Ext(source) => source.offer(mime.to_owned()),
            Self::Wlr(source) => source.offer(mime.to_owned()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }
}

/// What the registry says is here.
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };

        // The first of each: a guest has one seat, and a second announcement
        // of the same protocol is a compositor offering another version of it.
        match interface.as_str() {
            "wl_seat" => state.globals.seat.get_or_insert(name),
            "ext_data_control_manager_v1" => state.globals.ext.get_or_insert(name),
            "zwlr_data_control_manager_v1" => state.globals.wlr.get_or_insert(name),
            _ => return,
        };
    }
}

/// The device's events, which are the whole of what this side listens for.
macro_rules! device_dispatch {
    ($device:ty, $offer:ty, $module:ident, $wrap:ident) => {
        impl Dispatch<$device, ()> for State {
            fn event(
                state: &mut Self,
                _device: &$device,
                event: <$device as Proxy>::Event,
                (): &(),
                _connection: &Connection,
                _queue: &QueueHandle<Self>,
            ) {
                use $module::Event as DeviceEvent;

                match event {
                    // An offer arrives empty and names its formats in the run
                    // of `offer` events that follows, so all this does is make
                    // room for them.
                    DeviceEvent::DataOffer { id } => {
                        state.offers.insert(id.id(), Vec::new());
                    }
                    DeviceEvent::Selection { id: Some(offer) } => {
                        state.selection(Offer::$wrap(offer));
                    }
                    // The clipboard was emptied. There is nothing in the seam
                    // to say so with -- the other implementation's compositor
                    // has no such event either -- but the offer this side kept
                    // has to go, or a paste would answer from a selection that
                    // no longer exists.
                    DeviceEvent::Selection { id: None } => state.cleared(),
                    // The primary selection has no counterpart on a Windows
                    // host, so its offer is dropped rather than carried.
                    DeviceEvent::PrimarySelection { id: Some(offer) } => {
                        state.offers.remove(&offer.id());
                        offer.destroy();
                    }
                    DeviceEvent::Finished => {
                        let _ = state.events.send(Event::Closed);
                    }
                    _ => {}
                }
            }

            wayland_client::event_created_child!(State, $device, [
                $module::EVT_DATA_OFFER_OPCODE => ($offer, ()),
            ]);
        }
    };
}

device_dispatch!(
    ext::ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext::ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_device,
    Ext
);
device_dispatch!(
    wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    wlr_device,
    Wlr
);

/// An offer naming one of its formats.
macro_rules! offer_dispatch {
    ($offer:ty, $module:ident) => {
        impl Dispatch<$offer, ()> for State {
            fn event(
                state: &mut Self,
                offer: &$offer,
                event: <$offer as Proxy>::Event,
                (): &(),
                _connection: &Connection,
                _queue: &QueueHandle<Self>,
            ) {
                let $module::Event::Offer { mime_type } = event else {
                    return;
                };

                if let Some(mimes) = state.offers.get_mut(&offer.id()) {
                    mimes.push(mime_type);
                }
            }
        }
    };
}

offer_dispatch!(
    ext::ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_offer
);
offer_dispatch!(
    wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    wlr_offer
);

/// A source being asked for its bytes, or told it is no longer the selection.
macro_rules! source_dispatch {
    ($source:ty, $module:ident) => {
        impl Dispatch<$source, ()> for State {
            fn event(
                state: &mut Self,
                _source: &$source,
                event: <$source as Proxy>::Event,
                (): &(),
                _connection: &Connection,
                _queue: &QueueHandle<Self>,
            ) {
                match event {
                    $module::Event::Send { mime_type, fd } => state.transfer(&mime_type, fd),
                    $module::Event::Cancelled => state.cancelled(),
                    _ => {}
                }
            }
        }
    };
}

source_dispatch!(
    ext::ext_data_control_source_v1::ExtDataControlSourceV1,
    ext_source
);
source_dispatch!(
    wlr::zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    wlr_source
);

// Nothing here listens to a seat or to a manager: the seat is bound to name
// the device's seat and the manager to make the device, and both are finished
// the moment they have.
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ext::ext_data_control_manager_v1::ExtDataControlManagerV1);
delegate_noop!(State: ignore wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

/// One shape for everything the Wayland crates can fail with.
fn compositor<E: fmt::Display>(error: E) -> ClipboardError {
    ClipboardError::Compositor(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state with no compositor behind it, which is all `transfer` needs.
    fn detached() -> (State, Receiver<Event>) {
        let (events, receiver) = mpsc::channel();

        (
            State {
                events,
                shared: Arc::new(Mutex::new(Shared::default())),
                offers: HashMap::new(),
                globals: Globals::default(),
            },
            receiver,
        )
    }

    #[test]
    fn a_carried_format_is_announced_with_a_serial_that_fetches_its_descriptor() {
        let (mut state, events) = detached();
        let (_reader, writer) = pipe().expect("a pipe");

        state.transfer(Kind::Text.mime(), writer);

        let Ok(Event::Transfer { kind, serial }) = events.try_recv() else {
            panic!("a transfer of the text format");
        };
        assert_eq!(kind, Kind::Text);
        assert!(
            state
                .shared
                .lock()
                .expect("the state")
                .transfers
                .take(serial)
                .is_some()
        );
    }

    #[test]
    fn a_file_format_is_announced_under_the_name_it_was_asked_for() {
        let (mut state, events) = detached();
        let (_reader, writer) = pipe().expect("a pipe");

        state.transfer(GNOME_COPIED_MIME, writer);

        assert!(matches!(
            events.try_recv(),
            Ok(Event::TransferFiles { ref mime, .. }) if mime == GNOME_COPIED_MIME
        ));
    }

    #[test]
    fn a_format_this_side_never_offered_is_closed_rather_than_announced() {
        let (mut state, events) = detached();
        let (reader, writer) = pipe().expect("a pipe");

        state.transfer("application/x-nautilus-clipboard", writer);

        assert!(events.try_recv().is_err(), "nothing to answer");
        assert!(
            state
                .shared
                .lock()
                .expect("the state")
                .transfers
                .open
                .is_empty()
        );
        // The descriptor was dropped, so the reader sees the close that means
        // an empty selection rather than a wait that never ends.
        assert_eq!(
            drain(&reader, 1024, Duration::from_secs(2)).expect("a closed pipe"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn a_serial_fetches_back_the_descriptor_it_was_minted_for() {
        let mut transfers = Transfers::default();
        let (reader, writer) = pipe().expect("a pipe");

        let serial = transfers.hand_out(writer);
        let descriptor = transfers.take(serial).expect("the descriptor");

        fill(&descriptor, b"a selection").expect("a writable pipe");
        drop(descriptor);
        assert_eq!(
            drain(&reader, 1024, Duration::from_secs(2)).expect("a readable pipe"),
            b"a selection"
        );
    }

    #[test]
    fn two_transfers_never_share_a_serial() {
        let mut transfers = Transfers::default();
        let (_first_reader, first) = pipe().expect("a pipe");
        let (_second_reader, second) = pipe().expect("a pipe");

        let one = transfers.hand_out(first);
        let two = transfers.hand_out(second);

        assert_ne!(one, two);
    }

    #[test]
    fn a_serial_is_never_zero() {
        let mut transfers = Transfers {
            next: u32::MAX,
            open: HashMap::new(),
        };
        let (_reader, writer) = pipe().expect("a pipe");

        assert_eq!(transfers.hand_out(writer), 1);
    }

    #[test]
    fn a_serial_answered_twice_names_nothing_the_second_time() {
        let mut transfers = Transfers::default();
        let (_reader, writer) = pipe().expect("a pipe");

        let serial = transfers.hand_out(writer);

        assert!(transfers.take(serial).is_some());
        assert!(transfers.take(serial).is_none());
    }

    #[test]
    fn a_serial_nobody_minted_names_nothing() {
        assert!(Transfers::default().take(7).is_none());
    }

    #[test]
    fn a_session_with_neither_protocol_is_not_this_implementation() {
        let globals = Globals {
            registry: None,
            seat: Some(1),
            ext: None,
            wlr: None,
        };
        let connection = match Connection::connect_to_env() {
            Ok(connection) => connection,
            // No compositor on this machine, which is the ordinary case for a
            // build host. The branch above is what the assertion is about.
            Err(_) => return,
        };
        let queue = connection.new_event_queue::<State>();

        assert!(matches!(
            globals.bind(&queue.handle()),
            Err(ClipboardError::Compositor(_))
        ));
    }
}
