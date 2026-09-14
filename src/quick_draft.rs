//! 要件 12: the quick draft window.
//!
//! **A second window, and almost none of the editor in it.** What 要件 12 asks
//! for is a place to write a short message bound for somewhere else, where
//! `Enter` cannot send it — so there is no Markdown, no preview, no vertical
//! writing and no file. The text is Slint's own `TextInput` (`ui/quick-draft`),
//! and what is here is the part that outlives the window: where it was, whether
//! it stays on top, and the draft itself.
//!
//! **Opening it is a function, not a click** (要件 12.2). The menu calls
//! [`open`]; a global shortcut, when there is one, will call the same thing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, PhysicalPosition, PhysicalSize, SharedString,
    Timer, TimerMode, VecModel, WindowPosition, WindowSize,
};

use crate::app_data::{self, Draft, WindowPlace};
use crate::{AppWindow, QuickDraft};

/// How long the draft may sit unwritten after a keystroke (要件 12.4).
///
/// **The same two seconds the work copies wait**, and for the same reason: a
/// draft the writer is in the middle of typing is not worth a disk write per
/// character, and two seconds is short enough that nothing anyone would mind
/// losing fits in it.
const SAVE_SETTLE: Duration = Duration::from_secs(2);

/// How long "Copied" stays on screen (要件 12.3, "非侵襲的な表示").
const NOTICE_SETTLE: Duration = Duration::from_secs(2);

/// IME can swallow Shift release. Repair Slint's state before its built-in
/// TextInput processes selection, not merely in our shortcut callback.
fn install_shift_repair(window: &QuickDraft) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
    window.window().on_winit_window_event(|window, event| {
        if matches!(
            event,
            WindowEvent::MouseInput { .. } | WindowEvent::KeyboardInput { .. }
        ) {
            let (by_message, by_hand) = crate::shift_really_held();
            repair_shift_state(window, by_message, by_hand);
        }
        EventResult::Propagate
    });
}

pub(crate) fn repair_shift_state(window: &slint::Window, by_message: bool, by_hand: bool) {
    if by_message || by_hand {
        return;
    }
    // Release both sides, then let the original event run. No text editing or
    // selection operation is synthesized.
    for key in [slint::platform::Key::Shift, slint::platform::Key::ShiftR] {
        window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: key.into() });
    }
}

/// The quick draft window, while it is open.
///
/// **Held by the editor rather than by itself**, so that asking for it twice
/// brings the one that is open to the front instead of opening another. 要件
/// 12.4 keeps one draft; two windows over it would be two editors of one file.
#[derive(Default)]
pub struct QuickDraftWindow {
    open: Option<Live>,
}

struct Live {
    window: QuickDraft,
    /// Restarted on every keystroke: what is written is the draft as it stands
    /// when the writing stops.
    save: Rc<Timer>,
    notice: Rc<Timer>,
    /// The drafts behind this one, newest first (要件 12.4).
    history: Rc<RefCell<Vec<String>>>,
    /// The tab the draft was last sent to, as the editor names it. **Opaque**:
    /// this side keeps it and hands it back, and never reads it.
    target: Rc<RefCell<String>>,
}

impl QuickDraftWindow {
    /// Show the draft window, restoring what the last run left in it.
    ///
    /// **Whoever calls this is not the point** (要件 12.2): the menu does now
    /// and a global shortcut may later, and neither knows anything the other
    /// does not.
    pub fn open(held: &Rc<RefCell<Self>>, owner: &AppWindow, editor: Editor) {
        if let Some(live) = &held.borrow().open {
            // Already open: bring it forward rather than making a second one.
            let _ = live.window.show();
            live.window.invoke_take_focus();
            return;
        }
        let Ok(window) = QuickDraft::new() else {
            owner.set_render_status("クイック下書き: 窓を作れません".into());
            return;
        };
        wire_shortcuts(owner, &window);
        install_shift_repair(&window);
        let draft = directory()
            .and_then(|directory| app_data::read_draft(&directory))
            .unwrap_or_default();
        let history = directory()
            .map(|directory| app_data::read_history(&directory))
            .unwrap_or_default();
        show_history(&window, &history);
        window.set_text(SharedString::from(draft.text.as_str()));
        window.set_on_top(draft.on_top);
        if let Some(place) = draft.place {
            window
                .window()
                .set_position(WindowPosition::Physical(PhysicalPosition::new(
                    place.x, place.y,
                )));
            window
                .window()
                .set_size(WindowSize::Physical(PhysicalSize::new(
                    place.width,
                    place.height,
                )));
        }

        let live = Live {
            window: window.clone_strong(),
            save: Rc::new(Timer::default()),
            notice: Rc::new(Timer::default()),
            history: Rc::new(RefCell::new(history)),
            target: Rc::new(RefCell::new(draft.target.clone())),
        };
        held.borrow_mut().open = Some(live);
        wire(held, &window, editor);

        if window.show().is_err() {
            owner.set_render_status("クイック下書き: 窓を出せません".into());
            held.borrow_mut().open = None;
            return;
        }
        window.invoke_take_focus();
        // **The caret goes back where it was** (要件 12.4). After the window is
        // shown, because focus is what puts a caret anywhere at all.
        if let Some(caret) = draft.caret {
            let at = floor_char_boundary(&draft.text, caret) as i32;
            window.invoke_set_caret(at);
        }
    }

    /// Write the draft out now, whatever the timer was waiting for.
    ///
    /// **Called when the editor is closing**, which is the one moment a
    /// two-second wait is too long: the process is about to end.
    pub fn store_now(&self) {
        let Some(live) = &self.open else {
            return;
        };
        let target = live.target.borrow().clone();
        store(&live.window, &target);
    }
}

pub(crate) fn wire_shortcuts(owner: &AppWindow, window: &QuickDraft) {
    let owner_weak = owner.as_weak();
    let draft_weak = window.as_weak();
    window.on_shortcut_key(move |text, control, alt, shift| {
        let Some(owner) = owner_weak.upgrade() else {
            return false;
        };
        let command = crate::shortcuts::resolve(
            &owner.get_shortcut_bindings(),
            1,
            &text,
            control,
            alt,
            shift,
        );
        if command == -1 {
            return false;
        }
        if command == -2 {
            return true;
        }
        let weak = draft_weak.clone();
        Timer::single_shot(std::time::Duration::ZERO, move || {
            let Some(draft) = weak.upgrade() else {
                return;
            };
            match command {
                38 => draft.invoke_copy_all(),
                39 => {
                    draft.invoke_copy_all();
                    draft.invoke_copy_and_close();
                }
                40 => draft.invoke_clear_requested(),
                41 => draft.invoke_send_requested(),
                _ => {}
            }
        });
        true
    });
}

/// What the draft window is given of the editor, and all it is given.
///
/// **Two functions, and no more of the editor than that** — the tabs it may
/// send to, and the way to send. `searcher.rs` is handed a way to wake the
/// window and knows nothing else about it; this is the same arrangement seen
/// from the other side.
pub struct Editor {
    /// The tabs as they are now, told which one the draft is aimed at. **Asked
    /// each time the list is wanted**: a tab may have been opened or closed
    /// since the draft window was.
    pub tabs: Box<dyn Fn(&str) -> TabList>,
    /// Put this text into the tab at that place in the list, and say what to
    /// remember that tab as.
    pub paste: Box<dyn Fn(usize, &str) -> String>,
}

/// The editor's open tabs, as the draft window needs them.
pub struct TabList {
    /// A name each, in the order they are shown.
    pub rows: Vec<String>,
    /// Where in the list the remembered target is, or **-1 when it is not open
    /// any more** — which is what makes a click ask rather than send.
    pub target: i32,
    /// What the send button says.
    pub target_name: String,
}

/// Everything the window asks of the editor.
fn wire(held: &Rc<RefCell<QuickDraftWindow>>, window: &QuickDraft, editor: Editor) {
    let (Some(save), Some(notice), Some(history), Some(target)) = ({
        let borrowed = held.borrow();
        let live = borrowed.open.as_ref();
        (
            live.map(|live| live.save.clone()),
            live.map(|live| live.notice.clone()),
            live.map(|live| live.history.clone()),
            live.map(|live| live.target.clone()),
        )
    }) else {
        return;
    };
    // Shared because more than one of the callbacks below asks the editor
    // something, and each of them outlives this function.
    let editor = Rc::new(editor);
    show_tabs(window, &editor, &target);

    let weak = window.as_weak();
    let aimed = target.clone();
    window.on_edited(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let inner = window.as_weak();
        let aimed = aimed.clone();
        save.start(TimerMode::SingleShot, SAVE_SETTLE, move || {
            if let Some(window) = inner.upgrade() {
                let target = aimed.borrow().clone();
                store(&window, &target);
            }
        });
    });

    let weak = window.as_weak();
    window.on_copied(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.set_notice("Copied".into());
        let inner = window.as_weak();
        notice.start(TimerMode::SingleShot, NOTICE_SETTLE, move || {
            if let Some(window) = inner.upgrade() {
                window.set_notice(SharedString::new());
            }
        });
    });

    let weak = window.as_weak();
    let held_here = held.clone();
    let kept = history.clone();
    let aimed = target.clone();
    window.on_copy_and_close(move || {
        if let Some(window) = weak.upgrade() {
            let target = aimed.borrow().clone();
            retire(&window, &kept, &target);
            let _ = window.hide();
        }
        held_here.borrow_mut().open = None;
    });

    let weak = window.as_weak();
    let kept = history.clone();
    let aimed = target.clone();
    window.on_clear_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let target = aimed.borrow().clone();
        retire(&window, &kept, &target);
        window.set_notice("Cleared".into());
    });

    // 要件 12.4: the list, as it is at the moment it is wanted.
    let weak = window.as_weak();
    let asking = editor.clone();
    let aimed = target.clone();
    window.on_tabs_requested(move || {
        if let Some(window) = weak.upgrade() {
            show_tabs(&window, &asking, &aimed);
        }
    });

    // A click on the send button: the tab it names, if that tab is still open.
    let weak = window.as_weak();
    let asking = editor.clone();
    let held_here = held.clone();
    let kept = history.clone();
    let aimed = target.clone();
    window.on_send_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let at = show_tabs(&window, &asking, &aimed);
        if at < 0 {
            // **The tab it was aimed at has been closed.** Rather than pick
            // another one for the writer, show them the list.
            window.invoke_show_tabs();
            return;
        }
        send(&window, &asking, &held_here, &kept, &aimed, at as usize);
    });

    let weak = window.as_weak();
    let asking = editor.clone();
    let held_here = held.clone();
    let kept = history.clone();
    let aimed = target.clone();
    window.on_paste_to_tab_requested(move |at| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        send(
            &window,
            &asking,
            &held_here,
            &kept,
            &aimed,
            at.max(0) as usize,
        );
    });

    let weak = window.as_weak();
    let kept = history.clone();
    let aimed = target.clone();
    window.on_history_picked(move |at| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(entry) = kept.borrow().get(at.max(0) as usize).cloned() else {
            return;
        };
        // **What is in the window now is kept first.** Bringing one draft back
        // must not be a way to lose another, and the list is where a draft goes
        // when it leaves the window.
        let target = aimed.borrow().clone();
        retire(&window, &kept, &target);
        window.set_text(SharedString::from(entry.as_str()));
        window.invoke_take_focus();
        store(&window, &target);
    });

    // **Closing clears the window and keeps what was in it** (要件 12.4, as the
    // writer asked on 2026-08-27). A draft is written to be sent; one still
    // sitting there tomorrow is a message that was not sent, and the history is
    // where it can be found if it should have been.
    let weak = window.as_weak();
    let held_here = held.clone();
    let kept = history.clone();
    let aimed = target.clone();
    window.window().on_close_requested(move || {
        if let Some(window) = weak.upgrade() {
            let target = aimed.borrow().clone();
            retire(&window, &kept, &target);
        }
        held_here.borrow_mut().open = None;
        CloseRequestResponse::HideWindow
    });
}

/// Send the draft to the tab at `at`, and close the window behind it
/// (要件 12.4: the writer's "Paste Close").
///
/// **The tab it went to becomes the one the button names.** A draft usually
/// goes where the last one went, and the button is that answer written down.
fn send(
    window: &QuickDraft,
    editor: &Rc<Editor>,
    held: &Rc<RefCell<QuickDraftWindow>>,
    history: &Rc<RefCell<Vec<String>>>,
    target: &Rc<RefCell<String>>,
    at: usize,
) {
    let text = window.get_text().to_string();
    if text.is_empty() {
        return;
    }
    let named = (editor.paste)(at, &text);
    *target.borrow_mut() = named.clone();
    retire(window, history, &named);
    let _ = window.hide();
    held.borrow_mut().open = None;
}

/// Put the editor's tabs in front of the window, and say where in them the
/// draft is aimed.
fn show_tabs(window: &QuickDraft, editor: &Rc<Editor>, target: &Rc<RefCell<String>>) -> i32 {
    let aimed = target.borrow().clone();
    let list = (editor.tabs)(&aimed);
    let rows = list
        .rows
        .into_iter()
        .map(SharedString::from)
        .collect::<Vec<_>>();
    window.set_tabs(ModelRc::new(VecModel::from(rows)));
    window.set_tab_target(list.target);
    window.set_target_name(SharedString::from(list.target_name));
    list.target
}

/// Put what the window holds into the history, and leave the window empty.
///
/// **The one way a draft leaves the window**, whether it was cleared, sent,
/// copied away or closed on. Nothing here can lose text: it is written to the
/// history before it is taken off the screen.
fn retire(window: &QuickDraft, history: &Rc<RefCell<Vec<String>>>, target: &str) {
    let text = window.get_text().to_string();
    {
        let mut kept = history.borrow_mut();
        app_data::remember_draft(&mut kept, &text);
        if let Some(directory) = directory() {
            let _ = app_data::write_history(&directory, &kept);
        }
        show_history(window, &kept);
    }
    window.set_text(SharedString::new());
    store(window, target);
}

/// The line each kept draft is shown by.
fn show_history(window: &QuickDraft, entries: &[String]) {
    let rows = entries
        .iter()
        .map(|entry| SharedString::from(label_of(entry)))
        .collect::<Vec<_>>();
    window.set_history(ModelRc::new(VecModel::from(rows)));
}

/// A draft's first line, short enough for a row.
///
/// **The first line and how much more there is.** A draft is usually a message,
/// and a message says what it is in its first line; what the row has to add is
/// whether the rest of it is still there.
fn label_of(entry: &str) -> String {
    let mut lines = entry.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or_default().trim();
    let shown: String = first.chars().take(LABEL_CHARS).collect();
    let cut = shown.chars().count() < first.chars().count();
    let more = lines.next().is_some();
    match (cut, more) {
        (false, false) => shown,
        _ => format!("{shown}…"),
    }
}

/// How much of a kept draft's first line a row shows.
const LABEL_CHARS: usize = 34;

/// Write what the window holds, place and all./// Write what the window holds, place and all.
fn store(window: &QuickDraft, target: &str) {
    let Some(directory) = directory() else {
        return;
    };
    let draft = Draft {
        text: window.get_text().to_string(),
        caret: Some(window.get_caret().max(0) as usize),
        on_top: window.get_on_top(),
        target: target.to_owned(),
        place: place_of(window),
    };
    // Best effort, like every other thing written beside the document: a draft
    // that could not be put away must not stop the writing of it.
    let _ = app_data::write_draft(&directory, &draft);
}

fn place_of(window: &QuickDraft) -> Option<WindowPlace> {
    let handle = window.window();
    let position = handle.position();
    let size = handle.size();
    (size.width > 0 && size.height > 0).then_some(WindowPlace {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
}

fn directory() -> Option<PathBuf> {
    app_data::app_directory()
}

/// The nearest character boundary at or before `byte`.
///
/// A caret written by an earlier run is a byte offset into a text that may have
/// been edited since — by a later build, or by hand. **Rounded rather than
/// refused**, which is what the work copies do with the same problem (6.7).
fn floor_char_boundary(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::label_of;

    /// A row says what the draft was, which is its first line — and whether
    /// there is more of it than the row is showing.
    #[test]
    fn a_kept_draft_is_shown_by_its_first_line() {
        assert_eq!(label_of("送りたい文章"), "送りたい文章");
        // More lines under it, so the row says so.
        assert_eq!(label_of("一行目\n二行目"), "一行目…");
        // A leading blank line is not the first line.
        assert_eq!(label_of("\n  本題です\n"), "本題です");
        // And a line longer than the row is cut where the row ends.
        let long = "あ".repeat(60);
        let shown = label_of(&long);
        assert_eq!(shown.chars().count(), 35, "{shown}");
        assert!(shown.ends_with('…'));
    }
}
