//! The mechanics every offline pass shares.
//!
//! Post-processing is batch-sequential pipes-and-filters: a stage reads a
//! finished recording directory, does its work, writes one artifact beside the
//! audio, and records in `meta.json` what it did or why it declined. Echo
//! cancellation ([`crate::audio::process`]) is the first such stage;
//! transcription is the next, and the two share every part of that sentence
//! except "does its work".
//!
//! So only that shared part lives here — the crash-safe write, the WAV
//! plumbing, and the "has this already been done, by a current version?" check.
//! The decisions a pass makes, and the thresholds behind them, stay with the
//! pass.
//!
//! Deliberately **not** feature-gated, unlike `process`. Each stage sits behind
//! its own cargo feature, and a stage must not have to enable another one to
//! reuse a rename: transcription should never pull in the WebRTC C++ stack.
//! That also makes everything here testable in a build without `aec`, which is
//! most of the point.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::audio::meta::Meta;

/// Why a stage declined to produce its artifact.
///
/// A stage must always be able to say "I decided not to, and here is why". The
/// silent alternative — no output file and no explanation — is
/// indistinguishable from a crash.
///
/// Each stage names its own reasons, since nothing a transcriber declines for
/// makes sense to a canceller. What is shared is the contract those reasons
/// owe: a sentence for a human, and a stable machine-readable name for
/// `meta.json` and telemetry.
pub trait DeclineReason: std::fmt::Display {
    /// A stable, PII-free name, in snake_case. These reach telemetry and
    /// `meta.json`, so they must never be free-form text — the same contract as
    /// [`crate::audio::capture::CaptureError::kind`].
    fn kind(&self) -> &'static str;
}

/// What a stage left behind in `meta.json` the last time it ran.
///
/// A borrowed view rather than a shared struct, because each stage owns the
/// shape of its own block: `AecInfo` carries an ERLE figure and a delay
/// estimate, and a transcript block will carry neither. These three fields are
/// the only ones every stage has, and the only ones worth reasoning about
/// generically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageRecord<'a> {
    /// The stage's [`Stage::version`] at the time it ran.
    pub version: u32,
    /// Relative path of the artifact it wrote, or `None` when it wrote none.
    pub output: Option<&'a str>,
    /// `Some(kind)` when the stage looked and declined, from
    /// [`DeclineReason::kind`].
    pub declined: Option<&'a str>,
}

/// One offline pass over a finished recording.
///
/// Implementing this buys a stage the re-run check, and commits it to the two
/// properties that make a chain of passes debuggable: a version that moves
/// whenever the output would change, and a recorded outcome for every run —
/// including the runs that produced nothing.
pub trait Stage {
    /// The reasons this stage can decline for.
    type Decline: DeclineReason;

    /// Stable identifier for logs and telemetry, e.g. `"aec"`.
    fn name(&self) -> &'static str;

    /// Bumped whenever a change would give a different result for the same
    /// input, so an artifact left over from an older build is detectable rather
    /// than being mistaken for a current one.
    fn version(&self) -> u32;

    /// What this stage recorded in `meta.json`, if it has ever run over this
    /// recording.
    ///
    /// The one stage-specific half of the re-run check below: only the stage
    /// knows which block of `meta.json` is its own.
    fn record<'m>(&self, meta: &'m Meta) -> Option<StageRecord<'m>>;

    /// Whether the recording already carries this stage's artifact, made by
    /// this version of the stage.
    ///
    /// This is what makes `jotter process <dir>` cheap and safe to re-run:
    /// nothing current is recomputed, and an artifact from an older version is
    /// redone rather than trusted. A record with no `output` is never current
    /// whatever its version — a decline wrote no file, so there is nothing to
    /// keep, and the reason it declined may since have been fixed.
    fn is_current(&self, meta: &Meta) -> bool {
        self.record(meta)
            .is_some_and(|r| r.version == self.version() && r.output.is_some())
    }

    /// The string to record in `meta.json` for a decline.
    ///
    /// Always [`DeclineReason::kind`], never `Display`: the sentence is written
    /// for a human and may be reworded freely, whereas the kind is a contract
    /// that aggregate telemetry is grouped by.
    fn declined_kind(&self, decline: Option<&Self::Decline>) -> Option<String> {
        decline.map(|d| d.kind().to_string())
    }
}

/// Writes through a temporary sibling and renames it into place on success.
///
/// The same discipline as `Settings::save_to`, and necessary for every stage
/// because the process can be killed at any moment — a Ctrl-C, a closed
/// terminal: a pass killed mid-write would otherwise leave a truncated file
/// whose RIFF header claims it is complete. A half-written JSON transcript is
/// no better, which is why this takes a closure over the destination rather
/// than anything WAV-shaped.
///
/// `write` is handed the temporary path and may put anything there. If it
/// fails, the rename never happens and `path` is left exactly as it was.
pub fn write_atomic<T, E, F>(path: &Path, write: F) -> Result<T, E>
where
    F: FnOnce(&Path) -> Result<T, E>,
    E: From<std::io::Error>,
{
    let tmp = tmp_path(path);
    let value = write(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(value)
}

/// The temporary sibling [`write_atomic`] writes through.
///
/// Appends to the whole file name rather than replacing the extension, so the
/// artifact's own type stays visible (`mic_aec.wav` → `mic_aec.wav.tmp`) and
/// two stages writing different formats into one directory can never collide.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".tmp");
    PathBuf::from(name)
}

/// Reading and writing the mono i16 WAV files a recording is made of.
///
/// Here rather than in the pass that first needed it, because every stage needs
/// it: transcription reads the same two tracks the canceller does.
pub mod wav {
    use std::path::Path;

    /// A whole track in memory, at its recorded rate.
    ///
    /// Read whole rather than streamed because the passes want random access
    /// across the file — a delay estimate is measured from stretches spread
    /// through it — and mono i16 at 48 kHz is only ~6 MB a minute.
    pub struct Track {
        pub samples: Vec<i16>,
        pub sample_rate: u32,
    }

    pub fn read_track(path: &Path) -> Result<Track, hound::Error> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let samples = reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
        Ok(Track {
            samples,
            sample_rate: spec.sample_rate,
        })
    }

    /// Writes a mono i16 track, through [`super::write_atomic`] so a killed
    /// pass cannot leave a truncated one.
    pub fn write_track(path: &Path, sample_rate: u32, samples: &[i16]) -> Result<(), hound::Error> {
        super::write_atomic(path, |tmp| {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(tmp, spec)?;
            for &sample in samples {
                writer.write_sample(sample)?;
            }
            // Not just a flush: this is what rewrites the RIFF header with the
            // real length. Skip it and many tools refuse to open the file.
            writer.finalize()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::meta::AecInfo;

    /// A unique scratch directory; avoids a `tempfile` dependency, as
    /// `config::tests::scratch` does.
    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-stage-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn meta(stage: Option<AecInfo>) -> Meta {
        Meta {
            started_at: 0.0,
            ended_at: 10.0,
            mic: None,
            system: None,
            aec: stage,
            transcript: None,
            diarization: None,
            live: None,
        }
    }

    /// A stand-in for a real pass, so the shared mechanics can be tested in a
    /// build with no stage features on at all. It reads the `aec` block only
    /// because that is the one stage block `meta.json` has today.
    struct TestStage(u32);

    #[derive(Debug)]
    struct TestDecline;

    impl std::fmt::Display for TestDecline {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "nothing to do")
        }
    }

    impl DeclineReason for TestDecline {
        fn kind(&self) -> &'static str {
            "nothing_to_do"
        }
    }

    impl Stage for TestStage {
        type Decline = TestDecline;

        fn name(&self) -> &'static str {
            "test"
        }

        fn version(&self) -> u32 {
            self.0
        }

        fn record<'m>(&self, meta: &'m Meta) -> Option<StageRecord<'m>> {
            meta.aec.as_ref().map(|info| StageRecord {
                version: info.version,
                output: info.path.as_deref(),
                declined: info.bypassed.as_deref(),
            })
        }
    }

    fn ran(version: u32) -> AecInfo {
        AecInfo {
            path: Some("out.wav".into()),
            version,
            ..AecInfo::default()
        }
    }

    /// The whole reason `jotter process <dir>` is cheap to re-run. Getting this
    /// backwards either redoes minutes of work on every invocation or, worse,
    /// keeps an artifact an algorithm change has invalidated.
    #[test]
    fn a_current_artifact_is_recognised_and_a_stale_one_is_not() {
        let stage = TestStage(2);

        assert!(stage.is_current(&meta(Some(ran(2)))));
        assert!(!stage.is_current(&meta(Some(ran(1)))));
        // Version bumps are not ordered comparisons: anything but the current
        // version means "made by a different build", including a newer one.
        assert!(!stage.is_current(&meta(Some(ran(3)))));
        // Never run at all.
        assert!(!stage.is_current(&meta(None)));
    }

    /// A decline is reconsidered on every run — the recording may have been
    /// repaired, or the pass taught to handle it — so a block with a reason and
    /// no artifact must not read as finished work.
    #[test]
    fn a_recorded_decline_is_never_current() {
        let declined = AecInfo {
            path: None,
            version: 2,
            bypassed: Some("nothing_to_do".into()),
            ..AecInfo::default()
        };
        assert!(!TestStage(2).is_current(&meta(Some(declined))));
    }

    /// `kind()`, never `Display`: the sentence carries durations and
    /// device-shaped detail, and it is not a stable grouping key.
    #[test]
    fn a_decline_is_recorded_by_kind_not_by_its_message() {
        let stage = TestStage(1);
        assert_eq!(
            stage.declined_kind(Some(&TestDecline)),
            Some("nothing_to_do".into())
        );
        assert_eq!(stage.declined_kind(None), None);
    }

    #[test]
    fn write_atomic_renames_the_finished_file_into_place() {
        let dir = scratch("rename");
        let path = dir.join("artifact.json");

        write_atomic(&path, |tmp| std::fs::write(tmp, b"{\"ok\":true}"))
            .expect("write should succeed");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"ok\":true}"
        );
        // The sibling is gone, not merely unused: it was renamed, not copied.
        assert!(!tmp_path(&path).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug the tmp+rename discipline exists for. The process can be killed
    /// at any moment, so a pass can die at any point inside the closure;
    /// whatever it had written by then must not be sitting at the destination
    /// looking like a finished artifact.
    #[test]
    fn a_failed_write_leaves_nothing_at_the_destination() {
        let dir = scratch("failed");
        let path = dir.join("artifact.wav");

        let result: Result<(), std::io::Error> = write_atomic(&path, |tmp| {
            std::fs::write(tmp, b"half a file")?;
            Err(std::io::Error::other("killed mid-write"))
        });
        assert!(result.is_err());
        assert!(!path.exists(), "a partial artifact was published");

        // And an artifact already in place survives a failed attempt to replace
        // it — for AEC that file is the only cleaned copy of the recording.
        std::fs::write(&path, b"the previous good one").expect("seed");
        let result: Result<(), std::io::Error> =
            write_atomic(&path, |_| Err(std::io::Error::other("killed mid-write")));
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the previous good one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temporary file must be distinguishable from a finished artifact, or
    /// a killed pass leaves something a later run would mistake for output.
    #[test]
    fn the_temporary_sibling_keeps_the_artifact_extension() {
        let tmp = tmp_path(Path::new("/tmp/recording/mic_aec.wav"));
        assert_eq!(tmp, Path::new("/tmp/recording/mic_aec.wav.tmp"));
        assert_ne!(tmp, PathBuf::from("/tmp/recording/mic_aec.wav"));

        // Not `with_extension`, which would collapse two artifacts of different
        // formats onto the same temporary name.
        assert_ne!(
            tmp_path(Path::new("/tmp/recording/transcript.json")),
            tmp_path(Path::new("/tmp/recording/transcript.wav"))
        );
    }

    /// Round-trips through the real hound plumbing, since the sample type and
    /// channel count are what the rest of the pipeline assumes.
    #[test]
    fn a_wav_track_round_trips_through_the_helpers() {
        let dir = scratch("wav");
        let path = dir.join("track.wav");
        let samples: Vec<i16> = (0..480).map(|i| (i * 37 - 8_000) as i16).collect();

        wav::write_track(&path, 48_000, &samples).expect("write");
        let track = wav::read_track(&path).expect("read");

        assert_eq!(track.sample_rate, 48_000);
        assert_eq!(track.samples, samples);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
