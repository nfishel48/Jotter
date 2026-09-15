//! Meeting audio capture: your microphone and everyone else's audio, as two
//! separate tracks.
//!
//! Both sources go through cpal. The microphone is an ordinary input stream.
//! System audio ("everyone else") is captured by calling `build_input_stream`
//! on an *output* device — cpal turns that into a loopback capture. See
//! [`capture::open_loopback`] for why the choice of output device matters.

pub mod capture;
pub mod devices;
pub mod meta;
pub mod writer;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use cpal::traits::StreamTrait;

use capture::{CaptureError, OpenStream};
use devices::DeviceChoice;

/// Which sources to record.
///
/// Both are independently switchable mainly so the two macOS privacy
/// permissions can be exercised one at a time — they are granted separately
/// and fail separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sources {
    Both,
    MicOnly,
    SystemOnly,
}

impl Sources {
    fn wants_mic(self) -> bool {
        matches!(self, Self::Both | Self::MicOnly)
    }
    fn wants_system(self) -> bool {
        matches!(self, Self::Both | Self::SystemOnly)
    }
}

/// What to record and where to put it.
pub struct RecordConfig {
    pub sources: Sources,
    pub mic: DeviceChoice,
    pub system: DeviceChoice,
    pub out_dir: PathBuf,
    /// Open the system-audio stream even if the device is duplex. Diagnostic
    /// only: on a duplex device cpal records the microphone instead of system
    /// audio, so this is almost never what you want.
    pub allow_duplex_system: bool,
}

/// A recording in progress.
///
/// Holds the cpal streams, which are `!Send` — this value must stay on the
/// thread that created it.
pub struct RecordingHandle {
    mic: Option<OpenStream>,
    system: Option<OpenStream>,
    out_dir: PathBuf,
    started_at: SystemTime,
}

impl RecordingHandle {
    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }
}

/// Open the requested streams and begin capturing.
pub fn start(config: RecordConfig) -> Result<RecordingHandle, CaptureError> {
    std::fs::create_dir_all(&config.out_dir)?;

    let mic = config
        .sources
        .wants_mic()
        .then(|| capture::open_mic(config.mic, &config.out_dir.join("mic.wav")))
        .transpose()?;

    let system = config
        .sources
        .wants_system()
        .then(|| {
            capture::open_loopback(
                config.system,
                &config.out_dir.join("system.wav"),
                config.allow_duplex_system,
            )
        })
        .transpose()?;

    // cpal 0.17 stopped auto-starting streams on build; they must be played.
    for stream in [mic.as_ref(), system.as_ref()].into_iter().flatten() {
        stream.stream.play()?;
    }

    Ok(RecordingHandle {
        mic,
        system,
        out_dir: config.out_dir,
        started_at: SystemTime::now(),
    })
}

impl RecordingHandle {
    /// Stop capturing, flush the WAV files, and write `meta.json`.
    pub fn stop(self) -> Result<meta::Meta, CaptureError> {
        let RecordingHandle {
            mic,
            system,
            out_dir,
            started_at,
        } = self;

        // Pause before tearing down the writers so no callback races the
        // channel close.
        let finish = |open: Option<OpenStream>| -> Result<Option<meta::TrackInfo>, CaptureError> {
            let Some(open) = open else { return Ok(None) };
            let _ = open.stream.pause();
            drop(open.stream);
            open.track.finish().map(Some)
        };

        let mic = finish(mic)?;
        let system = finish(system)?;

        let meta = meta::Meta {
            started_at: meta::to_unix_secs(started_at),
            ended_at: meta::to_unix_secs(SystemTime::now()),
            mic,
            system,
        };
        meta.write(&out_dir.join("meta.json"))?;
        Ok(meta)
    }
}
