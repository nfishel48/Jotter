//! Acoustic echo cancellation: removing speaker bleed from the microphone track.
//!
//! When a meeting is held on laptop speakers rather than headphones, the
//! microphone re-captures the remote participants. Every remote voice then lands
//! in *both* recorded tracks, which defeats the point of recording two of them
//! and feeds transcription a doubled copy of the remote side.
//!
//! This wraps WebRTC's AEC3, via `webrtc-audio-processing`. Two things about the
//! shape of it are worth knowing before reading on:
//!
//! * The canceller is fed the *system* track as its render (far-end) stream and
//!   the *mic* track as its capture (near-end) stream. It is not a mixer —
//!   `system.wav` is an input to the estimate, never part of the output.
//! * **The two tracks are fed unaligned.** AEC3 estimates the echo delay itself,
//!   and trusting it rather than pre-shifting the audio removes a stage — and a
//!   class of bug — from our side: an alignment slightly too large asks the
//!   filter to model an echo arriving before its cause, which nothing can
//!   express. [`delay`] still measures the delay, but only to report it and to
//!   catch the cases where no single delay could work at all.
//!
//! # Why not write one
//!
//! A hand-rolled canceller over vendored Speex MDF was built first. It worked —
//! correct delay estimate, no damage to the near end — and reached 1.8 dB on the
//! reference recording. AEC3, on the same files, reaches 20 dB on echo-only
//! passages and 17.6 dB through double-talk for 0.29 dB of near-end loss.
//!
//! The gap is not a detail of tuning. AEC3 models the *nonlinear* part of the
//! echo path, which is most of it when the source is a laptop speaker driven
//! loud, and no linear adaptive filter can touch that however long its tail.
//! Two independent linear-only measurements of the same recording predicted a
//! ceiling in the low single digits; AEC3 cleared it by 30 dB, so the premise
//! those measurements rested on was simply wrong.

pub mod delay;

use webrtc_audio_processing::{Config, Processor};
// Aliased: the upstream enum selects *which* canceller, and the name is
// wanted here for the wrapper itself.
use webrtc_audio_processing_config::EchoCanceller as Aec3Mode;

/// Frame RMS, in `i16` units, below which a frame is treated as carrying no
/// signal.
///
/// 300 is the noise-floor threshold that reproduced the activity census of the
/// reference recording. It is a floor rather than the whole rule — see
/// [`active_threshold`].
pub const FLOOR_RMS: f32 = 300.0;

/// How the canceller is configured.
///
/// Just the sample rate, and deliberately so. AEC3's internals — filter length,
/// the nonlinear residual suppressor, the delay estimator — are not exposed by
/// the stable API, and that is the right trade: there is nothing here to get
/// wrong. A `--no-suppression` flag was written and then removed, because the
/// suppressor cannot be turned off through this API and a flag that silently
/// does nothing is worse than no flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AecConfig {
    pub sample_rate: u32,
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
        }
    }
}

impl AecConfig {
    pub fn for_rate(sample_rate: u32) -> Self {
        Self { sample_rate }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AecError {
    /// AEC3 rejected the sample rate, or could not allocate.
    Init(String),
    /// A frame was not [`EchoCanceller::frame_size`] samples long.
    FrameSize { expected: usize, got: usize },
}

impl std::fmt::Display for AecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(why) => write!(f, "could not start the echo canceller: {why}"),
            Self::FrameSize { expected, got } => {
                write!(f, "expected {expected} samples per frame, got {got}")
            }
        }
    }
}

impl std::error::Error for AecError {}

/// What a cancellation run achieved.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AecStats {
    pub frames: u64,
    /// Echo return loss enhancement over frames holding echo and no near-end
    /// speech: `10*log10(near_energy / output_energy)`. The headline figure.
    pub erle_db: Option<f32>,
    /// Level change over frames where the far end was *silent*. Near 0 is good;
    /// clearly negative means the canceller is eating the user's own voice,
    /// which is the failure an ERLE figure cannot see.
    pub near_gain_db: Option<f32>,
    /// Level change over double-talk frames.
    ///
    /// Reported, not gated: the frame holds both the echo (removable) and the
    /// user's voice (not), so the right answer depends on their ratio.
    pub double_talk_gain_db: Option<f32>,
    /// AEC3's own estimate of the echo delay, in milliseconds. An independent
    /// check on [`delay`]'s measurement, and the thing to look at first when a
    /// recording cancels badly.
    pub reported_delay_ms: Option<u32>,
}

/// Owns the AEC3 processor.
pub struct EchoCanceller {
    processor: Processor,
    frame_size: usize,
    render: Vec<Vec<f32>>,
    capture: Vec<Vec<f32>>,
}

impl EchoCanceller {
    pub fn new(config: AecConfig) -> Result<Self, AecError> {
        let processor =
            Processor::new(config.sample_rate).map_err(|e| AecError::Init(e.to_string()))?;
        let frame_size = processor.num_samples_per_frame();

        processor.set_config(Config {
            // `stream_delay_ms: None` is what lets AEC3 run its own delay
            // estimator, which is the whole reason the tracks are fed unaligned.
            echo_canceller: Some(Aec3Mode::Full {
                stream_delay_ms: None,
            }),
            // Strongly recommended alongside echo cancellation by the upstream
            // docs, and cheap: the mic's sub-80 Hz content is handling rumble,
            // never speech, and it only makes the echo estimate harder.
            high_pass_filter: Some(Default::default()),
            // Deliberately nothing else. Noise suppression and AGC would change
            // the user's voice for reasons unrelated to echo, and this pass
            // exists to remove echo — anything more is a decision for the
            // transcription step, which can see the whole recording.
            ..Default::default()
        });

        Ok(Self {
            processor,
            frame_size,
            render: vec![vec![0.0; frame_size]],
            capture: vec![vec![0.0; frame_size]],
        })
    }

    /// Samples per call. 10 ms at the configured rate, fixed by AEC3.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Cancels one frame.
    ///
    /// `near` is the microphone, `far` is what went to the speakers, and `out`
    /// receives the near end with the echo removed. All three must be
    /// [`Self::frame_size`] samples long.
    ///
    /// The render stream is submitted first, as AEC3 requires: it has to know
    /// what was played before it can recognise it coming back.
    pub fn cancel_frame(
        &mut self,
        near: &[i16],
        far: &[i16],
        out: &mut [i16],
    ) -> Result<(), AecError> {
        for (label, len) in [("near", near.len()), ("far", far.len()), ("out", out.len())] {
            let _ = label;
            if len != self.frame_size {
                return Err(AecError::FrameSize {
                    expected: self.frame_size,
                    got: len,
                });
            }
        }

        for (dst, &src) in self.render[0].iter_mut().zip(far) {
            *dst = i16_to_f32(src);
        }
        for (dst, &src) in self.capture[0].iter_mut().zip(near) {
            *dst = i16_to_f32(src);
        }

        self.processor
            .process_render_frame(&mut self.render)
            .map_err(|e| AecError::Init(e.to_string()))?;
        self.processor
            .process_capture_frame(&mut self.capture)
            .map_err(|e| AecError::Init(e.to_string()))?;

        for (dst, &src) in out.iter_mut().zip(&self.capture[0]) {
            *dst = f32_to_i16(src);
        }
        Ok(())
    }

    /// AEC3's own view of what it is doing — delay estimate, echo return loss,
    /// residual echo likelihood.
    pub fn reported_delay_ms(&self) -> Option<u32> {
        self.processor.get_stats().delay_ms
    }
}

/// `i16` to the `[-1.0, 1.0]` range AEC3 expects.
fn i16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / -(i16::MIN as f32)
}

/// Back to `i16`, clamping first.
///
/// The clamp is not decorative: the residual after subtracting an echo estimate
/// can exceed the input's magnitude, and `as i16` on an out-of-range float
/// saturates in Rust but the intent should be explicit. Same policy as
/// [`crate::audio::writer`], so both paths round a sample the same way.
fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// The RMS above which a frame counts as carrying signal.
///
/// The rule is `max(FLOOR_RMS, p90 / 10)` — a floor, or 20 dB below however loud
/// this particular track gets, whichever is higher. Relative to the track's own
/// level because "the far end is playing" is a statement about this recording,
/// not about an absolute number of `i16` counts.
///
/// A tenth-percentile noise-floor estimate (`4 * p10`) was tried first and is
/// wrong: it assumes at least 10% of frames are silent. On a continuously active
/// track p10 sits near the median, `4 * p10` exceeds every frame, and *nothing*
/// classifies as active. That is the dangerous direction to fail in — frames
/// holding echo then land in the near-end bucket, where the removed echo reads
/// as damage to the user's voice.
pub fn active_threshold(frame_rms: &[f32]) -> f32 {
    if frame_rms.is_empty() {
        return FLOOR_RMS;
    }
    let mut sorted: Vec<f32> = frame_rms.to_vec();
    sorted.sort_by(f32::total_cmp);
    let p90 = sorted[sorted.len() * 9 / 10];
    FLOOR_RMS.max(p90 / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift32. A PRNG rather than a `rand` dependency, and a fixed seed so a
    /// failure is reproducible instead of showing up one run in ten.
    struct Noise(u32);

    impl Noise {
        fn new(seed: u32) -> Self {
            Self(seed | 1)
        }

        fn next_f32(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
        }

        fn samples(&mut self, n: usize, amplitude: f32) -> Vec<f32> {
            (0..n).map(|_| self.next_f32() * amplitude).collect()
        }
    }

    /// Converts to `i16` and **refuses to clip**.
    ///
    /// Clipping is a nonlinearity, and a synthetic signal that overflows full
    /// scale silently caps every ERLE assertion in this module while looking
    /// exactly like a broken canceller. This panic is the difference between
    /// "the test signal is wrong" and an afternoon spent debugging the filter.
    fn to_i16(samples: &[f32]) -> Vec<i16> {
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            peak <= 1.0,
            "test signal clips at {peak:.3} of full scale; scale it down"
        );
        samples
            .iter()
            .map(|&s| (s * i16::MAX as f32) as i16)
            .collect()
    }

    /// A plausible small-room response: a delayed, exponentially decaying noise
    /// tail, normalised so that convolving with it scales the signal's RMS by
    /// `gain` rather than by `gain * sqrt(tail)`.
    fn room_ir(delay: usize, tail: usize, gain: f32) -> Vec<f32> {
        let mut noise = Noise::new(11);
        let mut ir = vec![0.0; delay + tail];
        for i in 0..tail {
            let decay = (-6.0 * i as f32 / tail as f32).exp();
            ir[delay + i] = noise.next_f32() * decay;
        }
        ir[delay] += 1.0;
        let norm = ir.iter().map(|h| h * h).sum::<f32>().sqrt();
        for h in &mut ir {
            *h *= gain / norm;
        }
        ir
    }

    fn convolve(far: &[f32], ir: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; far.len()];
        for (i, o) in out.iter_mut().enumerate() {
            for (k, &h) in ir.iter().enumerate() {
                if let Some(x) = i.checked_sub(k).map(|j| far[j]) {
                    *o += h * x;
                }
            }
        }
        out
    }

    fn energy(samples: &[i16]) -> f64 {
        samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    }

    /// Runs a whole buffer through, returning the output and the dB change over
    /// the final quarter — after AEC3 has converged.
    fn run(near: &[i16], far: &[i16], config: AecConfig) -> (Vec<i16>, f32) {
        let mut canceller = EchoCanceller::new(config).expect("canceller");
        let n = canceller.frame_size();
        let mut out = vec![0i16; near.len()];
        let mut frame = vec![0i16; n];

        for start in (0..near.len().saturating_sub(n)).step_by(n) {
            canceller
                .cancel_frame(&near[start..start + n], &far[start..start + n], &mut frame)
                .expect("frame");
            out[start..start + n].copy_from_slice(&frame);
        }

        let from = near.len() * 3 / 4;
        let change = 10.0 * (energy(&near[from..]) / energy(&out[from..]).max(1.0)).log10();
        (out, change as f32)
    }

    /// Synthetic tests run at 16 kHz, not the 48 kHz that ships.
    ///
    /// Convergence is measured in samples, so a third of the rate is a third of
    /// the work — and the near-end signal is built by naive convolution, whose
    /// cost is duration times tail length. At 48 kHz with a realistic 100 ms
    /// tail that is billions of operations and the test took over a minute,
    /// which is a minute added to every CI run. `the_shipping_rate_works` pins
    /// 48 kHz once.
    const TEST_RATE: u32 = 16_000;

    /// 30 ms of bulk delay and a 50 ms tail at [`TEST_RATE`], proportional to
    /// what the reference recording measured.
    fn test_room_ir() -> Vec<f32> {
        room_ir(480, 800, 0.4)
    }

    /// If this fails nothing else matters — and it also proves the bundled C++
    /// actually built and linked rather than silently doing nothing.
    #[test]
    fn cancels_a_room_echo_path() {
        let config = AecConfig::for_rate(TEST_RATE);
        let mut noise = Noise::new(1);
        let far = noise.samples(TEST_RATE as usize * 6, 0.5);
        let near = convolve(&far, &test_room_ir());

        let (_, erle) = run(&to_i16(&near), &to_i16(&far), config);
        assert!(erle > 20.0, "expected >20 dB on a room path, got {erle:.1}");
    }

    /// The one test at the rate that actually ships, since every other synthetic
    /// test runs at 16 kHz.
    #[test]
    fn the_shipping_rate_works() {
        let config = AecConfig::default();
        assert_eq!(config.sample_rate, 48_000);

        let mut noise = Noise::new(9);
        let far = noise.samples(48_000 * 3, 0.5);
        // A short tail here on purpose: this test is pinning the rate and the
        // frame size, not the depth of cancellation.
        let near = convolve(&far, &room_ir(1_430, 400, 0.4));

        let canceller = EchoCanceller::new(config).expect("canceller");
        assert_eq!(canceller.frame_size(), 480);

        let (_, erle) = run(&to_i16(&near), &to_i16(&far), config);
        assert!(erle > 10.0, "48 kHz reached only {erle:.1} dB");
    }

    /// The failure an ERLE number cannot see: the canceller eating the user's
    /// own voice. With no far-end signal there is nothing to subtract, so the
    /// level must not move.
    ///
    /// Not a sample-for-sample comparison — AEC3 runs a high-pass filter over
    /// the capture stream, so the output is never bit-identical.
    #[test]
    fn does_not_damage_near_end_speech_when_far_is_silent() {
        let config = AecConfig::for_rate(TEST_RATE);
        let mut noise = Noise::new(3);
        let near = to_i16(&noise.samples(TEST_RATE as usize * 4, 0.3));
        let far = vec![0i16; near.len()];

        let (_, change) = run(&near, &far, config);
        assert!(
            change.abs() < 1.0,
            "a silent far end must not change the near level, moved {change:.2} dB"
        );
    }

    /// Guards the regression that made the hand-rolled predecessor unusable: a
    /// canceller that gives up, or diverges, once the user talks over the
    /// remote side. 74% of the reference recording is double-talk, so a
    /// canceller that only works in the clear is no use at all.
    #[test]
    fn keeps_cancelling_through_double_talk() {
        let config = AecConfig::for_rate(TEST_RATE);
        let n = TEST_RATE as usize * 8;

        let mut far_noise = Noise::new(4);
        let mut near_noise = Noise::new(5);
        let far = far_noise.samples(n, 0.5);
        let mut near = convolve(&far, &test_room_ir());
        // Loud, independent near-end speech through the middle 60%.
        let talk = near_noise.samples(n, 0.4);
        for i in (n * 2 / 10)..(n * 8 / 10) {
            near[i] += talk[i];
        }

        let (_, erle) = run(&to_i16(&near), &to_i16(&far), config);
        assert!(
            erle > 15.0,
            "double-talk must not destroy the filter; tail ERLE was {erle:.1}"
        );
    }

    /// AEC3 finds the delay itself, which is why the tracks are fed unaligned.
    /// If this ever stops holding, `process` has to start pre-shifting again.
    #[test]
    fn reports_a_delay_without_being_told_one() {
        let config = AecConfig::for_rate(TEST_RATE);
        let mut noise = Noise::new(6);
        let far = noise.samples(TEST_RATE as usize * 6, 0.5);
        let near = convolve(&far, &test_room_ir());

        let mut canceller = EchoCanceller::new(config).expect("canceller");
        let n = canceller.frame_size();
        let (far, near) = (to_i16(&far), to_i16(&near));
        let mut frame = vec![0i16; n];
        for start in (0..near.len() - n).step_by(n) {
            canceller
                .cancel_frame(&near[start..start + n], &far[start..start + n], &mut frame)
                .expect("frame");
        }

        assert!(
            canceller.reported_delay_ms().is_some(),
            "AEC3 should report a delay estimate after converging"
        );
    }

    /// 10 ms at the configured rate, and the wrapper must refuse anything else
    /// rather than letting the C++ side panic on a short buffer.
    #[test]
    fn rejects_a_frame_of_the_wrong_length() {
        let mut canceller = EchoCanceller::new(AecConfig::default()).expect("canceller");
        assert_eq!(canceller.frame_size(), 480);

        let short = vec![0i16; 100];
        let mut out = vec![0i16; 480];
        assert!(matches!(
            canceller.cancel_frame(&short, &short, &mut out),
            Err(AecError::FrameSize { expected: 480, .. })
        ));
    }

    /// Guards a leak or a double free over the FFI boundary, which would only
    /// show up over a long session of repeated recordings.
    #[test]
    fn creating_and_dropping_repeatedly_is_sound() {
        for _ in 0..50 {
            let canceller = EchoCanceller::new(AecConfig::default()).expect("canceller");
            assert_eq!(canceller.frame_size(), 480);
        }
    }

    #[test]
    fn sample_conversion_round_trips_and_clamps() {
        for s in [i16::MIN, -1, 0, 1, i16::MAX] {
            let back = f32_to_i16(i16_to_f32(s));
            assert!(
                (i32::from(back) - i32::from(s)).abs() <= 1,
                "{s} round-tripped to {back}"
            );
        }
        // The residual can exceed full scale; it must saturate, not wrap.
        assert_eq!(f32_to_i16(4.0), i16::MAX);
        assert_eq!(f32_to_i16(-4.0), -i16::MAX);
    }

    /// The bug this guards, which cost an afternoon: a threshold derived from a
    /// *low* percentile classifies a uniformly loud track as entirely silent,
    /// because the low percentile is then no lower than the rest. Every frame
    /// holding echo lands in the near-end bucket, and the removed echo reads as
    /// damage to the user's own voice.
    #[test]
    fn a_uniformly_loud_track_is_classified_active() {
        let loud: Vec<f32> = vec![9_459.0; 100];
        let threshold = active_threshold(&loud);
        assert!(
            loud[0] > threshold,
            "a track that is loud throughout must count as active, \
             but the threshold came out at {threshold:.0}"
        );
    }

    /// The floor has to win on a digital-silence track, or a tap that recorded
    /// nothing would have its handful of dither frames called "far-end active".
    #[test]
    fn a_silent_track_falls_back_to_the_floor() {
        assert_eq!(active_threshold(&vec![0.0; 100]), FLOOR_RMS);
        assert_eq!(active_threshold(&[]), FLOOR_RMS);
    }

    /// 20 dB below the track's own loud level, so a quiet recording is not
    /// written off as silent and a loud one does not count its noise floor.
    #[test]
    fn the_threshold_tracks_the_recordings_own_level() {
        let mut frames: Vec<f32> = vec![100.0; 100];
        frames[90..].fill(20_000.0);
        assert_eq!(active_threshold(&frames), 2_000.0);
    }
}
