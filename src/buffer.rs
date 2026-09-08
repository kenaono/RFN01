//! Which file a document is, and what it takes to put it back.
//!
//! The text itself lives in the editor's shared buffer. This holds everything
//! about the document that is not its characters: where it came from, the
//! shape the file was in, and what the file was the last time the two agreed.
//! Keeping them apart is what lets several panes show the same text while only
//! one thing decides what saving it means (要件 7.6).
//!
//! Nothing here touches Windows or Slint.

use std::io;
use std::path::{Path, PathBuf};

use crate::file_io::{self, FileStamp, LoadError, TextForm};

/// A file the document has been saved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFile {
    pub path: PathBuf,
    /// The shape the file was in, so a save writes it back the same way.
    pub form: TextForm,
    /// What the file was when the editor and it last agreed.
    pub stamp: FileStamp,
}

/// Where a document's text came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Never saved: it has a number rather than a name (要件 8.4).
    Untitled(u32),
    Saved(SavedFile),
}

/// What another program has done to the file since the editor last looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalChange {
    /// The file is as the editor left it, or there is no file yet.
    None,
    Modified,
    /// Gone, or no longer reachable.
    Missing,
}

/// One document's identity.
#[derive(Clone, Debug)]
pub struct DocumentFile {
    origin: Origin,
    /// Whether the file mixed its line breaks, so the editor can say that the
    /// others were levelled rather than level them silently.
    mixed_newlines: bool,
    /// The outside change already told to the writer.
    ///
    /// Without it a file being written over and over — a sync, a build — would
    /// say so at every check rather than once per change (要件 8.3).
    reported: Option<FileStamp>,
}

impl DocumentFile {
    /// A document that has never been saved.
    pub fn untitled(number: u32) -> Self {
        Self {
            origin: Origin::Untitled(number),
            mixed_newlines: false,
            reported: None,
        }
    }

    /// Read a file and take it as the document's own.
    ///
    /// The text comes back separately because it belongs to the editor's
    /// shared buffer, not to this.
    pub fn open(path: &Path, limit: usize) -> Result<(Self, String), LoadError> {
        let loaded = file_io::read(path, limit)?;
        let saved = SavedFile {
            path: path.to_path_buf(),
            form: loaded.form,
            stamp: loaded.stamp,
        };
        let document = Self {
            origin: Origin::Saved(saved),
            mixed_newlines: loaded.form.mixed_newlines,
            reported: None,
        };
        Ok((document, loaded.text))
    }

    pub fn path(&self) -> Option<&Path> {
        match &self.origin {
            Origin::Untitled(_) => None,
            Origin::Saved(saved) => Some(&saved.path),
        }
    }

    pub fn mixed_newlines(&self) -> bool {
        self.mixed_newlines
    }

    /// The file was renamed or moved on disk, and the document follows it
    /// (要件 5.2).
    ///
    /// **Nothing is read.** A rename keeps the contents, the length and the
    /// modified time, so the stamp the watcher compares against is still the
    /// right one (要件 8.3) — and re-reading would throw away work that is not
    /// in the file yet. A document with no file has nothing to follow.
    pub fn follow_rename(&mut self, path: PathBuf) {
        if let Origin::Saved(saved) = &mut self.origin {
            saved.path = path;
        }
    }

    /// Which untitled buffer this is, or 0 once it has a file.
    ///
    /// A document with no name still has to have its work copy found again
    /// after a restart, and its number is the only thing it has (要件 8.4).
    pub fn untitled_number(&self) -> u32 {
        match &self.origin {
            Origin::Untitled(number) => *number,
            Origin::Saved(_) => 0,
        }
    }

    /// What to call this document on screen.
    pub fn title(&self) -> String {
        match &self.origin {
            Origin::Untitled(number) => format!("無題{number}"),
            Origin::Saved(saved) => file_title(&saved.path),
        }
    }

    /// The shape a save should write: the file's own, or the default for a
    /// document that has never had one.
    fn form(&self) -> TextForm {
        match &self.origin {
            Origin::Untitled(_) => TextForm::default(),
            Origin::Saved(saved) => saved.form,
        }
    }

    /// Write the document to `path` and take that file as its own.
    ///
    /// One call for both 上書き保存 and 名前を付けて保存: the two differ only in
    /// where the path came from, and either way the file just written becomes
    /// the baseline the watcher compares against (要件 8.2).
    pub fn save_to(&mut self, path: PathBuf, text: &str) -> io::Result<()> {
        let form = self.form();
        let stamp = file_io::save(&path, text, form)?;
        self.origin = Origin::Saved(SavedFile { path, form, stamp });
        // What the editor just wrote is not an outside change.
        self.reported = None;
        Ok(())
    }

    /// What has happened to the file since the editor last agreed with it.
    ///
    /// Compares the stamp rather than the contents. This is asked on the way
    /// into a save, and reading the file to answer it would cost more than the
    /// save (要件 8.3).
    pub fn external_change(&self) -> ExternalChange {
        let Origin::Saved(saved) = &self.origin else {
            return ExternalChange::None;
        };
        match FileStamp::read(&saved.path) {
            Ok(stamp) if stamp == saved.stamp => ExternalChange::None,
            Ok(_) => ExternalChange::Modified,
            Err(_) => ExternalChange::Missing,
        }
    }

    /// The stamp this document last agreed with its file at (要件 8.3).
    ///
    /// **What the watcher compares against**, as opposed to `current_stamp`,
    /// which is what the file says right now. A work copy carries this one, so
    /// that a document restored into another run keeps the same idea of "the
    /// version I was editing against" (2026-09-08).
    pub fn agreed_stamp(&self) -> Option<FileStamp> {
        match &self.origin {
            Origin::Saved(saved) => Some(saved.stamp),
            Origin::Untitled(_) => None,
        }
    }

    /// Put back the stamp a work copy was taken against (要件 8.3, 2026-09-08).
    ///
    /// **復元は元ファイルを読み直す**ので、そのままでは「合意した姿」が*いまの*
    /// ファイルになり、閉じているあいだの外部変更が無かったことになる。退避
    /// した時点の姿へ戻せば、`external_change`が最初の一度で食い違いを言う。
    pub fn agreed_at(&mut self, stamp: FileStamp) {
        if let Origin::Saved(saved) = &mut self.origin {
            saved.stamp = stamp;
        }
    }

    /// What the file is now, or `None` when there is none or it cannot be read.
    pub fn current_stamp(&self) -> Option<FileStamp> {
        let Origin::Saved(saved) = &self.origin else {
            return None;
        };
        FileStamp::read(&saved.path).ok()
    }

    /// Whether this outside change still has to be told to the writer.
    ///
    /// Records it as told, so that a file being written over and over says so
    /// once per change rather than at every check (要件 8.3).
    pub fn take_report(&mut self, stamp: FileStamp) -> bool {
        if self.reported == Some(stamp) {
            return false;
        }
        self.reported = Some(stamp);
        true
    }

    /// Read the file again and take what it now holds.
    ///
    /// The shape and the stamp come with it, so the document ends up as if it
    /// had just been opened — which is what 要件 8.3's「安全に再読み込みする」
    /// means for a document with nothing unsaved in it.
    pub fn reload(&mut self, limit: usize) -> Option<Result<String, LoadError>> {
        let path = self.path()?.to_path_buf();
        Some(match Self::open(&path, limit) {
            Ok((reopened, text)) => {
                *self = reopened;
                Ok(text)
            }
            Err(error) => Err(error),
        })
    }
}

/// A path's last component, or the whole path when it has none.
fn file_title(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::file_io::Newline;

    const LIMIT: usize = 1_000_000;

    fn scratch_directory(name: &str) -> PathBuf {
        let temporary = std::env::temp_dir();
        let directory = temporary.join(format!("rfnedit-buffer-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    #[test]
    fn a_new_document_is_numbered_rather_than_named() {
        let document = DocumentFile::untitled(2);
        assert_eq!(document.title(), "無題2");
        assert_eq!(document.path(), None);
    }

    #[test]
    fn an_opened_document_is_named_after_its_file() {
        let directory = scratch_directory("open");
        let path = directory.join("原稿.md");
        fs::write(&path, "本文").expect("writes");
        let (document, text) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert_eq!(text, "本文");
        assert_eq!(document.title(), "原稿.md");
        assert_eq!(document.path(), Some(path.as_path()));
        let _ = fs::remove_dir_all(&directory);
    }

    /// Saving a new document is what gives it a name.
    #[test]
    fn saving_an_untitled_document_gives_it_its_file() {
        let directory = scratch_directory("save-untitled");
        let path = directory.join("新規.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "本文").expect("saves");
        assert_eq!(document.title(), "新規.md");
        assert_eq!(fs::read(&path).expect("reads"), "本文".as_bytes());
        let _ = fs::remove_dir_all(&directory);
    }

    /// The shape of the file survives a round trip through the editor.
    #[test]
    fn saving_keeps_the_line_break_the_file_had() {
        let directory = scratch_directory("keeps-crlf");
        let path = directory.join("crlf.md");
        fs::write(&path, b"a\r\nb\r\n").expect("writes");
        let (mut document, text) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert_eq!(text, "a\nb\n");
        document.save_to(path.clone(), &text).expect("saves");
        assert_eq!(fs::read(&path).expect("reads"), b"a\r\nb\r\n");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn saving_under_a_new_name_moves_the_document_to_it() {
        let directory = scratch_directory("save-as");
        let first = directory.join("旧.md");
        let second = directory.join("新.md");
        fs::write(&first, "本文").expect("writes");
        let (mut document, text) = DocumentFile::open(&first, LIMIT).expect("opens");
        document.save_to(second.clone(), &text).expect("saves");
        assert_eq!(document.path(), Some(second.as_path()));
        assert!(first.exists(), "the original is left alone");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_document_that_has_no_file_has_nothing_to_compare() {
        let document = DocumentFile::untitled(1);
        assert_eq!(document.external_change(), ExternalChange::None);
    }

    #[test]
    fn a_freshly_saved_file_reports_no_outside_change() {
        let directory = scratch_directory("unchanged");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path, "本文").expect("saves");
        assert_eq!(document.external_change(), ExternalChange::None);
        let _ = fs::remove_dir_all(&directory);
    }

    /// The case 要件 8.2 asks about before it overwrites anything.
    #[test]
    fn another_program_writing_the_file_is_noticed() {
        let directory = scratch_directory("outside-write");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "本文").expect("saves");
        fs::write(&path, "別のアプリが書いた本文").expect("writes");
        let change = document.external_change();
        assert_eq!(change, ExternalChange::Modified);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_deleted_file_is_reported_as_missing() {
        let directory = scratch_directory("deleted");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "本文").expect("saves");
        fs::remove_file(&path).expect("removes");
        assert_eq!(document.external_change(), ExternalChange::Missing);
        let _ = fs::remove_dir_all(&directory);
    }

    /// A file being written over and over must be reported once per change.
    #[test]
    fn an_outside_change_is_reported_once() {
        let directory = scratch_directory("report-once");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "本文").expect("saves");
        fs::write(&path, "別のアプリが書いた").expect("writes");
        let stamp = document.current_stamp().expect("has a stamp");
        assert!(document.take_report(stamp), "the first time is news");
        assert!(!document.take_report(stamp), "the same change is not");
        let _ = fs::remove_dir_all(&directory);
    }

    /// Saving is not an outside change, so the next real one is still news.
    #[test]
    fn saving_forgets_what_was_reported() {
        let directory = scratch_directory("report-reset");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "一").expect("saves");
        fs::write(&path, "外から").expect("writes");
        let stamp = document.current_stamp().expect("has a stamp");
        assert!(document.take_report(stamp));
        document.save_to(path.clone(), "二").expect("saves again");
        let stamp = document.current_stamp().expect("has a stamp");
        assert!(document.take_report(stamp));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reloading_takes_what_the_file_now_holds() {
        let directory = scratch_directory("reload");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path.clone(), "はじめ").expect("saves");
        fs::write(&path, "あと\r\nから").expect("writes");
        let text = document.reload(LIMIT).expect("has a file").expect("reads");
        assert_eq!(text, "あと\nから");
        // The shape came with it, so saving writes CRLF back.
        assert_eq!(document.form().newline, Newline::Crlf);
        // And the reload is the new baseline, so nothing is outstanding.
        assert_eq!(document.external_change(), ExternalChange::None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_untitled_document_has_nothing_to_reload() {
        let mut document = DocumentFile::untitled(1);
        assert!(document.reload(LIMIT).is_none());
    }

    #[test]
    fn an_opened_file_remembers_that_its_breaks_were_mixed() {
        let directory = scratch_directory("mixed");
        let path = directory.join("note.md");
        fs::write(&path, b"a\r\nb\nc").expect("writes");
        let (document, _) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert!(document.mixed_newlines());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_new_document_is_saved_as_plain_utf8_with_line_feeds() {
        let directory = scratch_directory("default-form");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        document.save_to(path, "a\nb").expect("saves");
        assert_eq!(document.form().newline, Newline::Lf);
        assert!(!document.form().byte_order_mark);
        let _ = fs::remove_dir_all(&directory);
    }
}
