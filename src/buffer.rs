//! Which file a document is, and what it takes to put it back.
//!
//! The text itself lives in the editor's shared buffer. This holds everything
//! about the document that is not its characters: where it came from, the
//! shape the file was in, and what the file was the last time the two agreed.
//! Keeping them apart is what lets several panes show the same text while only
//! one thing decides what saving it means (要件 7.6).
//!
//! Nothing here touches Windows or Slint.

use std::path::{Path, PathBuf};

use crate::file_io::{self, Encoding, FileStamp, LoadError, SaveError, TextForm};

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
    /// **書き手が言った文字コード**（要件 E2 の②、書き手のレビュー 2026-09-11）。
    ///
    /// 自動判別では文字化けする原稿を「指定文字コードで開き直す」で直したのなら、
    /// **その読み方は読み直しでも引き継ぐ**——外のアプリが触った拍子に判別へ戻ると、
    /// 化けがそのまま帰ってくる（書き手の指摘：「BOMなしUTF-16などで文字化けが戻り、
    /// その後の保存形式まで変わる可能性があります」）。
    ///
    /// **書いたときも分かっている。**保存した文書は、書いた文字コードのものである。
    said: Option<Encoding>,
}

impl DocumentFile {
    /// A document that has never been saved.
    pub fn untitled(number: u32) -> Self {
        Self {
            origin: Origin::Untitled(number),
            mixed_newlines: false,
            reported: None,
            said: None,
        }
    }

    /// Read a file and take it as the document's own.
    ///
    /// The text comes back separately because it belongs to the editor's
    /// shared buffer, not to this.
    pub fn open(path: &Path, limit: usize) -> Result<(Self, String), LoadError> {
        Self::taken(path, file_io::read(path, limit)?)
    }

    /// 同じことを、**文字コードを言われて**する（要件 E2 の②）。
    ///
    /// **読めたときだけ文書になる**——`Err`のときここは何も作らないので、
    /// 呼ぶ側が持っている本文は1字も動かない。
    pub fn open_as(
        path: &Path,
        limit: usize,
        encoding: Encoding,
    ) -> Result<(Self, String), LoadError> {
        Self::taken(path, file_io::read_as(path, limit, encoding)?)
    }

    /// 読めたファイルを、この文書の出どころとして受け取る。
    fn taken(path: &Path, loaded: file_io::LoadedFile) -> Result<(Self, String), LoadError> {
        let saved = SavedFile {
            path: path.to_path_buf(),
            form: loaded.form,
            stamp: loaded.stamp,
        };
        let document = Self {
            origin: Origin::Saved(saved),
            mixed_newlines: loaded.form.mixed_newlines,
            reported: None,
            said: None,
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
    ///
    /// **画面に出すのもこれ**（要件 E2）——文字コードと改行は「この文書が何で
    /// 書かれているか」であって、ステータスバーはそれを言う。
    pub fn form(&self) -> TextForm {
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
    ///
    /// **書く形も言われる**（要件 E2 の③）。ふだんはこの文書がいま持っている形
    /// （`form()`）がそのまま渡ってくるが、「この文字コードで保存する」を選んだ
    /// ときは違う形が来る——**書けた時点から、この文書はその形のもの**である。
    /// **断られたときは何も変えない**：形も、ファイルも。
    pub fn save_to_as(
        &mut self,
        path: PathBuf,
        text: &str,
        form: TextForm,
    ) -> Result<(), SaveError> {
        let stamp = file_io::save(&path, text, form)?;
        self.took(path, form, stamp);
        Ok(())
    }

    /// 書けたファイルを、この文書の出どころとして受け取る。
    ///
    /// **書いた時点で、そのファイルの改行は1つ**（要件 E2 の⑤）。本文が持って
    /// いるのは`\n`だけで、`file_io::encode`はそれを`form.newline`に揃えて書く
    /// ——**混ざっていたのは読んだファイルのほうで、いま書いたファイルではない。**
    /// ここで畳まないと、帯が`（混在）`と言い続ける（③の「書けた時点から、その
    /// 文書はその形のもの」と同じ一文である）。
    fn took(&mut self, path: PathBuf, form: TextForm, stamp: FileStamp) {
        let form = TextForm {
            mixed_newlines: false,
            ..form
        };
        self.origin = Origin::Saved(SavedFile { path, form, stamp });
        // What the editor just wrote is not an outside change.
        self.reported = None;
        // **書いた形は分かっている**（書き手のレビュー 2026-09-11）。判別に任せて
        // 読み直すと、ここで書いたはずの文字コードが別のものに読まれうる。
        self.said = Some(form.encoding);
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
        let read = self.read_again(limit, None)?;
        let Some(said) = self.said else {
            return Some(read);
        };
        let Ok(text) = read else {
            return Some(read);
        };
        // **印（BOM）があれば、そちらが正。**印はファイル自身が「この文字コードで
        // ある」と言っているもので、外のアプリが別の形で書き直したのなら、
        // 言われた読み方はもう古い。
        if self.form().byte_order_mark {
            return Some(Ok(text));
        }
        // **言われた読み方で読めなければ、判別のままにする。**読み直しは、失敗して
        // も何も失わない操作である（②と同じ決まり）。
        match self.read_again(limit, Some(said)) {
            Some(Ok(again)) => Some(Ok(again)),
            _ => Some(Ok(text)),
        }
    }

    /// 言われた文字コードで読み直す（要件 E2 の②）。
    ///
    /// **読めたときだけ入れ替わる。**CP932の原稿をUTF-8で開き直そうとすれば
    /// `Err`が返り、文書は読めていたときのままである——**開き直しは、失敗して
    /// も何も失わない操作**でなければならない（要件 E2：「文字化けした内容を
    /// 確定しない」）。
    pub fn reopen_as(
        &mut self,
        limit: usize,
        encoding: Encoding,
    ) -> Option<Result<String, LoadError>> {
        self.read_again(limit, Some(encoding))
    }

    fn read_again(
        &mut self,
        limit: usize,
        encoding: Option<Encoding>,
    ) -> Option<Result<String, LoadError>> {
        let path = self.path()?.to_path_buf();
        let read = match encoding {
            Some(encoding) => Self::open_as(&path, limit, encoding),
            None => Self::open(&path, limit),
        };
        Some(match read {
            Ok((mut reopened, text)) => {
                // **言われた読み方は持ち越す**（`said`）。読み直しは同じ文書の
                // 続きであって、別の文書を開いたのではない。
                reopened.said = encoding.or(self.said);
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

    /// いまの文書の形のまま書く（本体では`write_document_in`がその形を渡す）。
    fn save_to(document: &mut DocumentFile, path: PathBuf, text: &str) -> Result<(), SaveError> {
        let form = document.form();
        document.save_to_as(path, text, form)
    }

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
        save_to(&mut document, path.clone(), "本文").expect("saves");
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
        save_to(&mut document, path.clone(), &text).expect("saves");
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
        save_to(&mut document, second.clone(), &text).expect("saves");
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
        save_to(&mut document, path, "本文").expect("saves");
        assert_eq!(document.external_change(), ExternalChange::None);
        let _ = fs::remove_dir_all(&directory);
    }

    /// The case 要件 8.2 asks about before it overwrites anything.
    #[test]
    fn another_program_writing_the_file_is_noticed() {
        let directory = scratch_directory("outside-write");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        save_to(&mut document, path.clone(), "本文").expect("saves");
        fs::write(&path, "別のアプリが書いた本文").expect("writes");
        let change = document.external_change();
        assert_eq!(change, ExternalChange::Modified);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn comparing_a_clone_does_not_acknowledge_or_overwrite_external_changes() {
        let directory = scratch_directory("comparison");
        let path = directory.join("note.md");
        fs::write(&path, "original").unwrap();
        let (document, _) = DocumentFile::open(&path, LIMIT).unwrap();
        let agreed = document.agreed_stamp();
        fs::write(&path, "external version").unwrap();
        let mut comparison = document.clone();
        assert_eq!(
            comparison.reload(LIMIT).unwrap().unwrap(),
            "external version"
        );
        assert_eq!(document.agreed_stamp(), agreed);
        assert_eq!(document.external_change(), ExternalChange::Modified);
        assert_eq!(fs::read_to_string(&path).unwrap(), "external version");
        fs::remove_file(&path).unwrap();
        assert!(comparison.reload(LIMIT).unwrap().is_err());
        assert_eq!(document.agreed_stamp(), agreed);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_deleted_file_is_reported_as_missing() {
        let directory = scratch_directory("deleted");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        save_to(&mut document, path.clone(), "本文").expect("saves");
        fs::remove_file(&path).expect("removes");
        assert_eq!(document.external_change(), ExternalChange::Missing);
        let _ = fs::remove_dir_all(&directory);
    }

    /// 書き手のレビュー 2026-09-11（P2）: **言われた文字コードは、読み直しても
    /// 引き継ぐ。**自動判別では化ける原稿を②で直したのに、外のアプリが触った拍子に
    /// 判別へ戻ったら、化けがそのまま帰ってくる。
    #[test]
    fn what_the_writer_said_survives_a_reload() {
        let directory = scratch_directory("said-encoding");
        let path = directory.join("note.txt");
        // **印の無いUTF-16 LE**——判別は英字だけなら当てられるが、ここでは
        // CP932として読めてしまうバイト列を置く。
        let said = "日本語の原稿";
        let bytes: Vec<u8> = said.encode_utf16().flat_map(u16::to_le_bytes).collect();
        fs::write(&path, &bytes).expect("writes");

        let (mut document, text) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert_ne!(
            text, said,
            "判別では当たらない（当たるならこの試験は無意味）"
        );

        // 書き手が「これはUTF-16 LEだ」と言った。
        let text = document
            .reopen_as(LIMIT, Encoding::Utf16Le)
            .expect("has a file")
            .expect("reads");
        assert_eq!(text, said);

        // 外のアプリが書き直した——**言われた読み方のまま読む。**
        let again = "書き足した日本語";
        let bytes: Vec<u8> = again.encode_utf16().flat_map(u16::to_le_bytes).collect();
        fs::write(&path, &bytes).expect("writes");
        let text = document.reload(LIMIT).expect("has a file").expect("reads");
        assert_eq!(text, again);

        // **印があれば、そちらが正**——外のアプリがUTF-8 BOMで書き直したのなら、
        // 言われた読み方はもう古い。
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice("日本語".as_bytes());
        fs::write(&path, &utf8).expect("writes");
        let text = document.reload(LIMIT).expect("has a file").expect("reads");
        assert_eq!(text, "日本語");

        let _ = fs::remove_dir_all(&directory);
    }

    /// A file being written over and over must be reported once per change.
    #[test]
    fn an_outside_change_is_reported_once() {
        let directory = scratch_directory("report-once");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        save_to(&mut document, path.clone(), "本文").expect("saves");
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
        save_to(&mut document, path.clone(), "一").expect("saves");
        fs::write(&path, "外から").expect("writes");
        let stamp = document.current_stamp().expect("has a stamp");
        assert!(document.take_report(stamp));
        save_to(&mut document, path.clone(), "二").expect("saves again");
        let stamp = document.current_stamp().expect("has a stamp");
        assert!(document.take_report(stamp));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reloading_takes_what_the_file_now_holds() {
        let directory = scratch_directory("reload");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        save_to(&mut document, path.clone(), "はじめ").expect("saves");
        fs::write(&path, "あと\r\nから").expect("writes");
        let text = document.reload(LIMIT).expect("has a file").expect("reads");
        assert_eq!(text, "あと\nから");
        // The shape came with it, so saving writes CRLF back.
        assert_eq!(document.form().newline, Newline::Crlf);
        // And the reload is the new baseline, so nothing is outstanding.
        assert_eq!(document.external_change(), ExternalChange::None);
        let _ = fs::remove_dir_all(&directory);
    }

    /// E2の③: **書けたら、その形がこの文書のものになる。**次の`Ctrl+S`も
    /// ステータスバーも、そこからはその文字コードで言う。
    #[test]
    fn saving_in_a_named_form_makes_it_the_documents_own() {
        let directory = scratch_directory("save-in-form");
        let path = directory.join("note.txt");
        let (mut document, _) = (DocumentFile::untitled(1), ());

        let form = TextForm {
            encoding: Encoding::Cp932,
            newline: Newline::Crlf,
            ..TextForm::default()
        };
        document
            .save_to_as(path.clone(), "春の海", form)
            .expect("書ける");

        assert_eq!(document.form().encoding, Encoding::Cp932);
        assert_eq!(document.form().newline, Newline::Crlf);
        // ファイルの側もそうなっている（判別が同じ答えを出す）。
        let loaded = crate::file_io::read(&path, LIMIT).expect("読める");
        assert_eq!(loaded.form.encoding, Encoding::Cp932);
        assert_eq!(loaded.text, "春の海");

        let _ = fs::remove_dir_all(&directory);
    }

    /// E2の③: **断られたら、形も変わらない。**「CP932で保存する」を選んで
    /// 断られた文書がCP932のものになっていたら、次の`Ctrl+S`が同じ断りを
    /// 繰り返すか、書き手の知らないうちに文字コードが変わっている。
    #[test]
    fn a_refused_save_leaves_the_form_where_it_was() {
        let directory = scratch_directory("save-as-refused");
        let path = directory.join("note.txt");
        let (mut document, _) = (DocumentFile::untitled(1), ());
        let before = document.form();

        let form = TextForm {
            encoding: Encoding::Cp932,
            ..TextForm::default()
        };
        let error = document
            .save_to_as(path.clone(), "猫は🐈です", form)
            .expect_err("断る");

        assert!(matches!(error, SaveError::Unmappable(_)));
        assert_eq!(document.form(), before);
        assert!(document.path().is_none(), "行き先も受け取っていない");
        assert!(!path.exists(), "ファイルも作られていない");

        let _ = fs::remove_dir_all(&directory);
    }

    /// E2の②: **言われた文字コードで読み直す。**
    #[test]
    fn reopening_takes_the_encoding_it_is_told() {
        let directory = scratch_directory("reopen-as");
        let path = directory.join("note.txt");
        let bytes = crate::code_page::encode(crate::code_page::CP932, "春の海").expect("書ける");
        fs::write(&path, &bytes).expect("writes");

        let (mut document, text) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert_eq!(text, "春の海");
        assert_eq!(document.form().encoding, Encoding::Cp932);

        // 同じバイト列をUTF-16 LEだと言えば、そう読む（読めた字は別物である）。
        let text = document
            .reopen_as(LIMIT, Encoding::Utf16Le)
            .expect("ファイルがある")
            .expect("読める");
        assert_eq!(document.form().encoding, Encoding::Utf16Le);
        assert_ne!(text, "春の海");

        let _ = fs::remove_dir_all(&directory);
    }

    /// E2の②: **読めなかったときは、文書が動かない。**開き直しは失敗しても
    /// 何も失わない操作でなければならない——ここが入れ替わってしまうと、
    /// 呼ぶ側は「読めていたときの形」を二度と言えなくなる。
    #[test]
    fn a_reopen_that_cannot_read_leaves_the_document_alone() {
        let directory = scratch_directory("reopen-refuses");
        let path = directory.join("note.txt");
        let bytes = crate::code_page::encode(crate::code_page::CP932, "日本語").expect("書ける");
        fs::write(&path, &bytes).expect("writes");

        let (mut document, _) = DocumentFile::open(&path, LIMIT).expect("opens");
        let before = document.form();

        let error = document
            .reopen_as(LIMIT, Encoding::Utf8)
            .expect("ファイルがある")
            .expect_err("断る");

        assert!(matches!(error, LoadError::Unreadable));
        assert_eq!(document.form(), before);
        assert_eq!(document.path(), Some(path.as_path()));

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

    /// E2の⑤: **書いた時点で、そのファイルの改行は1つ。**混ざっていたのは
    /// 読んだファイルのほうで、いま書いたファイルではない——ここで畳まないと、
    /// 帯が`（混在）`と言い続ける。
    #[test]
    fn writing_a_file_leaves_its_breaks_no_longer_mixed() {
        let directory = scratch_directory("mixed-saved");
        let path = directory.join("note.md");
        fs::write(&path, b"a\r\nb\nc").expect("writes");
        let (mut document, text) = DocumentFile::open(&path, LIMIT).expect("opens");
        assert!(document.form().mixed_newlines);

        save_to(&mut document, path.clone(), &text).expect("saves");
        assert!(!document.form().mixed_newlines);
        // 揃えた先は、読んだときの1つ目（要件 E2：元の形式を維持する）。
        assert_eq!(document.form().newline, Newline::Crlf);
        assert_eq!(fs::read(&path).expect("reads"), b"a\r\nb\r\nc");

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_new_document_is_saved_as_plain_utf8_with_line_feeds() {
        let directory = scratch_directory("default-form");
        let path = directory.join("note.md");
        let mut document = DocumentFile::untitled(1);
        save_to(&mut document, path, "a\nb").expect("saves");
        assert_eq!(document.form().newline, Newline::Lf);
        assert!(!document.form().byte_order_mark);
        let _ = fs::remove_dir_all(&directory);
    }
}
