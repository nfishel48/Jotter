//! Raw bindings to the vendored Speex echo canceller.
//!
//! Every `unsafe` in the AEC lives in this file. Callers get
//! [`super::EchoCanceller`], which is a safe RAII wrapper; nothing outside this
//! module sees a raw pointer.
//!
//! Declared by hand rather than generated with bindgen: the surface is five
//! functions and four constants, and a bindgen dependency would drag `clang` in
//! as a build requirement for a header we can read in one sitting.

use std::os::raw::{c_int, c_void};

/// Opaque `SpeexEchoState`. Never constructed on the Rust side — only ever held
/// behind a pointer from `speex_echo_state_init`.
#[repr(C)]
pub struct SpeexEchoState {
    _private: [u8; 0],
}

// The `speex_echo_ctl` request numbers we use, from vendor/speex/speex_echo.h.
// Named rather than inlined because they are positional ints in C and a typo
// would silently target a different setting.

/// `SPEEX_ECHO_SET_SAMPLING_RATE` — takes `*const c_int`.
pub const SET_SAMPLING_RATE: c_int = 24;
/// `SPEEX_ECHO_GET_SAMPLING_RATE` — takes `*mut c_int`.
pub const GET_SAMPLING_RATE: c_int = 25;
/// `SPEEX_ECHO_GET_IMPULSE_RESPONSE_SIZE` — takes `*mut c_int`.
pub const GET_IMPULSE_RESPONSE_SIZE: c_int = 27;
/// `SPEEX_ECHO_GET_IMPULSE_RESPONSE` — takes a `*mut spx_int32_t` buffer of the
/// size reported above.
pub const GET_IMPULSE_RESPONSE: c_int = 29;

unsafe extern "C" {
    /// `frame_size` should be 10-20 ms of samples; `filter_length` 100-500 ms,
    /// and a whole multiple of `frame_size`.
    pub fn speex_echo_state_init(frame_size: c_int, filter_length: c_int) -> *mut SpeexEchoState;

    pub fn speex_echo_state_destroy(st: *mut SpeexEchoState);

    /// `rec` is the mic (near end plus echo), `play` is what went to the
    /// speakers, `out` receives the near end with the echo removed. All three
    /// are `frame_size` samples long.
    pub fn speex_echo_cancellation(
        st: *mut SpeexEchoState,
        rec: *const i16,
        play: *const i16,
        out: *mut i16,
    );

    pub fn speex_echo_state_reset(st: *mut SpeexEchoState);

    /// Returns 0 on success, -1 for an unknown request.
    pub fn speex_echo_ctl(st: *mut SpeexEchoState, request: c_int, ptr: *mut c_void) -> c_int;
}
