# Echo cancellation sweep

| ERL (dB) | ERLE (dB) | near-end damage (dB) | far-only (s) | double-talk (s) | delay (ms) | used |
| ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| 0 | 12.5 | -0.2 | 0.1 | 8.0 | 16 | yes |
| 0 | 13.6 | -0.2 | 0.4 | 10.5 | 16 | yes |
| 0 | 48.6 | -0.3 | 0.4 | 10.8 | 16 | yes |
| 0 | 7.9 | -0.4 | 0.1 | 6.3 | 16 | yes |
| 6 | 10.8 | -0.2 | 0.6 | 7.5 | 16 | yes |
| 6 | 7.8 | -0.2 | 1.8 | 9.1 | 16 | yes |
| 6 | 15.2 | -0.3 | 1.1 | 10.1 | 16 | yes |
| 6 | 7.1 | -0.4 | 0.2 | 6.2 | 16 | yes |
| 12 | 12.9 | -0.2 | 2.9 | 5.2 | 16 | yes |
| 12 | 8.8 | -0.2 | 4.2 | 6.7 | 16 | yes |
| 12 | 12.4 | -0.3 | 2.3 | 8.9 | 16 | yes |
| 12 | 6.6 | -0.4 | 0.7 | 5.7 | 16 | yes |
| 18 | 10.5 | -0.2 | 4.6 | 3.5 | 16 | yes |
| 18 | 9.5 | -0.2 | 6.8 | 4.1 | 16 | yes |
| 18 | 9.0 | -0.3 | 4.5 | 6.7 | 16 | yes |
| 18 | 5.9 | -0.4 | 1.6 | 4.8 | 16 | yes |
| 24 | 7.2 | -0.2 | 6.3 | 1.8 | 16 | yes |
| 24 | 6.5 | -0.2 | 7.7 | 3.2 | 16 | yes |
| 24 | 6.0 | -0.3 | 5.5 | 5.7 | 16 | yes |
| 24 | 5.0 | -0.4 | 2.3 | 4.1 | 16 | yes |

`ERL` is how far the speaker bleed sits below the near-end voice — low is hard.
`ERLE` is echo removed, measured on far-only frames; higher is better.
`near-end damage` is level lost off the user's own voice on near-only frames;
near zero is the requirement, and `src/audio/meta.rs` rejects the cancelled
track below -1 dB however good the ERLE looks.
`used` says whether the cancelled track was kept, or names the reason the
pass bypassed.

**Read the far-only column before the ERLE column.** The activity classifier
(`process::classify`) labels frames by energy, per track, so when the bleed is
loud enough the far-only stretch is labelled double-talk instead — and the pass
bypasses with `no_far_only_windows` because it has nowhere to learn the echo
path. That is a real property of an energy-based classifier and not a fault in
the test set: at low ERL there is genuinely no stretch where the microphone is
quiet while the room is loud. A row with `far-only 0` and no ERLE is that
situation, and it is a result rather than a gap.
