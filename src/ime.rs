//! The editor's side of the IME (RFN01-83: through TSF).
//!
//! The window's TSF document lives in the vendored winit backend
//! (`i_slint_backend_winit::tsf`), shared by every text field. What the editor
//! adds is its own text: the body is drawn by the editor, and a hidden
//! `TextInput` only receives its keys, so [`ImeWatch`] offers the IME the text
//! around the caret instead, says where each character of a composition is
//! drawn, and selects what a reconversion picks.
//!
//! 一太郎 and Word put the candidate list beside the column being written, and
//! **the list itself runs vertically** (技術検証 7.2). Under TSF that is one
//! attribute the IME asks the document for, `TSATTRID_Text_VerticalWriting`
//! ([`set_vertical`]); MS-IME and ATOK both answer by turning their list.

use std::rc::Rc;

use i_slint_backend_winit::tsf;
use slint::ComponentHandle;
use windows::Win32::{
    Foundation::HWND,
    UI::Input::Ime::{
        ATTR_CONVERTED, ATTR_FIXEDCONVERTED, ATTR_INPUT_ERROR, ATTR_TARGET_CONVERTED,
        ATTR_TARGET_NOTCONVERTED,
    },
};

use crate::AppWindow;
use crate::directwrite_render::{ClauseLook, ImeClause};

fn with_winit(window: &slint::Window, f: impl FnOnce(&slint::winit_030::winit::window::Window)) {
    use slint::winit_030::WinitWindowAccessor;
    window.with_winit_window(|native| f(native));
}

/// Tell the IME whether the text being composed runs vertically.
pub fn set_vertical(window: &AppWindow, vertical: bool) {
    with_winit(window.window(), |native| {
        tsf::set_vertical(native, vertical)
    });
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

/// Where the caret's cell is, in the window's logical pixels: what the IME
/// places its windows beside. Exclude the rendered cell, not the invisible 1px
/// TextInput cursor. Keeping this in the host adapter also works for a
/// standalone Quick Draft window.
///
/// The caret moving is also when the text around it may have: the IME is told
/// to read it again.
pub(crate) fn candidate_area(
    window: &slint::Window,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    vertical: bool,
) {
    if ![x, y, width, height].iter().all(|v| v.is_finite()) {
        return;
    }
    let scale = window.scale_factor();
    let width = width.max(1.0);
    let height = height.max(1.0);
    with_winit(window, |native| {
        tsf::set_vertical(native, vertical);
        tsf::set_text_area(
            native,
            [x * scale, y * scale, width * scale, height * scale],
        );
        tsf::document_changed(native);
    });
}

/// RFN01-83: the text the IME is shown may be another one now — a page's,
/// or a field's: it reads it again.
pub(crate) fn text_changed(window: &slint::Window) {
    with_winit(window, tsf::document_changed);
}

/// RFN01-77: what the IME says each clause of `text` is.
///
/// **Empty unless the IME's own composition is `text`** — the caller then
/// draws the one line it always did.
pub(crate) fn clauses(window: &slint::Window, text: &str) -> Vec<ImeClause> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    with_winit(window, |native| {
        if let Some((composition, attributes)) = tsf::composition_attributes(native)
            && composition == text
            && attributes.len() == text.encode_utf16().count()
        {
            found = clauses_from_attributes(&attributes);
        }
    });
    found
}

/// RFN01-83: where the caret is in the composition `text`, in UTF-16 units —
/// `None` unless the IME's own composition is `text`.
pub(crate) fn composition_caret(window: &slint::Window, text: &str) -> Option<u32> {
    let mut found = None;
    with_winit(window, |native| {
        if let Some((composition, caret)) = tsf::composition_caret(native)
            && composition == text
        {
            found = Some(caret);
        }
    });
    found
}

/// RFN01-79: tell the IME to commit the conversion it is holding.
///
/// **The text it sends back arrives late**, from the event loop, after the
/// click that asked for it has moved the caret; the editor has already put the
/// text where it was typed, and drops this copy once ([`Settled`]).
pub(crate) fn complete(window: &slint::Window) {
    with_winit(window, tsf::complete);
}

/// The IME's conversion mode (`TF_CONVERSIONMODE_*`, the same bits as
/// `IME_CMODE_*`), for the log.
pub(crate) fn conversion_mode(_window: &slint::Window) -> Option<u32> {
    tsf::conversion_mode()
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

/// The editor's side of what the IME asks the window (RFN01-77, 78, 81, 83).
pub(crate) struct ImeHost {
    /// RFN01-83: whether the keys go to a page's own text now — its hidden
    /// field has the focus and the pane is not a shell. Otherwise the IME
    /// works on whichever text field has the focus, as Slint reports it.
    pub active: Box<dyn Fn() -> bool>,
    /// The clauses may have changed under a text that did not (RFN01-77).
    pub changed: Rc<dyn Fn()>,
    /// The text around the caret, for a reconversion (`false`) or for the IME
    /// to read before it converts (`true`, a document feed). `None` declines:
    /// a shell, or state being changed.
    pub around: Box<dyn Fn(bool) -> Option<Around>>,
    /// Select this stretch of the text `around` last offered, in UTF-16 units:
    /// it is what the IME is about to reconvert.
    pub choose: Box<dyn Fn(u32, u32) -> bool>,
    /// A line for the diagnostic log: which requests came, and the answers.
    pub log: Box<dyn Fn(&str)>,
    /// RFN01-81: where the composition's `n`th character is drawn, in the
    /// window's client area, physical pixels. `None` declines.
    pub char_rect: Box<dyn Fn(u32) -> Option<CharRect>>,
}

/// One character of a composition on screen (RFN01-81), client pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CharRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub vertical: bool,
}

thread_local! {
    /// RFN01-81: where the page of the pane being typed in starts, in the
    /// window's logical pixels — what the caret's own report to the IME was
    /// measured from (`candidate_area`), so a character can be put where the
    /// caret would be put.
    static PAGE_ORIGIN: std::cell::Cell<Option<(f32, f32)>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn remember_page_origin(x: f32, y: f32) {
    PAGE_ORIGIN.with(|origin| origin.set(Some((x, y))));
}

pub(crate) fn page_origin() -> Option<(f32, f32)> {
    PAGE_ORIGIN.with(std::cell::Cell::get)
}

/// The text the IME is shown (RFN01-78), positions in UTF-16 units of `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Around {
    pub text: String,
    pub caret: u32,
    pub selection: Option<(u32, u32)>,
    /// What is being converted, which is not in `text`.
    pub composition: String,
}

/// The editor's text, registered with the window's TSF document.
///
/// RFN01-78: **reconversion is the IME choosing a stretch of this text** and
/// converting it again; [`ImeHost::choose`] selects it, and the conversion
/// then replaces the selection as any conversion over a selection does. RFN01-81:
/// the IME places its prediction list from where each character is drawn.
pub(crate) struct ImeWatch;

struct HostDocument(ImeHost);

impl tsf::Document for HostDocument {
    fn active(&self) -> bool {
        (self.0.active)()
    }

    fn around(&self) -> Option<tsf::Around> {
        let around = (self.0.around)(true)?;
        let (caret, anchor) = match around.selection {
            Some((start, end)) if around.caret == start => (start, Some(end)),
            Some((start, end)) => (end, Some(start)),
            None => (around.caret, None),
        };
        Some(tsf::Around {
            text: around.text,
            caret,
            anchor,
        })
    }

    fn choose(&self, start: u32, end: u32) -> bool {
        (self.0.choose)(start, end)
    }

    fn char_rect(&self, nth: u32) -> Option<[f32; 4]> {
        let rect = (self.0.char_rect)(nth)?;
        Some([rect.x, rect.y, rect.width, rect.height])
    }

    fn changed(&self) {
        (self.0.changed)();
    }

    fn log(&self, line: &str) {
        (self.0.log)(line);
    }
}

impl ImeWatch {
    /// Offer `window`'s text to the IME. `None` if it is not a Win32 window yet.
    pub(crate) fn install(window: &slint::Window, host: ImeHost) -> Option<Box<Self>> {
        native_handle(window)?;
        let document: Rc<dyn tsf::Document> = Rc::new(HostDocument(host));
        with_winit(window, |native| tsf::set_document(native, document));
        Some(Box::new(Self))
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
