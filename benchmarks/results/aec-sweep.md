# Echo cancellation sweep

| ERL (dB) | ERLE (dB) | near-end damage (dB) | far-only (s) | double-talk (s) | delay (ms) | used | WER before | WER after | delta |
| ---: | ---: | ---: | ---: | ---: | ---: | :--- | ---: | ---: | ---: |
| 0 | 12.5 | -0.2 | 0.1 | 8.0 | 16 | yes | 50.0% | 33.3% | -16.7% |
| 0 | 13.6 | -0.2 | 0.4 | 10.5 | 16 | yes | 47.1% | 23.5% | -23.5% |
| 6 | 10.8 | -0.2 | 0.6 | 7.5 | 16 | yes | 50.0% | 33.3% | -16.7% |
| 6 | 7.8 | -0.2 | 1.8 | 9.1 | 16 | yes | 67.6% | 14.7% | -52.9% |
| 12 | 12.9 | -0.2 | 2.9 | 5.2 | 16 | yes | 75.0% | 33.3% | -41.7% |
| 12 | 8.8 | -0.2 | 4.2 | 6.7 | 16 | yes | 67.6% | 11.8% | -55.9% |
| 18 | 10.5 | -0.2 | 4.6 | 3.5 | 16 | yes | 11.1% | 30.6% | +19.4% |
| 18 | 9.5 | -0.2 | 6.8 | 4.1 | 16 | yes | 67.6% | 14.7% | -52.9% |
| 24 | 7.2 | -0.2 | 6.3 | 1.8 | 16 | yes | 11.1% | 16.7% | +5.6% |
| 24 | 6.5 | -0.2 | 7.7 | 3.2 | 16 | yes | 8.8% | 11.8% | +2.9% |

`ERL` is how far the speaker bleed sits below the near-end voice — low is hard.
`ERLE` is echo removed, measured on far-only frames; higher is better.
`near-end damage` is level lost off the user's own voice on near-only frames;
near zero is the requirement, and `src/audio/meta.rs` rejects the cancelled
track below -1 dB however good the ERLE looks.
`used` says whether the cancelled track was kept, or names the reason the
pass bypassed.
`WER before`/`after` are the mic transcript scored against the near speaker's
words alone, with and without the echo pass in front of it. `delta` is
negative when cancelling helped; `=` means the transcript did not change at
all, which is what happens when `meta.rs` rejects the cancelled track and
both passes end up reading the same audio.

**The delta column is the verdict.** Echo removed in dB is the mechanism, not
the result: a pass that improves ERLE by 20 dB and moves no words has not
helped anyone. Read the dB columns to explain the delta, not instead of it.

**Read the far-only column before the ERLE column.** The activity classifier
(`process::classify`) labels frames by energy, per track, so when the bleed is
loud enough the far-only stretch is labelled double-talk instead — and the pass
bypasses with `no_far_only_windows` because it has nowhere to learn the echo
path. That is a real property of an energy-based classifier and not a fault in
the test set: at low ERL there is genuinely no stretch where the microphone is
quiet while the room is loud. A row with `far-only 0` and no ERLE is that
situation, and it is a result rather than a gap.
