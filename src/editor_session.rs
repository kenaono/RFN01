//! A document and one independent editing view. No pane, TAB or window ownership.
use crate::editor_state::{EditorState, source_line_start};
use crate::open_document::{Change, OpenDocument};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
pub(crate) struct EditorSession {
    pub(crate) state: Rc<RefCell<EditorState>>,
    pub(crate) showing: Rc<RefCell<Rc<OpenDocument>>>,
}

impl EditorSession {
    pub(crate) fn new(document: Rc<OpenDocument>) -> Self {
        Self::with_state(document, Rc::new(RefCell::new(EditorState::default())))
    }

    pub(crate) fn with_state(document: Rc<OpenDocument>, state: Rc<RefCell<EditorState>>) -> Self {
        Self {
            state,
            showing: Rc::new(RefCell::new(document)),
        }
    }

    pub(crate) fn document(&self) -> Rc<OpenDocument> {
        self.showing.borrow().clone()
    }

    /// Commit typed text into the source snapshot used by the synchronous hit /
    /// preview resolver. The host must not retain this snapshot across events.
    pub(crate) fn insert_at(
        &self,
        mut source: String,
        start: usize,
        end: usize,
        input: String,
    ) -> Option<(String, usize, Change)> {
        let document = self.document();
        if document.read_only() || self.state.borrow().viewer {
            return None;
        }
        let removed = source.get(start..end)?.to_owned();
        source.replace_range(start..end, &input);
        let next = start + input.len();
        {
            let mut state = self.state.borrow_mut();
            state.caret_source_byte = Some(next);
            state.selection_anchor_source_byte = Some(next);
            state.active_line_start = Some(source_line_start(&source, next));
            state.preedit.clear();
            state.preferred_line = None;
            state.mark = false;
        }
        let change = Change {
            at: start,
            removed: removed.len(),
            inserted: input.len(),
        };
        document.record(start, removed, input);
        *document.text.borrow_mut() = source.clone();
        Some((source, next, change))
    }

    /// Replace an already resolved source span. Hit testing and preview mapping
    /// belong to the view; history and editing state belong to this session.
    pub(crate) fn splice(
        &self,
        start: usize,
        end: usize,
        text: &str,
        caret: usize,
    ) -> Option<(String, usize, Change)> {
        let document = self.document();
        if document.read_only() || self.state.borrow().viewer || start >= end {
            return None;
        }
        let mut source = document.text.borrow().clone();
        // A host may deliver a stale selection after its document changed.
        // Reject it instead of slicing through a character or panicking.
        let removed = source.get(start..end)?.to_owned();
        source.replace_range(start..end, text);
        let mut caret = caret.min(source.len());
        while !source.is_char_boundary(caret) {
            caret -= 1;
        }
        {
            let mut state = self.state.borrow_mut();
            state.caret_source_byte = Some(caret);
            state.selection_anchor_source_byte = Some(caret);
            state.active_line_start = Some(source_line_start(&source, caret));
            state.preferred_line = None;
            state.mark = false;
            state.rectangular = false;
        }
        let change = Change {
            at: start,
            removed: removed.len(),
            inserted: text.len(),
        };
        document.record(start, removed, text.to_owned());
        *document.text.borrow_mut() = source.clone();
        Some((source, caret, change))
    }

    /// History belongs to the document. Only this view's caret follows the undo;
    /// the host uses the returned change to update any other views of the document.
    pub(crate) fn undo(&self, forwards: bool) -> Option<(String, usize, Change)> {
        let document = self.document();
        if document.read_only() || self.state.borrow().viewer {
            return None;
        }
        let mut source = document.text.borrow().clone();
        let moved = {
            let mut history = document.history.borrow_mut();
            if forwards {
                history.redo_into(&mut source)
            } else {
                history.undo_into(&mut source)
            }
        };
        let (caret, change) = moved?;
        {
            let mut state = self.state.borrow_mut();
            state.caret_source_byte = Some(caret);
            state.selection_anchor_source_byte = Some(caret);
            state.active_line_start = Some(source_line_start(&source, caret));
            state.preedit.clear();
            state.preferred_line = None;
        }
        *document.text.borrow_mut() = source.clone();
        document.text.reconcile_saved();
        Some((source, caret, change))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::DocumentFile;

    #[test]
    fn replacement_and_deletion_share_history_and_reject_stale_ranges() {
        let document = OpenDocument::with_events(
            DocumentFile::untitled(1),
            "猫と犬\n次の行".into(),
            Default::default(),
        );
        let session = EditorSession::new(document.clone());
        let other = EditorSession::new(document.clone());
        other.state.borrow_mut().caret_source_byte = Some(0);
        {
            let mut state = session.state.borrow_mut();
            state.mark = true;
            state.rectangular = true;
        }
        let (text, caret, change) = session.splice(3, 6, "や", 6).unwrap();
        assert_eq!(text, "猫や犬\n次の行");
        assert_eq!(caret, 6);
        assert_eq!((change.at, change.removed, change.inserted), (3, 3, 3));
        assert!(!session.state.borrow().mark);
        assert!(!session.state.borrow().rectangular);
        assert_eq!(other.state.borrow().caret_source_byte, Some(0));
        assert_eq!(session.undo(false).unwrap().0, "猫と犬\n次の行");
        assert_eq!(session.undo(true).unwrap().0, "猫や犬\n次の行");
        assert_eq!(session.splice(0, 9, "", 0).unwrap().0, "\n次の行");
        assert_eq!(session.undo(false).unwrap().0, "猫や犬\n次の行");
        assert!(session.splice(1, 3, "", 0).is_none());
        assert!(session.splice(0, usize::MAX, "", 0).is_none());
        assert!(session.splice(3, 3, "", 0).is_none());
        session.state.borrow_mut().viewer = true;
        assert!(session.splice(0, 3, "", 0).is_none());
        assert_eq!(&*document.text.borrow(), "猫や犬\n次の行");
    }

    #[test]
    fn independent_views_share_document_history_without_a_window() {
        let document =
            OpenDocument::with_events(DocumentFile::untitled(1), "猫".into(), Default::default());
        let first = EditorSession::new(document.clone());
        let second = EditorSession::new(document.clone());
        first.state.borrow_mut().caret_source_byte = Some(3);
        second.state.borrow_mut().caret_source_byte = Some(0);
        document.record(3, String::new(), "犬".into());
        *document.text.borrow_mut() = "猫犬".into();
        assert_eq!(second.undo(false).unwrap().0, "猫");
        assert!(!document.text.edited());
        assert_eq!(first.state.borrow().caret_source_byte, Some(3));
        assert_eq!(first.undo(true).unwrap().0, "猫犬");
        assert_eq!(second.state.borrow().caret_source_byte, Some(3));
        first.state.borrow_mut().set_read_only(true, true);
        assert!(first.undo(false).is_none());
        assert_eq!(&*document.text.borrow(), "猫犬");
    }
}
