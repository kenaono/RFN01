//! Main-window adapter for document notifications. The document model stays UI-free.
use crate::open_document::{DocumentEvents, OpenDocument, SharedText};
use crate::{AppWindow, buffer::DocumentFile};
use slint::Weak;
use std::rc::Rc;

fn events(window: Weak<AppWindow>) -> DocumentEvents {
    let edited_window = window.clone();
    DocumentEvents {
        edited: Some(Rc::new(move |edited| {
            if let Some(window) = edited_window.upgrade() {
                window.set_document_edited(edited);
                window.invoke_republish_tabs();
            }
        })),
        editing: Some(Rc::new(move || {
            // RFN01-63: 綴りを確認している文書があれば、手が止まってから数え直す。
            crate::spelling_ui::touched();
            if let Some(window) = window.upgrade() {
                if !window.get_render_status().is_empty() {
                    window.set_render_status(Default::default());
                }
            }
        })),
    }
}

impl SharedText {
    #[cfg(test)]
    pub fn new(text: String, window: Weak<AppWindow>) -> Self {
        Self::with_events(text, events(window))
    }
}
impl OpenDocument {
    pub fn new(file: DocumentFile, text: String, window: Weak<AppWindow>) -> Rc<Self> {
        Self::with_events(file, text, events(window))
    }
    pub fn untitled(number: u32, window: Weak<AppWindow>) -> Rc<Self> {
        Self::new(DocumentFile::untitled(number), String::new(), window)
    }
    #[cfg(test)]
    pub fn snapshot(
        title: String,
        form: crate::file_io::TextForm,
        text: String,
        window: Weak<AppWindow>,
    ) -> Rc<Self> {
        let mut document = Self::new(DocumentFile::untitled(0), text, window);
        Rc::get_mut(&mut document).unwrap().external_snapshot = Some((title, form));
        document
    }
}
