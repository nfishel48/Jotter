//! Jotter: local meeting capture — your microphone and everyone else's audio as
//! two separate tracks — and the offline passes that turn a recording into a
//! transcript that knows who said what.
//!
//! This is the library the `jotter` command is built on, and it is meant to be
//! used the same way from any other program. Nothing here parses arguments or
//! prints; everything returns values and writes only into the recording
//! directory it was given.
//!
//! # Recording a meeting from a host application
//!
//! The whole flow is three calls: [`audio::start`] opens both streams and
//! begins writing `mic.wav` and `system.wav`; [`RecordingHandle::stop`]
//! finalises them and writes `meta.json`; [`audio::finish`] runs the offline
//! passes — echo cancellation, then transcription, then speaker identification
//! — over the finished directory. The transcript is then an ordinary file,
//! read with [`Transcript::read`].
//!
//! ```no_run
//! use jotter::audio::{self, FinishOptions, RecordConfig, Sources, StageOutcome};
//! use jotter::audio::devices::DeviceChoice;
//! use jotter::audio::transcript::Transcript;
//! use jotter::config::{self, Settings};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = config::recordings_root().join(audio::meta::timestamp_dir_name());
//! let recording = audio::start(RecordConfig {
//!     sources: Sources::Both,
//!     mic: DeviceChoice::Default,
//!     system: DeviceChoice::Default,
//!     out_dir: dir.clone(),
//!     allow_duplex_system: false,
//! })?;
//!
//! // ... the meeting happens. `RecordingHandle` owns the audio streams, which
//! // are not `Send`: stop it on the thread that started it.
//! std::thread::sleep(std::time::Duration::from_secs(60));
//! let meta = recording.stop()?;
//! println!("captured {:.0}s", meta.duration_secs());
//!
//! // The same choices the `jotter` command makes, read from the user's
//! // settings file. Build a `FinishOptions` by hand to decide per call instead.
//! let options = FinishOptions::from_settings(&Settings::load());
//! let report = audio::finish(&dir, &options);
//!
//! // A stage that failed or declined never makes this an `Err`: the audio on
//! // disk is the result, and the report says what happened to it.
//! # #[cfg(feature = "transcribe")]
//! if let StageOutcome::Failed(e) = &report.transcribe {
//!     eprintln!("transcription failed: {e}");
//! }
//! if let Some(path) = report.transcript_path() {
//!     for segment in Transcript::read(path)?.segments {
//!         println!("[{:>7.1}s {}] {}", segment.start, segment.track.as_str(), segment.text);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Transcription and speaker identification need models the library will not
//! download on its own; `models::fetch::fetch` (behind `transcribe`) fetches
//! them deliberately, and until then those stages decline with the reason
//! recorded in `meta.json`.
//!
//! # macOS
//!
//! System-audio capture is gated by TCC, which grants it only to a process with
//! a bundle identity and the usage-description keys in its `Info.plist`. A host
//! application that is itself a signed `.app` bundle needs nothing more than
//! those keys; a bare executable records digital silence on the system track.
//! See `docs/AUDIO_CAPTURE.md` in the repository.
//!
//! # Features
//!
//! Each offline pass is its own feature, so a build can drop the stack behind
//! it entirely:
//!
//! - `aec` — echo cancellation, via WebRTC's AudioProcessing built from C++
//!   source (needs `meson` and `ninja` at build time).
//! - `transcribe` — transcription and the model catalogue, via a statically
//!   linked sherpa-onnx.
//! - `diarize` — speaker identification. Implies `transcribe`.
//! - `telemetry` — anonymous usage reporting into **Jotter's** PostHog project.
//!   Off by default and meant for the `jotter` command only: a host application
//!   that enabled it would report its own usage under Jotter's name.
//!
//! The default is `aec`, `transcribe` and `diarize`. `--no-default-features`
//! leaves capture alone, which needs no C++ toolchain, no ONNX runtime and no
//! network code. [`audio::FinishReport`] has one field per compiled-in stage.
//!
//! [`RecordingHandle::stop`]: audio::RecordingHandle::stop
//! [`Transcript::read`]: audio::transcript::Transcript::read

pub mod audio;
pub mod config;
pub mod telemetry;

/// The speech-model catalogue. Gated with the stage that needs it: a build
/// without transcription has nothing to look a model up for.
#[cfg(feature = "transcribe")]
pub mod models;
