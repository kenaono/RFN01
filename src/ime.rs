//! Telling the IME which way the text runs.
//!
//! 一太郎 and Word put the candidate list beside the column being written, and
//! **the list itself runs vertically**. That is why it fits there: a vertical
//! list is narrow and grows downwards, so it does not expand over the text the
//! way a horizontal one does. Without it there is no good place to put the list
//! in vertical writing — every attempt trades covering the text for appearing a
//! dozen columns away (技術検証 7.2).
//!
//! The switch is the composition font. A face name beginning with `@` is the
//! long-standing way to say "this run is vertical" on Windows, and an escapement
//! of 2700 (270 degrees, in tenths) turns it. MS-IME and ATOK both answer by
//! turning their candidate list.

use slint::ComponentHandle;
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{DEFAULT_CHARSET, LOGFONTW, SHIFTJIS_CHARSET},
    UI::Input::Ime::{ImmGetContext, ImmReleaseContext, ImmSetCompositionFontW},
};

use crate::AppWindow;

/// The composition font's face, with and without the vertical marker.
///
/// The same family either way: this is not about what the IME draws — the
/// editor draws the text itself — but about what it is being told the run is.
const VERTICAL_FACE: &str = "@Yu Mincho";
const HORIZONTAL_FACE: &str = "Yu Mincho";

/// Tell the IME whether the text being composed runs vertically.
///
/// Best effort, and silent about failure. A composition font that cannot be set
/// leaves the IME laying its windows out as it did, which is the behaviour this
/// improves on rather than depends on.
pub fn set_vertical(window: &AppWindow, vertical: bool) {
    let Some(hwnd) = window_handle(window) else {
        return;
    };
    let mut font = LOGFONTW {
        lfHeight: -22,
        lfCharSet: if vertical {
            SHIFTJIS_CHARSET
        } else {
            DEFAULT_CHARSET
        },
        ..Default::default()
    };
    // Tenths of a degree, counted anticlockwise: 2700 lays the run top to
    // bottom. Both fields are set because IMEs have been seen to read either.
    if vertical {
        font.lfEscapement = 2700;
        font.lfOrientation = 2700;
    }
    let face = if vertical {
        VERTICAL_FACE
    } else {
        HORIZONTAL_FACE
    };
    for (slot, unit) in font.lfFaceName.iter_mut().zip(face.encode_utf16()) {
        *slot = unit;
    }

    // SAFETY: The handle belongs to a window that is alive for this call, and
    // the context is released on every path out.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_invalid() {
            return;
        }
        let _ = ImmSetCompositionFontW(context, &font);
        let _ = ImmReleaseContext(hwnd, context);
    }
}

/// The window's own handle, or `None` if this is not a Win32 window.
///
/// Shared with the file dialogs, which need an owner to be modal to.
pub fn window_handle(window: &AppWindow) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Slint hands back its own handle type; the trait method on it is what
    // reaches the platform one.
    let slint_handle = window.window().window_handle();
    let handle = slint_handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}
