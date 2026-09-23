//! Meeting audio capture: your microphone and everyone else's audio, as two
//! separate tracks.
//!
//! Both sources go through cpal. The microphone is an ordinary input stream.
//! System audio ("everyone else") is captured by calling `build_input_stream`
//! on an *output* device — cpal turns that into a loopback capture. See
//! [`capture::open_loopback`] for why the choice of output device matters.

#[cfg(feature = "aec")]
pub mod aec;
pub mod capture;
pub mod devices;
#[cfg(feature = "diarize")]
pub mod diarize;
// Ungated, like `transcript`: reading `live.jsonl` and the config and status
// types must not need the inference stack. Only the worker inside is gated.
pub mod live;
pub mod meta;
// Private, re-exported below: `audio::finish` is the name callers should use,
// and a public `pipeline` module would be a second path to the same items.
mod pipeline;
#[cfg(feature = "aec")]
pub mod process;
// Ungated on purpose, unlike the passes built on it: the shared stage
// mechanics must not sit behind any one stage's feature.
pub mod stage;
#[cfg(feature = "transcribe")]
pub mod transcribe;
// Ungated for the same reason `meta` is: reading a transcript and producing one
// are different jobs, and only the second needs the inference stack.
pub mod transcript;
pub mod writer;

pub use pipeline::{
    FinishOptions, FinishProgressFn, FinishReport, FinishStage, Skip, StageOutcome, finish,
    finish_with_progress,
};

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
    /// The sources in both, if any. Live transcription uses it to work out
    /// which recorded tracks it was actually asked for.
    #[cfg(feature = "transcribe")]
    fn intersect(self, other: Sources) -> Option<Sources> {
        match (
            self.wants_mic() && other.wants_mic(),
            self.wants_system() && other.wants_system(),
        ) {
            (true, true) => Some(Self::Both),
            (true, false) => Some(Self::MicOnly),
            (false, true) => Some(Self::SystemOnly),
            (false, false) => None,
        }
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
    /// Transcribe while recording, into `live.jsonl` beside the audio — see
    /// [`live`]. `None` is a plain recording: no worker, no copies of the
    /// audio, nothing different on disk.
    pub live: Option<live::LiveConfig>,
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
    live: Option<live::LiveTranscriber>,
}

impl RecordingHandle {
    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// How live transcription is getting on, or `None` when it was not asked
    /// for. A decline or a failure shows up here, while the recording carries
    /// on regardless.
    pub fn live_status(&self) -> Option<live::LiveStatus> {
        self.live.as_ref().map(live::LiveTranscriber::status)
    }
}

/// Open the requested streams and begin capturing.
pub fn start(config: RecordConfig) -> Result<RecordingHandle, CaptureError> {
    std::fs::create_dir_all(&config.out_dir)?;

    // Before the streams, which take their live feeds as they are built. The
    // worker loads its model in the background; capture does not wait for it,
    // and the audio it misses meanwhile waits in the queue.
    let live = config
        .live
        .as_ref()
        .map(|live| live::LiveTranscriber::start(&config.out_dir, live, config.sources));

    let mic = config
        .sources
        .wants_mic()
        .then(|| capture::open_mic(config.mic, &config.out_dir, meta::MIC_NAME, live.as_ref()))
        .transpose()?;

    let system = config
        .sources
        .wants_system()
        .then(|| {
            capture::open_loopback(
                config.system,
                &config.out_dir,
                meta::SYSTEM_NAME,
                config.allow_duplex_system,
                live.as_ref(),
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
        live,
    })
}

impl RecordingHandle {
    /// Stop capturing, flush the WAV files, finish the live transcript if
    /// there is one, and write `meta.json`.
    pub fn stop(self) -> Result<meta::Meta, CaptureError> {
        let RecordingHandle {
            mic,
            system,
            out_dir,
            started_at,
            live,
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
        let ended_at = SystemTime::now();

        // After the streams are gone, so every buffer the worker will ever get
        // is already queued and what it drains now is the whole tail. Bounded:
        // see `LiveTranscriber::stop`.
        let live = live.map(live::LiveTranscriber::stop);

        let meta = meta::Meta {
            started_at: meta::to_unix_secs(started_at),
            ended_at: meta::to_unix_secs(ended_at),
            mic,
            system,
            // Filled in afterwards by `finish`, not here: `stop()` only
            // finalises the audio, so a caller that needs the devices released
            // promptly is never held up by minutes of transcription.
            aec: None,
            transcript: None,
            diarization: None,
            live,
        };
        meta.write(&out_dir.join("meta.json"))?;
        Ok(meta)
    }
}
