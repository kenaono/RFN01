//! Turning a file into the text the engine holds, and back again.
//!
//! Nothing here touches Windows, DirectWrite or Slint. A document is a single
//! `String` whose only line break is `\n` (技術検証 7.6), so everything a file
//! had that the engine does not carry — a byte order mark, the kind of line
//! break it used — comes off on the way in and goes back on the way out.
//! Reading is where that belongs: `normalize_typed_input` already folds `\r\n`
//! and a lone `\r` on the two paths that reach the document from the keyboard,
//! and this is the third.
//!
//! Saving never writes over the target directly. The bytes go to a temporary
//! file beside it and are renamed on top of it once they are all there, so a
//! failure leaves the original as it was (要件 8.2).

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// UTF-8 byte order mark.
const BYTE_ORDER_MARK: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Suffix of the file a save writes before it renames.
///
/// Distinctive on purpose: if a save is interrupted between the write and the
/// rename, whatever is left behind should say what left it there.
const TEMPORARY_SUFFIX: &str = "rfnedit-tmp";

/// The line break a file used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Newline {
    #[default]
    Lf,
    Crlf,
    /// Carriage return alone, as classic Mac OS wrote.
    ///
    /// A case of its own rather than folded into the others, so that reading
    /// such a file and saving it back is not a change nobody asked for.
    Cr,
}

impl Newline {
    pub fn as_str(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::Crlf => "\r\n",
            Newline::Cr => "\r",
        }
    }
}

/// What a file was, apart from the text itself.
///
/// Carried alongside the document so a save can put the file back into the
/// shape it arrived in. A new document starts at the default — no mark, `\n` —
/// which is what this editor writes when nothing says otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextForm {
    pub byte_order_mark: bool,
    pub newline: Newline,
    /// Whether the file used more than one kind of line break.
    ///
    /// A document holds one kind, so a mixed file cannot be written back
    /// unchanged whatever is chosen. The flag exists so the choice can be said
    /// out loud rather than made silently: the editor keeps the first kind it
    /// saw and can tell the writer that the others were levelled.
    pub mixed_newlines: bool,
}

/// Enough of a file's identity to notice another program has written it.
///
/// Length as well as time because a same-second write of the same length is
/// the one case a timestamp misses, and the two together are what a watcher
/// can compare without reading the file (要件 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub length: u64,
}

impl FileStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            length: metadata.len(),
        }
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::of(&fs::metadata(path)?))
    }
}

/// A file, as the editor holds it.
#[derive(Clone, Debug)]
pub struct LoadedFile {
    /// The text with `\n` as its only line break, and no byte order mark.
    pub text: String,
    pub form: TextForm,
    pub stamp: FileStamp,
}

#[derive(Debug)]
pub enum LoadError {
    /// The bytes are not UTF-8.
    ///
    /// Turned away rather than guessed at. Reading a Shift_JIS file as if it
    /// were UTF-8 produces text that merely looks broken but saves back as
    /// real damage, and this editor's job is to not lose what it was given.
    NotUtf8,
    /// Larger than this editor takes (技術検証 3.10).
    TooLarge {
        characters: usize,
        limit: usize,
    },
    Io(io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotUtf8 => write!(formatter, "UTF-8として読めないファイルです"),
            LoadError::TooLarge { characters, limit } => write!(
                formatter,
                "{characters}文字のファイルは、上限{limit}文字を超えるため開けません"
            ),
            LoadError::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for LoadError {
    fn from(error: io::Error) -> Self {
        LoadError::Io(error)
    }
}

/// Which line break a file used, and whether it used more than one.
///
/// One pass over the bytes. A `\r\n` counts as one break of its own kind
/// rather than as a `\r` and a `\n`, which is the whole reason this cannot be
/// three independent searches.
fn detect_newline(text: &str) -> (Newline, bool) {
    let bytes = text.as_bytes();
    let mut first: Option<Newline> = None;
    let mut mixed = false;
    let mut index = 0;
    while index < bytes.len() {
        let kind = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                index += 1;
                Newline::Crlf
            }
            b'\r' => Newline::Cr,
            b'\n' => Newline::Lf,
            _ => {
                index += 1;
                continue;
            }
        };
        match first {
            None => first = Some(kind),
            Some(seen) if seen != kind => mixed = true,
            Some(_) => {}
        }
        index += 1;
    }
    (first.unwrap_or_default(), mixed)
}

/// Bytes to the text, and the shape the file was in.
///
/// Split out from [`read`] so the decisions — what counts as a line break,
/// what the limit means, what a byte order mark does — can be tested without a
/// filesystem.
pub fn decode(bytes: &[u8], limit: usize) -> Result<(String, TextForm), LoadError> {
    let byte_order_mark = bytes.starts_with(&BYTE_ORDER_MARK);
    let body = if byte_order_mark {
        &bytes[BYTE_ORDER_MARK.len()..]
    } else {
        bytes
    };
    let text = std::str::from_utf8(body).map_err(|_| LoadError::NotUtf8)?;
    let (newline, mixed_newlines) = detect_newline(text);
    let folded = newline != Newline::Lf || mixed_newlines;
    let text = if folded {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_owned()
    };
    // Counted after folding, because the limit is a statement about the
    // document and a CRLF file is not longer than the same document with LF.
    let characters = text.chars().count();
    if characters > limit {
        return Err(LoadError::TooLarge { characters, limit });
    }
    let form = TextForm {
        byte_order_mark,
        newline,
        mixed_newlines,
    };
    Ok((text, form))
}

/// Read a file into the shape the editor holds it in.
pub fn read(path: &Path, limit: usize) -> Result<LoadedFile, LoadError> {
    let bytes = fs::read(path)?;
    // Taken after the read rather than before, so that a file written while it
    // was being read leaves a stamp that does not match what was loaded, and
    // the watcher says so instead of missing it (要件 8.3).
    let stamp = FileStamp::read(path)?;
    let (text, form) = decode(&bytes, limit)?;
    Ok(LoadedFile { text, form, stamp })
}

/// The text as the file should hold it.
pub fn encode(text: &str, form: TextForm) -> Vec<u8> {
    let capacity = text.len() + BYTE_ORDER_MARK.len();
    let mut bytes = Vec::with_capacity(capacity);
    if form.byte_order_mark {
        bytes.extend_from_slice(&BYTE_ORDER_MARK);
    }
    if form.newline == Newline::Lf {
        bytes.extend_from_slice(text.as_bytes());
        return bytes;
    }
    let break_bytes = form.newline.as_str().as_bytes();
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            bytes.extend_from_slice(break_bytes);
        }
        bytes.extend_from_slice(line.as_bytes());
    }
    bytes
}

/// Where a save puts its bytes before it renames them onto the target.
///
/// Beside the target rather than in a temporary directory, because a rename is
/// only atomic within one volume. This is the one file the editor writes
/// inside the working folder, and it lives for the length of one save; 要件
/// 5.1 forbids leaving management files there, not writing the file 8.2 asks
/// for.
///
/// The serial keeps two saves of the same file from meeting. It is per
/// process, which is as far as this needs to reach: the case it guards is this
/// editor saving twice, not two editors saving at once.
fn temporary_path(path: &Path) -> PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().and_then(OsStr::to_str);
    let name = name.unwrap_or("save");
    let temporary = format!(".{name}.{serial}.{TEMPORARY_SUFFIX}");
    path.with_file_name(temporary)
}

/// Everything that has to reach the device before the rename.
///
/// `sync_all` and not just a flush: the point of the temporary file is that
/// the target is only replaced once the new contents exist, and buffered
/// contents do not exist.
fn write_all_to(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Write `bytes` to `path` without ever leaving it half written.
///
/// The whole point is the order. If any step fails, the temporary file goes
/// and the original is untouched.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<FileStamp> {
    let temporary = temporary_path(path);
    if let Err(error) = write_all_to(&temporary, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    FileStamp::read(path)
}

/// Save a document, putting the file back into the shape it arrived in.
///
/// Returns the stamp of what was written, which becomes the new baseline the
/// watcher compares against (要件 8.2).
pub fn save(path: &Path, text: &str, form: TextForm) -> io::Result<FileStamp> {
    write_atomically(path, &encode(text, form))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 1_000_000;

    fn form_of(bytes: &[u8]) -> TextForm {
        decode(bytes, LIMIT).expect("decodes").1
    }

    fn text_of(bytes: &[u8]) -> String {
        decode(bytes, LIMIT).expect("decodes").0
    }

    #[test]
    fn reads_line_feeds_unchanged() {
        assert_eq!(text_of(b"a\nb\n"), "a\nb\n");
        assert_eq!(form_of(b"a\nb\n").newline, Newline::Lf);
        assert!(!form_of(b"a\nb\n").mixed_newlines);
    }

    #[test]
    fn folds_carriage_return_line_feed() {
        assert_eq!(text_of(b"a\r\nb\r\n"), "a\nb\n");
        assert_eq!(form_of(b"a\r\nb\r\n").newline, Newline::Crlf);
    }

    #[test]
    fn folds_a_lone_carriage_return() {
        assert_eq!(text_of(b"a\rb\r"), "a\nb\n");
        assert_eq!(form_of(b"a\rb\r").newline, Newline::Cr);
    }

    /// The case that makes detection a single pass rather than three searches.
    #[test]
    fn a_crlf_file_is_not_mixed() {
        assert!(!form_of(b"a\r\nb\r\nc").mixed_newlines);
    }

    #[test]
    fn notices_mixed_line_breaks() {
        let form = form_of(b"a\r\nb\nc");
        assert_eq!(form.newline, Newline::Crlf);
        assert!(form.mixed_newlines);
    }

    #[test]
    fn a_file_without_line_breaks_takes_the_default() {
        let form = form_of("段落".as_bytes());
        assert_eq!(form.newline, Newline::Lf);
        assert!(!form.mixed_newlines);
    }

    #[test]
    fn takes_the_byte_order_mark_off_the_text() {
        let mut bytes = BYTE_ORDER_MARK.to_vec();
        bytes.extend_from_slice("本文".as_bytes());
        assert_eq!(text_of(&bytes), "本文");
        assert!(form_of(&bytes).byte_order_mark);
    }

    #[test]
    fn refuses_bytes_that_are_not_utf8() {
        let error = decode(&[0x82, 0xA0], LIMIT).expect_err("refuses");
        assert!(matches!(error, LoadError::NotUtf8));
    }

    #[test]
    fn refuses_a_file_over_the_limit() {
        let bytes = "あ".repeat(5);
        let error = decode(bytes.as_bytes(), 4).expect_err("refuses");
        match error {
            LoadError::TooLarge { characters, limit } => {
                assert_eq!(characters, 5);
                assert_eq!(limit, 4);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    /// The limit describes the document, so it must not move with the encoding
    /// of the file the document came from.
    #[test]
    fn the_limit_counts_the_folded_text() {
        assert!(decode(b"a\r\nb\r\nc", 5).is_ok());
    }

    #[test]
    fn a_document_at_the_limit_is_accepted() {
        let bytes = "あ".repeat(4);
        assert!(decode(bytes.as_bytes(), 4).is_ok());
    }

    #[test]
    fn writes_back_the_line_break_the_file_had() {
        let form = TextForm {
            byte_order_mark: false,
            newline: Newline::Crlf,
            mixed_newlines: false,
        };
        assert_eq!(encode("a\nb\n", form), b"a\r\nb\r\n");
    }

    #[test]
    fn writes_back_the_byte_order_mark() {
        let form = TextForm {
            byte_order_mark: true,
            newline: Newline::Lf,
            mixed_newlines: false,
        };
        let bytes = encode("本文", form);
        assert!(bytes.starts_with(&BYTE_ORDER_MARK));
        let body = &bytes[BYTE_ORDER_MARK.len()..];
        assert_eq!(body, "本文".as_bytes());
    }

    #[test]
    fn a_new_document_is_written_as_plain_utf8() {
        assert_eq!(encode("a\nb", TextForm::default()), b"a\nb");
    }

    /// Reading a file and saving it without editing must produce the same
    /// bytes. This is 要件 8.2「不用意に変更しない」in practice.
    #[test]
    fn decoding_and_encoding_round_trips() {
        let mut with_mark = BYTE_ORDER_MARK.to_vec();
        with_mark.extend_from_slice("本文\r\n".as_bytes());
        let files: [&[u8]; 5] = [
            b"a\nb\n",
            b"a\r\nb\r\n",
            b"a\rb\r",
            "見出し\r\n本文\r\n".as_bytes(),
            &with_mark,
        ];
        for original in files {
            let (text, form) = decode(original, LIMIT).expect("decodes");
            assert_eq!(encode(&text, form), original, "{form:?}");
        }
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let temporary = std::env::temp_dir();
        let directory = temporary.join(format!("rfnedit-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    #[test]
    fn saves_and_reads_a_file_back() {
        let directory = scratch_directory("round-trip");
        let path = directory.join("note.md");
        let form = TextForm {
            byte_order_mark: false,
            newline: Newline::Crlf,
            mixed_newlines: false,
        };
        let stamp = save(&path, "見出し\n本文\n", form).expect("saves");
        let loaded = read(&path, LIMIT).expect("reads");
        assert_eq!(loaded.text, "見出し\n本文\n");
        assert_eq!(loaded.form.newline, Newline::Crlf);
        assert_eq!(loaded.stamp.length, stamp.length);
        let _ = fs::remove_dir_all(&directory);
    }

    /// The reason saving goes through a temporary file at all.
    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        let directory = scratch_directory("no-leftovers");
        let path = directory.join("note.md");
        save(&path, "一度目", TextForm::default()).expect("saves");
        save(&path, "二度目", TextForm::default()).expect("saves");
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(&directory).expect("lists") {
            let name = entry.expect("entry").file_name();
            names.push(name.to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["note.md".to_owned()]);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn saving_over_a_file_replaces_its_contents() {
        let directory = scratch_directory("replace");
        let path = directory.join("note.md");
        let long = "長いほうの本文です";
        save(&path, long, TextForm::default()).expect("saves");
        save(&path, "短い", TextForm::default()).expect("saves");
        let loaded = read(&path, LIMIT).expect("reads");
        assert_eq!(loaded.text, "短い");
        let _ = fs::remove_dir_all(&directory);
    }

    /// The stamp has to move when the file does, or the watcher never fires.
    #[test]
    fn the_stamp_changes_when_the_length_does() {
        let directory = scratch_directory("stamp");
        let path = directory.join("note.md");
        let first = save(&path, "あ", TextForm::default()).expect("saves");
        let second = save(&path, "ああ", TextForm::default()).expect("saves");
        assert_ne!(first.length, second.length);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reading_a_missing_file_reports_io() {
        let directory = scratch_directory("missing");
        let path = directory.join("nothing.md");
        let error = read(&path, LIMIT).expect_err("fails");
        assert!(matches!(error, LoadError::Io(_)));
        let _ = fs::remove_dir_all(&directory);
    }
}
