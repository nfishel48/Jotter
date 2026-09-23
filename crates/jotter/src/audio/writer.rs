//! WAV writing, off the audio thread.
//!
//! cpal's data callback runs on a realtime audio thread; blocking it on file
//! I/O causes dropouts. So the callback only does cheap work (downmix to mono,
//! convert to `i16`) and hands an owned buffer to a writer thread over a
//! channel.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use super::capture::CaptureError;
use super::meta::TrackInfo;

/// Handle to one track's writer thread.
pub struct TrackWriter {
    tx: Option<Sender<Vec<i16>>>,
    thread: JoinHandle<Result<u64, hound::Error>>,
    /// Bare file name, not the full path: it is what lands in `meta.json`, and
    /// everything there is relative to the recording directory so a recording
    /// stays self-describing after it is moved or copied.
    file_name: String,
    device_name: String,
    device_id: Option<String>,
    sample_rate: u32,
    source_channels: u16,
    first_callback_nanos: Arc<AtomicU64>,
    stream_errors: Arc<AtomicU64>,
}

/// The callback-side half: everything the audio thread needs, and nothing that
/// would block it.
pub struct TrackSink {
    tx: Sender<Vec<i16>>,
    source_channels: u16,
    first_callback_nanos: Arc<AtomicU64>,
}

/// Sentinel for "no callback seen yet". A real `StreamInstant` of exactly zero
/// nanoseconds is not meaningfully distinguishable from unset here.
const UNSET: u64 = u64::MAX;

impl TrackSink {
    /// Called from the audio thread for every buffer.
    pub fn push<T>(&self, data: &[T], first_callback_nanos: u128)
    where
        T: cpal::Sample,
        f32: cpal::FromSample<T>,
    {
        self.first_callback_nanos
            .compare_exchange(
                UNSET,
                first_callback_nanos.min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok();

        let out = downmix_to_mono(data, self.source_channels.max(1) as usize);

        // A full or disconnected channel means the writer thread is gone or
        // hopelessly behind. Dropping the buffer is the only realtime-safe
        // option; the frame count in meta.json will reflect the loss.
        let _ = self.tx.send(out);
    }
}

/// Collapse interleaved samples to mono `i16` by averaging each frame.
///
/// System audio arrives stereo; the mic is already mono, in which case this is
/// a straight conversion. Split out of `push` so the conversion can be tested
/// without standing up a cpal stream.
fn downmix_to_mono<T>(data: &[T], channels: usize) -> Vec<i16>
where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    let channels = channels.max(1);
    let mut out = Vec::with_capacity(data.len() / channels);
    for frame in data.chunks_exact(channels) {
        let sum: f32 = frame
            .iter()
            .map(|s| cpal::Sample::to_sample::<f32>(*s))
            .sum();
        out.push(f32_to_i16(sum / channels as f32));
    }
    out
}

fn f32_to_i16(sample: f32) -> i16 {
    // Clamp before scaling: loopback audio can exceed [-1.0, 1.0] when an app
    // applies its own gain, and wrapping there would turn a loud passage into
    // harsh noise that wrecks transcription.
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

impl TrackWriter {
    /// Takes the recording directory and a file name rather than a whole path,
    /// and does the join itself. The name written to `meta.json` is then the
    /// same string that named the file on disk, so the two cannot disagree —
    /// which is exactly how `meta.json` came to hold absolute paths from the
    /// GUI and cwd-relative ones from the CLI.
    pub fn new(
        dir: &Path,
        file_name: &str,
        device_name: String,
        device_id: Option<String>,
        sample_rate: u32,
        source_channels: u16,
    ) -> Result<(TrackWriter, TrackSink), CaptureError> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let writer = hound::WavWriter::create(dir.join(file_name), spec)?;
        let (tx, rx): (Sender<Vec<i16>>, Receiver<Vec<i16>>) = mpsc::channel();

        let thread = std::thread::spawn(move || {
            let mut writer = writer;
            let mut frames: u64 = 0;
            for buf in rx {
                for sample in buf {
                    writer.write_sample(sample)?;
                    frames += 1;
                }
            }
            // Finalize writes the real RIFF length. Without it the file has a
            // placeholder header and many tools refuse to open it.
            writer.finalize()?;
            Ok(frames)
        });

        let first_callback_nanos = Arc::new(AtomicU64::new(UNSET));
        let stream_errors = Arc::new(AtomicU64::new(0));

        let sink = TrackSink {
            tx: tx.clone(),
            source_channels,
            first_callback_nanos: Arc::clone(&first_callback_nanos),
        };

        Ok((
            TrackWriter {
                tx: Some(tx),
                thread,
                file_name: file_name.to_string(),
                device_name,
                device_id,
                sample_rate,
                source_channels,
                first_callback_nanos,
                stream_errors,
            },
            sink,
        ))
    }

    /// Shared counter for cpal's error callback to bump.
    pub fn error_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.stream_errors)
    }

    /// Close the channel, wait for the writer thread, and report the track.
    pub fn finish(mut self) -> Result<TrackInfo, CaptureError> {
        // Dropping the last sender ends the writer thread's `for buf in rx`.
        self.tx.take();

        let frames = self
            .thread
            .join()
            .map_err(|_| CaptureError::WriterPanicked)??;

        let first = self.first_callback_nanos.load(Ordering::Relaxed);

        Ok(TrackInfo {
            path: self.file_name,
            device_name: self.device_name,
            device_id: self.device_id,
            sample_rate: self.sample_rate,
            channels: 1,
            source_channels: self.source_channels,
            frames,
            first_callback_nanos: (first != UNSET).then_some(first as u128),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_instead_of_wrapping() {
        // The bug this guards: without the clamp, `as i16` on an out-of-range
        // value wraps, turning a loud passage into harsh noise. Loopback audio
        // genuinely exceeds +/-1.0 when an app applies its own gain.
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(0.0), 0);
    }

    #[test]
    fn mono_passes_through() {
        assert_eq!(
            downmix_to_mono(&[0.0f32, 1.0, -1.0], 1),
            [0, i16::MAX, -i16::MAX]
        );
    }

    #[test]
    fn stereo_averages_each_frame() {
        // L=1.0 R=-1.0 cancels; L=R=0.5 stays 0.5.
        let out = downmix_to_mono(&[1.0f32, -1.0, 0.5, 0.5], 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], f32_to_i16(0.5));
    }

    #[test]
    fn ignores_trailing_partial_frame() {
        // chunks_exact drops a partial frame rather than reading past it or
        // fabricating a channel.
        assert_eq!(downmix_to_mono(&[1.0f32, 1.0, 1.0], 2).len(), 1);
    }

    #[test]
    fn zero_channels_does_not_divide_by_zero() {
        // source_channels comes from the driver; 0 would panic on divide.
        assert_eq!(downmix_to_mono(&[1.0f32], 0), [i16::MAX]);
    }
}
