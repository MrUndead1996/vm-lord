//! What may be copied between a host and a guest, and how one selection moves.
//!
//! Both ends run this. It holds the allowlist, the caps, the numbering of
//! offers and transfers and every rule about cancellation, and it holds them
//! once: a limit that only the viewer enforced would be a limit a guest could
//! ignore, and the other way round.
//!
//! Nothing here touches a socket, a compositor or a clipboard. A caller feeds
//! it what arrived and what the local clipboard did, and gets back a list of
//! [`Op`]s to carry out -- records to write, bytes to fetch, bytes to apply.
//! Time arrives as an argument for the same reason: a transfer that stops
//! moving has to be cancelled, and a machine that read the clock itself could
//! not be tested without waiting.
//!
//! The model is pull, in both directions. A side announces what its selection
//! can produce and sends nothing until the other asks, so a picture copied in a
//! guest costs nothing until somebody pastes it on the host.

pub mod files;
pub mod path;

use std::time::{Duration, Instant};

use crate::v1::CancelReason;

/// UTF-8 text, which is `CF_UNICODETEXT` on the other side.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// HTML, which Windows carries inside a CF_HTML envelope.
pub const HTML_MIME: &str = "text/html";

/// A BMP, which is a DIB with a file header in front of it.
pub const BMP_MIME: &str = "image/bmp";

/// A PNG, which many GTK applications offer and no Windows format holds.
pub const PNG_MIME: &str = "image/png";

/// The most mime types an offer may name.
pub const MAX_MIME_TYPES: usize = 16;

/// The most one text or HTML transfer may carry.
pub const MAX_TEXT_TRANSFER: usize = 8 * 1024 * 1024;

/// The most one image transfer may carry.
pub const MAX_IMAGE_TRANSFER: usize = 32 * 1024 * 1024;

/// How much of a transfer one record carries.
///
/// Below [`crate::record::CLIPBOARD_MAX_PAYLOAD`] with room for the message's
/// own fields, so that a full chunk never becomes a record its channel refuses.
pub const CHUNK: usize = 60 * 1024;

/// How long a transfer may make no progress before it is cancelled.
pub const IDLE: Duration = Duration::from_secs(5);

/// What one side may put on the other's clipboard.
///
/// An allowlist rather than a pass-through. AppSandbox forwards any registered
/// Windows format by name, which is an unbounded channel between a guest and
/// its host offered to whatever either side happens to register; these four are
/// what people actually copy. Files are refused by policy and are task #139's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// UTF-8 text.
    Text,
    /// HTML.
    Html,
    /// A BMP.
    Bmp,
    /// A PNG.
    Png,
}

/// The order kinds are offered and pulled in, which is what makes an offer's
/// contents the same however a peer happened to list them.
const ORDER: [Kind; 4] = [Kind::Text, Kind::Html, Kind::Bmp, Kind::Png];

impl Kind {
    /// The kind a mime type names, if the allowlist has one.
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            TEXT_MIME => Some(Self::Text),
            HTML_MIME => Some(Self::Html),
            BMP_MIME => Some(Self::Bmp),
            PNG_MIME => Some(Self::Png),
            _ => None,
        }
    }

    /// What this kind is called on the wire.
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::Text => TEXT_MIME,
            Self::Html => HTML_MIME,
            Self::Bmp => BMP_MIME,
            Self::Png => PNG_MIME,
        }
    }

    /// The most one transfer of this kind may carry.
    #[must_use]
    pub fn cap(self) -> usize {
        match self {
            Self::Text | Self::Html => MAX_TEXT_TRANSFER,
            Self::Bmp | Self::Png => MAX_IMAGE_TRANSFER,
        }
    }

    /// Whether this kind is a picture, of which one selection carries one.
    #[must_use]
    pub fn is_image(self) -> bool {
        matches!(self, Self::Bmp | Self::Png)
    }
}

/// One format of one selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    /// Which format these bytes are in.
    pub kind: Kind,
    /// The bytes themselves, never logged.
    pub bytes: Vec<u8>,
}

/// A message this machine puts on the clipboard channel.
///
/// The wire forms are `ClipboardOffer`, `ClipboardRequest`, `ClipboardData` and
/// `ClipboardCancel`; encoding them belongs to the caller, which is what keeps
/// this module free of the schema's `Vec<String>` allocations on every offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// My selection changed, and this is what it can produce.
    Offer {
        /// Names the selection.
        serial: u32,
        /// What may be asked for, in [`ORDER`].
        mime_types: Vec<&'static str>,
    },
    /// Send me one of them.
    Request {
        /// The offer this answers.
        serial: u32,
        /// Which format.
        mime_type: &'static str,
        /// Names the transfer that follows.
        transfer: u32,
    },
    /// A chunk of one, in order.
    Data {
        /// The transfer it belongs to.
        transfer: u32,
        /// The bytes, never logged.
        chunk: Vec<u8>,
        /// Whether the transfer ends here.
        last: bool,
    },
    /// That transfer is over and its bytes are not coming.
    Cancel {
        /// The transfer that ended.
        transfer: u32,
        /// Why it did.
        reason: CancelReason,
    },
}

/// What the caller must do, beyond writing records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Put this message on the clipboard channel.
    Send(Message),
    /// Fetch this format from the local clipboard, then call
    /// [`Exchange::produced`] with it -- or [`Exchange::unavailable`] if it
    /// cannot be had.
    Produce {
        /// Which format to fetch.
        kind: Kind,
        /// The transfer it answers.
        transfer: u32,
    },
    /// Put all of this on the local clipboard, as one selection.
    Apply {
        /// Every format the peer's selection produced, in [`ORDER`].
        pieces: Vec<Piece>,
    },
}

/// What this side is serving to the peer.
struct Outgoing {
    /// The serial of the selection this side last announced.
    serial: u32,
    /// The transfer being served, and the kind it asked for.
    transfer: Option<(u32, Kind)>,
    /// When that transfer last moved.
    since: Option<Instant>,
}

/// What this side is pulling from the peer.
struct Incoming {
    /// The serial of the peer's selection, if one has been offered.
    serial: Option<u32>,
    /// The kinds still to pull, in [`ORDER`].
    queue: Vec<Kind>,
    /// The transfer in flight and the kind it is carrying.
    transfer: Option<(u32, Kind)>,
    /// What has arrived for that transfer.
    buffer: Vec<u8>,
    /// The formats that have arrived whole.
    pieces: Vec<Piece>,
    /// When the transfer in flight last moved.
    since: Option<Instant>,
}

/// One side of one session's clipboard.
pub struct Exchange {
    outgoing: Outgoing,
    incoming: Incoming,
    /// The next transfer id. It never repeats within a session, so a chunk of a
    /// cancelled transfer is ignored rather than appended to its successor.
    next_transfer: u32,
}

impl Default for Exchange {
    fn default() -> Self {
        Self::new()
    }
}

impl Exchange {
    /// A clipboard with nothing offered in either direction.
    #[must_use]
    pub fn new() -> Self {
        Self {
            outgoing: Outgoing {
                serial: 0,
                transfer: None,
                since: None,
            },
            incoming: Incoming {
                serial: None,
                queue: Vec::new(),
                transfer: None,
                buffer: Vec::new(),
                pieces: Vec::new(),
                since: None,
            },
            next_transfer: 1,
        }
    }

    /// The local clipboard changed, and this is what it can produce.
    ///
    /// The caller decides when this may be called -- the viewer calls it only
    /// while its window has focus -- and this decides what it costs, which is
    /// one record however large the selection is.
    pub fn local_offer(&mut self, kinds: &[Kind], now: Instant) -> Vec<Op> {
        let mut ops = Vec::new();
        ops.extend(self.cancel_outgoing(CancelReason::Superseded));

        self.outgoing.serial = self.outgoing.serial.wrapping_add(1);
        self.outgoing.since = Some(now);

        let mime_types = ordered(kinds).into_iter().map(Kind::mime).collect();
        ops.push(Op::Send(Message::Offer {
            serial: self.outgoing.serial,
            mime_types,
        }));

        ops
    }

    /// The peer wants one format of the selection this side announced.
    pub fn peer_request(
        &mut self,
        serial: u32,
        mime_type: &str,
        transfer: u32,
        now: Instant,
    ) -> Vec<Op> {
        if serial != self.outgoing.serial {
            return vec![Op::Send(Message::Cancel {
                transfer,
                reason: CancelReason::Superseded,
            })];
        }
        let Some(kind) = Kind::from_mime(mime_type) else {
            return vec![Op::Send(Message::Cancel {
                transfer,
                reason: CancelReason::Unavailable,
            })];
        };

        let mut ops = self.cancel_outgoing(CancelReason::Superseded);
        self.outgoing.transfer = Some((transfer, kind));
        self.outgoing.since = Some(now);
        ops.push(Op::Produce { kind, transfer });

        ops
    }

    /// The bytes the local clipboard produced for a transfer.
    pub fn produced(&mut self, transfer: u32, bytes: Vec<u8>, now: Instant) -> Vec<Op> {
        let Some((current, kind)) = self.outgoing.transfer else {
            return Vec::new();
        };
        if current != transfer {
            return Vec::new();
        }

        if bytes.len() > kind.cap() {
            self.outgoing.transfer = None;
            self.outgoing.since = None;

            return vec![Op::Send(Message::Cancel {
                transfer,
                reason: CancelReason::TooLarge,
            })];
        }

        self.outgoing.transfer = None;
        self.outgoing.since = Some(now);

        let mut ops = Vec::new();
        let mut rest = bytes.as_slice();
        loop {
            let take = rest.len().min(CHUNK);
            let (chunk, tail) = rest.split_at(take);
            rest = tail;
            ops.push(Op::Send(Message::Data {
                transfer,
                chunk: chunk.to_vec(),
                last: rest.is_empty(),
            }));
            if rest.is_empty() {
                break;
            }
        }

        ops
    }

    /// The local clipboard could not produce what a transfer asked for.
    pub fn unavailable(&mut self, transfer: u32) -> Vec<Op> {
        match self.outgoing.transfer {
            Some((current, _)) if current == transfer => {
                self.outgoing.transfer = None;
                self.outgoing.since = None;

                vec![Op::Send(Message::Cancel {
                    transfer,
                    reason: CancelReason::Unavailable,
                })]
            }
            _ => Vec::new(),
        }
    }

    /// The peer's selection changed.
    ///
    /// What is pulled is what the allowlist has: text, HTML and one picture --
    /// a BMP if the peer has one, since a DIB is a BMP without its file header
    /// and needs no codec. An offer of nothing allowed is dropped in silence: a
    /// guest copying a spreadsheet's internal format is behaving correctly.
    pub fn peer_offer(&mut self, serial: u32, mime_types: &[String], now: Instant) -> Vec<Op> {
        let mut kinds = Vec::new();
        for mime in mime_types.iter().take(MAX_MIME_TYPES) {
            if let Some(kind) = Kind::from_mime(mime) {
                kinds.push(kind);
            }
        }
        let mut wanted = ordered(&kinds);
        // One picture, not two of the same picture.
        if wanted.contains(&Kind::Bmp) {
            wanted.retain(|kind| *kind != Kind::Png);
        }

        let mut ops = self.cancel_incoming(CancelReason::Superseded);
        self.forget_incoming();

        if wanted.is_empty() {
            return ops;
        }

        self.incoming.serial = Some(serial);
        self.incoming.queue = wanted;
        ops.extend(self.request_next(now));

        ops
    }

    /// A chunk of the transfer this side is pulling.
    pub fn peer_data(&mut self, transfer: u32, chunk: &[u8], last: bool, now: Instant) -> Vec<Op> {
        let Some((current, kind)) = self.incoming.transfer else {
            return Vec::new();
        };
        if current != transfer {
            return Vec::new();
        }

        if self.incoming.buffer.len() + chunk.len() > kind.cap() {
            let ops = self.cancel_incoming(CancelReason::TooLarge);
            self.forget_incoming();

            return ops;
        }

        self.incoming.buffer.extend_from_slice(chunk);
        self.incoming.since = Some(now);

        if !last {
            return Vec::new();
        }

        let bytes = std::mem::take(&mut self.incoming.buffer);
        self.incoming.pieces.push(Piece { kind, bytes });
        self.incoming.transfer = None;
        self.incoming.since = None;

        let ops = self.request_next(now);
        if !ops.is_empty() {
            return ops;
        }

        let pieces = std::mem::take(&mut self.incoming.pieces);
        self.incoming.serial = None;

        vec![Op::Apply { pieces }]
    }

    /// The peer gave up on a transfer.
    ///
    /// An incoming one abandons the whole offer rather than pulling the next
    /// format: a peer that cannot produce one format of a selection has a
    /// selection this side should not half-apply.
    pub fn peer_cancel(&mut self, transfer: u32, _reason: CancelReason) -> Vec<Op> {
        if self
            .incoming
            .transfer
            .is_some_and(|(current, _)| current == transfer)
        {
            self.forget_incoming();

            return Vec::new();
        }

        if self
            .outgoing
            .transfer
            .is_some_and(|(current, _)| current == transfer)
        {
            self.outgoing.transfer = None;
            self.outgoing.since = None;
        }

        Vec::new()
    }

    /// The window lost focus, so nothing may cross in either direction.
    ///
    /// A VM in the background can neither read what its user copies elsewhere
    /// nor quietly replace what is on their clipboard.
    pub fn focus_lost(&mut self, _now: Instant) -> Vec<Op> {
        let mut ops = self.cancel_outgoing(CancelReason::FocusLost);
        ops.extend(self.cancel_incoming(CancelReason::FocusLost));
        self.forget_incoming();

        ops
    }

    /// Cancels whatever has stopped moving.
    pub fn tick(&mut self, now: Instant) -> Vec<Op> {
        let mut ops = Vec::new();

        if self.outgoing.transfer.is_some()
            && self
                .outgoing
                .since
                .is_some_and(|since| now.duration_since(since) > IDLE)
        {
            ops.extend(self.cancel_outgoing(CancelReason::TimedOut));
        }

        if self.incoming.transfer.is_some()
            && self
                .incoming
                .since
                .is_some_and(|since| now.duration_since(since) > IDLE)
        {
            ops.extend(self.cancel_incoming(CancelReason::TimedOut));
            self.forget_incoming();
        }

        ops
    }

    /// Asks for the next format of the peer's selection, if one is left.
    fn request_next(&mut self, now: Instant) -> Vec<Op> {
        let Some(serial) = self.incoming.serial else {
            return Vec::new();
        };
        if self.incoming.queue.is_empty() {
            return Vec::new();
        }

        let kind = self.incoming.queue.remove(0);
        let transfer = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        self.incoming.transfer = Some((transfer, kind));
        self.incoming.buffer.clear();
        self.incoming.since = Some(now);

        vec![Op::Send(Message::Request {
            serial,
            mime_type: kind.mime(),
            transfer,
        })]
    }

    /// Ends the transfer this side is serving, if there is one.
    fn cancel_outgoing(&mut self, reason: CancelReason) -> Vec<Op> {
        match self.outgoing.transfer.take() {
            Some((transfer, _)) => {
                self.outgoing.since = None;

                vec![Op::Send(Message::Cancel { transfer, reason })]
            }
            None => Vec::new(),
        }
    }

    /// Ends the transfer this side is pulling, if there is one.
    fn cancel_incoming(&mut self, reason: CancelReason) -> Vec<Op> {
        match self.incoming.transfer.take() {
            Some((transfer, _)) => {
                self.incoming.since = None;

                vec![Op::Send(Message::Cancel { transfer, reason })]
            }
            None => Vec::new(),
        }
    }

    /// Drops everything pulled so far, whole pieces included.
    fn forget_incoming(&mut self) {
        self.incoming.serial = None;
        self.incoming.queue.clear();
        self.incoming.transfer = None;
        self.incoming.buffer = Vec::new();
        self.incoming.pieces = Vec::new();
        self.incoming.since = None;
    }
}

/// The kinds of `kinds`, deduplicated and in [`ORDER`].
fn ordered(kinds: &[Kind]) -> Vec<Kind> {
    ORDER
        .into_iter()
        .filter(|kind| kinds.contains(kind))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn mimes(kinds: &[Kind]) -> Vec<String> {
        kinds.iter().map(|kind| kind.mime().to_owned()).collect()
    }

    /// The serial of the offer `local_offer` just announced.
    fn announced(ops: &[Op]) -> u32 {
        match ops.last() {
            Some(Op::Send(Message::Offer { serial, .. })) => *serial,
            other => panic!("an offer, not {other:?}"),
        }
    }

    #[test]
    fn the_allowlist_names_four_kinds_and_nothing_else() {
        assert_eq!(
            Kind::from_mime("text/plain;charset=utf-8"),
            Some(Kind::Text)
        );
        assert_eq!(Kind::from_mime("text/html"), Some(Kind::Html));
        assert_eq!(Kind::from_mime("image/bmp"), Some(Kind::Bmp));
        assert_eq!(Kind::from_mime("image/png"), Some(Kind::Png));

        // Files are refused by policy, not by omission: task #139 owns them.
        assert_eq!(Kind::from_mime("text/uri-list"), None);
        assert_eq!(Kind::from_mime("text/plain"), None);
        assert_eq!(Kind::from_mime("application/x-anything"), None);
    }

    #[test]
    fn text_and_images_have_their_own_caps() {
        assert_eq!(Kind::Text.cap(), 8 * 1024 * 1024);
        assert_eq!(Kind::Html.cap(), 8 * 1024 * 1024);
        assert_eq!(Kind::Bmp.cap(), 32 * 1024 * 1024);
        assert_eq!(Kind::Png.cap(), 32 * 1024 * 1024);
    }

    #[test]
    fn a_chunk_fits_the_channel_it_travels_on() {
        assert!(CHUNK < crate::record::CLIPBOARD_MAX_PAYLOAD as usize);
    }

    #[test]
    fn a_local_offer_is_announced_and_served_on_request() {
        let mut exchange = Exchange::new();
        let now = t0();

        let ops = exchange.local_offer(&[Kind::Html, Kind::Text], now);
        let Op::Send(Message::Offer { serial, mime_types }) = &ops[0] else {
            panic!("an offer is announced");
        };
        // Announced in the canonical order, not the caller's.
        assert_eq!(mime_types, &vec![TEXT_MIME, HTML_MIME]);
        let serial = *serial;

        let ops = exchange.peer_request(serial, TEXT_MIME, 1, now);
        assert!(matches!(
            ops.as_slice(),
            [Op::Produce {
                kind: Kind::Text,
                transfer: 1
            }]
        ));

        let ops = exchange.produced(1, b"hello".to_vec(), now);
        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Data {
                transfer: 1,
                last: true,
                ..
            })]
        ));
    }

    #[test]
    fn a_request_against_a_superseded_offer_is_refused() {
        let mut exchange = Exchange::new();
        let now = t0();
        let first = announced(&exchange.local_offer(&[Kind::Text], now));
        let _ = exchange.local_offer(&[Kind::Text], now);

        let ops = exchange.peer_request(first, TEXT_MIME, 7, now);

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel {
                transfer: 7,
                reason: CancelReason::Superseded
            })]
        ));
    }

    #[test]
    fn a_request_for_a_format_outside_the_allowlist_is_refused() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = announced(&exchange.local_offer(&[Kind::Text], now));

        let ops = exchange.peer_request(serial, "text/uri-list", 2, now);

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel {
                transfer: 2,
                reason: CancelReason::Unavailable
            })]
        ));
    }

    #[test]
    fn a_peer_offer_is_pulled_and_applied_once_every_kind_has_arrived() {
        let mut exchange = Exchange::new();
        let now = t0();

        let ops = exchange.peer_offer(4, &mimes(&[Kind::Text, Kind::Html]), now);
        let Op::Send(Message::Request {
            mime_type,
            transfer,
            ..
        }) = &ops[0]
        else {
            panic!("the first kind is requested");
        };
        assert_eq!(*mime_type, TEXT_MIME);
        let first = *transfer;
        // One transfer in flight: the second kind is not asked for yet.
        assert_eq!(ops.len(), 1);

        let ops = exchange.peer_data(first, b"plain", true, now);
        let Op::Send(Message::Request {
            mime_type,
            transfer,
            ..
        }) = &ops[0]
        else {
            panic!("the second kind follows the first");
        };
        assert_eq!(*mime_type, HTML_MIME);
        let second = *transfer;
        assert_ne!(second, first, "a transfer id is never reused");

        let ops = exchange.peer_data(second, b"<i>rich</i>", true, now);
        let Op::Apply { pieces } = &ops[0] else {
            panic!("both kinds are applied together");
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].kind, Kind::Text);
        assert_eq!(pieces[0].bytes, b"plain");
        assert_eq!(pieces[1].kind, Kind::Html);
        assert_eq!(pieces[1].bytes, b"<i>rich</i>");
    }

    #[test]
    fn only_one_image_kind_is_pulled_and_bmp_wins() {
        let mut exchange = Exchange::new();

        let ops = exchange.peer_offer(1, &mimes(&[Kind::Png, Kind::Bmp]), t0());

        let Op::Send(Message::Request { mime_type, .. }) = &ops[0] else {
            panic!("an image is requested");
        };
        assert_eq!(*mime_type, BMP_MIME);
    }

    #[test]
    fn a_png_alone_is_still_pulled() {
        let mut exchange = Exchange::new();

        let ops = exchange.peer_offer(1, &mimes(&[Kind::Png]), t0());

        let Op::Send(Message::Request { mime_type, .. }) = &ops[0] else {
            panic!("a picture is requested");
        };
        assert_eq!(*mime_type, PNG_MIME);
    }

    #[test]
    fn an_offer_of_nothing_allowed_is_dropped_without_a_word() {
        let mut exchange = Exchange::new();

        let ops = exchange.peer_offer(
            1,
            &["text/uri-list".to_owned(), "application/x-lotus".to_owned()],
            t0(),
        );

        assert!(ops.is_empty());
    }

    #[test]
    fn an_offer_naming_more_types_than_the_cap_is_truncated() {
        let mut exchange = Exchange::new();
        let mut offered: Vec<String> = (0..MAX_MIME_TYPES)
            .map(|index| format!("application/x-{index}"))
            .collect();
        offered.push(TEXT_MIME.to_owned());

        let ops = exchange.peer_offer(1, &offered, t0());

        assert!(ops.is_empty(), "what is past the cap is not looked at");
    }

    #[test]
    fn a_body_past_its_cap_is_cancelled_rather_than_sent() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = announced(&exchange.local_offer(&[Kind::Text], now));
        let _ = exchange.peer_request(serial, TEXT_MIME, 3, now);

        let ops = exchange.produced(3, vec![b'x'; MAX_TEXT_TRANSFER + 1], now);

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel {
                transfer: 3,
                reason: CancelReason::TooLarge
            })]
        ));
    }

    #[test]
    fn an_arriving_body_past_its_cap_is_cancelled_mid_stream() {
        let mut exchange = Exchange::new();
        let now = t0();
        let transfer = match &exchange.peer_offer(1, &mimes(&[Kind::Text]), now)[0] {
            Op::Send(Message::Request { transfer, .. }) => *transfer,
            other => panic!("a request, not {other:?}"),
        };

        let mut ops = Vec::new();
        for _ in 0..=(MAX_TEXT_TRANSFER / CHUNK) + 1 {
            ops = exchange.peer_data(transfer, &vec![b'x'; CHUNK], false, now);
            if !ops.is_empty() {
                break;
            }
        }

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel {
                reason: CancelReason::TooLarge,
                ..
            })]
        ));
    }

    #[test]
    fn losing_focus_cancels_both_directions() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = announced(&exchange.local_offer(&[Kind::Text], now));
        let _ = exchange.peer_request(serial, TEXT_MIME, 5, now);
        let _ = exchange.peer_offer(9, &mimes(&[Kind::Text]), now);

        let ops = exchange.focus_lost(now);

        let cancels = ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    Op::Send(Message::Cancel {
                        reason: CancelReason::FocusLost,
                        ..
                    })
                )
            })
            .count();
        assert_eq!(cancels, 2);
    }

    #[test]
    fn a_transfer_that_stops_moving_times_out() {
        let mut exchange = Exchange::new();
        let now = t0();
        let _ = exchange.peer_offer(1, &mimes(&[Kind::Text]), now);

        let ops = exchange.tick(now + IDLE + Duration::from_millis(1));

        assert!(matches!(
            ops.as_slice(),
            [Op::Send(Message::Cancel {
                reason: CancelReason::TimedOut,
                ..
            })]
        ));
    }

    #[test]
    fn a_transfer_that_is_still_moving_is_left_alone() {
        let mut exchange = Exchange::new();
        let now = t0();
        let transfer = match &exchange.peer_offer(1, &mimes(&[Kind::Text]), now)[0] {
            Op::Send(Message::Request { transfer, .. }) => *transfer,
            other => panic!("a request, not {other:?}"),
        };
        let later = now + IDLE;
        let _ = exchange.peer_data(transfer, b"still going", false, later);

        assert!(exchange.tick(later + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn a_chunked_body_arrives_in_order_and_whole() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = announced(&exchange.local_offer(&[Kind::Bmp], now));
        let _ = exchange.peer_request(serial, BMP_MIME, 2, now);
        let body: Vec<u8> = (0..CHUNK * 2 + 17).map(|index| index as u8).collect();

        let ops = exchange.produced(2, body.clone(), now);

        let mut rebuilt = Vec::new();
        let mut ended = false;
        for op in &ops {
            let Op::Send(Message::Data { chunk, last, .. }) = op else {
                panic!("only data, not {op:?}");
            };
            assert!(chunk.len() <= CHUNK);
            rebuilt.extend_from_slice(chunk);
            ended = *last;
        }
        assert!(ended);
        assert_eq!(rebuilt, body);
    }

    #[test]
    fn an_empty_selection_is_one_empty_last_chunk() {
        let mut exchange = Exchange::new();
        let now = t0();
        let serial = announced(&exchange.local_offer(&[Kind::Text], now));
        let _ = exchange.peer_request(serial, TEXT_MIME, 4, now);

        let ops = exchange.produced(4, Vec::new(), now);

        assert_eq!(
            ops,
            vec![Op::Send(Message::Data {
                transfer: 4,
                chunk: Vec::new(),
                last: true
            })]
        );
    }

    #[test]
    fn a_cancelled_offer_is_abandoned_rather_than_half_applied() {
        let mut exchange = Exchange::new();
        let now = t0();
        let first = match &exchange.peer_offer(2, &mimes(&[Kind::Text, Kind::Html]), now)[0] {
            Op::Send(Message::Request { transfer, .. }) => *transfer,
            other => panic!("a request, not {other:?}"),
        };
        let _ = exchange.peer_data(first, b"plain", true, now);
        let second = 2;

        let ops = exchange.peer_cancel(second, CancelReason::Unavailable);

        assert!(ops.is_empty());
        // The text that did arrive is dropped with the rest of the selection.
        assert!(exchange.peer_data(first, b"more", true, now).is_empty());
    }

    #[test]
    fn a_chunk_of_a_transfer_that_ended_is_ignored() {
        let mut exchange = Exchange::new();
        let now = t0();
        let transfer = match &exchange.peer_offer(1, &mimes(&[Kind::Text]), now)[0] {
            Op::Send(Message::Request { transfer, .. }) => *transfer,
            other => panic!("a request, not {other:?}"),
        };
        let _ = exchange.peer_cancel(transfer, CancelReason::TimedOut);

        assert!(exchange.peer_data(transfer, b"late", true, now).is_empty());
    }
}
