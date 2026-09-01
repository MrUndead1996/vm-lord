//! The window's sound: one WASAPI endpoint, rebuilt when the host's changes.
//!
//! What arrives on the audio channel is PCM in whatever format the guest
//! pinned on its loopback. This renderer does not match the endpoint's mix
//! format to it -- it initialises with the guest's format and lets WASAPI
//! convert, which is the one arrangement that survives a guest whose loopback
//! was pinned by PipeWire before the daemon got there.
//!
//! A device change rebuilds the endpoint and nothing else. The channel stays
//! bound: what output a person on the host chose is not a fact the guest knows
//! or should, the wire format does not change with it, and a rebind costs a
//! round trip that can fail.
//!
//! No line here carries a sample. A format, a frame count, a stream position
//! and an outcome are what an audio problem is diagnosed from.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vmlord_display_protocol::audio::{Format, SampleFormat, frames_between};
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE, EDataFlow, ERole, IAudioClient,
            IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
            IMMNotificationClient_Impl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX,
            eConsole, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
        },
    },
    core::{PCWSTR, implement},
};

/// `WAVE_FORMAT_IEEE_FLOAT`, written out rather than imported.
///
/// It lives in `Win32::Media::Multimedia`, and turning that whole feature on
/// for one integer would compile everything else in it as well.
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;

/// How much of the endpoint's buffer this renderer asks for.
///
/// A tenth of a second: enough that a scheduling hiccup on either side does
/// not turn into a gap, short enough that the sound stays with the picture.
const BUFFER_DURATION_100NS: i64 = 1_000_000;

/// Why a renderer could not be built.
#[derive(Debug)]
pub enum RendererError {
    /// There is no default output endpoint, or COM would not hand one over.
    NoEndpoint(windows::core::Error),
    /// The endpoint refused the guest's format, or would not start.
    Endpoint(windows::core::Error),
}

impl std::fmt::Display for RendererError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEndpoint(error) => {
                write!(formatter, "this host has no audio output: {error}")
            }
            Self::Endpoint(error) => write!(formatter, "the audio endpoint refused it: {error}"),
        }
    }
}

impl std::error::Error for RendererError {}

/// The `WAVEFORMATEX` an endpoint is initialised with.
///
/// The guest's format rather than the endpoint's mix format: WASAPI is asked to
/// convert, so what the guest pinned travels unchanged and the host adapts.
#[must_use]
pub fn wave_format(format: Format) -> WAVEFORMATEX {
    let bits = u16::try_from(format.sample_format.bytes() * 8).unwrap_or(16);
    let channels = u16::try_from(format.channels).unwrap_or(2);
    let block_align = channels * bits / 8;

    WAVEFORMATEX {
        wFormatTag: u16::try_from(match format.sample_format {
            SampleFormat::FloatLe => WAVE_FORMAT_IEEE_FLOAT,
            SampleFormat::S16Le | SampleFormat::S32Le => WAVE_FORMAT_PCM,
        })
        .unwrap_or(1),
        nChannels: channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// COM, initialised for the thread that holds it.
///
/// WASAPI is COM, and a thread that has not called `CoInitializeEx` is answered
/// `CO_E_NOTINITIALIZED` by every call -- which reads as "this host has no
/// audio output" and is nothing of the kind. The audio thread is this crate's
/// own, so nobody else initialises it.
///
/// Multithreaded, because this thread pumps no window messages: an apartment
/// would need one.
pub struct Com;

impl Com {
    /// Initialises COM for the calling thread.
    #[must_use]
    pub fn initialize() -> Self {
        // SAFETY: called once on a thread this crate owns, and undone in
        // `Drop` on that same thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        Self
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances the call in `initialize`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// What the host's endpoint changes are reported through.
///
/// The callbacks arrive on somebody else's thread, so they do the least they
/// can: set a flag the audio thread reads between periods.
#[derive(Clone)]
pub struct DeviceWatch {
    wanted: Arc<AtomicBool>,
}

impl DeviceWatch {
    /// A watch with nothing registered, for tests that have no COM.
    #[must_use]
    pub fn detached() -> Self {
        Self {
            wanted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Asks the audio thread to rebuild its endpoint.
    pub fn request(&self) {
        self.wanted.store(true, Ordering::Relaxed);
    }

    /// Takes the request, if one is outstanding.
    ///
    /// Taking rather than reading: one notification is one rebuild, and a flag
    /// that stayed set would rebuild the endpoint on every period.
    pub fn take_request(&self) -> bool {
        self.wanted.swap(false, Ordering::Relaxed)
    }
}

/// The `IMMNotificationClient` that feeds a [`DeviceWatch`].
#[implement(IMMNotificationClient)]
struct Notifications {
    watch: DeviceWatch,
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        // The endpoint in use may have been disabled or unplugged.
        self.watch.request();

        Ok(())
    }

    fn OnDeviceAdded(&self, _device: &PCWSTR) -> windows::core::Result<()> {
        // A host that had no output at all may have acquired one, and a parked
        // renderer has no other way to hear about it.
        self.watch.request();

        Ok(())
    }

    fn OnDeviceRemoved(&self, _device: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _device: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only this session's own direction and role: a change to the capture
        // default, or to the communications role, is not this renderer's.
        if flow == eRender && role == eConsole {
            self.watch.request();
        }

        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// One session's sound on the host.
pub struct Renderer {
    enumerator: IMMDeviceEnumerator,
    client: Option<IAudioClient>,
    render: Option<IAudioRenderClient>,
    format: Format,
    wave: WAVEFORMATEX,
    muted: bool,
    /// Where the stream was when the last period was played, for the gap a
    /// jump in it stands for.
    last_position: Option<u32>,
    /// Whether the absence of an endpoint has already been reported.
    reported: bool,
    watch: DeviceWatch,
    _notifications: Option<IMMNotificationClient>,
}

impl Renderer {
    /// Builds a renderer around the host's default output.
    ///
    /// # Errors
    ///
    /// [`RendererError`] when COM will not produce an enumerator at all. An
    /// endpoint that cannot be initialised is not an error here: the renderer
    /// parks and revives on the next notification.
    pub fn new(format: Format) -> Result<Self, RendererError> {
        // SAFETY: the caller holds a [`Com`] for this thread -- without one
        // every call below answers `CO_E_NOTINITIALIZED` -- and the class and
        // interface are the ones this crate names.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(RendererError::NoEndpoint)?;
        let watch = DeviceWatch::detached();
        let notifications: IMMNotificationClient = Notifications {
            watch: watch.clone(),
        }
        .into();
        // SAFETY: the callback outlives the registration -- it is held in the
        // renderer, and unregistered in `Drop` before it is released.
        let registered =
            unsafe { enumerator.RegisterEndpointNotificationCallback(&notifications) }.is_ok();

        let mut renderer = Self {
            enumerator,
            client: None,
            render: None,
            format,
            wave: wave_format(format),
            muted: false,
            last_position: None,
            reported: false,
            watch,
            _notifications: registered.then_some(notifications),
        };
        renderer.rebuild();

        Ok(renderer)
    }

    /// The watch its notifications feed, for the thread that pumps it.
    #[must_use]
    pub fn watch(&self) -> DeviceWatch {
        self.watch.clone()
    }

    /// Whether there is an endpoint to play through.
    #[must_use]
    pub fn parked(&self) -> bool {
        self.render.is_none()
    }

    /// Silences the renderer without letting the guest's pacing change.
    ///
    /// A muted renderer keeps taking periods and throwing them away rather
    /// than stopping the stream: that is what makes unmuting immediate, and it
    /// leaves the format the guest pinned alone.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Whether it is muted.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Takes the format the guest is now sending, if it has changed.
    pub fn set_format(&mut self, format: Format) {
        if format == self.format {
            return;
        }

        tracing::info!(
            "the guest's audio format changed to {} Hz, {} channels, {:?}",
            format.sample_rate,
            format.channels,
            format.sample_format
        );
        self.format = format;
        self.wave = wave_format(format);
        self.last_position = None;
        self.rebuild();
    }

    /// Plays one period, or drops it.
    ///
    /// Dropped rather than queued when the endpoint's buffer is full: late
    /// audio is worse than absent audio, and a renderer that waited would make
    /// the guest wait with it.
    pub fn play(&mut self, position: u32, pcm: &[u8]) {
        if self.watch.take_request() {
            self.rebuild();
        }
        if let Some(previous) = self.last_position
            && let gap = frames_between(previous, position)
            && gap > 0
        {
            tracing::debug!("{gap} frames of audio were not sent");
        }
        self.last_position =
            Some(position.wrapping_add(
                u32::try_from(pcm.len() / self.format.bytes_per_frame()).unwrap_or(0),
            ));

        if self.muted {
            return;
        }
        let (Some(client), Some(render)) = (self.client.as_ref(), self.render.as_ref()) else {
            return;
        };

        let frames = u32::try_from(pcm.len() / self.format.bytes_per_frame()).unwrap_or(0);
        if frames == 0 {
            return;
        }

        // SAFETY: the client is initialised and started, and the buffer the
        // endpoint hands back is `frames * nBlockAlign` bytes of its own.
        unsafe {
            let Ok(size) = client.GetBufferSize() else {
                return;
            };
            let Ok(padding) = client.GetCurrentPadding() else {
                return;
            };
            if size - padding < frames {
                tracing::debug!("a period of {frames} frames did not fit and was dropped");

                return;
            }
            let Ok(buffer) = render.GetBuffer(frames) else {
                return;
            };
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), buffer, pcm.len());
            let _ = render.ReleaseBuffer(frames, 0);
        }
    }

    /// Builds the endpoint again, keeping the channel that feeds it.
    ///
    /// A failure parks the renderer rather than ending anything: the guest is
    /// still sending, the channel is still bound, and the next notification is
    /// what revives it.
    fn rebuild(&mut self) {
        self.release();

        match self.activate() {
            Ok((client, render)) => {
                self.client = Some(client);
                self.render = Some(render);
                self.reported = false;
                tracing::info!("the host's audio endpoint is playing this session");
            }
            Err(error) => {
                // Once, not once a period: a host with no output would
                // otherwise fill the log at fifty lines a second.
                if !self.reported {
                    self.reported = true;
                    tracing::warn!("{error}");
                }
            }
        }
    }

    /// Activates the current default endpoint for this session's format.
    fn activate(&self) -> Result<(IAudioClient, IAudioRenderClient), RendererError> {
        // SAFETY: the enumerator is live, and every out-parameter here is one
        // this call owns.
        unsafe {
            let device: IMMDevice = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(RendererError::NoEndpoint)?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(RendererError::Endpoint)?;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_DURATION_100NS,
                    0,
                    &raw const self.wave,
                    None,
                )
                .map_err(RendererError::Endpoint)?;
            let render: IAudioRenderClient =
                client.GetService().map_err(RendererError::Endpoint)?;
            client.Start().map_err(RendererError::Endpoint)?;

            Ok((client, render))
        }
    }

    /// Stops and lets go of the endpoint, if there is one.
    fn release(&mut self) {
        if let Some(client) = self.client.take() {
            // SAFETY: the client is one this renderer started.
            unsafe {
                let _ = client.Stop();
            }
        }
        self.render = None;
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.release();

        if let Some(notifications) = self._notifications.take() {
            // SAFETY: this is the callback registered in `new`, and it is
            // unregistered before it is released.
            unsafe {
                let _ = self
                    .enumerator
                    .UnregisterEndpointNotificationCallback(&notifications);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WAVEFORMATEX` is `#[repr(packed)]`, so its fields are copied out
    /// before they are compared: a reference to one would be unaligned.
    fn fields(wave: &WAVEFORMATEX) -> (u32, u16, u32, u16, u16, u32) {
        (
            u32::from(wave.wFormatTag),
            wave.nChannels,
            wave.nSamplesPerSec,
            wave.wBitsPerSample,
            wave.nBlockAlign,
            wave.nAvgBytesPerSec,
        )
    }

    #[test]
    fn a_pinned_format_becomes_the_wave_format_wasapi_converts_from() {
        let wave = wave_format(Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S16Le,
            frames_per_period: 480,
        });

        assert_eq!(
            fields(&wave),
            (WAVE_FORMAT_PCM, 2, 48_000, 16, 4, 48_000 * 4)
        );
    }

    #[test]
    fn a_float_format_is_tagged_as_float_rather_than_pcm() {
        let wave = wave_format(Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::FloatLe,
            frames_per_period: 480,
        });

        assert_eq!(
            fields(&wave),
            (WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 32, 8, 48_000 * 8)
        );
    }

    #[test]
    fn a_notification_asks_for_a_rebuild_exactly_once() {
        let watch = DeviceWatch::detached();

        watch.request();
        watch.request();

        assert!(watch.take_request(), "the first take sees the request");
        assert!(
            !watch.take_request(),
            "and a flag left set would rebuild on every period"
        );
    }
}
