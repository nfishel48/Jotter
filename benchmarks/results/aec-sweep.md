# Echo cancellation sweep

| ERL (dB) | ERLE (dB) | near-end damage (dB) | far-only (s) | double-talk (s) | delay (ms) | used | WER before | WER after | delta |
| ---: | ---: | ---: | ---: | ---: | ---: | :--- | ---: | ---: | ---: |
| 0 | 6.1 | -0.4 | 0.6 | 6.0 | 16 | yes | 30.8% | 15.4% | -15.4% |
| 0 | 14.0 | -0.4 | 0.6 | 10.2 | 16 | yes | 130.4% | 60.9% | -69.6% |
| 6 | 9.0 | -0.4 | 1.0 | 5.6 | 16 | yes | 26.9% | 0.0% | -26.9% |
| 6 | 12.8 | -0.3 | 1.7 | 9.1 | 16 | yes | 78.3% | 8.7% | -69.6% |
| 12 | 5.7 | -0.4 | 1.8 | 4.8 | 16 | no (ERLE 5.7 < 6) | 23.1% | 23.1% | = |
| 12 | 9.1 | -0.3 | 4.0 | 6.8 | 16 | yes | 65.2% | 4.3% | -60.9% |
| 18 | 6.3 | -0.4 | 2.6 | 4.0 | 16 | yes | 0.0% | 0.0% | = |
| 18 | 6.6 | -0.3 | 5.5 | 5.3 | 16 | yes | 69.6% | 8.7% | -60.9% |
| 24 | 2.8 | -0.4 | 2.7 | 3.9 | 16 | no (ERLE 2.8 < 6) | 0.0% | 0.0% | = |
| 24 | 2.5 | -0.3 | 5.7 | 5.1 | 16 | no (ERLE 2.5 < 6) | 26.1% | 26.1% | = |

`ERL` is how far the speaker bleed sits below the near-end voice — low is hard.
`ERLE` is echo removed, measured on far-only frames; higher is better.
`near-end damage` is level lost off the user's own voice on near-only frames;
near zero is the requirement, and `src/audio/meta.rs` rejects the cancelled
track below -1 dB however good the ERLE looks.
`used` says whether the *recogniser* read the cancelled track — the gate in
`Meta::preferred_mic_path` — naming the figure that failed when it did not.
A pass can write a file and still be overruled, and that is not a failure:
it is the product declining to transcribe audio it judged worse than the raw.
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
