//! Compiles the vendored Speex echo canceller.
//!
//! Compiles nothing unless the `aec` feature is on, so a build without it needs
//! no C toolchain at all — the same guarantee `telemetry` gives for the network
//! stack.
//!
//! The gate is an environment variable rather than `#[cfg(feature = "aec")]`
//! because Cargo hands features to a build script only as `CARGO_FEATURE_*`;
//! `cfg(feature = ...)` is always false here and would silently skip the build.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Not `cfg!(feature = "aec")` — see the module comment. This is the only
    // thing standing between an `aec`-less build and a C compiler requirement.
    if std::env::var_os("CARGO_FEATURE_AEC").is_none() {
        return;
    }

    let vendor = std::path::Path::new("src/audio/aec/vendor");

    let mut build = cc::Build::new();
    build
        .include(vendor)
        // `arch.h` includes "speex/speexdsp_types.h", so the vendor root — not the
        // `speex/` subdirectory — is what has to be on the include path.
        .files(
            ["mdf.c", "fftwrap.c", "kiss_fft.c", "kiss_fftr.c"]
                .iter()
                .map(|f| vendor.join(f)),
        )
        // Mandatory: `arch.h:57` is an #error if neither this nor FIXED_POINT is set.
        .define("FLOATING_POINT", None)
        // `fftwrap.c` selects its FFT backend purely by #ifdef and has no default, so
        // without this it compiles to an empty translation unit and mdf.c fails to link.
        .define("USE_KISS_FFT", None)
        // We link statically, so the upstream dllexport decoration is not wanted.
        .define("EXPORT", Some(""))
        // mdf.c writes to stderr when its divergence detector fires ("The echo
        // canceller started acting funny and got slapped"). It fires legitimately
        // on a hard double-talk burst and recovers, so it is not an error — but a
        // CLI has no business printing that at a user mid-meeting. The health
        // signal we actually act on is the ERLE and near-gain recorded in
        // meta.json, which says more than a stderr line ever could.
        .define("DISABLE_WARNINGS", None)
        // Upstream C, not ours to clean up. Left on for anything that could indicate a
        // miscompile rather than a style opinion.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .warnings(false);

    build.compile("speexdsp_aec");

    for entry in std::fs::read_dir(vendor).expect("vendor directory is missing") {
        let path = entry.expect("vendor directory entry").path();
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed={}", vendor.join("speex").display());
}
