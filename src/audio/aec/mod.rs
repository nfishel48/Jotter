//! Acoustic echo cancellation: removing speaker bleed from the microphone track.
//!
//! When a meeting is held on laptop speakers rather than headphones, the
//! microphone re-captures the remote participants. Every remote voice then lands
//! in *both* recorded tracks, which defeats the point of recording two of them
//! and feeds transcription a doubled copy of the remote side.
//!
//! This is a safe wrapper over the vendored Speex multi-delay block
//! frequency-domain adaptive filter (`vendor/mdf.c`). The interesting choices are
//! recorded in `vendor/README.md`; the two that matter for reading this file:
//!
//! * The canceller is fed the *system* track as its far-end reference and the
//!   *mic* track as its near end. It is not a mixer — `system.wav` is an input
//!   to the estimate, never part of the output.
//! * Speex works natively on `i16`, which is exactly what jotter's WAVs already
//!   hold, so there is no float conversion anywhere in the chain.
//!
//! Measured on a real 8m56s Linux/PipeWire meeting recording: the echo path is
//! ~30 ms of bulk delay with a reverb tail, magnitude-squared coherence between
//! the two tracks of 0.93-0.97 across 300-1000 Hz, and a theoretical linear
//! cancellation ceiling of 14-18 dB.
//!
//! That ceiling is why no residual suppressor is applied: what survives a linear
//! filter is not linearly predictable from the far end, and gating it would cost
//! more in transcription accuracy than the residual echo does. It is also why
//! the filter is sized tightly rather than generously — see [`AecConfig`].

mod sys;

use std::os::raw::{c_int, c_void};
use std::ptr::NonNull;

/// Frame RMS, in `i16` units, below which a frame is treated as having no
/// far-end signal.
///
/// 300 is the noise-floor threshold that reproduced the activity census of the
/// reference recording (28 s silence / 91 s near-only / 22 s far-only / 394 s
/// double-talk). It is a floor rather than the whole rule — see
/// [`active_threshold`].
pub const FLOOR_RMS: f32 = 300.0;

/// How the canceller is configured. Not a persisted type, so unlike
/// [`crate::config::Settings`] it is free to hold non-`Eq` fields if it ever
/// needs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AecConfig {
    pub sample_rate: u32,
    /// Samples per call. Speex asks for 10-20 ms (`vendor/speex/speex_echo.h`).
    pub frame_size: usize,
    /// Length of echo tail to model, in samples. Must be a whole multiple of
    /// `frame_size`; Speex asks for 100-500 ms.
    pub filter_length: usize,
}

impl Default for AecConfig {
    /// The shipping configuration: 10 ms frames and a 150 ms tail at 48 kHz.
    ///
    /// The filter has to span the bulk delay *plus* the reverb tail. One shorter
    /// than the delay cancels nothing whatsoever — the echo it is hunting for has
    /// not arrived inside its window yet.
    ///
    /// But longer is emphatically not better. Every tap beyond that is another
    /// free parameter fitted from the same data, and misadjustment noise grows
    /// with the count: on a fixed synthetic path, going from 400 to 4000 taps
    /// *loses* 50 dB of cancellation. `an_overlong_filter_costs_cancellation`
    /// pins the shape of that curve.
    ///
    /// Hence 150 ms rather than the 250 ms the coherence measurement alone would
    /// suggest: the bulk delay is removed by alignment before the canceller ever
    /// sees the audio, leaving the ~100 ms tail to model plus headroom for a
    /// delay estimate that came out a little short. 480 factors as 2^5*3*5,
    /// which kiss_fft handles natively.
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            frame_size: 480,
            filter_length: 7_200,
        }
    }
}

impl AecConfig {
    /// The same proportions as [`Default`] at an arbitrary rate: 10 ms frames,
    /// 150 ms tail. Used by tests, which run at 8 kHz because convergence is
    /// measured in samples and a sixth of the samples is a sixth of the wait.
    pub fn for_rate(sample_rate: u32) -> Self {
        let frame_size = (sample_rate / 100) as usize;
        Self {
            sample_rate,
            frame_size,
            filter_length: frame_size * 15,
        }
    }

    /// Filter length as milliseconds of echo tail.
    pub fn filter_ms(&self) -> u32 {
        (self.filter_length as u64 * 1_000 / self.sample_rate.max(1) as u64) as u32
    }

    fn validate(&self) -> Result<(), AecError> {
        if self.frame_size == 0 || self.filter_length == 0 {
            return Err(AecError::InvalidConfig(
                "frame_size and filter_length must be non-zero",
            ));
        }
        if !self.filter_length.is_multiple_of(self.frame_size) {
            return Err(AecError::InvalidConfig(
                "filter_length must be a whole multiple of frame_size",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecError {
    /// `speex_echo_state_init` returned null, which it only does on allocation
    /// failure.
    AllocationFailed,
    InvalidConfig(&'static str),
}

impl std::fmt::Display for AecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AllocationFailed => write!(f, "could not allocate the echo canceller"),
            Self::InvalidConfig(why) => write!(f, "invalid echo canceller configuration: {why}"),
        }
    }
}

impl std::error::Error for AecError {}

/// What a cancellation run achieved. Feeds both the CLI report and `meta.json`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AecStats {
    pub frames: u64,
    /// Echo return loss enhancement over frames where the far end was active:
    /// `10*log10(near_energy / output_energy)`. Higher is better; 0 dB means
    /// nothing was removed. `None` when the far end was never active.
    pub erle_db: Option<f32>,
    /// Level change over frames where the far end was *silent*, in dB. Near 0 is
    /// good; negative means the filter is eating the user's own voice, which is
    /// the failure mode that an ERLE figure cannot see. `None` when the far end
    /// was always active.
    pub near_gain_db: Option<f32>,
}

/// Owns the C canceller state.
///
/// Exclusively owned, so moving it to a worker thread is sound — which matters
/// because the GUI runs the pass off the egui thread. It is deliberately not
/// `Sync`: `speex_echo_cancellation` mutates the state on every call.
pub struct EchoCanceller {
    state: NonNull<sys::SpeexEchoState>,
    config: AecConfig,
}

// SAFETY: the state is reachable only through this struct — `state` is private
// and never copied out — and every method takes `&mut self`, so there is no way
// to reach it from two threads at once.
unsafe impl Send for EchoCanceller {}

impl EchoCanceller {
    pub fn new(config: AecConfig) -> Result<Self, AecError> {
        config.validate()?;

        // SAFETY: both arguments are validated positive above. The returned
        // pointer is either null (checked) or a state we now own.
        let state = unsafe {
            sys::speex_echo_state_init(config.frame_size as c_int, config.filter_length as c_int)
        };
        let state = NonNull::new(state).ok_or(AecError::AllocationFailed)?;
        let canceller = Self { state, config };

        // Not optional. mdf.c defaults to 8 kHz and picks its DC-notch radius
        // from the rate (`vendor/mdf.c:499-507`), so skipping this quietly
        // mis-tunes every 48 kHz recording with no other symptom.
        let mut rate = config.sample_rate as c_int;
        // SAFETY: SET_SAMPLING_RATE reads a single c_int through the pointer.
        unsafe {
            sys::speex_echo_ctl(
                canceller.state.as_ptr(),
                sys::SET_SAMPLING_RATE,
                (&raw mut rate).cast::<c_void>(),
            );
        }

        Ok(canceller)
    }

    pub fn config(&self) -> AecConfig {
        self.config
    }

    /// Cancels one frame. All three slices must be exactly `frame_size` long.
    ///
    /// `near` is the microphone (near end plus echo), `far` is what was played
    /// to the speakers, and `out` receives the near end with the echo removed.
    ///
    /// # Panics
    /// If any slice is not `frame_size` long. This is a programming error rather
    /// than a runtime condition — the C function would read out of bounds.
    pub fn cancel_frame(&mut self, near: &[i16], far: &[i16], out: &mut [i16]) {
        let n = self.config.frame_size;
        assert_eq!(near.len(), n, "near frame must be frame_size samples");
        assert_eq!(far.len(), n, "far frame must be frame_size samples");
        assert_eq!(out.len(), n, "out frame must be frame_size samples");

        // SAFETY: all three buffers are exactly frame_size long, asserted above,
        // which is the length the C function reads and writes.
        unsafe {
            sys::speex_echo_cancellation(
                self.state.as_ptr(),
                near.as_ptr(),
                far.as_ptr(),
                out.as_mut_ptr(),
            );
        }
    }

    /// Forgets everything learned. Not used by the two-pass driver, which
    /// deliberately carries the converged filter from pass 1 into pass 2.
    pub fn reset(&mut self) {
        // SAFETY: `state` is a live state for the lifetime of `self`.
        unsafe { sys::speex_echo_state_reset(self.state.as_ptr()) };
    }

    /// The sampling rate the C state believes it is running at.
    ///
    /// Exists so a test can prove the `SET_SAMPLING_RATE` call in [`Self::new`]
    /// actually landed.
    pub fn sampling_rate(&self) -> u32 {
        let mut rate: c_int = 0;
        // SAFETY: GET_SAMPLING_RATE writes a single c_int through the pointer.
        unsafe {
            sys::speex_echo_ctl(
                self.state.as_ptr(),
                sys::GET_SAMPLING_RATE,
                (&raw mut rate).cast::<c_void>(),
            );
        }
        rate.max(0) as u32
    }

    /// The converged echo path estimate, as filter taps.
    ///
    /// The diagnostic that answers "did it learn the right delay": the argmax
    /// should sit at the measured bulk delay. `vendor/mdf.c:1255-1272` writes
    /// `spx_int32_t` taps scaled by 32767, which is undone here.
    pub fn impulse_response(&self) -> Vec<f32> {
        let mut len: c_int = 0;
        // SAFETY: GET_IMPULSE_RESPONSE_SIZE writes a single c_int.
        unsafe {
            sys::speex_echo_ctl(
                self.state.as_ptr(),
                sys::GET_IMPULSE_RESPONSE_SIZE,
                (&raw mut len).cast::<c_void>(),
            );
        }
        if len <= 0 {
            return Vec::new();
        }

        let mut taps = vec![0i32; len as usize];
        // SAFETY: the buffer is exactly the length the C side just reported, and
        // the request writes spx_int32_t, which is i32.
        unsafe {
            sys::speex_echo_ctl(
                self.state.as_ptr(),
                sys::GET_IMPULSE_RESPONSE,
                taps.as_mut_ptr().cast::<c_void>(),
            );
        }
        taps.iter().map(|&t| t as f32 / 32767.0).collect()
    }
}

impl Drop for EchoCanceller {
    fn drop(&mut self) {
        // SAFETY: `state` came from speex_echo_state_init, is freed exactly once
        // because `EchoCanceller` is not `Clone`, and is unreachable afterwards.
        unsafe { sys::speex_echo_state_destroy(self.state.as_ptr()) };
    }
}

/// Whole-buffer cancellation, and the seam the unit tests drive.
///
/// Split out from the file-level driver for the same reason
/// [`crate::audio::writer::downmix_to_mono`] was split out of `push`: so the
/// signal processing can be tested without standing up files, threads or a cpal
/// stream. The two slices must already be bulk-delay aligned, and the output is
/// the same length as `near`.
pub fn cancel(
    near: &[i16],
    far: &[i16],
    config: &AecConfig,
) -> Result<(Vec<i16>, AecStats), AecError> {
    let mut canceller = EchoCanceller::new(*config)?;
    let n = config.frame_size;

    let mut out = vec![0i16; near.len()];
    let mut near_frame = vec![0i16; n];
    let mut far_frame = vec![0i16; n];
    let mut out_frame = vec![0i16; n];

    // Which frames count toward which metric. Derived from the far track rather
    // than hardcoded, so a quiet recording is not classified as all-silent.
    let far_rms: Vec<f32> = far.chunks(n).map(rms).collect();
    let threshold = active_threshold(&far_rms);

    let mut stats = AecStats::default();
    let (mut echo_in, mut echo_out) = (0.0f64, 0.0f64);
    let (mut quiet_in, mut quiet_out) = (0.0f64, 0.0f64);

    for (index, start) in (0..near.len()).step_by(n).enumerate() {
        // The trailing partial frame is zero-padded rather than dropped: the
        // output must be sample-for-sample as long as mic.wav, or every
        // alignment assumption downstream of it breaks.
        let end = (start + n).min(near.len());
        let len = end - start;
        near_frame[..len].copy_from_slice(&near[start..end]);
        near_frame[len..].fill(0);
        let far_end = (start + n).min(far.len());
        let far_len = far_end.saturating_sub(start);
        far_frame[..far_len].copy_from_slice(&far[start..far_end]);
        far_frame[far_len..].fill(0);

        canceller.cancel_frame(&near_frame, &far_frame, &mut out_frame);
        out[start..end].copy_from_slice(&out_frame[..len]);

        let (in_energy, out_energy) = (energy(&near_frame[..len]), energy(&out_frame[..len]));
        if far_rms.get(index).copied().unwrap_or(0.0) > threshold {
            echo_in += in_energy;
            echo_out += out_energy;
        } else {
            quiet_in += in_energy;
            quiet_out += out_energy;
        }
        stats.frames += 1;
    }

    stats.erle_db = ratio_db(echo_in, echo_out);
    stats.near_gain_db = ratio_db(quiet_out, quiet_in);
    Ok((out, stats))
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
/// as damage to the user's voice. Erring toward "active" merely dilutes the ERLE
/// figure slightly.
///
/// Shared by the delay estimator and the file-level pass so that "active" means
/// one thing everywhere.
pub fn active_threshold(frame_rms: &[f32]) -> f32 {
    if frame_rms.is_empty() {
        return FLOOR_RMS;
    }
    let mut sorted: Vec<f32> = frame_rms.to_vec();
    sorted.sort_by(f32::total_cmp);
    let p90 = sorted[sorted.len() * 9 / 10];
    FLOOR_RMS.max(p90 / 10.0)
}

fn energy(samples: &[i16]) -> f64 {
    samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
}

fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (energy(samples) / samples.len() as f64).sqrt() as f32
}

/// `10*log10(a/b)`, or `None` when there is nothing to compare.
fn ratio_db(a: f64, b: f64) -> Option<f32> {
    if a <= 0.0 || b <= 0.0 {
        return None;
    }
    Some((10.0 * (a / b).log10()) as f32)
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
    /// Clipping is a nonlinearity, and no linear filter can cancel it — a
    /// synthetic signal that overflows full scale silently caps every ERLE
    /// assertion in this module at a couple of dB and looks exactly like a
    /// broken canceller. This panic is the difference between "the test signal
    /// is wrong" and an afternoon spent debugging the filter.
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

    /// `far` convolved with `ir`, delayed by construction of `ir`.
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

    /// A single-tap "room": pure delay and gain.
    fn delay_ir(delay: usize, gain: f32) -> Vec<f32> {
        let mut ir = vec![0.0; delay + 1];
        ir[delay] = gain;
        ir
    }

    /// A plausible small-room response: a delayed, exponentially decaying noise
    /// tail, normalised so that convolving with it scales the signal's RMS by
    /// `gain` rather than by `gain * sqrt(tail)`.
    ///
    /// The normalisation is the whole point. Without it the taps sum
    /// incoherently and a 1200-tap tail at gain 0.4 produces a near signal with
    /// an RMS above full scale.
    fn room_ir(delay: usize, tail: usize, gain: f32) -> Vec<f32> {
        let mut noise = Noise::new(11);
        let mut ir = vec![0.0; delay + tail];
        for i in 0..tail {
            let decay = (-6.0 * i as f32 / tail as f32).exp();
            ir[delay + i] = noise.next_f32() * decay;
        }
        // A direct path plus the tail, the way a real room sounds.
        ir[delay] += 1.0;

        let norm = ir.iter().map(|h| h * h).sum::<f32>().sqrt();
        for h in &mut ir {
            *h *= gain / norm;
        }
        ir
    }

    /// ERLE over the tail of the signal, so the figure reflects the converged
    /// filter rather than being dragged down by the startup transient.
    ///
    /// `from` is a fraction of the total length; tests with a double-talk burst
    /// must start the window *after* the burst ends, or the near-end speech they
    /// deliberately injected shows up as uncancelled echo.
    fn erle_after(near: &[i16], out: &[i16], from: f64) -> f32 {
        let start = (near.len() as f64 * from) as usize;
        let n: f64 = energy(&near[start..]);
        let o: f64 = energy(&out[start..]);
        (10.0 * (n / o.max(1.0)).log10()) as f32
    }

    fn converged_erle(near: &[i16], out: &[i16]) -> f32 {
        erle_after(near, out, 0.75)
    }

    const RATE: u32 = 8_000;

    /// If this fails nothing else in the module matters — and it is also what
    /// proves the vendored C actually built and linked, rather than compiling to
    /// an empty translation unit because a backend #define was missing.
    #[test]
    fn cancels_a_pure_delay_and_gain_path() {
        let config = AecConfig::for_rate(RATE);
        let mut noise = Noise::new(1);
        let far = noise.samples(RATE as usize * 8, 0.5);
        let near = convolve(&far, &delay_ir(300, 0.5));

        let (far, near) = (to_i16(&far), to_i16(&near));
        let (out, stats) = cancel(&near, &far, &config).expect("canceller");

        let erle = converged_erle(&near, &out);
        assert!(
            erle > 20.0,
            "expected >20 dB on a single-tap path, got {erle:.1}"
        );
        assert!(stats.erle_db.expect("far end was active") > 10.0);
    }

    /// The bug this guards: a filter shorter than the echo tail cancels the first
    /// few milliseconds perfectly and the tail not at all, which still shows a
    /// respectable-looking ERLE. Only a response spanning most of the filter
    /// catches it.
    #[test]
    fn cancels_a_multi_tap_room_response() {
        let config = AecConfig::for_rate(RATE);
        let mut noise = Noise::new(2);
        let far = noise.samples(RATE as usize * 12, 0.5);
        // 250 ms of delay-plus-tail against a 250 ms filter.
        let near = convolve(&far, &room_ir(240, 1_200, 0.4));

        let (far, near) = (to_i16(&far), to_i16(&near));
        let (out, _) = cancel(&near, &far, &config).expect("canceller");

        let erle = converged_erle(&near, &out);
        assert!(
            erle > 15.0,
            "expected >15 dB on a room response, got {erle:.1}"
        );
    }

    /// The failure that an ERLE number cannot see: the filter eating the user's
    /// own voice. With no far-end signal there is nothing to subtract, so the
    /// level must not move.
    ///
    /// Not a sample-for-sample comparison: mdf.c runs a DC notch and
    /// pre-emphasis over the mic input (`vendor/mdf.c:718-726`), so the output is
    /// never bit-identical even when the filter is doing nothing.
    #[test]
    fn does_not_damage_near_end_speech_when_far_is_silent() {
        let config = AecConfig::for_rate(RATE);
        let mut noise = Noise::new(3);
        let near = noise.samples(RATE as usize * 4, 0.3);
        let far = vec![0i16; near.len()];

        let near = to_i16(&near);
        let (_, stats) = cancel(&near, &far, &config).expect("canceller");

        let change = stats.near_gain_db.expect("far end was silent throughout");
        assert!(
            change.abs() < 0.5,
            "silent far end must not change the near level, moved {change:.2} dB"
        );
        assert!(
            stats.erle_db.is_none(),
            "no frame should count as far-active"
        );
    }

    /// Guards exactly the divergence a fixed-step NLMS showed on the reference
    /// recording, where the residual came out *louder* than the input. Speex's
    /// variable learning rate and two-path filter (`TWO_PATH`, `vendor/mdf.c:34`)
    /// are the reason this holds, and are why the canceller is vendored rather
    /// than written by hand — 74% of that recording is double-talk.
    #[test]
    fn survives_double_talk_without_diverging() {
        let config = AecConfig::for_rate(RATE);
        let mut far_noise = Noise::new(4);
        let mut near_noise = Noise::new(5);

        let n = RATE as usize * 16;
        let far = far_noise.samples(n, 0.5);
        let mut near = convolve(&far, &delay_ir(300, 0.4));
        // Loud, independent near-end speech through the middle 60%.
        let talk = near_noise.samples(n, 0.6);
        for i in (n * 2 / 10)..(n * 8 / 10) {
            near[i] += talk[i];
        }

        let (far, near) = (to_i16(&far), to_i16(&near));
        let (out, _) = cancel(&near, &far, &config).expect("canceller");

        // Measure from 0.85, not 0.75: the burst runs to 0.8, and including any
        // of it would score the near-end speech we injected as leftover echo.
        let erle = erle_after(&near, &out, 0.85);
        assert!(
            erle > 10.0,
            "double-talk must not destroy the filter; tail ERLE was {erle:.1}"
        );
    }

    /// mdf.c defaults to 8 kHz and derives its DC-notch radius from the rate
    /// (`vendor/mdf.c:499-507`). A missing `SET_SAMPLING_RATE` therefore
    /// mis-tunes every 48 kHz recording and produces no error, no warning and no
    /// other symptom.
    #[test]
    fn sampling_rate_is_pushed_into_the_c_state() {
        let canceller = EchoCanceller::new(AecConfig::default()).expect("canceller");
        assert_eq!(canceller.sampling_rate(), 48_000);
    }

    /// Proves the diagnostic the verification script relies on: after converging
    /// on a known delay, the largest tap sits at that delay.
    #[test]
    fn impulse_response_peaks_at_the_injected_delay() {
        let config = AecConfig::for_rate(RATE);
        let delay = 300;
        let mut noise = Noise::new(6);
        let far = noise.samples(RATE as usize * 8, 0.5);
        let near = convolve(&far, &delay_ir(delay, 0.5));
        let (far, near) = (to_i16(&far), to_i16(&near));

        let mut canceller = EchoCanceller::new(config).expect("canceller");
        let n = config.frame_size;
        let mut out = vec![0i16; n];
        for start in (0..near.len() - n).step_by(n) {
            canceller.cancel_frame(&near[start..start + n], &far[start..start + n], &mut out);
        }

        let ir = canceller.impulse_response();
        let peak = ir
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .expect("a non-empty impulse response");
        let error = peak.abs_diff(delay);
        assert!(
            error <= 2,
            "impulse response peaked at {peak}, expected ~{delay}"
        );
    }

    /// Guards a config that would make the C side read past the end of a
    /// partition, which is a crash rather than a bad number.
    #[test]
    fn rejects_a_filter_length_that_is_not_a_multiple_of_the_frame() {
        let config = AecConfig {
            sample_rate: 48_000,
            frame_size: 480,
            filter_length: 12_001,
        };
        assert!(matches!(
            EchoCanceller::new(config),
            Err(AecError::InvalidConfig(_))
        ));
    }

    /// Guards a leak or a double free in the RAII wrapper, which would only show
    /// up over a long session of repeated recordings.
    #[test]
    fn creating_and_dropping_repeatedly_is_sound() {
        for _ in 0..200 {
            let canceller = EchoCanceller::new(AecConfig::for_rate(RATE)).expect("canceller");
            assert_eq!(canceller.sampling_rate(), RATE);
        }
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

    /// The one slow test: pins the configuration that actually ships, since
    /// every other test runs at 8 kHz.
    #[test]
    fn the_shipping_48k_config_converges_on_its_tail() {
        let config = AecConfig::default();
        assert_eq!(config.filter_ms(), 150);

        let mut noise = Noise::new(7);
        let far = noise.samples(48_000 * 6, 0.5);
        // ~30 ms of bulk delay, as measured on the reference recording.
        let near = convolve(&far, &room_ir(1_430, 4_800, 0.4));

        let (far, near) = (to_i16(&far), to_i16(&near));
        let (out, stats) = cancel(&near, &far, &config).expect("canceller");

        let erle = converged_erle(&near, &out);
        assert!(erle > 12.0, "shipping config reached only {erle:.1} dB");
        assert!(stats.frames > 0);
    }

    /// Pins the tradeoff that sets `filter_length`, because it is deeply
    /// counter-intuitive and the obvious "bigger is safer" instinct is wrong.
    ///
    /// Measured on a single-tap path with the echo at 37.5 ms:
    ///
    /// | taps  |   ms | ERLE     |
    /// |-------|------|----------|
    /// |   160 |   20 |  0.3 dB  |  filter shorter than the delay: nothing
    /// |   400 |   50 | 73.4 dB  |  just covers it
    /// |   800 |  100 | 56.5 dB  |
    /// |  2000 |  250 | 23.1 dB  |
    /// |  4000 |  500 | 21.4 dB  |
    ///
    /// Both ends of that curve are failure modes, and each guards a different
    /// mistake: shrinking the filter below the bulk delay (which returns
    /// *nothing*, not merely less), and reaching for a longer one when
    /// cancellation disappoints, which makes it worse.
    #[test]
    fn an_overlong_filter_costs_cancellation() {
        let mut noise = Noise::new(1);
        let far_f = noise.samples(8_000 * 8, 0.5);
        let near_f = convolve(&far_f, &delay_ir(300, 0.5));
        let (far, near) = (to_i16(&far_f), to_i16(&near_f));

        let erle_at = |taps: usize| {
            let config = AecConfig {
                sample_rate: 8_000,
                frame_size: 80,
                filter_length: taps,
            };
            let (out, _) = cancel(&near, &far, &config).expect("canceller");
            converged_erle(&near, &out)
        };

        // A filter shorter than the 37.5 ms delay cannot see the echo at all.
        assert!(
            erle_at(160) < 3.0,
            "a filter shorter than the bulk delay must cancel ~nothing"
        );
        // Just long enough is dramatically better than generously long.
        let snug = erle_at(400);
        let baggy = erle_at(4_000);
        assert!(snug > 40.0, "a snug filter should excel, got {snug:.1} dB");
        assert!(
            snug > baggy + 15.0,
            "10x the taps should cost >15 dB to misadjustment: {snug:.1} vs {baggy:.1}"
        );
    }
}
