# Vendored Speex DSP — the echo canceller only

Copied verbatim from [xiph/speexdsp](https://github.com/xiph/speexdsp) at commit
`7a158783df74efe7c2d1c6ee8363c1e695c71226` (recorded in `.upstream-sha`).
BSD-3-Clause; see `COPYING`.

## Why vendor rather than depend

The alternative was `webrtc-audio-processing`, whose AEC3 is the better canceller.
It needs `clang` + `pkg-config` + `meson` + `ninja-build` on every builder and pulls
its C++ in as a git submodule. `.github/workflows/release.yml` cross-builds
`aarch64-apple-darwin` *and* `x86_64-apple-darwin` and `lipo`s them into one binary;
meson-driven cross builds for the non-native arch are exactly where that breaks.

These four C files are built by `cc`, which handles `-arch` itself, so the release
job is untouched. `mdf.c` is the canceller PulseAudio's `module-echo-cancel` shipped
for years.

## What is here, and what is deliberately not

| File | Role |
| --- | --- |
| `mdf.c` | The multi-delay block frequency-domain adaptive filter. The whole point. |
| `fftwrap.c` | FFT backend shim. Has no default backend — see `USE_KISS_FFT` below. |
| `kiss_fft.c`, `kiss_fftr.c` | The FFT itself. Bundled, so no FFT dependency is needed anywhere in jotter. |
| headers | `arch.h`, `os_support.h`, `math_approx.h`, `pseudofloat.h`, `fixed_generic.h`, `fftwrap.h`, `kiss_fft.h`, `kiss_fftr.h`, `_kiss_fft_guts.h` |
| `speex/speex_echo.h`, `speex/speexdsp_types.h` | The public API. |
| `speex/speexdsp_config_types.h` | **Hand-written.** Upstream ships only a `.in` autotools template. |

Not vendored, on purpose:

- **`smallft.c`** — the alternative FFT backend. We use kiss.
- **`preprocess.c`, `filterbank.c`** — the residual echo suppressor and noise gate.
  It is a nonlinear gate, and aggressive suppression damages transcription more than
  residual echo does. If it is ever added it ships default-off.
- **`resample.c`, `jitter.c`, `buffer.c`, `scal.c`** — unrelated to echo cancellation.

`mdf.c` uses no `VARDECL`/`ALLOC` stack macros, so `stack_alloc.h` is not needed either.

## Build defines (see `build.rs`)

| Define | Why |
| --- | --- |
| `FLOATING_POINT` | Mandatory. `arch.h:57` is an `#error` if neither this nor `FIXED_POINT` is set. |
| `USE_KISS_FFT` | `fftwrap.c` has no default backend and will not compile without one. |
| `EXPORT=` | Empty; we link statically. |

`HAVE_CONFIG_H` is left undefined, so no generated `config.h` is needed.
`VAR_ARRAYS` and `USE_ALLOCA` are left undefined: without them `fftwrap.c` uses
fixed `MAX_FFT_SIZE` stack arrays instead of C99 VLAs, which keeps the code portable
to toolchains without VLA support. Those code paths are inside `#ifdef FIXED_POINT`
and are dead in our build regardless.

## Updating

Bump the commit, re-copy the files listed above, keep the hand-written
`speexdsp_config_types.h`, update `.upstream-sha`, and run `scripts/check_aec.sh`
against a real recording — the unit tests prove the binding works, but only the ERLE
gate proves the canceller still cancels.
