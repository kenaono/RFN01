//! What an open document holds, and how a change to it is taken back.
//!
//! `buffer.rs` answers "which file is this document"; this answers "what is in
//! it right now". The two are apart for the reason 要件 7.6 gives: several
//! panes show one text, and only one thing decides what saving it means.
//!
//! Three things live here, and they are one thing seen from three sides.
//!
//! - [`SharedText`] is the characters plus **the two flags every writer has to
//!   move**: whether the text agrees with its file (要件 8.2) and when the
//!   changes now waiting for a work copy began (要件 8.1). The flags live with
//!   the text because **every writer goes through `borrow_mut`** — one door,
//!   so neither flag can be forgotten at a door nobody thought of.
//! - [`History`] is what can be taken back (要件 7.1), **one per document
//!   rather than per pane** (要件 7.6).
//! - [`OpenDocument`] holds those two, the file, and the per-line counts, and
//!   is what a tab actually points at. **The `Rc` is the registry**: a document
//!   lives while a tab holds it and goes when the last one lets go.
//!
//! Slint is here only as a `Weak<AppWindow>` that [`SharedText`] rings when the
//! unsaved marker moves; nothing here draws or lays anything out.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::Weak;

use crate::AppWindow;
use crate::buffer::DocumentFile;
use crate::document::DocumentCounts;

/// The document's text, and whether it has changed since it last agreed with
/// its file.
///
/// The flag lives with the text because **every writer goes through
/// `borrow_mut`**. Threading it through the four editing functions instead
/// would hold until the fifth one was written; this cannot be forgotten, which
/// is the same reason a block decides its size from its own measurements
/// rather than from a running total (技術検証 3.4).
pub struct SharedText {
    pub text: RefCell<String>,
    pub edited: Cell<bool>,
    /// When the text last changed, and when the run of changes now waiting for
    /// a work copy began. 要件 8.1 states two rules and they are measured from
    /// these two moments; see [`work_copy_due`].
    pub changed_at: Cell<Instant>,
    pub pending_since: Cell<Option<Instant>>,
    /// Told as soon as the flag moves, so the title marker does not depend on
    /// remembering to refresh it at each of the places an edit can begin.
    pub window: Weak<AppWindow>,
}

impl SharedText {
    pub fn new(text: String, window: Weak<AppWindow>) -> Self {
        Self {
            text: RefCell::new(text),
            edited: Cell::new(false),
            changed_at: Cell::new(Instant::now()),
            pending_since: Cell::new(None),
            window,
        }
    }

    pub fn borrow(&self) -> Ref<'_, String> {
        self.text.borrow()
    }

    /// Borrowing the text to write *is* the edit.
    pub fn borrow_mut(&self) -> RefMut<'_, String> {
        let now = Instant::now();
        self.changed_at.set(now);
        if self.pending_since.get().is_none() {
            self.pending_since.set(Some(now));
        }
        self.set_edited(true);
        self.forget_status();
        self.text.borrow_mut()
    }

    /// 帯に出ている知らせを畳む（書き手の報告 2026-09-10：「一度出ると
    /// 出っぱなし」）。
    ///
    /// **打ち始めたということは、その知らせは読み終わったということである。**
    /// ここに置いてあるのは`set_edited`と同じ理由で、**編集はぜんぶこの door を
    /// 通る**——どこか1つの編集経路で消し忘れる、ということが起きない。
    /// **本文を入れ替える道（`replace_document`）もここを通る**ので、入れ替えた
    /// あとに出す言葉（「CP932で開き直しました」）は消えない：先に畳んで、
    /// それから言う、の順になる。
    fn forget_status(&self) {
        if let Some(window) = self.window.upgrade()
            && !window.get_render_status().is_empty()
        {
            window.set_render_status(Default::default());
        }
    }

    /// When the changes now waiting for a work copy began, if any are.
    pub fn pending_since(&self) -> Option<Instant> {
        self.pending_since.get()
    }

    pub fn changed_at(&self) -> Instant {
        self.changed_at.get()
    }

    /// A work copy has caught up with the text.
    pub fn work_copy_written(&self) {
        self.pending_since.set(None);
    }

    /// The copy that holds this text is filed under a name the document no
    /// longer has: its file was renamed (要件 5.2), so one has to be written
    /// again under the new one. A document that agrees with its file has
    /// nothing waiting either way.
    pub fn mark_pending(&self) {
        if self.edited.get() && self.pending_since.get().is_none() {
            self.pending_since.set(Some(Instant::now()));
        }
    }

    pub fn edited(&self) -> bool {
        self.edited.get()
    }

    /// Restored from a work copy: the text does not agree with its file, but
    /// the copy on disk already holds it, so nothing is waiting to be written.
    pub fn mark_restored(&self) {
        self.pending_since.set(None);
        self.set_edited(true);
    }

    /// The text now agrees with its file: it was just opened, or just saved.
    pub fn mark_saved(&self) {
        self.pending_since.set(None);
        self.set_edited(false);
    }

    /// Only the moves are reported. A keystroke in an already-edited document
    /// changes nothing the window shows.
    ///
    /// **The strips are published again as well.** A tab's unsaved marker is
    /// this flag, and this is the one time it changes without the writer having
    /// touched the strip — which is why the window carries a callback back into
    /// the tab code for it.
    pub fn set_edited(&self, edited: bool) {
        if self.edited.replace(edited) == edited {
            return;
        }
        if let Some(window) = self.window.upgrade() {
            window.set_document_edited(edited);
            window.invoke_republish_tabs();
        }
    }
}

/// One open document: its text, and the file it came from.
///
/// **A document is shared; a tab is one pane's view of it** (要件 7.6). Two
/// panes showing the same file hold the same `Rc`, so an edit made through one
/// *is* the text the other draws, and `Rc::ptr_eq` answers "the same document?"
/// — the question that decides whether an edit in one pane has to redraw
/// another.
///
/// The design asked for a registry of documents with ids (ペイン分割設計 3).
/// **The `Rc` is that registry**: a document lives while a tab holds it and
/// goes when the last one lets go, with no table to keep in step and no id that
/// can name something that is no longer there.
pub struct OpenDocument {
    pub file: RefCell<DocumentFile>,
    pub text: SharedText,
    /// What has been done to this text and can be taken back (要件 7.1). It
    /// belongs to the document because 要件 7.6 says it does: one file, one
    /// history, however many panes are showing it.
    pub history: RefCell<History>,
    /// The per-line counts: the statistics behind the status bar and the
    /// heading level of every line.
    ///
    /// **A property of the text, not of any view of it** (ペイン分割設計 2), so
    /// panes share one. It used to sit in `RenderCache`, where one slot served
    /// every pane — right, because it is keyed by the text and rebuilds when it
    /// does not match, but a full re-scan every time two panes showing
    /// different documents took turns.
    pub counts: RefCell<CountsSlot>,
}

impl OpenDocument {
    pub fn new(file: DocumentFile, text: String, window: Weak<AppWindow>) -> Rc<Self> {
        Rc::new(Self {
            file: RefCell::new(file),
            text: SharedText::new(text, window),
            history: RefCell::new(History::default()),
            counts: RefCell::new(CountsSlot::default()),
        })
    }

    /// Remember a change that has just gone into the text.
    pub fn record(&self, at: usize, removed: String, inserted: String) {
        self.history.borrow_mut().record(Edit {
            at,
            removed,
            inserted,
            made_at: Instant::now(),
        });
    }

    /// A document that has never been saved (要件 8.4).
    pub fn untitled(number: u32, window: Weak<AppWindow>) -> Rc<Self> {
        Self::new(DocumentFile::untitled(number), String::new(), window)
    }
}

/// One change to a document's text, and everything it takes to undo it.
///
/// A range of the old text, and what went in its place. Both halves are kept
/// because an undo has to put back exactly what was there, and a redo exactly
/// what replaced it — **the text itself, not a copy of the document**: a
/// snapshot per keystroke of a document this editor will open is megabytes.
#[derive(Clone, Debug)]
pub struct Edit {
    /// Where the change begins, in source bytes.
    pub at: usize,
    /// What was in `at..at + removed.len()` before.
    pub removed: String,
    /// What is in `at..at + inserted.len()` now.
    pub inserted: String,
    /// When it was made. A run of typing joins into one entry, and this is
    /// what says the run has stopped.
    pub made_at: Instant,
}

impl Edit {
    /// Whether this is a run of typing rather than a deletion or a
    /// replacement, which is the only kind that joins with the one before.
    pub fn is_typing(&self) -> bool {
        self.removed.is_empty() && !self.inserted.contains('\n')
    }

    /// Whether this is one backspace or one delete: text taken out and
    /// nothing put in.
    pub fn is_deletion(&self) -> bool {
        self.inserted.is_empty() && !self.removed.contains('\n')
    }
}

/// How long a run of typing may pause and still be one undo (要件 7.1).
///
/// Undoing a paragraph one character at a time is not what Ctrl+Z is for, and
/// undoing a whole session at once is worse. A pause is the writer stopping to
/// think, which is where they would expect the step to end.
pub const UNDO_JOIN_IDLE: Duration = Duration::from_millis(1200);
/// The most changes a document remembers.
///
/// Each holds only the text that changed, so the list is small until it is very
/// long. This is a bound on the pathological case, not a budget anybody is
/// meant to notice.
pub const UNDO_DEPTH: usize = 500;

/// A document's undo history.
///
/// **One per document, not per pane** (要件 7.6): the same file open twice is
/// one text with one history, so an undo in either pane takes back whatever was
/// done last, in whichever pane it was done.
#[derive(Default)]
pub struct History {
    pub done: Vec<Edit>,
    pub undone: Vec<Edit>,
}

impl History {
    /// Remember a change that has just been made.
    ///
    /// A change that continues the one before it joins it, so that a run of
    /// typing is one step. **Anything recorded throws away what was undone**:
    /// the redo list is a path back to a text that no longer exists once the
    /// writer has gone somewhere else.
    pub fn record(&mut self, edit: Edit) {
        self.undone.clear();
        if let Some(last) = self.done.last_mut()
            && edit.made_at.duration_since(last.made_at) <= UNDO_JOIN_IDLE
        {
            // Typing that carries on where the last left off.
            if last.is_typing() && edit.is_typing() && edit.at == last.at + last.inserted.len() {
                last.inserted.push_str(&edit.inserted);
                last.made_at = edit.made_at;
                return;
            }
            // Backspace, which takes the character before the last one taken.
            if last.is_deletion() && edit.is_deletion() && edit.at + edit.removed.len() == last.at {
                last.removed.insert_str(0, &edit.removed);
                last.at = edit.at;
                last.made_at = edit.made_at;
                return;
            }
            // Delete, which takes the character after, leaving the position
            // where it was.
            if last.is_deletion() && edit.is_deletion() && edit.at == last.at {
                last.removed.push_str(&edit.removed);
                last.made_at = edit.made_at;
                return;
            }
        }
        self.done.push(edit);
        if self.done.len() > UNDO_DEPTH {
            self.done.remove(0);
        }
    }

    /// Take the last change back out of `source`, and say where the caret goes.
    ///
    /// **Checked against the text before anything is moved.** A document can be
    /// replaced wholesale — a reload, the measurement sample — and a position
    /// remembered from before that names something else now. Rather than trust
    /// it, the whole history goes: nothing here is worth a panic in front of
    /// somebody's manuscript.
    pub fn undo_into(&mut self, source: &mut String) -> Option<(usize, Change)> {
        let edit = self.done.pop()?;
        let end = edit.at + edit.inserted.len();
        if source.get(edit.at..end) != Some(edit.inserted.as_str()) {
            self.forget();
            return None;
        }
        let change = Change {
            at: edit.at,
            removed: edit.inserted.len(),
            inserted: edit.removed.len(),
        };
        let caret = replace_source_range(source, (edit.at, end), &edit.removed);
        self.undone.push(edit);
        Some((caret, change))
    }

    /// Put back the last change that was taken out.
    pub fn redo_into(&mut self, source: &mut String) -> Option<(usize, Change)> {
        let edit = self.undone.pop()?;
        let end = edit.at + edit.removed.len();
        if source.get(edit.at..end) != Some(edit.removed.as_str()) {
            self.forget();
            return None;
        }
        let change = Change {
            at: edit.at,
            removed: edit.removed.len(),
            inserted: edit.inserted.len(),
        };
        let caret = replace_source_range(source, (edit.at, end), &edit.inserted);
        self.done.push(edit);
        Some((caret, change))
    }

    /// Nothing that was done can be taken back any more.
    pub fn forget(&mut self) {
        self.done.clear();
        self.undone.clear();
    }
}

/// The document's per-line counts, kept up to date rather than rebuilt.
///
/// Replaces two caches that each walked the whole document whenever any of it
/// changed: the statistics behind the status bar, and the heading level of
/// every logical line. Both are sums or maxima over lines, and a keystroke
/// changes one line (技術検証 7.1).
#[derive(Default)]
pub struct CountsSlot {
    pub source: String,
    pub counts: DocumentCounts,
    pub started: bool,
}

impl CountsSlot {
    pub fn get(&mut self, source: &str) -> &DocumentCounts {
        if !self.started || self.source != source {
            self.counts.refresh(source);
            self.source.clear();
            self.source.push_str(source);
            self.started = true;
        }
        &self.counts
    }
}

/// One edit, as another view of the same document needs to know it (要件 7.6).
///
/// A byte count is all it takes: where the change began, how much went out and
/// how much came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub at: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Change {
    /// Where a position held somewhere else ends up.
    ///
    /// **Carried, not clamped.** Another pane's caret used only to be rounded to
    /// a character boundary (6.7), which left it on the same byte while the text
    /// under it moved — so typing in one pane slid the other's caret along the
    /// line. A position before the change does not move; one after it moves by
    /// what the change added or took away; one *inside* what was removed has
    /// nothing left to point at and goes to where the change began.
    pub fn moved(self, position: usize) -> usize {
        if position <= self.at {
            position
        } else if position >= self.at + self.removed {
            position - self.removed + self.inserted
        } else {
            self.at
        }
    }
}

pub fn replace_source_range(
    source: &mut String,
    (start, end): (usize, usize),
    replacement: &str,
) -> usize {
    source.replace_range(start..end, replacement);
    start + replacement.len()
}
