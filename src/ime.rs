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

use std::rc::Rc;
use std::time::Duration;

use slint::ComponentHandle;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{DEFAULT_CHARSET, LOGFONTW, SHIFTJIS_CHARSET},
    UI::{
        Input::Ime::{
            ATTR_CONVERTED, ATTR_FIXEDCONVERTED, ATTR_INPUT_ERROR, ATTR_TARGET_CONVERTED,
            ATTR_TARGET_NOTCONVERTED, CPS_COMPLETE, GCS_COMPATTR, GCS_COMPSTR,
            IME_COMPOSITION_STRING, ImmGetCompositionStringW, ImmGetContext,
            ImmGetConversionStatus, ImmNotifyIME, ImmReleaseContext, ImmSetCompositionFontW,
            NI_COMPOSITIONSTR,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{WM_IME_COMPOSITION, WM_NCDESTROY},
    },
};

use crate::AppWindow;
use crate::directwrite_render::{ClauseLook, ImeClause};

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
    set_composition_font(window.window(), vertical, 22.0);
}

fn set_composition_font(window: &slint::Window, vertical: bool, size: f32) {
    let Some(hwnd) = native_handle(window) else {
        return;
    };
    let mut font = LOGFONTW {
        lfHeight: -(size * window.scale_factor()).round().max(1.0) as i32,
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
    native_handle(window.window())
}

fn native_handle(window: &slint::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Slint hands back its own handle type; the trait method on it is what
    // reaches the platform one.
    let slint_handle = window.window_handle();
    let handle = slint_handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// Exclude the rendered cell, not the invisible 1px TextInput cursor. Keeping
/// this in the host adapter also works for a standalone Quick Draft window.
pub(crate) fn candidate_area(
    window: &slint::Window,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    vertical: bool,
) {
    use slint::winit_030::{
        WinitWindowAccessor,
        winit::dpi::{LogicalPosition, LogicalSize},
    };
    if ![x, y, width, height].iter().all(|v| v.is_finite()) {
        return;
    }
    let width = width.max(1.0);
    let height = height.max(1.0);
    set_composition_font(window, vertical, if vertical { width } else { height });
    window.with_winit_window(|native| {
        native.set_ime_cursor_area(LogicalPosition::new(x, y), LogicalSize::new(width, height));
    });
}

/// RFN01-77: what the IME says each clause of `text` is.
///
/// winit hands over the composition as a string and nothing else, so every
/// clause used to get the same line. The IME still holds the attributes while
/// its text is being shown: one byte per UTF-16 unit of the composition.
///
/// **Empty unless the IME's own composition is `text`** — the caller then
/// draws the one line it always did. That covers a window that is not Win32,
/// an IME that keeps no IMM context, and a text that has moved on since.
pub(crate) fn clauses(window: &slint::Window, text: &str) -> Vec<ImeClause> {
    if text.is_empty() {
        return Vec::new();
    }
    let Some(hwnd) = native_handle(window) else {
        return Vec::new();
    };
    // SAFETY: The handle belongs to a window that is alive for this call, and
    // the context is released on every path out.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_invalid() {
            return Vec::new();
        }
        let read = |kind: IME_COMPOSITION_STRING| -> Vec<u8> {
            let size = ImmGetCompositionStringW(context, kind, None, 0);
            if size <= 0 {
                return Vec::new();
            }
            let mut bytes = vec![0_u8; size as usize];
            let got = ImmGetCompositionStringW(
                context,
                kind,
                Some(bytes.as_mut_ptr().cast()),
                size as u32,
            );
            bytes.truncate(got.max(0) as usize);
            bytes
        };
        let composed = read(GCS_COMPSTR);
        let attributes = read(GCS_COMPATTR);
        let _ = ImmReleaseContext(hwnd, context);
        let units: Vec<u16> = composed
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if !text.encode_utf16().eq(units.iter().copied()) {
            return Vec::new();
        }
        clauses_from_attributes(&attributes)
    }
}

/// RFN01-79: tell the IME to commit the conversion it is holding.
///
/// **The text it sends back arrives late**, through winit's queue, after the
/// click that asked for it has moved the caret; the editor has already put the
/// text where it was typed, and drops this copy once ([`Settled`]).
///
/// **A commit, not a cancel**: it is what Notepad's click does, and the IME
/// learns from it. Asked to end a conversion either way, ATOK goes back to the
/// input method it is set to (measured 2026-11-02: a romaji switched on by key
/// fell back to the configured kana input, mode 25 → 9). That is ATOK's own
/// rule, and the writer's configured method is kept.
pub(crate) fn complete(window: &slint::Window) {
    let Some(hwnd) = native_handle(window) else {
        return;
    };
    // SAFETY: The handle belongs to a window that is alive for this call, and
    // the context is released on every path out.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_invalid() {
            return;
        }
        let _ = ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_COMPLETE, 0);
        let _ = ImmReleaseContext(hwnd, context);
    }
}

/// The IME's conversion mode as Windows holds it (`IME_CMODE_*`), for the log.
///
/// RFN01-79: whether committing from the editor leaves the writer's mode alone
/// — romaji stays romaji — is a question only a real IME answers.
pub(crate) fn conversion_mode(window: &slint::Window) -> Option<u32> {
    let hwnd = native_handle(window)?;
    // SAFETY: as in `clauses`.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_invalid() {
            return None;
        }
        let mut mode = Default::default();
        let read = ImmGetConversionStatus(context, Some(&mut mode), None).as_bool();
        let _ = ImmReleaseContext(hwnd, context);
        read.then_some(mode.0)
    }
}

/// RFN01-79: a conversion the editor committed itself, whose copy from the IME
/// is still on its way.
///
/// **Dropped once, and only if it is the same text.** Anything else the IME
/// commits is the writer's and goes in; a new conversion starting means the
/// copy is not coming, and `forget` lets it go.
#[derive(Default)]
pub(crate) struct Settled(std::cell::RefCell<Option<String>>);

impl Settled {
    pub(crate) fn expect(&self, text: &str) {
        *self.0.borrow_mut() = Some(text.to_owned());
    }

    /// Whether `text` is the copy being waited for. Either way, it is no
    /// longer waited for.
    pub(crate) fn is_copy(&self, text: &str) -> bool {
        self.0
            .borrow_mut()
            .take()
            .is_some_and(|expected| expected == text)
    }

    pub(crate) fn forget(&self) {
        self.0.borrow_mut().take();
    }
}

/// RFN01-77: tells the editor when the IME's clauses change under a text that
/// did not.
///
/// **Moving to the next clause changes no character.** winit passes the
/// composition on as a string, Slint reports a change only when the string
/// changes, and so the thick line stayed on the first clause however often the
/// writer pressed `→`. This watches the window's own `WM_IME_COMPOSITION`: each
/// one that carries attributes asks `changed` to look again.
///
/// **`changed` runs from the event loop, not from inside the message.** The
/// message arrives while winit is dispatching, and an IME can send it from
/// inside a call the editor makes with its state borrowed; looking then would
/// trip over that borrow.
pub(crate) struct ClauseWatch {
    hwnd: HWND,
    changed: Rc<dyn Fn()>,
}

/// "RFNI": one subclass per purpose, apart from the title bar's.
const WATCH_ID: usize = 0x52464e49;

impl ClauseWatch {
    /// Start watching `window`, or `None` if it is not a Win32 window yet.
    pub(crate) fn install(
        window: &slint::Window,
        changed: impl Fn() + 'static,
    ) -> Option<Box<Self>> {
        let hwnd = native_handle(window)?;
        let watch = Box::new(Self {
            hwnd,
            changed: Rc::new(changed),
        });
        // SAFETY: the box is not moved while installed — the subclass holds
        // its address, and `Drop` takes the subclass off before it goes.
        let installed = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(watch_proc),
                WATCH_ID,
                (&*watch as *const Self) as usize,
            )
        };
        installed.as_bool().then_some(watch)
    }
}

impl Drop for ClauseWatch {
    fn drop(&mut self) {
        // SAFETY: removing a subclass that may already be gone is harmless.
        unsafe {
            let _ = RemoveWindowSubclass(self.hwnd, Some(watch_proc), WATCH_ID);
        }
    }
}

unsafe extern "system" fn watch_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(watch_proc), WATCH_ID);
            return DefSubclassProc(hwnd, message, w, l);
        }
        // winit first: it is what turns the message into the composition the
        // editor shows, and the attributes are read against that.
        let result = DefSubclassProc(hwnd, message, w, l);
        if message == WM_IME_COMPOSITION && (l.0 as u32 & GCS_COMPATTR.0) != 0 {
            let watch = &*(data as *const ClauseWatch);
            let changed = watch.changed.clone();
            slint::Timer::single_shot(Duration::ZERO, move || changed());
        }
        result
    }
}

/// Runs of the same attribute, as clauses (RFN01-77).
///
/// Two neighbouring clauses with the same attribute — two converted ones on
/// either side of the target — come out as one run. They are drawn as one
/// line, which is what the IME's attributes can say: where one clause ends is
/// a separate question (`GCS_COMPCLAUSE`) this does not ask.
pub(crate) fn clauses_from_attributes(attributes: &[u8]) -> Vec<ImeClause> {
    let mut clauses: Vec<ImeClause> = Vec::new();
    for (at, attribute) in attributes.iter().enumerate() {
        let look = look_of(*attribute);
        match clauses.last_mut() {
            Some(last) if last.look == look => last.length += 1,
            _ => clauses.push(ImeClause {
                start: at as u32,
                length: 1,
                look,
            }),
        }
    }
    clauses
}

/// The line each IMM attribute is drawn with, the way Windows' own controls
/// draw them: unconverted dotted, converted thin, the clause being converted
/// thick, and input the IME rejected wavy.
fn look_of(attribute: u8) -> ClauseLook {
    match u32::from(attribute) {
        ATTR_TARGET_CONVERTED | ATTR_TARGET_NOTCONVERTED => ClauseLook::Target,
        ATTR_CONVERTED | ATTR_FIXEDCONVERTED => ClauseLook::Converted,
        ATTR_INPUT_ERROR => ClauseLook::Error,
        _ => ClauseLook::Input,
    }
}

/// The looks as a short line for the log: `I3 T2 C2`, or `none`.
pub(crate) fn describe(clauses: &[ImeClause]) -> String {
    if clauses.is_empty() {
        return "none".to_owned();
    }
    clauses
        .iter()
        .map(|clause| {
            let letter = match clause.look {
                ClauseLook::Input => 'I',
                ClauseLook::Converted => 'C',
                ClauseLook::Target => 'T',
                ClauseLook::Error => 'E',
            };
            format!("{letter}{}", clause.length)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_become_runs_of_one_look() {
        // 「今日は|いい|天気」の2つ目を変換している途中：変換済み・注目・変換済み・未変換。
        let attributes = [2, 2, 2, 1, 1, 2, 2, 0];
        let looks: Vec<_> = clauses_from_attributes(&attributes)
            .iter()
            .map(|clause| (clause.start, clause.length, clause.look))
            .collect();
        assert_eq!(
            looks,
            vec![
                (0, 3, ClauseLook::Converted),
                (3, 2, ClauseLook::Target),
                (5, 2, ClauseLook::Converted),
                (7, 1, ClauseLook::Input),
            ]
        );
    }

    #[test]
    fn every_imm_attribute_has_a_look() {
        assert_eq!(look_of(0), ClauseLook::Input);
        assert_eq!(look_of(1), ClauseLook::Target);
        assert_eq!(look_of(2), ClauseLook::Converted);
        assert_eq!(look_of(3), ClauseLook::Target);
        assert_eq!(look_of(4), ClauseLook::Error);
        assert_eq!(look_of(5), ClauseLook::Converted);
        assert!(clauses_from_attributes(&[]).is_empty());
    }

    #[test]
    fn a_settled_conversion_drops_its_copy_once() {
        let settled = Settled::default();
        settled.expect("今日は");
        assert!(settled.is_copy("今日は"), "IMEから届いた写しは捨てる");
        assert!(!settled.is_copy("今日は"), "2度目は書き手が打ったもの");
        settled.expect("今日は");
        assert!(!settled.is_copy("明日は"), "違う字は書き手のもの");
        assert!(!settled.is_copy("今日は"), "違う字が来たら待つのをやめる");
        settled.expect("今日は");
        settled.forget();
        assert!(!settled.is_copy("今日は"), "新しい変換が始まったら待たない");
    }

    #[test]
    fn the_log_line_names_looks_and_lengths() {
        assert_eq!(
            describe(&clauses_from_attributes(&[2, 2, 1, 0])),
            "C2 T1 I1"
        );
        assert_eq!(describe(&[]), "none");
    }
}
