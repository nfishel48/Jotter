//! Measuring how far the mic track lags the system track.
//!
//! # What this is for, and what it is not
//!
//! **The measurement is never applied to the audio.** AEC3 runs its own delay
//! estimator, and [`super`] feeds it the two tracks unaligned precisely so that
//! estimate is the one in charge. This module exists for three other reasons:
//!
//! 1. **Reporting.** `meta.json` records the delay next to AEC3's own figure, so
//!    the two can be compared. A wide disagreement is the first thing to look at
//!    when a recording cancels badly.
//! 2. **The drift guard.** Two devices on different clocks walk apart, and *no*
//!    single delay works for the whole recording — including AEC3's. Detecting
//!    that is what lets the pass decline instead of producing a track that is
//!    cancelled at the start and untouched by the end.
//! 3. **Sanity.** A lag that would have the echo precede its cause means the two
//!    tracks are swapped, which is worth catching before anything else runs.
//!
//! An earlier version *did* shift the mic track by this measurement. That was a
//! mistake in a specific and instructive way: an alignment slightly too *large*
//! is unrecoverable, because it asks the filter to model an echo arriving before
//! its cause, and no amount of filter length can express that. Letting AEC3
//! estimate its own delay removes the failure mode entirely.
//!
//! # The sign convention
//!
//! **A positive delay means `mic.wav` lags `system.wav`**: `mic[n + d]` holds
//! the echo of `system[n]`. Stated here as a single sentence, and asserted by
//! `delay_sign_is_mic_minus_system`, because a reported delay with the wrong
//! sign would send someone debugging in exactly the wrong direction.
//!
//! `first_callback_nanos` explains only part of it. On the reference recording
//! the system stream's first callback was 5.33 ms after the mic's, while the echo
//! arrives ~29.8 ms later in the file: the rest is the output buffer, the air,
//! and the input buffer. So the metadata is a bound and a fallback, never the
//! estimate — worth saying plainly, because "the metadata says 5 ms, use 5 ms"
//! is the obvious wrong move.
//!
//! # No FFT
//!
//! The estimate is deliberately time-domain and two-stage: a coarse search over
//! 8x-decimated audio across the whole plausible delay range, then a refinement
//! at full rate within a couple of decimated samples. GCC-PHAT would give a
//! sharper peak, but decimating makes the coarse range affordable and the
//! refinement is a few dozen lags, so the whole estimator needs no FFT and no
//! dependency.
//!
//! An earlier version bracketed the delay from the 100 ms activity envelope
//! instead. It cannot work, and the way it fails is quiet: a 30 ms delay is a
//! third of one envelope frame, so the coarse stage always answers zero, and any
//! fine search narrower than the envelope frame then looks in the wrong place
//! and finds nothing. Whatever brackets the delay has to resolve finer than the
//! delay itself.

use super::{FLOOR_RMS, active_threshold};

/// Decimation factor for the coarse search. At 48 kHz this gives 6 kHz, so a
/// coarse lag is accurate to ~0.17 ms — already finer than the filter cares
/// about, and the refinement pass then removes even that.
pub const COARSE_DECIMATION: usize = 8;

/// Refinement range at full rate, in decimated samples either side of the
/// coarse answer. Two is enough: the coarse stage cannot be off by more than
/// one decimated sample plus rounding.
const REFINE_DECIMATED_SAMPLES: usize = 2;

/// Lag range searched, relative to no delay at all.
///
/// Slightly negative at the bottom because the system stream's first callback
/// can land *after* the mic's — 5.33 ms later on the reference recording — so
/// the file-relative lag has a little headroom below zero even though the
/// acoustic path itself cannot be negative.
const MIN_SEARCH_MS: f32 = -20.0;
const MAX_SEARCH_MS: f32 = 400.0;

/// How many times the runner-up lag a peak must score to count as located.
///
/// A prominence ratio, so this does not have to scale with the search range —
/// see the note in [`best_lag`]. Noise sits near 1.0; a real delay reaches 5-10.
const MIN_CONFIDENCE: f32 = 2.0;

/// Segments needed before an estimate is trusted.
const MIN_SEGMENTS: usize = 3;

/// Spread across segments above which the estimate is not a single delay.
/// The reference recording spans 2.45 ms.
const MAX_SPREAD_MS: f32 = 5.0;

/// Drift above which no single delay works for the whole recording.
///
/// 20 ppm is ~1 ms per minute. Two devices on one clock — the normal case, and
/// what the reference recording measured — drift at essentially 0; a USB mic
/// against built-in speakers is two clock domains and can reach 100 ppm, which
/// is 0.36 s per hour. No single delay describes a recording like that, so even
/// AEC3's own estimator is chasing a moving target, and declining is the honest
/// answer.
const MAX_DRIFT_PPM: f32 = 20.0;

/// An acoustic path slower than this is not acoustic.
const MAX_PLAUSIBLE_DELAY_SECS: f32 = 0.5;

/// Where a delay estimate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelaySource {
    /// Correlated from the audio itself. What we want.
    Measured,
    /// `first_callback_nanos` from `meta.json`. Explains only the stream start
    /// offset, not the acoustic path, so it is an underestimate. Recorded as the
    /// best available figure when correlation found nothing.
    MetaOffset,
    /// No estimate at all.
    Zero,
}

impl DelaySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::MetaOffset => "meta_offset",
            Self::Zero => "zero",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayEstimate {
    /// Frames the mic track lags the system track. Positive is normal.
    pub frames: i64,
    pub source: DelaySource,
    /// Median peak-to-average ratio across accepted segments.
    pub confidence: f32,
    pub segments_used: usize,
    pub spread_ms: f32,
    pub drift_ppm: f32,
}

impl DelayEstimate {
    /// The fallback when correlation found nothing usable.
    ///
    /// Not a failure. Nothing downstream depends on this number being right —
    /// AEC3 finds its own delay — so an unmeasurable one costs only the
    /// cross-check. Declining to process at all is reserved for positive
    /// evidence that *no* single delay can work, which is [`combine`]'s job.
    pub fn unaligned(meta_offset_frames: Option<i64>) -> Self {
        let (frames, source) = match meta_offset_frames {
            Some(frames) if frames >= 0 => (frames, DelaySource::MetaOffset),
            _ => (0, DelaySource::Zero),
        };
        Self {
            frames,
            source,
            confidence: 0.0,
            segments_used: 0,
            spread_ms: 0.0,
            drift_ppm: 0.0,
        }
    }
}

/// Why an estimate could not be trusted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DelayError {
    /// Too few segments correlated clearly. Fall back rather than bypass.
    Inconclusive,
    /// The per-segment estimates disagree too much to be one delay.
    Unstable { spread_ms: f32 },
    /// The clocks are drifting apart. No single delay works.
    Drifting { ppm: f32 },
    /// The echo would precede the audio that caused it.
    PrecedesPlayback,
    /// Slower than any acoustic path.
    Implausible,
}

/// One segment's measurement, as fed to [`combine`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentLag {
    /// Where in the recording it was measured, in seconds. Used for the drift
    /// fit, so segments must be spread across the file rather than clustered.
    pub at_secs: f32,
    pub frames: i64,
    pub confidence: f32,
}

/// Reduces per-segment measurements to one delay, or explains why it cannot.
///
/// Pure, so every rejection rule is testable without touching audio.
pub fn combine(
    segments: &[SegmentLag],
    sample_rate: u32,
    meta_offset_frames: Option<i64>,
) -> Result<DelayEstimate, DelayError> {
    let mut accepted: Vec<SegmentLag> = segments
        .iter()
        .copied()
        .filter(|s| s.confidence >= MIN_CONFIDENCE)
        .collect();

    if accepted.len() < MIN_SEGMENTS {
        return Err(DelayError::Inconclusive);
    }

    // Drift is checked before the spread rule, and on purpose: drifting clocks
    // also look "unstable", but they are a different answer. Unstable says
    // "these numbers are noise"; drifting says "the delay is real and moving",
    // which is the one case where no single delay can ever work.
    let drift_ppm = drift_ppm(&accepted, sample_rate);
    if drift_ppm.abs() > MAX_DRIFT_PPM {
        return Err(DelayError::Drifting { ppm: drift_ppm });
    }

    accepted.sort_by_key(|s| s.frames);
    // Median, not mean: one segment that locked onto a dropout or an unrelated
    // transient must not drag the answer.
    let frames = accepted[accepted.len() / 2].frames;
    let confidences = {
        let mut c: Vec<f32> = accepted.iter().map(|s| s.confidence).collect();
        c.sort_by(f32::total_cmp);
        c[c.len() / 2]
    };

    let spread = accepted[accepted.len() - 1].frames - accepted[0].frames;
    let spread_ms = frames_to_ms(spread, sample_rate);
    if spread_ms > MAX_SPREAD_MS {
        return Err(DelayError::Unstable { spread_ms });
    }

    // The echo cannot arrive before the sound that caused it. A file-relative
    // lag more negative than the stream-start offset means exactly that, and in
    // practice means the two tracks are swapped.
    let floor = -meta_offset_frames.unwrap_or(0) - (sample_rate as i64 / 200);
    if frames < floor {
        return Err(DelayError::PrecedesPlayback);
    }
    if frames.unsigned_abs() as f32 / sample_rate as f32 > MAX_PLAUSIBLE_DELAY_SECS {
        return Err(DelayError::Implausible);
    }

    Ok(DelayEstimate {
        frames,
        source: DelaySource::Measured,
        confidence: confidences,
        segments_used: accepted.len(),
        spread_ms,
        drift_ppm,
    })
}

/// Least-squares slope of lag against time, in parts per million.
fn drift_ppm(segments: &[SegmentLag], sample_rate: u32) -> f32 {
    let n = segments.len() as f64;
    if n < 2.0 || sample_rate == 0 {
        return 0.0;
    }
    let mean_t = segments.iter().map(|s| s.at_secs as f64).sum::<f64>() / n;
    let mean_y = segments.iter().map(|s| s.frames as f64).sum::<f64>() / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for s in segments {
        let dt = s.at_secs as f64 - mean_t;
        num += dt * (s.frames as f64 - mean_y);
        den += dt * dt;
    }
    if den <= 0.0 {
        return 0.0;
    }
    // Frames of lag gained per second, over samples per second, is a
    // dimensionless rate; times 1e6 is ppm.
    ((num / den) / sample_rate as f64 * 1e6) as f32
}

fn frames_to_ms(frames: i64, sample_rate: u32) -> f32 {
    if sample_rate == 0 {
        return 0.0;
    }
    frames as f32 * 1_000.0 / sample_rate as f32
}

/// First-difference pre-emphasis, as `f32`.
///
/// Speech has a steep spectral tilt, so its autocorrelation is broad and a raw
/// peak wanders with the vowel; flattening the spectrum gives the peak a
/// locatable maximum. Cheap enough to be worth doing at both search stages.
fn emphasise(samples: &[i16]) -> Vec<f32> {
    samples
        .windows(2)
        .map(|w| f32::from(w[1]) - f32::from(w[0]))
        .collect()
}

/// Box-filters and decimates by `factor`.
///
/// The averaging is the anti-alias filter. Crude as filters go, but the coarse
/// stage only needs to locate a peak to within one decimated sample, and the
/// refinement pass re-measures at full rate anyway.
///
/// **Must be given raw samples, never pre-emphasised ones.** Box-averaging a
/// first difference collapses to `(x[n+factor] - x[n]) / factor` — a difference
/// of two samples `factor` apart rather than an average of `factor` samples. Two
/// such signals whose true delay is not a whole multiple of `factor` then
/// difference *disjoint* pairs of samples and correlate at zero. Measured: with
/// the echo at 1431 samples and a factor of 8, the correlation at the true lag
/// came out at 0.008 against a noise floor of 0.07 — the estimator confidently
/// returned a lag nine times too large. Pre-emphasis belongs only at the
/// full-rate stage, where there is no decimation grid to fall between.
fn decimate(samples: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(factor)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect()
}

/// Normalised cross-correlation of `far` against `near` over a lag range.
///
/// Returns the lag maximising `|corr|` such that `near[t + lag] ~ far[t]`,
/// together with a peak-to-background ratio.
///
/// Both slices are in the same units and `near` must extend far enough to cover
/// `far.len() + max_lag`. A lag outside the available data scores zero rather
/// than wrapping or panicking.
fn best_lag(near: &[f32], far: &[f32], min_lag: isize, max_lag: isize) -> Option<(isize, f32)> {
    if far.is_empty() || min_lag > max_lag {
        return None;
    }
    let far_norm = far.iter().map(|v| v * v).sum::<f32>().sqrt();
    if far_norm <= 0.0 {
        return None;
    }

    let mut scores = Vec::with_capacity((max_lag - min_lag + 1) as usize);
    for lag in min_lag..=max_lag {
        let start = lag;
        let end = lag + far.len() as isize;
        if start < 0 || end > near.len() as isize {
            scores.push(0.0);
            continue;
        }
        let window = &near[start as usize..end as usize];
        let norm = window.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm <= 0.0 {
            scores.push(0.0);
            continue;
        }
        let dot: f32 = window.iter().zip(far).map(|(a, b)| a * b).sum();
        scores.push((dot / (norm * far_norm)).abs());
    }

    let (peak_index, &peak) = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))?;
    if peak <= 0.0 {
        return None;
    }

    // Prominence: the peak against the *best of the rest*, excluding its own
    // neighbourhood. A broad hump is not a located delay, and an average that
    // included the peak would score one as though it were.
    //
    // Deliberately not peak-over-mean, which was tried and is wrong: the
    // expected maximum of N noise samples grows with N, so peak-over-mean rises
    // with the size of the search range. Pure noise over 2520 lags scored 4.4
    // and sailed past a threshold of 4. Peak-over-runner-up has no such
    // dependence — it sits near 1 for noise whatever the range, and at 5-10 for
    // a real delay.
    let exclude = (scores.len() / 32).max(4);
    let runner_up = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(peak_index) > exclude)
        .map(|(_, &s)| s)
        .fold(0.0f32, f32::max);
    let confidence = if runner_up > 0.0 {
        peak / runner_up
    } else {
        // Nothing else correlated at all, which only happens on synthetic input.
        f32::INFINITY
    };

    Some((peak_index as isize + min_lag, confidence))
}

/// Measures the lag of one segment: coarse on decimated audio, then refined at
/// full rate.
///
/// `far` is the reference segment. `near` must start at the same file position
/// as `far` and extend at least [`MAX_SEARCH_MS`] beyond it, so the whole search
/// range is available.
///
/// Returns the lag in full-rate samples, and a confidence.
pub fn segment_lag(near: &[i16], far: &[i16], sample_rate: u32) -> Option<(isize, f32)> {
    if far.is_empty() || near.len() <= far.len() {
        return None;
    }

    let ms_to_samples = |ms: f32| (ms * sample_rate as f32 / 1_000.0) as isize;
    let d = COARSE_DECIMATION as isize;

    // Coarse stage on *raw* samples — see the warning on `decimate`.
    let to_f32 = |s: &[i16]| -> Vec<f32> { s.iter().map(|&v| f32::from(v)).collect() };
    let far_d = decimate(&to_f32(far), COARSE_DECIMATION);
    let near_d = decimate(&to_f32(near), COARSE_DECIMATION);
    let (coarse, confidence) = best_lag(
        &near_d,
        &far_d,
        ms_to_samples(MIN_SEARCH_MS) / d,
        ms_to_samples(MAX_SEARCH_MS) / d,
    )?;

    // Refinement is where pre-emphasis earns its keep: at full rate there is no
    // decimation grid to fall between, and flattening speech's spectral tilt
    // narrows the peak so the maximum is actually locatable.
    let far_e = emphasise(far);
    let near_e = emphasise(near);

    // Refine within a couple of decimated samples of the coarse answer. The
    // confidence from the coarse stage is the one reported: it was measured
    // against the whole plausible range, so it says "this is the delay, not
    // merely the best of a handful of neighbours".
    let centre = coarse * d;
    let span = (REFINE_DECIMATED_SAMPLES as isize) * d;
    let (refined, _) = best_lag(&near_e, &far_e, centre - span, centre + span)?;

    Some((refined, confidence))
}

/// Per-frame RMS of a whole track.
pub fn frame_rms(samples: &[i16], frame: usize) -> Vec<f32> {
    if frame == 0 {
        return Vec::new();
    }
    samples
        .chunks(frame)
        .map(|c| {
            let energy: f64 = c.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            (energy / c.len() as f64).sqrt() as f32
        })
        .collect()
}

/// Indices of frames where the far end is active and the near end is not.
///
/// These are the stretches where the mic track holds nothing *but* echo, which
/// is where a delay can be measured without the user's own voice competing.
pub fn far_only_frames(mic_rms: &[f32], far_rms: &[f32]) -> Vec<usize> {
    let near_threshold = active_threshold(mic_rms).max(FLOOR_RMS);
    let far_threshold = active_threshold(far_rms).max(FLOOR_RMS);
    (0..mic_rms.len().min(far_rms.len()))
        .filter(|&i| far_rms[i] > far_threshold && mic_rms[i] <= near_threshold)
        .collect()
}

/// Samples of `near` that must follow a segment for the whole range to be
/// searchable.
pub fn search_headroom(sample_rate: u32) -> usize {
    (sample_rate as f32 * MAX_SEARCH_MS / 1_000.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn samples(&mut self, n: usize, amplitude: f32) -> Vec<i16> {
            (0..n)
                .map(|_| (self.next_f32() * amplitude * i16::MAX as f32) as i16)
                .collect()
        }
    }

    fn seg(at_secs: f32, frames: i64) -> SegmentLag {
        SegmentLag {
            at_secs,
            frames,
            confidence: 10.0,
        }
    }

    /// The reference recording's delay is ~1430 frames at 48 kHz. Within a
    /// couple of samples is far better than needed — the filter's tail absorbs
    /// far more than that — but a correct estimator should manage it.
    /// The reference recording's delay is ~1430 frames at 48 kHz. The estimator
    /// must find it from a standing start, with no hint about where to look —
    /// which is exactly what the envelope-bracketing version could not do.
    #[test]
    fn finds_a_known_delay_in_synthetic_noise() {
        let rate = 48_000u32;
        let delay = 1_431usize;

        let mut noise = Noise::new(1);
        let far = noise.samples(24_000, 0.5);

        let headroom = search_headroom(rate);
        let mut near = vec![0i16; far.len() + headroom];
        for (i, &s) in far.iter().enumerate() {
            near[delay + i] = (f32::from(s) * 0.3) as i16;
        }
        let mut mic_noise = Noise::new(2);
        let floor = mic_noise.samples(near.len(), 0.02);
        for (n, f) in near.iter_mut().zip(floor) {
            *n = n.saturating_add(f);
        }

        let (lag, confidence) = segment_lag(&near, &far, rate).expect("a measurable lag");
        assert!(
            (lag - delay as isize).abs() <= 2,
            "expected ~{delay}, got {lag}"
        );
        assert!(confidence > MIN_CONFIDENCE, "weak peak: {confidence:.1}");
    }

    /// The single easiest thing in this module to get backwards, and backwards
    /// makes the canceller *add* echo instead of removing it. Stated as an
    /// assertion so the convention cannot be re-derived wrongly.
    #[test]
    fn delay_sign_is_mic_minus_system() {
        let rate = 48_000u32;
        let true_delay = 1_200usize;

        let mut noise = Noise::new(3);
        let far = noise.samples(24_000, 0.5);
        let mut near = vec![0i16; far.len() + search_headroom(rate)];
        for (i, &s) in far.iter().enumerate() {
            near[true_delay + i] = (f32::from(s) * 0.5) as i16;
        }

        // A mic track that lags the system track yields a *positive* lag. The
        // opposite sign would shift the reference the wrong way and make the
        // canceller add echo rather than remove it.
        let (lag, _) = segment_lag(&near, &far, rate).expect("a measurable lag");
        assert!(
            lag > 0,
            "a lagging mic track must give a positive delay, got {lag}"
        );
        assert!((lag - true_delay as isize).abs() <= 2, "got {lag}");
    }

    /// Guards confidently applying a garbage shift. Two unrelated signals have
    /// no peak to find, and the estimator has to say so rather than return its
    /// argmax with a straight face.
    #[test]
    fn rejects_an_uncorrelated_pair() {
        let rate = 48_000u32;
        let mut a = Noise::new(4);
        let mut b = Noise::new(9_999);
        let far = a.samples(24_000, 0.5);
        let near = b.samples(24_000 + search_headroom(rate), 0.5);

        let (_, confidence) = segment_lag(&near, &far, rate).expect("a result");
        assert!(
            confidence < MIN_CONFIDENCE,
            "uncorrelated signals must not look confident: {confidence:.1}"
        );
    }

    /// One segment locking onto a dropout must not drag the answer, which is
    /// exactly what a mean would let it do.
    #[test]
    fn median_ignores_one_bad_segment() {
        let segments = [
            seg(10.0, 1_430),
            seg(100.0, 1_432),
            seg(200.0, 1_428),
            seg(300.0, 9_000),
            seg(400.0, 1_431),
        ];
        // The outlier is 7568 frames out, so it would drift-fit as well; drop it
        // from the drift check by giving it a low confidence, which is what the
        // real fine pass would report for a spurious peak.
        let mut segments = segments;
        segments[3].confidence = 1.0;

        let estimate = combine(&segments, 48_000, Some(256)).expect("an estimate");
        assert_eq!(estimate.frames, 1_431);
        assert_eq!(estimate.segments_used, 4);
    }

    /// Guards a swapped mic/system pair being "corrected" into nonsense: the
    /// echo of a sound cannot reach the microphone before the sound exists.
    #[test]
    fn refuses_a_delay_that_precedes_playback() {
        let segments = [seg(10.0, -5_000), seg(100.0, -5_000), seg(200.0, -5_000)];
        assert_eq!(
            combine(&segments, 48_000, Some(256)),
            Err(DelayError::PrecedesPlayback)
        );
    }

    /// Nothing acoustic is half a second slow. A "delay" that large means the
    /// correlation found something that is not the echo.
    #[test]
    fn refuses_an_implausibly_long_delay() {
        let segments = [seg(10.0, 40_000), seg(100.0, 40_000), seg(200.0, 40_000)];
        assert_eq!(
            combine(&segments, 48_000, Some(256)),
            Err(DelayError::Implausible)
        );
    }

    /// Drifting clocks must be reported as drift, not averaged into a median
    /// that is wrong everywhere. 200 ppm over 400 s is 3840 frames of walk.
    #[test]
    fn detects_linear_drift_and_declines() {
        let rate = 48_000u32;
        let segments: Vec<SegmentLag> = (0..8)
            .map(|i| {
                let t = i as f32 * 50.0;
                let drifted = 1_430.0 + t * 200e-6 * rate as f32;
                seg(t, drifted as i64)
            })
            .collect();

        match combine(&segments, rate, Some(256)) {
            Err(DelayError::Drifting { ppm }) => {
                assert!((ppm - 200.0).abs() < 20.0, "ppm came out {ppm:.0}")
            }
            other => panic!("expected drift, got {other:?}"),
        }
    }

    /// The reference recording measured 28.65-31.10 ms across the file, a
    /// 2.45 ms spread with no trend. That must read as one stable delay, not as
    /// instability — otherwise the feature declines on exactly the recording it
    /// was built for.
    #[test]
    fn the_reference_recordings_spread_reads_as_stable() {
        let rate = 48_000u32;
        let ms = |v: f32| (v * rate as f32 / 1_000.0) as i64;
        let segments = [
            seg(30.0, ms(29.79)),
            seg(180.0, ms(28.65)),
            seg(450.0, ms(29.69)),
            seg(520.0, ms(31.10)),
        ];

        let estimate = combine(&segments, rate, Some(256)).expect("a stable estimate");
        assert_eq!(estimate.source, DelaySource::Measured);
        assert!(estimate.spread_ms < MAX_SPREAD_MS);
        assert!(estimate.drift_ppm.abs() < MAX_DRIFT_PPM);
        // The median of the four, which is what a 150 ms filter is sized around.
        assert!((frames_to_ms(estimate.frames, rate) - 29.69).abs() < 0.1);
    }

    /// Too few clear segments is *not* grounds to refuse to cancel — the
    /// filter's tail can absorb an unmeasured 30 ms. Only positive evidence
    /// that no single delay works (drift, or a violated sanity gate) is.
    #[test]
    fn too_few_segments_is_inconclusive_rather_than_fatal() {
        let segments = [seg(10.0, 1_430), seg(100.0, 1_431)];
        assert_eq!(
            combine(&segments, 48_000, Some(256)),
            Err(DelayError::Inconclusive)
        );

        // And the fallback is usable rather than empty.
        let fallback = DelayEstimate::unaligned(Some(256));
        assert_eq!(fallback.frames, 256);
        assert_eq!(fallback.source, DelaySource::MetaOffset);

        let nothing = DelayEstimate::unaligned(None);
        assert_eq!(nothing.frames, 0);
        assert_eq!(nothing.source, DelaySource::Zero);
    }

    /// The bug this guards is the one that made the first version of this
    /// estimator return nothing at all: whatever brackets the delay must resolve
    /// finer than the delay itself. A 100 ms envelope frame cannot locate a
    /// 30 ms delay — it can only ever answer zero.
    #[test]
    fn the_coarse_stage_resolves_finer_than_the_delay_it_looks_for() {
        let rate = 48_000u32;
        // The reference recording's ~30 ms delay, the case that failed.
        let delay_ms = 29.8f32;
        let delay = (delay_ms * rate as f32 / 1_000.0) as usize;

        // One decimated sample is the coarse stage's resolution.
        let coarse_resolution_ms = COARSE_DECIMATION as f32 * 1_000.0 / rate as f32;
        assert!(
            coarse_resolution_ms < delay_ms / 10.0,
            "coarse resolution {coarse_resolution_ms:.2}ms is too blunt for a \
             {delay_ms}ms delay"
        );

        let mut noise = Noise::new(8);
        let far = noise.samples(24_000, 0.5);
        let mut near = vec![0i16; far.len() + search_headroom(rate)];
        for (i, &s) in far.iter().enumerate() {
            near[delay + i] = (f32::from(s) * 0.4) as i16;
        }

        let (lag, _) = segment_lag(&near, &far, rate).expect("a measurable lag");
        assert!(
            (lag - delay as isize).abs() <= 2,
            "got {lag}, expected ~{delay}"
        );
    }

    /// Box-filter averaging is the anti-alias filter, so a decimated run of
    /// constant samples must come back at the same level rather than scaled.
    #[test]
    fn decimate_preserves_level() {
        let samples = vec![100.0f32; 80];
        let out = decimate(&samples, 8);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|&v| (v - 100.0).abs() < 1e-3));
        // A factor of one is a copy, not a no-op returning nothing.
        assert_eq!(decimate(&samples, 1).len(), 80);
    }

    /// A lag whose window falls outside the available data must score zero
    /// rather than panicking or wrapping around.
    #[test]
    fn best_lag_handles_out_of_range_lags() {
        let far = [1.0f32, -1.0, 1.0, -1.0];
        let near = [0.0f32, 1.0, -1.0, 1.0, -1.0, 0.0];
        let (lag, _) = best_lag(&near, &far, -50, 50).expect("a peak");
        assert_eq!(lag, 1);

        assert!(best_lag(&near, &[], -5, 5).is_none());
        assert!(best_lag(&near, &far, 5, -5).is_none());
        assert!(best_lag(&near, &[0.0; 4], -5, 5).is_none());
    }

    /// Far-only frames are where the delay is measurable and where ERLE is
    /// meaningful. Picking up double-talk frames here would let the user's own
    /// voice compete with the echo for the correlation peak.
    #[test]
    fn far_only_frames_exclude_double_talk() {
        let mic = [
            0.0, 0.0, 9_000.0, 9_000.0, 0.0, 0.0, 9_000.0, 9_000.0, 0.0, 0.0,
        ];
        let far = [
            0.0, 9_000.0, 9_000.0, 0.0, 9_000.0, 0.0, 0.0, 9_000.0, 9_000.0, 0.0,
        ];

        let far_only = far_only_frames(&mic, &far);
        assert!(far_only.contains(&1), "far active, near quiet");
        assert!(!far_only.contains(&2), "double-talk must be excluded");
        assert!(!far_only.contains(&3), "near-only must be excluded");
        assert!(!far_only.contains(&0), "silence must be excluded");
    }

    #[test]
    fn frame_rms_is_zero_for_silence_and_nonzero_for_signal() {
        assert_eq!(frame_rms(&[0i16; 480], 480), vec![0.0]);
        let rms = frame_rms(&[1_000i16; 480], 480);
        assert!((rms[0] - 1_000.0).abs() < 1.0);
        assert!(frame_rms(&[1i16; 10], 0).is_empty());
    }

    #[test]
    fn delay_sources_have_stable_names() {
        assert_eq!(DelaySource::Measured.as_str(), "measured");
        assert_eq!(DelaySource::MetaOffset.as_str(), "meta_offset");
        assert_eq!(DelaySource::Zero.as_str(), "zero");
    }
}
