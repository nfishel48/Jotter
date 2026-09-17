# Audio capture — status

Captures your microphone and everyone else's audio as two separate WAV tracks,
app-agnostically, via cpal. **Verified working end to end on macOS 26.3.1.**

## Quick start

```sh
scripts/run_app.sh                # build + bundle + launch the tray app
scripts/check_audio.sh            # CLI loop: tone → record → analyze (silent)
scripts/check_audio.sh system     # isolate system audio
scripts/check_audio.sh mic        # isolate microphone
scripts/bundle.sh                 # rebuild build/Jotter.app
```

One binary serves both front ends. `jotter` with no arguments opens the tray app
— which is what LaunchServices does when it starts the bundle — and the
subcommands are the CLI:

```sh
jotter devices                                    # list devices and loopback flags
jotter record --duration 10                       # capture both tracks for 10s
jotter record --only mic                          # one permission at a time
jotter record --system <id> --out /tmp/take1
jotter --help
```

Either half can be dropped at build time: `cargo build --no-default-features
--features cli` leaves out eframe/egui/tray-icon entirely, and `--features gui`
leaves out clap.

## The tray app

`scripts/run_app.sh` builds, bundles and launches it. The app has a Dock icon
(`LSUIElement` is `false` in `scripts/bundle.sh`) and shows its window on
launch; closing the window hides it rather than quitting, and the tray reopens
it. The menu has **Start/Stop Recording**,
**Settings** and **Quit**; the record item's label flips to reflect state, and
left-clicking the icon opens the settings window.

Settings shows a record button with an elapsed timer, mic and system-audio
device pickers (locked while recording, since a device cannot change underneath
a live stream), and a per-track summary when a recording finishes.

A recording survives closing the window. Both exit paths finalize it first —
the tray's Quit calls `stop_recording` before `exit(0)`, and `App::on_exit`
covers Cmd-Q, which bypasses the tray menu entirely. This matters more than it
looks: an abandoned stream leaves a WAV whose RIFF header still holds a
placeholder length, and many tools refuse to open it, so a whole meeting would
be unreadable.

Recordings go to `~/Documents/Jotter/2026-09-15_14-32-08/`. The path has to be
absolute — a bundled app's working directory is `/`, so a relative path would
try to write to `/recordings`. Folder names are local time, zero-padded so
lexical order matches chronological order (`chrono` is a dependency for exactly
this; deriving a DST-correct local offset by hand is a real bug surface).

Documents is TCC-gated, so the **first** recording triggers a one-time "access
files in your Documents folder" prompt — hence `NSDocumentsFolderUsageDescription`
in the plist. If it is denied, `create_dir_all` fails and the error shows in the
settings pane rather than failing silently.

The settings pane has an **Open recordings folder** button and makes the saved
path of a finished recording a clickable link. Both create the directory before
opening it, so the button still works before the first recording.

Two eframe details this depends on:

- Tray polling lives in `App::logic`, **not** `App::ui`. eframe 0.36 runs no
  egui pass at all while the window is hidden and calls `logic` instead — so in
  `ui` the tray menu would be dead exactly when it is the only interface the
  user has.
- `logic` only runs when a repaint is pending, so it re-arms itself with
  `request_repaint_after` (200ms recording, 500ms idle). Measured cost of that
  loop is ~0.4% CPU.

Use `/usr/bin/python3` explicitly in anything you add — the `python3` on PATH is
a homebrew alias pointing at a binary that no longer exists.

Last verified run:

```
mic.wav        8.00s  48000Hz 1ch  peak= 5786  rms= -36.5dBFS  -> audio present
system.wav     8.00s  48000Hz 1ch  peak= 8190  rms= -15.1dBFS  -> audio present
track offset: +0.007s
  system.wav 440Hz purity: 1.0000
  mic.wav    440Hz purity: 0.0001
  PASS: tone is isolated to system.wav; tracks are not crossed.
```

## You must run it through the .app bundle

This is the single most important operational fact. A bare `cargo build` binary
**cannot** capture system audio, and fails silently:

```
$ codesign -dv target/debug/jotter
Identifier=jotter-884d72896c4f307f      # changes every rebuild
flags=0x20002(adhoc,linker-signed)
$ file target/debug/jotter
Mach-O 64-bit executable arm64          # not a bundle
```

A linker-signed Mach-O has no bundle identifier, so TCC cannot register it in
the Privacy lists. It never prompts — it just feeds the tap digital zeros. The
observed symptoms were a system track of 7.9s of perfectly zero samples, and a
microphone stream that blocked ~7 minutes on a prompt that could not be
displayed before failing with `Illegal operation`.

`scripts/bundle.sh` fixes this by assembling `build/Jotter.app` with a stable
`CFBundleIdentifier` (`com.nfishel.jotter`) and the three usage-description
keys. Launch it through LaunchServices so TCC attributes the request to the
bundle rather than to your terminal:

```sh
open -a build/Jotter.app --stdout /tmp/jotter.out --stderr /tmp/jotter.err \
     --args record --only both --duration 10 --out /tmp/rec
```

Running `build/Jotter.app/Contents/MacOS/jotter` directly does *not* work —
that re-attributes the request to the terminal. `open` detaches stdout, which
is why `--stdout`/`--stderr` are needed to see output.

The bundle lives in `build/`, not `target/`, so `cargo clean` cannot destroy it.
TCC keys partly on path; losing it means re-granting.

Caveat: `security find-identity` reports no signing identities on this machine,
so the bundle is ad-hoc signed and keyed by cdhash. A rebuild may invalidate the
grant and re-prompt. Creating a self-signed code-signing certificate would give
a stable identity and fix that permanently.

## Testing is silent by default

CoreAudio process taps capture the stream **before** device volume, so
`check_audio.sh` sets output volume to 0 for the duration and still captures at
full amplitude (verified: `peak=8190` with volume at 0), restoring your volume
afterwards. Pass `--audible` to hear it.

Muting is not only a courtesy — it removes the acoustic path between speakers
and microphone, which is what makes the swapped-track check below meaningful.

## Linux (PipeWire)

**Status: compiles clean, runtime untested.** Verified only by
`scripts/check_linux_build.sh`, which runs `cargo check --all-targets` in a
`rust:1-bookworm` container with the real system headers. Nobody has yet
confirmed that monitor capture actually produces audio on a live PipeWire
session — that needs a Linux machine.

```sh
scripts/check_linux_build.sh         # cargo check in a container
scripts/check_linux_build.sh build   # full build (slower)
```

The Linux target adds cpal's `pipewire` feature (see `Cargo.toml`). cpal's
`default_host()` then prefers PipeWire over PulseAudio and ALSA whenever the
daemon is running, so no host selection is needed in our code.

**The loopback idiom is the same on all three platforms** — open an *input*
stream on an *output* device — which is why one code path covers them:

| Platform | Mechanism |
| --- | --- |
| macOS | `CATapDescription` over all processes + private aggregate device |
| Windows | `AUDCLNT_STREAMFLAGS_LOOPBACK` |
| Linux | `STREAM_CAPTURE_SINK` on a sink node — PipeWire monitor capture |

The `supports_input()` duplex guard is meaningful on Linux too, and for a
non-obvious reason: the PipeWire host **overrides** `supports_input()` to report
the node's direction rather than probing for configs
(`host/pipewire/device.rs:232`), so a sink answers `false` exactly as a
CoreAudio output device does.

Build dependencies (Debian/Ubuntu names):

```
pkg-config clang libclang-dev
libpipewire-0.3-dev libspa-0.2-dev libasound2-dev
libgtk-3-dev libayatana-appindicator3-dev
libx11-dev libxcursor-dev libxrandr-dev libxi-dev
libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
```

This list is what CI installs; keep the two in step, since a package that is
only in one of them shows up as a build that works in exactly one place.

The X11 packages are required even on a Wayland desktop (Ubuntu's default since
21.04). Which display server the *session* runs is a runtime choice; winit
compiles its x11 and wayland backends both and selects at startup, and gtk3
links X11 unconditionally. There is deliberately **no `libxdo-dev`**: `tray-icon`
enables `libxdo` by default, but muda only uses it to synthesise X11 key events
for predefined menu items (Copy/Paste/…), which this tray does not use and which
could not work under Wayland anyway. `Cargo.toml` turns that feature off, so the
bare `-lxdo` link it adds is gone.

Differences from macOS:

- **No bundle, no permissions dance.** Run `scripts/run_app.sh`, which builds
  and launches `target/debug/jotter` directly. `scripts/bundle.sh` refuses to
  run on Linux.
- **The tone is audible during tests.** Muting is a macOS-only trick: CoreAudio
  taps are verified to capture before device volume, but no equivalent
  guarantee is assumed for a sink monitor, where muting might record silence
  and turn a passing test into a confusing failure. Use headphones.
- **The swapped-track check is skipped**, because it depends on muting to
  remove the speaker-to-mic path. Compare the 440 Hz purity figures by hand.
- **If PipeWire is not running**, cpal falls back to ALSA, where sinks are not
  capturable and system audio cannot work at all. The scripts warn about this
  up front rather than letting you record eight seconds of nothing.
- Recordings honour `XDG_DOCUMENTS_DIR` when set, falling back to
  `~/Documents/Jotter`.

## What is verified working (macOS)

**Device topology — the main risk from planning, and it resolved well.**

```
NAME                     DIRECTION IN    OUT   LOOPBACK
Nate's AirPods Pro #2    Input     true  false NO        default-in
Nate's AirPods Pro #2    Output    false true  yes       default-out
MacBook Pro Microphone   Input     true  false NO
MacBook Pro Speakers     Output    false true  yes
```

cpal has no explicit loopback API: calling `build_input_stream()` on an *output*
device makes the CoreAudio backend create a `CATapDescription` over all
processes (that's what makes it app-agnostic) plus a private aggregate device.
It only takes that branch when the device reports `supports_input() == false`:

```rust
// cpal-0.18.2/src/host/coreaudio/macos/device.rs:727
let mut audio_unit = if self.supports_input() {
    audio_unit_from_device(self, AudioUnitMode::Input)?              // plain mic!
} else {
    loopback_aggregate.replace(LoopbackDevice::from_device(self)?);  // loopback
    ...
};
```

Hand it a **duplex** device and it silently records the microphone into your
"system audio" file, with no error, discovered only at transcription time.
Your AirPods enumerate as two *separate unidirectional* devices, so the output
side takes the loopback branch correctly. `open_loopback` refuses duplex devices
outright; `--force-system-on-duplex` overrides for diagnosis only.

Also confirmed: callbacks only flow while audio is actually routed to the tapped
device. An idle device yields zero frames — which is why `check_audio.sh` plays
a tone rather than testing against silence. And the tap is created `Unmuted`, so
the meeting still plays out of your speakers during capture.

**The tracks are genuinely separate.** Amplitude alone cannot prove this —
speaker bleed puts the tone in both files, so a crossed-streams bug would still
look like "both tracks have audio". `analyze_wav.py` therefore measures how much
of each track's energy sits at exactly 440 Hz (Goertzel, no numpy — the system
python has none):

| Track | 440 Hz purity |
| --- | --- |
| `system.wav` | 1.0000 — a pure digital tone straight from the tap |
| `mic.wav` | 0.0001 — room audio, essentially no tone content |

With output muted there is no acoustic path, so any pure tone appearing in
`mic.wav` would prove the streams are crossed. It doesn't. Measured track offset
is +0.007s.

## Echo cancellation

On speakers, the microphone re-captures the remote participants. Every remote
voice then lands in *both* tracks, which defeats the point of recording two of
them and hands transcription a doubled copy of the remote side. Headphones make
the problem vanish; laptop speakers make it the dominant content of `mic.wav`.

```
jotter process recordings/<dir>      # clean an existing recording
jotter record --duration 600 --aec   # clean it as soon as recording stops
scripts/check_aec.sh recordings/<dir># grade the result
```

**On by default.** Turn it off in the settings pane, with `--no-aec`, or by
setting `aec_enabled` to `false` in the config file.

Defaulting on is only defensible because the pass cannot damage a recording:
`mic.wav` is never modified, a recording it cannot handle is declined with the
reason recorded, and `Meta::preferred_mic_path()` refuses to pass on a result
whose own measurements do not clear the bar. The cost of being wrong about any
given recording is one unused file. With headphones there is no echo to remove
and it costs a few seconds of processing that finds nothing.

### What it achieves

Measured on a 8m56s Linux/PipeWire meeting recorded on laptop speakers, by
activity class:

| what was happening | secs | level change |
| --- | --- | --- |
| remote side only | 122 | **-21.1 dB** |
| both at once | 233 | **-18.0 dB** |
| you only | 40 | -0.4 dB |
| neither | 141 | -0.8 dB |

Echo goes from roughly 1000 rms to 19 while the user's own voice is left alone.
Per band the removal is even at 17-21 dB from 150 Hz to 8 kHz.

The two numbers are not interchangeable and the second is the one that matters.
An echo canceller that quietly eats the near end scores beautifully on echo
removal alone, so a pass is graded on both, and `scripts/check_aec.py`
re-measures them independently of the Rust code that produced them.

### Design notes

- **WebRTC AEC3, not a hand-rolled filter.** One was built first, over vendored
  Speex MDF, and reached 1.2 dB where AEC3 reaches 21. The gap is not tuning:
  AEC3 models the *nonlinear* part of the echo path, which is most of it when the
  source is a small speaker driven loud, and no linear adaptive filter reaches
  that however long its tail. Two independent linear-only measurements of this
  recording predicted a ceiling in the low single digits; AEC3 cleared it by
  30 dB, so the premise those measurements rested on was simply wrong.
- **The tracks are fed unaligned.** AEC3 estimates the echo delay itself. An
  earlier version pre-shifted the mic track by our own measurement, which is
  actively dangerous: an alignment slightly *too large* asks the filter to model
  an echo arriving before its cause, and nothing can express that. We still
  measure the delay — `meta.json` records both estimates so they can be compared
  — but only to report it and to run the drift and swapped-track guards.
- **Two passes.** The first converges the filter and its output is discarded; the
  second re-runs from the start with it already trained, so the opening of the
  recording is cancelled as well as the rest. Worth 5 dB on echo-only passages.
- **Costs meson and ninja at build time.** The `bundled` feature compiles
  WebRTC's C++ from source rather than linking a system
  `libwebrtc-audio-processing`, so no user is missing a package. All of it sits
  behind the `aec` cargo feature, and CI checks the leg without it so turning it
  off keeps needing no C++ toolchain.

### When it declines

Subtracting a misaligned reference *adds* uncorrelated energy, which is worse
than doing nothing, so the pass would rather stop than guess. Every refusal is
recorded in `meta.json` as `aec.bypassed` — a pass can always say what it
decided and why, because silence is indistinguishable from a crash.

The sharpest case is macOS-specific. An idle output device yields no frames at
all, so silence is compressed *out* of `system.wav` rather than recorded, and the
two tracks end up different lengths with no single delay able to align them.
`recordings/1789486023/meta.json` has 10.0s of mic against 4.6s of system from
the same recording. The pass bypasses above 250 ms of difference; the reference
Linux pair differs by 21 ms, which is two callback buffers and must not trip it.

Clock drift is the other one. Two devices on one clock — the normal case — drift
at essentially zero, but a USB mic against built-in speakers is two clock domains
and can reach 100 ppm, or 0.36 s per hour. No single delay describes a recording
like that, including AEC3's own, so above 20 ppm the pass declines.

## Code layout

```
src/lib.rs              pub mod audio, plus the feature-gated cli and ui
src/main.rs             clap parsing; no subcommand -> GUI, otherwise -> cli
src/cli.rs              record / devices subcommands       (feature "cli")
src/ui.rs               App state machine, tray pumping    (feature "gui")
src/audio/mod.rs        Recorder: start(RecordConfig) -> RecordingHandle, .stop()
src/audio/devices.rs    enumeration, direction classification, default selection
src/audio/capture.rs    open_mic / open_loopback, duplex guard, error mapping
src/audio/writer.rs     mpsc -> hound writer thread, f32->i16, mono downmix
src/audio/meta.rs       meta.json sidecar, timestamp_dir_name()
src/audio/aec/mod.rs    WebRTC AEC3 wrapper, activity thresholds  (feature "aec")
src/audio/aec/delay.rs  echo-delay measurement, drift and swap guards
src/audio/process.rs    the offline pass: WAV I/O, two passes, meta rewrite
```

Output per recording:

```
recordings/<timestamp>/
├── mic.wav      (you)
├── mic_aec.wav  (you, with speaker echo removed — only if the pass ran)
├── system.wav   (everyone else)
└── meta.json
```

### Design notes worth not re-deriving

- **Two tracks, not one mixed file.** Merged mono loses overlapping speech
  (Whisper drops or garbles a speaker), forces diarization to recover "me" from
  scratch instead of knowing it for free, and prevents per-track gain
  normalization. You can always mix down later; you can never un-mix.
- **Native 48 kHz is preserved.** Resampling to Whisper's 16 kHz belongs at
  transcription time with a real resampler; decimating in the audio callback
  would alias and cost exactly the quality this is optimizing for.
- **The callback never touches the filesystem.** It downmixes, converts to i16,
  and hands an owned buffer to a writer thread over an mpsc channel. Blocking a
  realtime audio thread on I/O causes dropouts.
- **Echo cancellation is offline, and additive.** It runs after the recording
  ends, not in the callback, and writes a second file rather than modifying
  `mic.wav`. The raw mic track is the one artifact that cannot be recreated.
- **A pure tone is a useless reference for testing echo cancellation.** It
  excites one frequency, so the echo path is unidentifiable everywhere else and
  the resulting figure looks spectacular while meaning nothing. `check_audio.sh`
  has a 440 Hz tone right there and it is the wrong tool; `check_aec.sh` uses
  speech-shaped noise. Related: `check_audio.sh` *mutes* output deliberately, to
  remove the very speaker-to-mic path echo cancellation exists to address.
- **`f32 → i16` clamps before scaling.** Loopback audio can exceed ±1.0 when an
  app applies its own gain, and wrapping would turn a loud passage into harsh
  noise.
- **Mic defaults to the built-in mic, not the system default.** macOS drops
  Bluetooth headsets into a degraded call mode once their mic is activated,
  which hurts transcription of your own track. Override with
  `jotter record --mic <id>`.
- **`stream.play()`, not `start()`.** cpal 0.17 stopped auto-starting streams.
  `start()`/`stop()` exist only on unreleased master; 0.18.2 is `play()`/`pause()`.
- **Errors print cpal's `ErrorKind`.** cpal's own `Display` prints only the
  backend message and drops the kind, which is why the first failure read as a
  useless `Illegal operation` instead of something permission-shaped.
- **`meta.json` records each stream's first-callback `StreamInstant`.** The two
  streams have independent clocks; on macOS both instants derive from host time,
  so their difference aligns the tracks. Drift is negligible here — same
  physical device shares a clock, and action-item extraction isn't lip-sync
  sensitive.

## Build layout

```
scripts/bundle.sh       assembles build/Jotter.app (Info.plist + ad-hoc signing)
scripts/check_audio.sh  end-to-end: build → tone → record → analyze → verdict
scripts/analyze_wav.py  per-track duration/peak/RMS + 440Hz purity
build/Jotter.app        the signed bundle (gitignored; survives cargo clean)
```

`bundle.sh` copies the single `jotter` binary into the bundle and pins one
bundle id, so one permission grant covers the CLI and the tray app. It takes no
arguments — which of the two you get is decided by the arguments you pass to
`open --args`, not at bundle time.

## Next steps

1. Real meeting test: a 30+ min call, confirming the files finalize cleanly and
   `stream_errors` stays 0 in `meta.json`.
2. Tray wiring — Start/Stop recording in the existing menu, device pickers in
   the settings pane. `RecordingHandle` owns `!Send` cpal streams, so it has to
   live on the thread that created it.
3. Hand the WAVs to whisper → `action_items.sh`, and confirm two-track input
   actually improves speaker attribution over a mixed file.
4. Consider a self-signed certificate so TCC grants survive rebuilds.
5. Gap-fill `system.wav` at the writer, so an idle output device produces silence
   rather than a shorter file. Fixes alignment for every consumer, not just echo
   cancellation, and would let the pass stop declining on such recordings.
6. Run `scripts/check_aec.sh --live` on Linux/PipeWire; echo cancellation has
   only been measured against a recorded Linux session, not a live loop.

## Known gaps

- The macOS idle-tap gap is *guarded against* rather than fixed: the pass
  declines when the tracks differ in length. The real fix is gap-filling at the
  writer, using the per-buffer callback timestamps `TrackSink::push` already
  receives and currently discards after the first. Until then, a recording whose
  output device sat idle partway through gets no echo removal at all — correctly,
  but it is a silent loss of the feature rather than a failure.
- **Echo cancellation has only been measured against a recorded session**, not a
  live acoustic loop. `scripts/check_aec.sh --live` exists for that and has not
  been run on either platform.
- **Linux is compile-verified only** — no one has run it against a live
  PipeWire session. See the Linux section above.
- **Windows is entirely unverified**, not even compile-checked. The loopback
  idiom is the same and WASAPI loopback is well-trodden, but nothing here
  has exercised it.
- Longest recording tested is ~8 seconds. Nothing yet confirms a 30-minute
  capture finalizes cleanly or that `stream_errors` stays 0.
- No disk-space guard on long recordings: 48 kHz 16-bit mono ≈ 5.5 MB/min/track.
