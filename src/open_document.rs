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
//! Hosts supply notifications; documents do not own or address a window.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::buffer::DocumentFile;
use crate::document::{DocumentCounts, Reading};

/// Notifications supplied by the embedding host. Empty hooks support headless documents.
#[derive(Clone, Default)]
pub struct DocumentEvents {
    pub edited: Option<Rc<dyn Fn(bool)>>,
    pub editing: Option<Rc<dyn Fn()>>,
}

/// The document's text, and whether it has changed since it last agreed with
/// its file.
///
/// The flag lives with the text because **every writer goes through
/// `borrow_mut`**. Threading it through the four editing functions instead
/// would hold until the fifth one was written; this cannot be forgotten, which
/// is the same reason a block decides its size from its own measurements
/// rather than from a running total (技術検証 3.4).
pub struct SharedText {
    // None means recovery could not establish the last saved text.
    saved_text: RefCell<Option<String>>,
    pub text: RefCell<String>,
    pub edited: Cell<bool>,
    /// When the text last changed, and when the run of changes now waiting for
    /// a work copy began. 要件 8.1 states two rules and they are measured from
    /// these two moments; see [`work_copy_due`].
    pub changed_at: Cell<Instant>,
    pub pending_since: Cell<Option<Instant>>,
    /// How many characters the text holds, once somebody has asked (RFN01-6
    /// D). **Forgotten by every edit** — they all come through
    /// [`Self::borrow_mut`] — and told again by the one edit that knows what it
    /// added and took away, so a keystroke does not count the whole document
    /// to see whether it is still inside the limit.
    characters: Cell<Option<usize>>,
    /// Told as soon as the flag moves, so the title marker does not depend on
    /// remembering to refresh it at each of the places an edit can begin.
    events: DocumentEvents,
}

impl SharedText {
    pub fn with_events(text: String, events: DocumentEvents) -> Self {
        Self {
            saved_text: RefCell::new(Some(text.clone())),
            text: RefCell::new(text),
            edited: Cell::new(false),
            changed_at: Cell::new(Instant::now()),
            pending_since: Cell::new(None),
            characters: Cell::new(None),
            events,
        }
    }

    /// How many characters the text holds, counted only when no edit since the
    /// last count has said.
    pub fn character_count(&self) -> usize {
        match self.characters.get() {
            Some(count) => count,
            None => {
                let count = self.text.borrow().chars().count();
                self.characters.set(Some(count));
                count
            }
        }
    }

    /// An edit that knows what it added and took away says what the count is
    /// now, after its [`Self::borrow_mut`] has let the old one go.
    pub fn set_character_count(&self, count: usize) {
        debug_assert_eq!(count, self.text.borrow().chars().count());
        self.characters.set(Some(count));
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
        self.characters.set(None);
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
        if let Some(notify) = &self.events.editing {
            notify();
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
        if self.edited.get() {
            self.retry_work_copy();
        }
    }

    /// Retry either a backup or removal of a stale backup after Undo.
    pub fn retry_work_copy(&self) {
        if self.pending_since.get().is_none() {
            self.pending_since.set(Some(Instant::now()));
        }
    }

    pub fn edited(&self) -> bool {
        self.edited.get()
    }

    /// Restored from a work copy: the text does not agree with its file, but
    /// the copy on disk already holds it, so nothing is waiting to be written.
    pub fn mark_restored(&self, saved_text: Option<String>) {
        *self.saved_text.borrow_mut() = saved_text;
        self.pending_since.set(None);
        self.set_edited(true);
    }

    /// The text now agrees with its file: it was just opened, or just saved.
    pub fn mark_saved(&self) {
        *self.saved_text.borrow_mut() = Some(self.text.borrow().clone());
        self.pending_since.set(None);
        self.set_edited(false);
    }

    /// Undo/Redo compares exact text with the last successful save, not a hash.
    /// Ordinary keystrokes do not scan or clone the saved document.
    pub fn reconcile_saved(&self) {
        let edited = self
            .saved_text
            .borrow()
            .as_ref()
            .is_none_or(|saved| *self.text.borrow() != *saved);
        // Keep the pending work: reaching the saved text must retire the old
        // backup through the same writer queue, even if it is still in flight.
        self.set_edited(edited);
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
        if let Some(notify) = &self.events.edited {
            notify(edited);
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
    pub comparison_peer: RefCell<std::rc::Weak<OpenDocument>>,
    comparison_cache: RefCell<Option<(Instant, Instant, Rc<crate::comparison::Difference>)>>,
    /// Label of an immutable, transient external-version snapshot.
    pub external_snapshot: Option<(String, crate::file_io::TextForm)>,
    pub file: RefCell<DocumentFile>,
    pub text: SharedText,
    /// **外で変わったまま、まだ片付いていない**（要件 8.3、書き手のレビュー S2）。
    ///
    /// **消えない印**である——知らせは`render_status`にも出るが、あちらは書き手が
    /// 次へ動けば畳む（6.6）。外部変更は**片付くまで残っていなければならない**：
    /// 長く書き続けているあいだに外の原稿も変わったことを、保存のときに初めて
    /// 知るのでは、そこでどちらかを捨てることになる。
    ///
    /// 下りるのは**読み直したときと、書いたとき**だけ。
    pub outside: Cell<bool>,
    pub missing: Cell<bool>,
    /// Failure recovery remains available even when ordinary backups are off.
    pub protective_recovery: Cell<bool>,
    pub recovery_failed: Cell<bool>,
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
    pub fn with_events(file: DocumentFile, text: String, events: DocumentEvents) -> Rc<Self> {
        Rc::new(Self {
            comparison_peer: RefCell::new(std::rc::Weak::new()),
            comparison_cache: RefCell::new(None),
            external_snapshot: None,
            file: RefCell::new(file),
            text: SharedText::with_events(text, events),
            outside: Cell::new(false),
            missing: Cell::new(false),
            protective_recovery: Cell::new(false),
            recovery_failed: Cell::new(false),
            history: RefCell::new(History::default()),
            counts: RefCell::new(CountsSlot::default()),
        })
    }

    pub fn read_only(&self) -> bool {
        self.external_snapshot.is_some()
    }

    #[cfg(test)]
    pub fn compare_with(self: &Rc<Self>, other: &Rc<Self>) {
        self.stop_comparison();
        *self.comparison_peer.borrow_mut() = Rc::downgrade(other);
        *other.comparison_peer.borrow_mut() = Rc::downgrade(self);
        *other.comparison_cache.borrow_mut() = None;
    }

    pub fn stop_comparison(self: &Rc<Self>) {
        let peer = self.comparison_peer.borrow().upgrade();
        if let Some(other) = peer {
            let paired = other
                .comparison_peer
                .borrow()
                .upgrade()
                .is_some_and(|held| Rc::ptr_eq(&held, self));
            if paired {
                *other.comparison_peer.borrow_mut() = std::rc::Weak::new();
                *other.comparison_cache.borrow_mut() = None;
            }
        }
        *self.comparison_peer.borrow_mut() = std::rc::Weak::new();
        *self.comparison_cache.borrow_mut() = None;
    }

    pub fn differences(&self) -> Option<Rc<crate::comparison::Difference>> {
        let peer = self.comparison_peer.borrow().upgrade()?;
        let keys = (self.text.changed_at(), peer.text.changed_at());
        if let Some((a, b, result)) = self.comparison_cache.borrow().as_ref()
            && (*a, *b) == keys
        {
            return Some(result.clone());
        }
        let result = Rc::new(crate::comparison::compare(
            &self.text.borrow(),
            &peer.text.borrow(),
        ));
        *self.comparison_cache.borrow_mut() = Some((keys.0, keys.1, result.clone()));
        Some(result)
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
    pub separate_next: bool,
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
        let separate = std::mem::take(&mut self.separate_next);
        if let Some(last) = self.done.last_mut()
            && !separate
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
    pub counts: DocumentCounts,
}

impl CountsSlot {
    /// 要件 E9: `ruby`は記法を読むかどうか。**古くなったかどうかを決めるのは
    /// `DocumentCounts`のほう**——切り替えたときに行を捨てるのはあちらの仕事で、
    /// ここはただ渡す。
    ///
    /// RFN01-6 C: **本文の写しはあちらが持つ**。ここにも写しを置いて比べていた
    /// ころは、1000万字で30MBの写しがもう1本あり、比べるたびに全部を読んでいた。
    pub fn get(&mut self, source: &str, reading: Reading) -> &DocumentCounts {
        self.counts.refresh(source, reading);
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

#[cfg(test)]
mod snapshot_tests {
    use slint::Weak;
    #[test]
    fn document_notifies_its_host_without_a_window() {
        let flags = Rc::new(RefCell::new(Vec::new()));
        let edits = Rc::new(Cell::new(0));
        let notify_flags = flags.clone();
        let notify_edits = edits.clone();
        let text = SharedText::with_events(
            "原稿".into(),
            DocumentEvents {
                edited: Some(Rc::new(move |flag| notify_flags.borrow_mut().push(flag))),
                editing: Some(Rc::new(move || notify_edits.set(notify_edits.get() + 1))),
            },
        );
        text.borrow_mut().push('一');
        text.borrow_mut().push('二');
        assert_eq!(&*flags.borrow(), &[true]);
        assert_eq!(edits.get(), 2);
        *text.borrow_mut() = "原稿".into();
        text.reconcile_saved();
        assert_eq!(&*flags.borrow(), &[true, false]);
        assert!(!text.edited());
    }
    use super::*;

    #[test]
    fn saved_text_matches_after_undo_and_changes_again_after_redo() {
        for (original, replacement) in [("", "入力"), ("保存した本文", "全置換した本文")]
        {
            let text = SharedText::new(original.into(), Weak::default());
            let mut history = History::default();
            history.record(Edit {
                at: 0,
                removed: original.into(),
                inserted: replacement.into(),
                made_at: Instant::now(),
            });
            *text.borrow_mut() = replacement.into();
            assert!(text.edited());
            history.undo_into(&mut text.borrow_mut()).unwrap();
            text.reconcile_saved();
            assert!(!text.edited());
            history.redo_into(&mut text.borrow_mut()).unwrap();
            text.reconcile_saved();
            assert!(text.edited());
        }
    }

    #[test]
    fn saving_splits_continuous_typing_and_keeps_undo_before_save() {
        let text = SharedText::new(String::new(), Weak::default());
        let mut history = History::default();
        history.record(Edit {
            at: 0,
            removed: "".into(),
            inserted: "a".into(),
            made_at: Instant::now(),
        });
        *text.borrow_mut() = "a".into();
        text.mark_saved();
        history.separate_next = true;
        history.record(Edit {
            at: 1,
            removed: "".into(),
            inserted: "b".into(),
            made_at: Instant::now(),
        });
        *text.borrow_mut() = "ab".into();
        history.undo_into(&mut text.borrow_mut()).unwrap();
        text.reconcile_saved();
        assert_eq!(&*text.borrow(), "a");
        assert!(!text.edited());
        history.undo_into(&mut text.borrow_mut()).unwrap();
        text.reconcile_saved();
        assert!(text.edited());
        history.redo_into(&mut text.borrow_mut()).unwrap();
        text.reconcile_saved();
        assert!(!text.edited());
    }

    #[test]
    fn comparison_updates_after_manual_merge_and_undo_without_acknowledging_conflict() {
        let source = OpenDocument::new(DocumentFile::untitled(1), "猫".into(), Weak::default());
        let external = OpenDocument::snapshot(
            "原稿".into(),
            crate::file_io::TextForm::default(),
            "犬".into(),
            Weak::default(),
        );
        source.outside.set(true);
        source.compare_with(&external);
        let first = source.differences().unwrap();
        assert_eq!(first.ranges, vec![0..3]);
        assert!(
            Rc::ptr_eq(&first, &source.differences().unwrap()),
            "unchanged text reuses the result"
        );
        source.record(0, "猫".into(), "犬".into());
        *source.text.borrow_mut() = external.text.borrow().clone();
        assert!(source.differences().unwrap().ranges.is_empty());
        assert!(external.differences().unwrap().ranges.is_empty());
        source
            .history
            .borrow_mut()
            .undo_into(&mut source.text.borrow_mut())
            .unwrap();
        assert_eq!(source.differences().unwrap().ranges, vec![0..3]);
        assert_eq!(&*external.text.borrow(), "犬");
        assert!(source.outside.get());
        assert!(!external.text.edited());
        source.stop_comparison();
        assert!(source.differences().is_none());
        assert!(external.differences().is_none());
        assert!(source.outside.get());
    }

    #[test]
    fn comparing_again_retires_the_previous_pair_without_keeping_documents_alive() {
        let source = OpenDocument::untitled(1, Weak::default());
        let first = OpenDocument::untitled(2, Weak::default());
        let next = OpenDocument::untitled(3, Weak::default());
        source.compare_with(&first);
        source.compare_with(&next);
        assert!(first.differences().is_none());
        assert!(next.differences().is_some());
        first.stop_comparison();
        assert!(source.differences().is_some());
        drop(next);
        assert!(source.differences().is_none());
    }

    #[test]
    fn external_snapshot_is_detached_from_the_original_file_and_work_copy() {
        let document = OpenDocument::snapshot(
            "原稿.md".into(),
            crate::file_io::TextForm::default(),
            "外部の本文".into(),
            Weak::default(),
        );
        assert!(document.read_only());
        assert!(document.file.borrow().path().is_none());
        assert!(!document.text.edited());
        assert!(document.text.pending_since().is_none());
        assert_eq!(document.text.borrow().as_str(), "外部の本文");
        assert!(document.history.borrow().done.is_empty());
        assert!(!OpenDocument::untitled(1, Weak::default()).read_only());
    }
}
