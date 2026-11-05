// RFN Edit (RFN01-83): the Text Services Framework for every Slint `TextInput`.
//
// winit talks to the IME through IMM32 only, and today's IMEs (ATOK, the new
// Microsoft IME) are TSF text services reached through Windows' compatibility
// layer. This module is the window's own TSF document instead: one
// `ITextStoreACP` per window, focused while a `TextInput` has the keyboard,
// whose edits become Slint's composition events.
//
// What the IME sees is one of two things:
//
// * **a field** — the focused `TextInput`'s own text and caret, as Slint
//   reports them (`InputMethodProperties`). Edits outside the composition
//   (a reconversion) go back as `replacement_range`.
// * **a document** — an application that draws its own text registers a
//   [`Document`] for the window and says when it is the one being typed in
//   (RFN Edit's body text, which a hidden `TextInput` only receives keys for).
//   The IME then reads the text around the caret from it, is told whether the
//   text runs vertically and where each character of the composition is drawn,
//   and a reconversion selects the stretch the IME chose ([`Document::choose`]).
//
// Locks: an IME asks for one, synchronously or not, at any time — including
// from inside a call the application or Slint is making. Those are granted
// later, from the event loop; a request from the IME's own keystroke handling
// (a hook in the message pump) is granted at once, and the composition events
// it produces are dispatched at once too, so a commit reaches Slint before the
// Enter key that follows it.

#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

use i_slint_core as corelib;
use i_slint_core::input::{InternalKeyEvent, KeyEvent, KeyEventType};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, FORMATETC, IDataObject};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_I4,
};
use windows::Win32::UI::TextServices::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::winitwindowadapter::WinitWindowAdapter;

/// The text around the caret that a [`Document`] offers the IME. Positions are
/// UTF-16 units of `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Around {
    /// The text the IME may read: the caret's paragraph or more.
    pub text: String,
    /// Where the caret is in `text`.
    pub caret: u32,
    /// The other end of a selection, or `None` for a bare caret.
    pub anchor: Option<u32>,
}

/// An application's own text, offered to the IME in place of the focused
/// `TextInput`'s (see the module comment). Every method is called from the UI
/// thread, never while one of them is already running.
pub trait Document {
    /// Whether the keyboard is in this document now, rather than in a field.
    fn active(&self) -> bool;
    /// The text around the caret, or `None` to offer nothing.
    fn around(&self) -> Option<Around>;
    /// Select `start..end` of the text `around` last returned: the IME is about
    /// to convert it again. `false` declines.
    fn choose(&self, start: u32, end: u32) -> bool;
    /// Where the composition's `nth` character is drawn: `[x, y, width,
    /// height]` in the window's client area, physical pixels.
    fn char_rect(&self, nth: u32) -> Option<[f32; 4]>;
    /// The clauses of the composition changed (see [`composition_attributes`]).
    fn changed(&self);
    /// A line for the application's diagnostics.
    fn log(&self, line: &str);
}

#[path = "tsf_plan.rs"]
mod plan;
use plan::{Event, Notice, Plan, Source, Text, unit_at};

// ---------------------------------------------------------------------------
// The COM side.

struct Thread {
    manager: ITfThreadMgr,
    client: u32,
    categories: Option<ITfCategoryMgr>,
    attributes: Option<ITfDisplayAttributeMgr>,
}

thread_local! {
    static THREAD: RefCell<Option<Rc<Thread>>> = const { RefCell::new(None) };
    static WINDOWS: RefCell<HashMap<isize, Rc<Window>>> = RefCell::new(HashMap::new());
    static DOCUMENTS: RefCell<HashMap<isize, Rc<dyn Document>>> = RefCell::new(HashMap::new());
    /// Calls in from Slint or the application now running (see the module
    /// comment): locks asked for meanwhile are granted later.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct Inside;
impl Inside {
    fn new() -> Self {
        DEPTH.with(|d| d.set(d.get() + 1));
        Inside
    }
}
impl Drop for Inside {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}
fn inside() -> bool {
    DEPTH.with(|d| d.get() > 0)
}

fn thread() -> Option<Rc<Thread>> {
    THREAD.with(|held| {
        if let Some(thread) = held.borrow().as_ref() {
            return Some(thread.clone());
        }
        // SAFETY: plain COM creation on the UI thread, which winit initialised.
        let made = unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER).ok()?;
            let client = manager.Activate().ok()?;
            let categories =
                CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER).ok();
            let attributes =
                CoCreateInstance(&CLSID_TF_DisplayAttributeMgr, None, CLSCTX_INPROC_SERVER).ok();
            Rc::new(Thread { manager, client, categories, attributes })
        };
        *held.borrow_mut() = Some(made.clone());
        Some(made)
    })
}

/// One window's TSF document.
struct Window {
    hwnd: HWND,
    adapter: RefCell<Weak<WinitWindowAdapter>>,
    docmgr: ITfDocumentMgr,
    context: ITfContext,
    state: Rc<State>,
}

#[derive(Default)]
struct State {
    hwnd: Cell<isize>,
    text: RefCell<Text>,
    sink: RefCell<Option<ITextStoreACPSink>>,
    lock: Cell<u32>,
    /// Locks granted later, one grant per request, in the order asked.
    pending: RefCell<std::collections::VecDeque<u32>>,
    edited: Cell<bool>,
    focused: Cell<bool>,
    vertical: Cell<bool>,
    /// The caret's cell, client pixels: the document's (`set_text_area`), or
    /// the field's own cursor.
    area: Cell<Option<[f32; 4]>>,
    wanted: RefCell<Vec<GUID>>,
    view: RefCell<Option<ITfCompositionView>>,
    /// One `TF_DA_ATTR_INFO` per UTF-16 unit of the composition.
    attributes: RefCell<Vec<u8>>,
    /// Composition events for Slint not yet dispatched.
    queued: RefCell<Vec<Event>>,
    /// Drop the composition's events (its `TextInput` lost the keyboard).
    discard: Cell<bool>,
    /// What the IME was last told about the writing direction (for the log).
    answered: Cell<Option<bool>>,
    /// Where the caret was in the composition when last handed on.
    reported_caret: Cell<Option<u32>>,
    /// Our own read session is running: the lock it takes hands nothing on.
    reading: Cell<bool>,
    /// The application asked for the composition to end: a lock it causes is
    /// granted at once, so the end is done before the call returns.
    allow_sync: Cell<bool>,
    /// The focused field as Slint last reported it: text, caret and anchor
    /// (bytes), for when the document stops being the one typed in.
    field: RefCell<Option<(String, usize, Option<usize>)>>,
    adapter: RefCell<Weak<WinitWindowAdapter>>,
}

impl State {
    fn hwnd(&self) -> HWND {
        HWND(self.hwnd.get() as *mut _)
    }
    fn document(&self) -> Option<Rc<dyn Document>> {
        DOCUMENTS.with(|d| d.borrow().get(&self.hwnd.get()).cloned())
    }
    fn log(&self, line: &str) {
        if let Some(document) = self.document() {
            document.log(line);
        }
    }
    fn screen(&self, x: f32, y: f32) -> POINT {
        let mut point = POINT { x: x.round() as i32, y: y.round() as i32 };
        // SAFETY: a point this function owns, and a window that may be gone.
        unsafe {
            let _ = ClientToScreen(self.hwnd(), &mut point);
        }
        point
    }
}

fn window_of(hwnd: isize) -> Option<Rc<Window>> {
    WINDOWS.with(|w| w.borrow().get(&hwnd).cloned())
}

fn window_for(hwnd: HWND) -> Option<Rc<Window>> {
    let key = hwnd.0 as isize;
    if let Some(window) = window_of(key) {
        return Some(window);
    }
    let thread = thread()?;
    let state = Rc::new(State::default());
    state.hwnd.set(key);
    // SAFETY: creating this window's document on the UI thread.
    let window = unsafe {
        let docmgr = thread.manager.CreateDocumentMgr().ok()?;
        let store: ITextStoreACP = Store { state: state.clone() }.into();
        let unknown: IUnknown = store.cast().ok()?;
        let mut context = None;
        let mut cookie = 0;
        docmgr.CreateContext(thread.client, 0, &unknown, &mut context, &mut cookie).ok()?;
        let context = context?;
        docmgr.Push(&context).ok()?;
        Rc::new(Window { hwnd, adapter: RefCell::default(), docmgr, context, state })
    };
    WINDOWS.with(|w| w.borrow_mut().insert(key, window.clone()));
    Some(window)
}

// ---------------------------------------------------------------------------
// Called by the window adapter.

fn hwnd_of(winit_window: &winit::window::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match winit_window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// A `TextInput` took the keyboard, or reports where it stands now.
pub(crate) fn enable(
    adapter: &Rc<WinitWindowAdapter>,
    winit_window: &winit::window::Window,
    props: &corelib::window::InputMethodProperties,
    scale: f32,
    first: bool,
) {
    let _inside = Inside::new();
    let Some(hwnd) = hwnd_of(winit_window) else { return };
    let Some(window) = window_for(hwnd) else { return };
    *window.adapter.borrow_mut() = Rc::downgrade(adapter);
    *window.state.adapter.borrow_mut() = Rc::downgrade(adapter);
    if first || !window.state.focused.get() {
        window.state.focused.set(true);
        window.state.discard.set(false);
        if let Some(thread) = thread() {
            // SAFETY: TSF calls on the UI thread, with objects this module owns.
            unsafe {
                let _ = thread.manager.AssociateFocus(window.hwnd, &window.docmgr);
                let _ = thread.manager.SetFocus(&window.docmgr);
            }
        }
    }
    // The field's own cursor is where its composition is drawn.
    let origin = props.cursor_rect_origin;
    let size = props.cursor_rect_size;
    let document = window.state.document().filter(|d| d.active());
    if document.is_none() {
        window.state.area.set(Some([
            origin.x * scale,
            origin.y * scale,
            size.width.max(1.0) * scale,
            size.height.max(1.0) * scale,
        ]));
    }
    refresh_with(&window.state, Some(props));
}

/// No `TextInput` has the keyboard: the IME is off, as Slint has it.
pub(crate) fn disable(winit_window: &winit::window::Window) {
    let _inside = Inside::new();
    let Some(hwnd) = hwnd_of(winit_window) else { return };
    let Some(window) = window_of(hwnd.0 as isize) else { return };
    if !window.state.focused.replace(false) {
        return;
    }
    // Slint drops a composition its `TextInput` loses; so does the IME here.
    if window.state.text.borrow().comp.is_some() {
        window.state.discard.set(true);
        terminate(&window);
    }
    if let Some(thread) = thread() {
        // SAFETY: as in `enable`.
        unsafe {
            let _ = thread.manager.AssociateFocus(window.hwnd, None);
        }
    }
}

/// Called before the native window is released. Invalidate the old state
/// before calling into COM, which can synchronously call the text store back.
pub(crate) fn detach(winit_window: &winit::window::Window) {
    let Some(hwnd) = hwnd_of(winit_window) else { return };
    let key = hwnd.0 as isize;
    let window = WINDOWS.with(|windows| windows.borrow_mut().remove(&key));
    let document = DOCUMENTS.with(|documents| documents.borrow_mut().remove(&key));
    if let Some(window) = window {
        let state = &window.state;
        state.hwnd.set(0);
        state.focused.set(false);
        state.discard.set(true);
        state.pending.borrow_mut().clear();
        state.queued.borrow_mut().clear();
        state.adapter.replace(Weak::new());
        let _inside = Inside::new();
        if let Some(thread) = thread() {
            // SAFETY: these are the TSF objects and HWND being detached.
            unsafe {
                let _ = thread.manager.AssociateFocus(hwnd, None);
                let _ = window.docmgr.Pop(TF_POPF_ALL);
            }
        }
        let sink = state.sink.borrow_mut().take();
        let view = state.view.borrow_mut().take();
        drop((sink, view));
    }
    drop(document);
}

fn terminate(window: &Window) {
    if let Ok(services) = window.context.cast::<ITfContextOwnerCompositionServices>() {
        window.state.allow_sync.set(true);
        // SAFETY: ends whatever composition the context has.
        unsafe {
            let _ = services.TerminateComposition(None);
        }
        window.state.allow_sync.set(false);
    }
    // The composition's text has gone where it was made; read where the
    // caret is now.
    if window.state.text.borrow().comp.is_none() {
        let state = window.state.clone();
        corelib::timers::Timer::single_shot(Duration::ZERO, move || refresh_with(&state, None));
    }
}

/// Read the text again — from the document, or from `props` for a field — and
/// tell the IME if it changed.
fn refresh_with(state: &Rc<State>, props: Option<&corelib::window::InputMethodProperties>) {
    let document = state.document().filter(|d| d.active());
    let source = if document.is_some() { Source::Document } else { Source::Field };
    if state.text.borrow().source != Some(source) && state.text.borrow().comp.is_none() {
        state.log(&format!("tsf source={source:?} vertical={}", state.vertical.get()));
    }
    if let Some(props) = props {
        *state.field.borrow_mut() =
            Some((props.text.to_string(), props.cursor_position, props.anchor_position));
    }
    let field = state.field.borrow().clone();
    let notice = match (&document, field) {
        (Some(document), _) => {
            // Busy: what the IME has is kept, and asked again next time.
            let Some(around) = document.around() else { return };
            let base: Vec<u16> = around.text.encode_utf16().collect();
            let length = base.len() as u32;
            let caret = around.caret.min(length);
            let anchor = around.anchor.unwrap_or(caret).min(length);
            state.text.borrow_mut().take(Source::Document, base, String::new(), caret, anchor)
        }
        (None, Some((text, cursor, anchor))) => {
            let caret = unit_at(&text, cursor);
            let anchor = anchor.map_or(caret, |a| unit_at(&text, a));
            let base = text.encode_utf16().collect();
            state.text.borrow_mut().take(Source::Field, base, text, caret, anchor)
        }
        (None, None) => Notice::Nothing,
    };
    if notice != Notice::Nothing {
        notify(state, notice);
    }
}

/// Tell the IME what changed: a stretch of the text, or only the selection.
fn notify(state: &Rc<State>, notice: Notice) {
    if state.lock.get() != 0 {
        // Not while it holds a lock: once it lets go.
        let state = state.clone();
        corelib::timers::Timer::single_shot(Duration::ZERO, move || notify(&state, notice));
        return;
    }
    let Some(sink) = state.sink.borrow().clone() else { return };
    // SAFETY: the sink TSF gave this store.
    unsafe {
        if let Notice::Text(start, old_end, new_end) = notice {
            let change = TS_TEXTCHANGE {
                acpStart: start as i32,
                acpOldEnd: old_end as i32,
                acpNewEnd: new_end as i32,
            };
            let _ = sink.OnTextChange(TEXT_STORE_TEXT_CHANGE_FLAGS(0), &change);
        }
        let _ = sink.OnSelectionChange();
        if matches!(notice, Notice::Text(..)) {
            let _ = sink.OnLayoutChange(TS_LC_CHANGE, 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Called by the application (re-exported from the crate root).

fn with_window(winit_window: &winit::window::Window, f: impl FnOnce(&Rc<Window>)) {
    let _inside = Inside::new();
    if let Some(window) = hwnd_of(winit_window).and_then(|h| window_of(h.0 as isize)) {
        f(&window);
    }
}

/// Whether this native window currently has an application text host.
pub fn has_document(winit_window: &winit::window::Window) -> bool {
    hwnd_of(winit_window).is_some_and(|hwnd| {
        DOCUMENTS.with(|documents| documents.borrow().contains_key(&(hwnd.0 as isize)))
    })
}

/// Register the application's own text for this window (see [`Document`]).
pub fn set_document(winit_window: &winit::window::Window, document: Rc<dyn Document>) {
    if let Some(hwnd) = hwnd_of(winit_window) {
        DOCUMENTS.with(|d| d.borrow_mut().insert(hwnd.0 as isize, document));
    }
    // A field may have the keys already, as the window's first: read again.
    document_changed(winit_window);
}

/// Whether the document's text runs vertically: the IME lays its candidate
/// list out to match (`TSATTRID_Text_VerticalWriting`).
pub fn set_vertical(winit_window: &winit::window::Window, vertical: bool) {
    with_window(winit_window, |window| {
        if window.state.vertical.replace(vertical) != vertical {
            window.state.log(&format!("tsf vertical={vertical}"));
            if let Some(sink) = window.state.sink.borrow().clone() {
                if window.state.lock.get() == 0 {
                    let length = window.state.text.borrow().text.len() as i32;
                    // SAFETY: the sink TSF gave this store.
                    unsafe {
                        let _ = sink.OnAttrsChange(0, length, &[TSATTRID_Text_VerticalWriting]);
                        let _ = sink.OnLayoutChange(TS_LC_CHANGE, 1);
                    }
                }
            }
        }
    });
}

/// The document's caret cell, client pixels: what the IME places its windows
/// beside when it asks about anything but a character of the composition.
pub fn set_text_area(winit_window: &winit::window::Window, rect: [f32; 4]) {
    with_window(winit_window, |window| {
        let before = window.state.area.replace(Some(rect));
        if before != Some(rect) && window.state.lock.get() == 0 {
            if let Some(sink) = window.state.sink.borrow().clone() {
                // SAFETY: the sink TSF gave this store.
                unsafe {
                    let _ = sink.OnLayoutChange(TS_LC_CHANGE, 1);
                }
            }
        }
    });
}

/// The document's text or caret changed: read it again.
pub fn document_changed(winit_window: &winit::window::Window) {
    with_window(winit_window, |window| {
        if window.state.focused.get() {
            refresh_with(&window.state, None);
        }
    });
}

/// Commit the composition now (RFN01-79). Its text still comes to the
/// `TextInput` as a commit, from the event loop.
pub fn complete(winit_window: &winit::window::Window) {
    with_window(winit_window, |window| {
        if window.state.text.borrow().comp.is_some() {
            terminate(window);
        }
    });
}

/// The composition and one `TF_DA_ATTR_INFO` value per UTF-16 unit of it —
/// the same numbers as IMM32's `ATTR_*` — or `None` while there is none.
pub fn composition_attributes(winit_window: &winit::window::Window) -> Option<(String, Vec<u8>)> {
    let mut found = None;
    with_window(winit_window, |window| {
        let text = window.state.text.borrow();
        let Some((a, b)) = text.comp else { return };
        let b = (b as usize).min(text.text.len());
        let preedit = String::from_utf16_lossy(&text.text[a as usize..b]);
        found = Some((preedit, window.state.attributes.borrow().clone()));
    });
    found
}

/// The composition and where the caret is in it, in UTF-16 units — what the
/// writer moves with ← and → while reading is still being typed.
pub fn composition_caret(winit_window: &winit::window::Window) -> Option<(String, u32)> {
    let mut found = None;
    with_window(winit_window, |window| {
        let text = window.state.text.borrow();
        let Some((a, b)) = text.comp else { return };
        let b = (b as usize).min(text.text.len());
        let preedit = String::from_utf16_lossy(&text.text[a as usize..b]);
        let caret = text.sel.1.clamp(a, b as u32) - a;
        found = Some((preedit, caret));
    });
    found
}

/// The IME's conversion mode (`TF_CONVERSIONMODE_*`), for diagnostics.
pub fn conversion_mode() -> Option<u32> {
    let thread = thread()?;
    // SAFETY: reading a compartment of this thread's manager.
    unsafe {
        let compartments: ITfCompartmentMgr = thread.manager.cast().ok()?;
        let compartment =
            compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION).ok()?;
        let value = compartment.GetValue().ok()?;
        if value.Anonymous.Anonymous.vt == VT_I4 {
            Some(value.Anonymous.Anonymous.Anonymous.lVal as u32)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// After a lock: hand the IME's edits on.

fn after_lock(state: &Rc<State>) {
    if state.hwnd.get() == 0 || state.reading.get() {
        return;
    }
    settle_edits(state);
    flush(state);
}

/// Work out what the edits so far mean and queue it. Also called when a
/// composition ends inside a lock, before the IME starts the next one in the
/// same lock: the two are told apart, commit first.
fn settle_edits(state: &Rc<State>) {
    if !state.edited.replace(false) {
        return;
    }
    let plan = state.text.borrow_mut().plan();
    if plan == Plan::default() {
        return;
    }
    if let Some((start, end)) = plan.choose {
        if let Some(document) = state.document() {
            let chosen = document.choose(start, end);
            document.log(&format!("tsf choose {start}..{end} chosen={chosen}"));
        }
    }
    if let Some(event) = plan.event {
        if state.discard.get() {
            if event.commit {
                state.discard.set(false);
            }
            return;
        }
        state.queued.borrow_mut().push(event);
    }
}

/// Hand the queued events to Slint: now, or from the event loop when called
/// from inside the application.
fn flush(state: &Rc<State>) {
    // The clauses first: the composition is drawn as soon as Slint has it,
    // and drawn with the IME's lines rather than a plain one for a frame.
    let composing = state.text.borrow().comp.is_some();
    let fresh = composing && !inside() && state.lock.get() == 0 && read_attributes(state);
    if !state.queued.borrow().is_empty() {
        if inside() || state.lock.get() != 0 {
            let state = state.clone();
            corelib::timers::Timer::single_shot(Duration::ZERO, move || dispatch(&state));
        } else {
            dispatch(state);
        }
    }
    // The caret moving inside the composition changes no character, and Slint
    // reports nothing: the document is told, as for the clauses.
    let caret = composing.then(|| {
        let text = state.text.borrow();
        text.comp.map_or(0, |(a, b)| text.sel.1.clamp(a, b) - a)
    });
    if state.reported_caret.replace(caret) != caret && caret.is_some() {
        let state = state.clone();
        corelib::timers::Timer::single_shot(Duration::ZERO, move || {
            if let Some(document) = state.document() {
                document.changed();
            }
        });
    }
    if !composing {
        state.attributes.borrow_mut().clear();
    } else if !fresh {
        let state = state.clone();
        corelib::timers::Timer::single_shot(Duration::ZERO, move || {
            read_attributes(&state);
        });
    }
}

fn dispatch(state: &Rc<State>) {
    let events = std::mem::take(&mut *state.queued.borrow_mut());
    let Some(adapter) = state.adapter.borrow().upgrade() else { return };
    for event in events {
        if state.hwnd.get() == 0 { return; }
        let mut key_event = KeyEvent::default();
        key_event.text = event.text.into();
        let internal = InternalKeyEvent {
            event_type: if event.commit {
                KeyEventType::CommitComposition
            } else {
                KeyEventType::UpdateComposition
            },
            key_event,
            replacement_range: event.replace.map(|(a, b)| a..b),
            preedit_text: event.preedit.into(),
            preedit_selection: event.preedit_selection.map(|(a, b)| a..b),
            cursor_position: event.cursor,
            anchor_position: event.cursor,
            ..Default::default()
        };
        adapter.dispatch_ime_event(internal);
    }
    // A document takes a commit into its own text: read it back.
    if state.document().is_some_and(|d| d.active()) && state.text.borrow().comp.is_none() {
        refresh_with(state, None);
    }
}

// ---------------------------------------------------------------------------
// The clauses: the display attribute the IME put on each part of the
// composition, read in an edit session of our own.

/// `true` when the attributes were read.
fn read_attributes(state: &Rc<State>) -> bool {
    let Some(window) = window_of(state.hwnd.get()) else { return false };
    let Some(thread) = thread() else { return false };
    if state.view.borrow().is_none() {
        return false;
    }
    if state.reading.replace(true) {
        return false;
    }
    let session: ITfEditSession = Reading { state: state.clone(), thread: thread.clone() }.into();
    // SAFETY: a synchronous read session on this window's context.
    let result = unsafe {
        window.context.RequestEditSession(thread.client, &session, TF_ES_READ | TF_ES_SYNC)
    };
    state.reading.set(false);
    match result {
        Ok(granted) if granted.is_ok() => true,
        _ => false,
    }
}

#[implement(ITfEditSession)]
struct Reading {
    state: Rc<State>,
    thread: Rc<Thread>,
}

impl ITfEditSession_Impl for Reading_Impl {
    fn DoEditSession(&self, ec: u32) -> windows_core::Result<()> {
        let state = &self.state;
        let Some(view) = state.view.borrow().clone() else { return Ok(()) };
        let Some((start, end)) = state.text.borrow().comp else { return Ok(()) };
        let window = window_of(state.hwnd.get()).ok_or(windows_core::Error::from(E_FAIL))?;
        let length = (end - start) as usize;
        let mut found = vec![TF_ATTR_INPUT.0 as u8; length];
        // SAFETY: reading properties inside the session TSF granted.
        unsafe {
            let range = view.GetRange()?;
            let property = window.context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            let mut ranges = None;
            property.EnumRanges(ec, &mut ranges, &range)?;
            let ranges = ranges.ok_or(windows_core::Error::from(E_FAIL))?;
            loop {
                let mut one = [None];
                let mut fetched = 0;
                if ranges.Next(&mut one, &mut fetched).is_err() || fetched == 0 {
                    break;
                }
                let Some(part) = one[0].take() else { break };
                let Ok(value) = property.GetValue(ec, &part) else { continue };
                if value.Anonymous.Anonymous.vt != VT_I4 {
                    continue;
                }
                let atom = value.Anonymous.Anonymous.Anonymous.lVal as u32;
                let Some(attr) = self.attribute(atom) else { continue };
                let Ok(acp) = part.cast::<ITfRangeACP>() else { continue };
                let (mut a, mut n) = (0, 0);
                if acp.GetExtent(&mut a, &mut n).is_err() {
                    continue;
                }
                let from = (a as i64 - start as i64).max(0) as usize;
                let to = ((a + n) as i64 - start as i64).max(0) as usize;
                for slot in found.iter_mut().take(to.min(length)).skip(from) {
                    *slot = attr;
                }
            }
        }
        let changed = *state.attributes.borrow() != found;
        *state.attributes.borrow_mut() = found;
        if changed {
            if let Some(document) = state.document() {
                let state = state.clone();
                corelib::timers::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(document) = state.document() {
                        document.changed();
                    }
                });
                let _ = document;
            }
        }
        Ok(())
    }
}

impl Reading_Impl {
    fn attribute(&self, atom: u32) -> Option<u8> {
        let categories = self.thread.categories.as_ref()?;
        let attributes = self.thread.attributes.as_ref()?;
        // SAFETY: TSF's own managers, asked about an atom it handed out.
        unsafe {
            let guid = categories.GetGUID(atom).ok()?;
            let mut info = None;
            let mut owner = GUID::zeroed();
            attributes.GetDisplayAttributeInfo(&guid, &mut info, &mut owner).ok()?;
            let mut attribute = TF_DISPLAYATTRIBUTE::default();
            info?.GetAttributeInfo(&mut attribute).ok()?;
            let value = attribute.bAttr.0;
            (0..=5).contains(&value).then_some(value as u8)
        }
    }
}

// ---------------------------------------------------------------------------
// The text store.

#[implement(ITextStoreACP, ITfContextOwnerCompositionSink)]
struct Store {
    state: Rc<State>,
}

impl Store {
    fn need(&self, write: bool) -> windows_core::Result<()> {
        let lock = self.state.lock.get();
        let ok = if write {
            lock & TS_LF_READWRITE.0 == TS_LF_READWRITE.0
        } else {
            lock & TS_LF_READ.0 != 0
        };
        if ok { Ok(()) } else { Err(TS_E_NOLOCK.into()) }
    }
}

fn grant(state: &Rc<State>, flags: u32) -> HRESULT {
    let Some(sink) = state.sink.borrow().clone() else { return E_FAIL };
    state.lock.set(flags & TS_LF_READWRITE.0);
    // SAFETY: the sink TSF gave this store; the IME edits inside this call.
    let result = unsafe { sink.OnLockGranted(TEXT_STORE_LOCK_FLAGS(flags & TS_LF_READWRITE.0)) };
    state.lock.set(0);
    after_lock(state);
    match result {
        Ok(()) => HRESULT(0),
        Err(error) => error.code(),
    }
}

fn grant_later(state: &Rc<State>) {
    let state = Rc::downgrade(state);
    corelib::timers::Timer::single_shot(Duration::ZERO, move || {
        let Some(state) = state.upgrade().filter(|state| state.hwnd.get() != 0) else { return };
        if inside() || state.lock.get() != 0 {
            grant_later(&state);
            return;
        }
        // **One grant per request**, as TSF expects: two requests answered
        // with one grant would leave one of the IME's edit sessions unrun.
        let next = state.pending.borrow_mut().pop_front();
        if let Some(flags) = next {
            let _ = grant(&state, flags);
            if !state.pending.borrow().is_empty() {
                grant_later(&state);
            }
        }
    });
}

fn attribute_value(vertical: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: vertical.into() },
            }),
        },
    }
}

impl ITextStoreACP_Impl for Store_Impl {
    fn AdviseSink(&self, riid: *const GUID, punk: Ref<IUnknown>, _mask: u32) -> windows_core::Result<()> {
        // SAFETY: TSF passes a valid interface id.
        if unsafe { *riid } != ITextStoreACPSink::IID {
            return Err(E_INVALIDARG.into());
        }
        let sink: ITextStoreACPSink = punk.ok()?.cast()?;
        *self.state.sink.borrow_mut() = Some(sink);
        Ok(())
    }
    fn UnadviseSink(&self, _punk: Ref<IUnknown>) -> windows_core::Result<()> {
        *self.state.sink.borrow_mut() = None;
        Ok(())
    }
    fn RequestLock(&self, flags: u32) -> windows_core::Result<HRESULT> {
        if self.state.hwnd.get() == 0 { return Err(E_FAIL.into()); }
        let state = &self.state;
        let sync = flags & TS_LF_SYNC != 0;
        // A read-only lock changes nothing here or in the application, so it
        // is granted at once even from inside a call (Copilot Keyboard asks for
        // synchronous ones after a layout change).
        let read_only = flags & TS_LF_READWRITE.0 == TS_LF_READ.0;
        if state.lock.get() != 0 || (inside() && !state.allow_sync.get() && !read_only) {
            if sync {
                return Ok(TS_E_SYNCHRONOUS);
            }
            state.pending.borrow_mut().push_back(flags);
            grant_later(state);
            return Ok(TS_S_ASYNC);
        }
        Ok(grant(state, flags))
    }
    fn GetStatus(&self) -> windows_core::Result<TS_STATUS> {
        Ok(TS_STATUS { dwDynamicFlags: 0, dwStaticFlags: TS_SS_NOHIDDENTEXT })
    }
    fn QueryInsert(&self, a: i32, b: i32, _cch: u32, ps: *mut i32, pe: *mut i32) -> windows_core::Result<()> {
        // SAFETY: TSF's out parameters.
        unsafe {
            *ps = a;
            *pe = b;
        }
        Ok(())
    }
    fn GetSelection(&self, _index: u32, count: u32, psel: *mut TS_SELECTION_ACP, pfetched: *mut u32) -> windows_core::Result<()> {
        self.need(false)?;
        let (a, b) = self.state.text.borrow().sel;
        // SAFETY: TSF's out parameters, room for `count`.
        unsafe {
            if count == 0 {
                *pfetched = 0;
                return Ok(());
            }
            *psel = TS_SELECTION_ACP {
                acpStart: a as i32,
                acpEnd: b as i32,
                style: TS_SELECTIONSTYLE { ase: TS_AE_END, fInterimChar: false.into() },
            };
            *pfetched = 1;
        }
        Ok(())
    }
    fn SetSelection(&self, count: u32, psel: *const TS_SELECTION_ACP) -> windows_core::Result<()> {
        self.need(true)?;
        if count > 0 {
            // SAFETY: TSF passes `count` selections.
            let sel = unsafe { *psel };
            let mut text = self.state.text.borrow_mut();
            let length = text.text.len() as i32;
            let a = sel.acpStart.clamp(0, length) as u32;
            let b = sel.acpEnd.clamp(0, length) as u32;
            text.sel = (a.min(b), a.max(b));
            self.state.edited.set(true);
        }
        Ok(())
    }
    fn GetText(
        &self,
        start: i32,
        end: i32,
        pch: PWSTR,
        cchreq: u32,
        pcchret: *mut u32,
        prun: *mut TS_RUNINFO,
        crunreq: u32,
        pcrunret: *mut u32,
        pnext: *mut i32,
    ) -> windows_core::Result<()> {
        self.need(false)?;
        let text = self.state.text.borrow();
        let length = text.text.len() as i32;
        let end = if end < 0 { length } else { end.min(length) };
        if start < 0 || start > end {
            return Err(TS_E_INVALIDPOS.into());
        }
        let n = ((end - start) as u32).min(cchreq) as usize;
        // SAFETY: TSF's buffers, of the sizes it gave.
        unsafe {
            if n > 0 && !pch.is_null() {
                std::ptr::copy_nonoverlapping(text.text.as_ptr().add(start as usize), pch.0, n);
            }
            *pcchret = n as u32;
            if crunreq > 0 && !prun.is_null() {
                *prun = TS_RUNINFO { uCount: n as u32, r#type: TS_RT_PLAIN };
                *pcrunret = 1;
            } else if !pcrunret.is_null() {
                *pcrunret = 0;
            }
            *pnext = start + n as i32;
        }
        Ok(())
    }
    fn SetText(&self, _flags: u32, start: i32, end: i32, pch: &PCWSTR, cch: u32) -> windows_core::Result<TS_TEXTCHANGE> {
        self.need(true)?;
        // SAFETY: TSF passes `cch` units.
        let inserted = unsafe { std::slice::from_raw_parts(pch.0, cch as usize) }.to_vec();
        let mut text = self.state.text.borrow_mut();
        let length = text.text.len() as i32;
        if start < 0 || end > length || start > end {
            return Err(TS_E_INVALIDPOS.into());
        }
        text.text.splice(start as usize..end as usize, inserted.iter().copied());
        self.state.edited.set(true);
        Ok(TS_TEXTCHANGE { acpStart: start, acpOldEnd: end, acpNewEnd: start + cch as i32 })
    }
    fn GetFormattedText(&self, _a: i32, _b: i32) -> windows_core::Result<IDataObject> {
        Err(E_NOTIMPL.into())
    }
    fn GetEmbedded(&self, _p: i32, _g: *const GUID, _r: *const GUID) -> windows_core::Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }
    fn QueryInsertEmbedded(&self, _g: *const GUID, _f: *const FORMATETC) -> windows_core::Result<BOOL> {
        Ok(false.into())
    }
    fn InsertEmbedded(&self, _f: u32, _a: i32, _b: i32, _d: Ref<IDataObject>) -> windows_core::Result<TS_TEXTCHANGE> {
        Err(E_NOTIMPL.into())
    }
    fn InsertTextAtSelection(
        &self,
        flags: u32,
        pch: &PCWSTR,
        cch: u32,
        ps: *mut i32,
        pe: *mut i32,
        pchange: *mut TS_TEXTCHANGE,
    ) -> windows_core::Result<()> {
        self.need(true)?;
        let (a, b) = self.state.text.borrow().sel;
        if flags & TS_IAS_QUERYONLY != 0 {
            // SAFETY: TSF's out parameters.
            unsafe {
                *ps = a as i32;
                *pe = b as i32;
            }
            return Ok(());
        }
        // SAFETY: TSF passes `cch` units.
        let inserted = unsafe { std::slice::from_raw_parts(pch.0, cch as usize) }.to_vec();
        {
            let mut text = self.state.text.borrow_mut();
            text.text.splice(a as usize..b as usize, inserted.iter().copied());
            let end = a + cch;
            text.sel = (end, end);
        }
        self.state.edited.set(true);
        // SAFETY: TSF's out parameters, any of which may be null.
        unsafe {
            if !ps.is_null() {
                *ps = a as i32;
            }
            if !pe.is_null() {
                *pe = (a + cch) as i32;
            }
            if !pchange.is_null() {
                *pchange = TS_TEXTCHANGE { acpStart: a as i32, acpOldEnd: b as i32, acpNewEnd: (a + cch) as i32 };
            }
        }
        Ok(())
    }
    fn InsertEmbeddedAtSelection(&self, _f: u32, _d: Ref<IDataObject>, _a: *mut i32, _b: *mut i32, _c: *mut TS_TEXTCHANGE) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn RequestSupportedAttrs(&self, _flags: u32, n: u32, attrs: *const GUID) -> windows_core::Result<()> {
        self.want(n, attrs);
        Ok(())
    }
    fn RequestAttrsAtPosition(&self, _pos: i32, n: u32, attrs: *const GUID, _flags: u32) -> windows_core::Result<()> {
        self.want(n, attrs);
        Ok(())
    }
    fn RequestAttrsTransitioningAtPosition(&self, _pos: i32, _n: u32, _attrs: *const GUID, _flags: u32) -> windows_core::Result<()> {
        self.state.wanted.borrow_mut().clear();
        Ok(())
    }
    fn FindNextAttrTransition(
        &self,
        _start: i32,
        halt: i32,
        _n: u32,
        _attrs: *const GUID,
        _flags: u32,
        pnext: *mut i32,
        pfound: *mut BOOL,
        poffset: *mut i32,
    ) -> windows_core::Result<()> {
        // SAFETY: TSF's out parameters.
        unsafe {
            *pnext = halt;
            *pfound = false.into();
            if !poffset.is_null() {
                *poffset = 0;
            }
        }
        Ok(())
    }
    fn RetrieveRequestedAttrs(&self, count: u32, vals: *mut TS_ATTRVAL, pfetched: *mut u32) -> windows_core::Result<()> {
        let wanted = std::mem::take(&mut *self.state.wanted.borrow_mut());
        let vertical = self.state.vertical.get() && self.state.document().is_some_and(|d| d.active());
        if !wanted.is_empty() && self.state.answered.replace(Some(vertical)) != Some(vertical) {
            self.state.log(&format!("tsf answered vertical={vertical}"));
        }
        let mut n = 0;
        for id in wanted.iter().take(count as usize) {
            // SAFETY: TSF's array, room for `count`.
            unsafe {
                vals.add(n).write(TS_ATTRVAL { idAttr: *id, dwOverlapId: 0, varValue: attribute_value(vertical) });
            }
            n += 1;
        }
        // SAFETY: TSF's out parameter.
        unsafe { *pfetched = n as u32 };
        Ok(())
    }
    fn GetEndACP(&self) -> windows_core::Result<i32> {
        self.need(false)?;
        Ok(self.state.text.borrow().text.len() as i32)
    }
    fn GetActiveView(&self) -> windows_core::Result<u32> {
        Ok(1)
    }
    fn GetACPFromPoint(&self, _v: u32, _p: *const POINT, _f: u32) -> windows_core::Result<i32> {
        Err(E_NOTIMPL.into())
    }
    fn GetTextExt(&self, _v: u32, a: i32, b: i32, prc: *mut RECT, pclipped: *mut BOOL) -> windows_core::Result<()> {
        self.need(false)?;
        let state = &self.state;
        let comp = state.text.borrow().comp;
        let document = state.document().filter(|d| d.active());
        let mut rect: Option<[f32; 4]> = None;
        if let (Some(document), Some((start, end))) = (document, comp) {
            let a = (a.max(0) as u32).clamp(start, end);
            let b = (b.max(0) as u32).clamp(start, end);
            let last = if b > a { b - 1 } else { a };
            for nth in (a - start)..=(last - start) {
                if let Some(cell) = document.char_rect(nth) {
                    rect = Some(match rect {
                        None => cell,
                        Some(r) => {
                            let left = r[0].min(cell[0]);
                            let top = r[1].min(cell[1]);
                            let right = (r[0] + r[2]).max(cell[0] + cell[2]);
                            let bottom = (r[1] + r[3]).max(cell[1] + cell[3]);
                            [left, top, right - left, bottom - top]
                        }
                    });
                }
            }
        }
        let [x, y, w, h] = rect.or(state.area.get()).ok_or(windows_core::Error::from(TS_E_NOLAYOUT))?;
        let p0 = state.screen(x, y);
        let p1 = state.screen(x + w, y + h);
        // SAFETY: TSF's out parameters.
        unsafe {
            *prc = RECT { left: p0.x, top: p0.y, right: p1.x, bottom: p1.y };
            *pclipped = false.into();
        }
        Ok(())
    }
    fn GetScreenExt(&self, _v: u32) -> windows_core::Result<RECT> {
        let mut client = RECT::default();
        // SAFETY: a rectangle this function owns.
        unsafe {
            let _ = GetClientRect(self.state.hwnd(), &mut client);
        }
        let p0 = self.state.screen(0.0, 0.0);
        Ok(RECT { left: p0.x, top: p0.y, right: p0.x + client.right, bottom: p0.y + client.bottom })
    }
    fn GetWnd(&self, _v: u32) -> windows_core::Result<HWND> {
        Ok(self.state.hwnd())
    }
}

impl Store_Impl {
    fn want(&self, n: u32, attrs: *const GUID) {
        let ids = if attrs.is_null() {
            &[][..]
        } else {
            // SAFETY: TSF passes `n` ids.
            unsafe { std::slice::from_raw_parts(attrs, n as usize) }
        };
        let mut wanted = self.state.wanted.borrow_mut();
        wanted.clear();
        if ids.contains(&TSATTRID_Text_VerticalWriting) {
            wanted.push(TSATTRID_Text_VerticalWriting);
        }
    }
}

fn extent(view: &ITfCompositionView) -> Option<(u32, u32)> {
    // SAFETY: TSF's own objects.
    unsafe {
        let range = view.GetRange().ok()?;
        let acp: ITfRangeACP = range.cast().ok()?;
        let (mut a, mut n) = (0, 0);
        acp.GetExtent(&mut a, &mut n).ok()?;
        Some((a.max(0) as u32, (a + n).max(0) as u32))
    }
}

impl ITfContextOwnerCompositionSink_Impl for Store_Impl {
    fn OnStartComposition(&self, view: Ref<ITfCompositionView>) -> windows_core::Result<BOOL> {
        let view = view.ok()?.clone();
        self.state.text.borrow_mut().comp = extent(&view);
        *self.state.view.borrow_mut() = Some(view);
        self.state.edited.set(true);
        Ok(true.into())
    }
    fn OnUpdateComposition(&self, view: Ref<ITfCompositionView>, _range: Ref<ITfRange>) -> windows_core::Result<()> {
        let view = view.ok()?.clone();
        self.state.text.borrow_mut().comp = extent(&view);
        *self.state.view.borrow_mut() = Some(view);
        self.state.edited.set(true);
        Ok(())
    }
    fn OnEndComposition(&self, _view: Ref<ITfCompositionView>) -> windows_core::Result<()> {
        self.state.text.borrow_mut().comp = None;
        *self.state.view.borrow_mut() = None;
        self.state.edited.set(true);
        // The commit is worked out now, against the text the composition was
        // made in, before the IME starts another one in this same lock.
        settle_edits(&self.state);
        if self.state.lock.get() == 0 {
            flush(&self.state);
        }
        Ok(())
    }
}

