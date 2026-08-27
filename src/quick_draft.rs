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
    CloseRequestResponse, ComponentHandle, PhysicalPosition, PhysicalSize, SharedString, Timer,
    TimerMode, WindowPosition, WindowSize,
};

use crate::app_data::{self, Draft, DraftPlace};
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
}

impl QuickDraftWindow {
    /// Show the draft window, restoring what the last run left in it.
    ///
    /// **Whoever calls this is not the point** (要件 12.2): the menu does now
    /// and a global shortcut may later, and neither knows anything the other
    /// does not.
    pub fn open(held: &Rc<RefCell<Self>>, owner: &AppWindow) {
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
        let draft = directory()
            .and_then(|directory| app_data::read_draft(&directory))
            .unwrap_or_default();
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
        };
        held.borrow_mut().open = Some(live);
        wire(held, &window, owner);

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
        store(&live.window);
    }
}

/// Everything the window asks of the editor.
fn wire(held: &Rc<RefCell<QuickDraftWindow>>, window: &QuickDraft, owner: &AppWindow) {
    let saving = held.borrow().open.as_ref().map(|live| live.save.clone());
    let noticing = held.borrow().open.as_ref().map(|live| live.notice.clone());

    if let Some(save) = saving {
        let weak = window.as_weak();
        window.on_edited(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let inner = window.as_weak();
            save.start(TimerMode::SingleShot, SAVE_SETTLE, move || {
                if let Some(window) = inner.upgrade() {
                    store(&window);
                }
            });
        });
    }

    if let Some(notice) = noticing {
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
    }

    let weak = window.as_weak();
    let held_here = held.clone();
    window.on_copy_and_close(move || {
        if let Some(window) = weak.upgrade() {
            store(&window);
            let _ = window.hide();
        }
        held_here.borrow_mut().open = None;
    });

    let weak = window.as_weak();
    window.on_clear_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // 要件 12.4: the draft goes only when the writer says so, and then it
        // goes from the disk as well — a cleared draft that came back on the
        // next run would not have been cleared.
        window.set_text(SharedString::new());
        window.set_notice("Cleared".into());
        store(&window);
    });

    let weak = window.as_weak();
    let owner_weak = owner.as_weak();
    window.on_promote_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        promote(&window, owner_weak.upgrade().as_ref());
    });

    // **Closing the window keeps the draft** (要件 12.4). The window is let go
    // of, so the next request builds a fresh one — the draft is on disk and the
    // window is not what holds it.
    let weak = window.as_weak();
    let held_here = held.clone();
    window.window().on_close_requested(move || {
        if let Some(window) = weak.upgrade() {
            store(&window);
        }
        held_here.borrow_mut().open = None;
        CloseRequestResponse::HideWindow
    });
}

/// 要件 12.4: the draft becomes an ordinary Markdown file.
///
/// **The draft stays**, because promoting is a copy going out rather than the
/// draft leaving: the writer asked for a file, not for an empty window.
fn promote(window: &QuickDraft, owner: Option<&AppWindow>) {
    let text = window.get_text().to_string();
    if text.is_empty() {
        return;
    }
    let handle = owner.map(crate::ime::window_handle).unwrap_or_default();
    let Some(path) = crate::file_dialog::save_document_as(handle, "下書き.md") else {
        return;
    };
    match crate::file_io::save(&path, &text, crate::file_io::TextForm::default()) {
        Ok(_) => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            window.set_notice(SharedString::from(format!("Saved as {name}")));
        }
        Err(error) => {
            window.set_notice(SharedString::from(format!("保存できません: {error}")));
        }
    }
}

/// Write what the window holds, place and all.
fn store(window: &QuickDraft) {
    let Some(directory) = directory() else {
        return;
    };
    let draft = Draft {
        text: window.get_text().to_string(),
        caret: Some(window.get_caret().max(0) as usize),
        on_top: window.get_on_top(),
        place: place_of(window),
    };
    // Best effort, like every other thing written beside the document: a draft
    // that could not be put away must not stop the writing of it.
    let _ = app_data::write_draft(&directory, &draft);
}

fn place_of(window: &QuickDraft) -> Option<DraftPlace> {
    let handle = window.window();
    let position = handle.position();
    let size = handle.size();
    (size.width > 0 && size.height > 0).then_some(DraftPlace {
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
