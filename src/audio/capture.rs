//! cpal stream construction.
//!
//! Microphone and system audio are deliberately separate functions rather than
//! one parameterised helper: they differ in which config accessor applies and
//! in which failure modes matter, and hiding that behind a shared abstraction
//! is what makes the loopback footgun easy to trip.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat, Stream, SupportedStreamConfig};

use super::devices::{self, DeviceChoice, DeviceInfo};
use super::writer::{TrackSink, TrackWriter};

/// Which track a failure belongs to. Without this the two streams' errors are
/// indistinguishable, and they fail for very different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mic,
    System,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mic => "microphone",
            Self::System => "system audio",
        }
    }

    /// Platform-specific advice for a failure that looks like denied access.
    ///
    /// The two platforms fail for unrelated reasons — macOS gates capture
    /// behind TCC, while on Linux it is normally the PipeWire daemon being
    /// absent or unreachable — so generic wording would help with neither.
    fn access_hint(self) -> &'static str {
        #[cfg(target_os = "macos")]
        {
            match self {
                Self::Mic => {
                    "Grant access under System Settings → Privacy & Security → Microphone.\n\
                     Because an unbundled binary has no identity of its own, macOS \
                     attributes the request to the terminal running it. Run from \
                     build/Jotter.app instead — see docs/AUDIO_CAPTURE.md."
                }
                Self::System => {
                    "Grant access under System Settings → Privacy & Security → \
                     Screen & System Audio Recording.\n\
                     An unbundled binary cannot be granted this at all: macOS feeds \
                     the tap digital silence instead of prompting. Run from \
                     build/Jotter.app — see docs/AUDIO_CAPTURE.md."
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            match self {
                Self::Mic => {
                    "Check that PipeWire is running (`systemctl --user status pipewire`) \
                     and that the device is not exclusively held by another client \
                     (`pw-top`)."
                }
                Self::System => {
                    "System audio needs the PipeWire host, which requires both a running \
                     daemon and a build with the `pipewire` feature.\n\
                     Check `systemctl --user status pipewire` and `wpctl status`. If cpal \
                     fell back to ALSA, sinks are not capturable and this will not work."
                }
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = self;
            "Check that the audio backend is running and the device is available."
        }
    }
}

#[derive(Debug)]
pub enum CaptureError {
    /// A failure attributable to one specific stream.
    Stream {
        source: Source,
        err: cpal::Error,
    },
    Cpal(cpal::Error),
    Wav(hound::Error),
    Io(std::io::Error),
    NoSuchDevice(String),
    NoInputDevice,
    NoOutputDevice,
    /// The chosen system-audio device also exposes an input.
    ///
    /// cpal only takes its loopback branch when a device reports no input
    /// support. On a duplex device it silently opens an ordinary capture
    /// stream, so the "system audio" file would contain the microphone. That
    /// failure is invisible until transcription, so it is refused up front.
    DuplexSystemDevice {
        name: String,
    },
    UnsupportedSampleFormat(SampleFormat),
    WriterPanicked,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stream { source, err } => {
                // cpal's own Display prints only the backend message and drops
                // the ErrorKind, which is where the useful signal lives —
                // "Illegal operation" vs PermissionDenied read very
                // differently. Print both.
                write!(
                    f,
                    "{} capture failed: {} ({:?})",
                    source.label(),
                    err,
                    err.kind()
                )?;
                if permission_shaped(err.kind()) {
                    write!(f, "\n\n{}", source.access_hint())?;
                }
                Ok(())
            }
            Self::Cpal(e) => write!(f, "audio error: {e} ({:?})", e.kind()),
            Self::Wav(e) => write!(f, "wav error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::NoSuchDevice(id) => write!(f, "no device with id {id}"),
            Self::NoInputDevice => write!(f, "no input device available"),
            Self::NoOutputDevice => write!(f, "no output device available"),
            Self::DuplexSystemDevice { name } => write!(
                f,
                "{name:?} reports both input and output, so cpal would record its \
                 microphone instead of system audio.\n\
                 Pick an output-only device — `jotter devices` marks which ones \
                 those are — or pass `--force-system-on-duplex` to override for \
                 diagnosis."
            ),
            Self::UnsupportedSampleFormat(fmt) => {
                write!(f, "unsupported sample format: {fmt:?}")
            }
            Self::WriterPanicked => write!(f, "wav writer thread panicked"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Error kinds that plausibly mean "the OS refused access".
///
/// `PermissionDenied` is the honest one, but a TCC prompt that is dismissed or
/// left unanswered surfaces from CoreAudio as an unclassified backend status
/// instead, so those are worth the same hint.
fn permission_shaped(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::PermissionDenied
            | cpal::ErrorKind::UnsupportedOperation
            | cpal::ErrorKind::BackendError
            | cpal::ErrorKind::DeviceNotAvailable
    )
}

impl From<cpal::Error> for CaptureError {
    fn from(e: cpal::Error) -> Self {
        Self::Cpal(e)
    }
}
impl From<hound::Error> for CaptureError {
    fn from(e: hound::Error) -> Self {
        Self::Wav(e)
    }
}
impl From<std::io::Error> for CaptureError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A started stream and the writer that drains it.
pub struct OpenStream {
    pub stream: Stream,
    pub track: TrackWriter,
    pub info: DeviceInfo,
}

/// Open the microphone as an ordinary input stream.
pub fn open_mic(choice: DeviceChoice, path: &Path) -> Result<OpenStream, CaptureError> {
    let (device, info) = devices::resolve_mic(choice)?;
    let config = device
        .default_input_config()
        .map_err(|err| CaptureError::Stream {
            source: Source::Mic,
            err,
        })?;
    build(device, info, config, path, Source::Mic)
}

/// Open a loopback capture of system audio.
///
/// There is no explicit loopback API in cpal. Calling `build_input_stream` on a
/// device that reports no input support is the idiom on every backend, though
/// each implements it differently:
///
/// - **CoreAudio** builds a `CATapDescription` over all processes plus a
///   private aggregate device (`host/coreaudio/macos/device.rs`). The tap is
///   unmuted, so the meeting still plays out of the speakers.
/// - **WASAPI** sets `AUDCLNT_STREAMFLAGS_LOOPBACK`.
/// - **PipeWire** sets `STREAM_CAPTURE_SINK` when the target node's role is a
///   sink (`host/pipewire/device.rs`), which is PipeWire's monitor capture.
///   Requires cpal's `pipewire` feature — enabled for Linux in Cargo.toml.
///   The ALSA fallback has no equivalent, so if the PipeWire daemon is not
///   running this silently becomes an ordinary capture attempt.
///
/// Conveniently the `supports_input()` test is meaningful on all three: the
/// PipeWire host overrides it to report direction rather than probing configs,
/// so a sink answers `false` just as a CoreAudio output device does.
///
/// Two consequences, both load-bearing:
///
/// 1. The device must not support input, or cpal records the microphone
///    instead. That is what `allow_duplex` overrides, and it is a footgun
///    rather than a feature.
/// 2. The config must come from `default_output_config()` — the device has no
///    input config to ask for.
pub fn open_loopback(
    choice: DeviceChoice,
    path: &Path,
    allow_duplex: bool,
) -> Result<OpenStream, CaptureError> {
    let (device, info) = devices::resolve_system(choice)?;

    if info.supports_input && !allow_duplex {
        return Err(CaptureError::DuplexSystemDevice {
            name: info.name.clone(),
        });
    }

    let config = device
        .default_output_config()
        .map_err(|err| CaptureError::Stream {
            source: Source::System,
            err,
        })?;
    build(device, info, config, path, Source::System)
}

fn build(
    device: Device,
    info: DeviceInfo,
    config: SupportedStreamConfig,
    path: &Path,
    source: Source,
) -> Result<OpenStream, CaptureError> {
    let sample_format = config.sample_format();
    let sample_rate = config.sample_rate();
    let channels = config.channels();
    let stream_config = config.config();

    let (track, sink) = TrackWriter::new(
        path,
        info.name.clone(),
        info.id.clone(),
        sample_rate,
        channels,
    )?;
    let errors = track.error_counter();

    let stream = match sample_format {
        SampleFormat::F32 => build_typed::<f32>(&device, &stream_config, sink, errors),
        SampleFormat::I16 => build_typed::<i16>(&device, &stream_config, sink, errors),
        SampleFormat::U16 => build_typed::<u16>(&device, &stream_config, sink, errors),
        SampleFormat::I32 => build_typed::<i32>(&device, &stream_config, sink, errors),
        other => return Err(CaptureError::UnsupportedSampleFormat(other)),
    }
    .map_err(|err| CaptureError::Stream { source, err })?;

    Ok(OpenStream {
        stream,
        track,
        info,
    })
}

fn build_typed<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    sink: TrackSink,
    errors: std::sync::Arc<AtomicU64>,
) -> Result<Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let stream = device.build_input_stream(
        *config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            sink.push(data, info.timestamp().callback.as_nanos());
        },
        move |err| {
            // Counted rather than just logged: a stream that dies partway
            // through a meeting otherwise leaves a short file and no evidence.
            errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("stream error: {err}");
        },
        None,
    )?;
    Ok(stream)
}
