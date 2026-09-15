//! Turning a file into the text the engine holds, and back again.
//!
//! Windowsに触るのは文字コードの変換だけで、それは`code_page`にある（要件 E2）。
//! ここにあるのは「何で読むか、読めなければどうするか」の判断で、表そのものは
//! Windowsが持っている。DirectWriteにもSlintにも触らない。A document is a single
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

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// UTF-8 byte order mark.
const BYTE_ORDER_MARK: [u8; 3] = [0xEF, 0xBB, 0xBF];
/// UTF-16の印。**リトルエンディアンが先**——Windowsの「Unicode」はこれである。
const UTF16_LE_MARK: [u8; 2] = [0xFF, 0xFE];
const UTF16_BE_MARK: [u8; 2] = [0xFE, 0xFF];

/// ファイルが使っていた文字コード（要件 E2）。
///
/// **初期版が受け持つのはこの4つ**（要件 E2）——UTF-8（BOMの有無は[`TextForm`]が
/// 別に持つ）、UTF-16のLEとBE、そしてWindowsの日本語CP932。**判別の順は
/// [`decode`]にあり、ここは「何だったか」を言うだけの型である。**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
    /// Windowsの日本語（Shift_JISの拡張）。**日本語のWindowsで書かれた古い原稿は
    /// たいていこれ**で、この編集器が読めなければ書き手はまず別の道具を開くことになる。
    Cp932,
}

impl Encoding {
    /// ステータスバーに出す名前（要件 E2）。
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Cp932 => "CP932",
        }
    }
}

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
    /// 読んだときの文字コード（要件 E2）。**保存はこれで書き戻す**——無指定の
    /// 保存が元の形式を保つ、というのがE2の完了の目安である。
    pub encoding: Encoding,
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
    /// どの文字コードでも読めなかった（要件 E2）。
    ///
    /// **推し量らずに断る。**Shift_JISのファイルをUTF-8として読めば、見た目が
    /// 壊れるだけでなく、保存した瞬間に本当に壊れる——この編集器の仕事は、
    /// 渡されたものを失わないことである。
    ///
    /// 2026-09-10（E2）まではUTF-8だけを試していた。いまはUTF-16（印つき）と
    /// CP932も試すので、ここへ来るのは**本当にどれでもないもの**である。
    Unreadable,
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
            LoadError::Unreadable => formatter.write_str(&crate::say!(
                "UTF-8・UTF-16・CP932のどれとしても読めないファイルです",
                "The file cannot be read as UTF-8, UTF-16 or CP932"
            )),
            LoadError::TooLarge { characters, limit } => formatter.write_str(&crate::say!(
                "{characters}文字のファイルは、上限{limit}文字を超えるため開けません",
                "Cannot open a file of {characters} characters: the limit is {limit}"
            )),
            LoadError::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for LoadError {
    fn from(error: io::Error) -> Self {
        LoadError::Io(error)
    }
}

/// 保存が断られた理由（要件 E2 の③）。
///
/// **`io::Error`に畳まない。**「表せない字がある」は書き手が選び直せる断りで
/// あって、書けなかったという事故ではない——文字列にしてしまうと、呼ぶ側は
/// **どの字か**を画面の選択肢にできない。
#[derive(Debug)]
pub enum SaveError {
    /// この文字コードで表せない、**最初の**字。
    ///
    /// **数えない**（書き手の判断 2026-09-10）。3万字のうち1000字が入らないとき、
    /// その1000という数は書き手の役に立たない——答えは「この文字コードでは
    /// 保存できない」で、次にすることはUTF-8で保存することである。
    ///
    /// **まだ1バイトも書いていない。**ファイルは元のままである。
    Unmappable(char),
    Io(io::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Unmappable(character) => formatter.write_str(&crate::say!(
                "表せない字があります（{character}）",
                "Some characters cannot be written ({character})"
            )),
            SaveError::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(error: io::Error) -> Self {
        SaveError::Io(error)
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
    let (text, encoding, byte_order_mark) = read_bytes(bytes)?;
    fold(text, encoding, byte_order_mark, limit)
}

/// 同じことを、**文字コードを言われて**する（要件 E2 の②）。
///
/// **判別を通らない**のが違いの全部で、折り返しも上限もBOMの扱いも[`decode`]と
/// 同じ道である——「開き直した文書」と「開いた文書」が別の作りになれば、
/// 保存で違いが出る。
///
/// **印の無いUTF-16も、言われれば読む。**[`read_bytes`]が自分では選ばないのは
/// 向きを決める当てが無いからで、書き手が「LEだ」と言ったのならその当ては
/// もうある——②はそのための操作である。
pub fn decode_as(
    bytes: &[u8],
    limit: usize,
    encoding: Encoding,
) -> Result<(String, TextForm), LoadError> {
    let (text, byte_order_mark) = read_bytes_as(bytes, encoding)?;
    fold(text, encoding, byte_order_mark, limit)
}

/// 読めた字を、この編集器が持つ形へ（改行は`\n`ひとつ、上限は文書に対する数）。
fn fold(
    text: String,
    encoding: Encoding,
    byte_order_mark: bool,
    limit: usize,
) -> Result<(String, TextForm), LoadError> {
    let text = text.as_str();
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
        encoding,
        byte_order_mark,
        newline,
        mixed_newlines,
    };
    Ok((text, form))
}

/// どの文字コードで読むかを決めて、読む（要件 E2）。
///
/// **順番が規則である。**
///
/// 1. **印があれば、それが答え。**UTF-16のLE／BEとUTF-8のBOMは、書いた側が
///    「これで読め」と言い残したものである。
/// 2. **UTF-8として読めれば、UTF-8。**いまの原稿はたいていこれで、しかも
///    「たまたまUTF-8に見える」並びは短い文以外ではまず起きない。
/// 3. **それからCP932。**日本語のWindowsで書かれた古い原稿はこれで、読めなければ
///    書き手はまず別の道具を開くことになる。
/// 4. **それでも読めなければ、印の無いUTF-16 LEとして推定する**（要件 E2 の④、
///    書き手の判断 2026-09-10）。**LEが先**——Windowsの「Unicode」はこれである。
/// 5. その読みにも`NUL`が並ぶなら、**それは平文ではない**（[`LoadError::Unreadable`]）。
///
/// **推定して開き、違っていたら書き手が読み直す**（書き手の判断 2026-09-10：
/// 「推定で開いて、おかしければユーザーが読み直すのでいい……少なくとも、私は普段
/// そうしています」）。**読み直す道はもうある**——ステータスバーの帯を押せば
/// `この文字コードで開き直す`が開く（②）。ここで問いを立てるのは、その道の手前に
/// 関所をもう1つ作ることでしかない。
///
/// **要件 E2 の「文字化けした内容を確定しない」は、保存が守っている。**開くことは
/// 確定ではない——ファイルは1バイトも動かず、帯は何として読んだかを言い、選び直せる。
/// 確定するのは書くときで、そこは③が断る。
fn read_bytes(bytes: &[u8]) -> Result<(String, Encoding, bool), LoadError> {
    if let Some(body) = bytes.strip_prefix(&UTF16_LE_MARK) {
        return Ok((utf16(body, false), Encoding::Utf16Le, true));
    }
    if let Some(body) = bytes.strip_prefix(&UTF16_BE_MARK) {
        return Ok((utf16(body, true), Encoding::Utf16Be, true));
    }
    if let Some(body) = bytes.strip_prefix(&BYTE_ORDER_MARK) {
        let text = std::str::from_utf8(body).map_err(|_| LoadError::Unreadable)?;
        return Ok((text.to_owned(), Encoding::Utf8, true));
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && !holds_nul(text)
    {
        return Ok((text.to_owned(), Encoding::Utf8, false));
    }
    if let Some(text) = crate::code_page::decode(crate::code_page::CP932, bytes)
        && !holds_nul(&text)
    {
        return Ok((text, Encoding::Cp932, false));
    }
    let guessed = utf16(bytes, false);
    if holds_nul(&guessed) {
        return Err(LoadError::Unreadable);
    }
    Ok((guessed, Encoding::Utf16Le, false))
}

/// 読めた本文に`NUL`が混ざっているか（要件 E2 の④、書き手の判断 2026-09-10）。
///
/// **平文はこれを持たない。**印の無いUTF-16のファイルは、改行かASCIIが1字でも
/// あれば必ず`NUL`を持つ——`春の海\n`をUTF-16 LEで書いた
/// `25 66 6E 30 77 6D 0A 00`は**UTF-8として正しく読めてしまう**（`%fn0wm·`）ので、
/// これが無ければ判別は文字化けを黙って確定する。
///
/// **この1つの規則が、判別の残り全部を決めている。**`NUL`が並ぶ読みを飛ばすから
/// 印の無いUTF-16まで落ちてこられるし、**その推定にも`NUL`が並ぶなら平文ではない**
/// ——実行ファイルや画像は`00 00`を必ず持つので、そこで断られる。推定して開く道と、
/// 開かない道を分けているのはここだけである。
///
/// **これは判別だけの規則である。**[`read_bytes_as`]は見ない——書き手が
/// 「CP932だ」と言ったのなら、`NUL`が並ぶのはその問いへの答えであって、
/// 編集器が代わりに考え直すところではない（②の決めごと）。
fn holds_nul(text: &str) -> bool {
    text.contains('\0')
}

/// 言われた文字コードで読む（要件 E2 の②）。
///
/// **印は、あれば脱がせる。**BOMは字ではなく「この向きで読め」という言い残し
/// なので、本文へ混ぜるとファイルの先頭に見えない字が1つ増える——UTF-8の
/// `EF BB BF`とUTF-16の`FF FE`／`FE FF`は、その文字コードのものだけを外す。
/// **印が無くても読む**：あるかどうかは[`TextForm`]が覚え、保存で書き戻す。
///
/// **CP932は印を持たない**ので、先頭の3バイトも字として読もうとする——UTF-8の
/// BOM（`EF BB BF`）はCP932の表に無い並びなので、そこで断られる。断られたほうが
/// 良い：読めない字を`?`にして開けば、書き手はBOMがあったことを二度と知れない。
///
/// 読めなければ[`LoadError::Unreadable`]で、**本文には触らない**（呼ぶ側は
/// 元の文書をそのまま持っている）。
fn read_bytes_as(bytes: &[u8], encoding: Encoding) -> Result<(String, bool), LoadError> {
    match encoding {
        Encoding::Utf8 => {
            let (body, mark) = match bytes.strip_prefix(&BYTE_ORDER_MARK) {
                Some(body) => (body, true),
                None => (bytes, false),
            };
            let text = std::str::from_utf8(body).map_err(|_| LoadError::Unreadable)?;
            Ok((text.to_owned(), mark))
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big_endian = encoding == Encoding::Utf16Be;
            let own = if big_endian {
                UTF16_BE_MARK
            } else {
                UTF16_LE_MARK
            };
            let (body, mark) = match bytes.strip_prefix(&own) {
                Some(body) => (body, true),
                None => (bytes, false),
            };
            Ok((utf16(body, big_endian), mark))
        }
        Encoding::Cp932 => match crate::code_page::decode(crate::code_page::CP932, bytes) {
            Some(text) => Ok((text, false)),
            None => Err(LoadError::Unreadable),
        },
    }
}

/// UTF-16のバイト列を文字へ。
///
/// **奇数バイトで終わっていても読む。**最後の半端な1バイトは落とす——そこで断ると、
/// 書きかけで途切れたファイルが開けなくなる。対になっていないサロゲートは
/// `U+FFFD`になる（`from_utf16_lossy`）：**それは読めない字であって、
/// 読めないファイルではない。**
fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Read a file into the shape the editor holds it in.
pub fn read(path: &Path, limit: usize) -> Result<LoadedFile, LoadError> {
    read_with(path, limit, None)
}

/// 同じことを、**文字コードを言われて**する（要件 E2 の②）。
pub fn read_as(path: &Path, limit: usize, encoding: Encoding) -> Result<LoadedFile, LoadError> {
    read_with(path, limit, Some(encoding))
}

/// 判別に任せるか、言われた文字コードで読むか。**ファイルに触る道は1本**
/// ——印（`stamp`）を取る順番のような決めごとが2つに分かれない。
fn read_with(
    path: &Path,
    limit: usize,
    encoding: Option<Encoding>,
) -> Result<LoadedFile, LoadError> {
    let bytes = fs::read(path)?;
    // Taken after the read rather than before, so that a file written while it
    // was being read leaves a stamp that does not match what was loaded, and
    // the watcher says so instead of missing it (要件 8.3).
    let stamp = FileStamp::read(path)?;
    let (text, form) = match encoding {
        Some(encoding) => decode_as(&bytes, limit, encoding)?,
        None => decode(&bytes, limit)?,
    };
    Ok(LoadedFile { text, form, stamp })
}

/// The text as the file should hold it.
///
/// **表せない字があれば書かない**（要件 E2）。`?`に替えて保存するのは、書き手の
/// 原稿を編集器が黙って書き換えることである——返ってくるのは**入らなかった最初の字**で、
/// 呼ぶ側はそれを画面に出せる。UTF-8とUTF-16は何でも表せるので、この`Err`が
/// 起きるのはCP932だけである。
pub fn encode(text: &str, form: TextForm) -> Result<Vec<u8>, char> {
    let text = with_newlines(text, form.newline);
    let mut bytes = Vec::with_capacity(text.len() + BYTE_ORDER_MARK.len());
    match form.encoding {
        Encoding::Utf8 => {
            if form.byte_order_mark {
                bytes.extend_from_slice(&BYTE_ORDER_MARK);
            }
            bytes.extend_from_slice(text.as_bytes());
        }
        // **UTF-16は印を必ず書く。**印の無いUTF-16は読み手が向きを決められない
        // ——[`read_bytes`]がそれを読まないのと同じ理由である。
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big_endian = form.encoding == Encoding::Utf16Be;
            bytes.extend_from_slice(if big_endian {
                &UTF16_BE_MARK
            } else {
                &UTF16_LE_MARK
            });
            for unit in text.encode_utf16() {
                let pair = if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
        }
        Encoding::Cp932 => {
            bytes = crate::code_page::encode(crate::code_page::CP932, &text)?;
        }
    }
    Ok(bytes)
}

/// 改行を、そのファイルの書き方へ戻す。
fn with_newlines(text: &str, newline: Newline) -> Cow<'_, str> {
    if newline == Newline::Lf {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace('\n', newline.as_str()))
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
///
/// **その文字コードで表せない字があれば、1バイトも書かない**（要件 E2）。
/// 半分だけ書き換えたファイルを残さないのは`write_atomically`と同じ考え方で、
/// ここではさらに手前——**書き始める前に断る。**
pub fn save(path: &Path, text: &str, form: TextForm) -> Result<FileStamp, SaveError> {
    // **どの字かは呼ぶ側へ返す**（要件 E2 の③）。書き手が次にすることは
    // 「その字を直す」か「別の文字コードで保存する」か「無視して保存する」かで、
    // どれを選ぶにも原稿が壊れていないことが要る——ここではまだ1バイトも
    // 書いていない。
    let bytes = encode(text, form).map_err(SaveError::Unmappable)?;
    Ok(write_atomically(path, &bytes)?)
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

    /// E2（2026-09-10）: `82 A0`はCP932の「あ」なので、**もう読める**。
    /// 断るのは、どの文字コードでもないバイト列だけ。
    #[test]
    fn refuses_bytes_that_are_not_text_in_any_encoding() {
        let (text, form) = decode(&[0x82, 0xA0], LIMIT).expect("CP932として読める");
        assert_eq!(text, "あ");
        assert_eq!(form.encoding, Encoding::Cp932);

        // **平文でないものは、推定の先でも開かない**（要件 E2 の④）。PNGの頭は
        // `00 00`を持つので、UTF-16 LEとして読んでも`NUL`が並ぶ——実行ファイルも
        // 画像も、ここで断られる。
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        ];
        let error = decode(&png, LIMIT).expect_err("平文ではない");
        assert!(matches!(error, LoadError::Unreadable));
    }

    /// E2: 印のあるUTF-16は、その印が答え。**印の無いUTF-16は読まない**
    /// ——向きを決める当てが無いからである。
    #[test]
    fn a_byte_order_mark_says_which_utf16_it_is() {
        let mut le = UTF16_LE_MARK.to_vec();
        let mut be = UTF16_BE_MARK.to_vec();
        for unit in "春".encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
            be.extend_from_slice(&unit.to_be_bytes());
        }

        let (text, form) = decode(&le, LIMIT).expect("読める");
        assert_eq!((text.as_str(), form.encoding), ("春", Encoding::Utf16Le));
        let (text, form) = decode(&be, LIMIT).expect("読める");
        assert_eq!((text.as_str(), form.encoding), ("春", Encoding::Utf16Be));

        // 印を外すと、それはもうUTF-16として読まれない（この並びはCP932で読める）。
        let bare = &le[UTF16_LE_MARK.len()..];
        let (_, form) = decode(bare, LIMIT).expect("何かとしては読める");
        assert_ne!(form.encoding, Encoding::Utf16Le);
    }

    /// E2: **CP932はUTF-8のあと。**いまの原稿はたいていUTF-8で、日本語のWindowsで
    /// 書かれた古い原稿がCP932である。
    #[test]
    fn utf8_is_tried_before_cp932() {
        let (_, form) = decode("日本語".as_bytes(), LIMIT).expect("読める");
        assert_eq!(form.encoding, Encoding::Utf8);

        let bytes = crate::code_page::encode(crate::code_page::CP932, "日本語").expect("書ける");
        let (text, form) = decode(&bytes, LIMIT).expect("読める");
        assert_eq!((text.as_str(), form.encoding), ("日本語", Encoding::Cp932));
    }

    /// E2: 見本の一式が、書いたとおりの文字コードとして読める。
    ///
    /// **実際のファイルで確かめる。**バイト列を試験の中で組み立てるのと、
    /// 書き手が開くファイルを読むのは別のことである——この4つは
    /// `testdata/15〜18_文字コード_*.txt`で、画面で確かめるときにも同じものを開く。
    #[test]
    fn the_sample_files_read_as_what_they_were_written_as() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let samples = [
            ("15_文字コード_CP932.txt", Encoding::Cp932, Newline::Crlf),
            (
                "16_文字コード_UTF16LE.txt",
                Encoding::Utf16Le,
                Newline::Crlf,
            ),
            ("17_文字コード_UTF16BE.txt", Encoding::Utf16Be, Newline::Lf),
            ("18_文字コード_UTF8BOM.txt", Encoding::Utf8, Newline::Lf),
        ];
        for (name, encoding, newline) in samples {
            let loaded = read(&here.join(name), LIMIT).expect(name);
            assert_eq!(loaded.form.encoding, encoding, "{name}");
            assert_eq!(loaded.form.newline, newline, "{name}");
            assert!(loaded.text.contains("春の海"), "{name}");
            // 読んだ形で書き戻せば、同じバイト列になる。
            let written = encode(&loaded.text, loaded.form).expect("書ける");
            assert_eq!(
                written,
                fs::read(here.join(name)).expect("読める"),
                "{name}"
            );
        }
    }

    /// E2の⑤（書き手の報告 2026-09-10）: **改行が混ざった見本を、実際に読む。**
    ///
    /// 混在は①から画面に出ていたが、**testdataに混ざった見本が1つも無かった**
    /// ——`08_CRLFとBOM.md`は名前のとおりCRLFとBOMのファイルで、混ざってはいない
    /// （私はそれを確かめずに「これで見てください」と言った）。**見本が無ければ、
    /// 画面で確かめようがない。**
    #[test]
    fn the_mixed_sample_reads_as_mixed_and_keeps_the_first_kind() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let loaded = read(&here.join("21_改行の混在.md"), LIMIT).expect("見本がある");
        assert!(loaded.form.mixed_newlines);
        // **覚えるのは最初に見た1つ**（要件 E2：無指定の保存では元の形式を維持する）。
        assert_eq!(loaded.form.newline, Newline::Crlf);
        // 本文が持つ改行は`\n`ひとつだけ（技術検証 7.6）。
        assert!(!loaded.text.contains('\r'), "畳んである");
    }

    /// E2の④（書き手の判断 2026-09-10）: **推定で開く。**
    ///
    /// 「推定で開いて、おかしければユーザーが読み直すのでいい」——**読み直す道は
    /// もうある**（②の帯の一覧）。印の無いUTF-16はUTF-8でもCP932でもないので、
    /// 判別はそこまで落ちてきて**UTF-16 LEと見なす**。見本2つで確かめる：
    /// 日本語のほう（19番）はUTF-8として読めずに落ちてきて、英字のほう（20番）は
    /// **UTF-8として正しく読めてしまう**ので`NUL`の規則が要る。
    #[test]
    fn an_unmarked_utf16_file_is_guessed_rather_than_refused() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        for (name, opening) in [
            ("19_文字コード_UTF16LE印なし.txt", "# 文字コードの見本"),
            ("20_文字コード_UTF16LE印なし英字.txt", "# Encoding sample"),
        ] {
            let loaded = read(&here.join(name), LIMIT).expect(name);
            assert_eq!(loaded.form.encoding, Encoding::Utf16Le, "{name}");
            // 印は無かった、と覚えている（保存で書き戻さないために）。
            assert!(!loaded.form.byte_order_mark, "{name}");
            assert!(
                loaded.text.starts_with(opening),
                "{name}: {:?}",
                loaded.text
            );
        }
    }

    /// E2の④（書き手の判断 2026-09-10）: **`NUL`が混ざる読みは、その文字コードの
    /// 平文ではない。**
    ///
    /// `春の海\n`をUTF-16 LEで書いたバイト列は**UTF-8として正しく読める**
    /// （`%fn0wm·`）——この規則が無ければ、判別はそこで止まって文字化けを配る。
    /// **判別が採らないのと、読めないのは別**なので、言われれば読む（②）。
    #[test]
    fn a_reading_full_of_nul_bytes_is_not_taken_as_text() {
        let mut bare = Vec::new();
        for unit in "春の海\n".encode_utf16() {
            bare.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(
            std::str::from_utf8(&bare).is_ok(),
            "UTF-8としては読めてしまう"
        );

        let (text, form) = decode(&bare, LIMIT).expect("推定で読める");
        assert_eq!(text, "春の海\n");
        assert_eq!(form.encoding, Encoding::Utf16Le);

        // 言われればUTF-8としても読む——判別が採らないだけである。
        let (mojibake, _) = decode_as(&bare, LIMIT, Encoding::Utf8).expect("読める");
        assert!(mojibake.starts_with("%fn0wm"), "{mojibake:?}");
    }

    /// E2の②: **印の無いUTF-16も、言われれば読む。**判別が自分では選ばない
    /// のは向きを決める当てが無いからで、書き手が「LEだ」と言ったのならその
    /// 当てはもうある——それが「指定文字コードで開き直す」という操作である。
    #[test]
    fn a_named_encoding_reads_utf16_without_a_mark() {
        let mut bare = Vec::new();
        for unit in "春の海".encode_utf16() {
            bare.extend_from_slice(&unit.to_le_bytes());
        }

        // 判別に任せれば、これはUTF-16にならない。
        let (_, guessed) = decode(&bare, LIMIT).expect("何かとしては読める");
        assert_ne!(guessed.encoding, Encoding::Utf16Le);

        let (text, form) = decode_as(&bare, LIMIT, Encoding::Utf16Le).expect("読める");
        assert_eq!(text, "春の海");
        assert_eq!(form.encoding, Encoding::Utf16Le);
        // 印は無かった、と覚えている。
        assert!(!form.byte_order_mark);
    }

    /// E2の②: **自分の印は脱がせ、他人の印は字として読む。**
    ///
    /// 印は字ではなく「この向きで読め」という言い残しなので、その文字コードの
    /// ものだけを外す。**外さなかった印は本文の1字になる**——UTF-16 LEの
    /// ファイルをBEだと言って開けば、先頭に`U+FFFE`が立つ。それは間違いでは
    /// なく「BEの表ではそういう字だ」という答えで、書き手はそれを見て選び直せる
    /// （ファイルには触っていない）。
    #[test]
    fn a_named_encoding_only_takes_off_its_own_mark() {
        let mut little = UTF16_LE_MARK.to_vec();
        for unit in "本文".encode_utf16() {
            little.extend_from_slice(&unit.to_le_bytes());
        }

        let (text, form) = decode_as(&little, LIMIT, Encoding::Utf16Le).expect("読める");
        assert_eq!(text, "本文");
        assert!(form.byte_order_mark);

        let (text, form) = decode_as(&little, LIMIT, Encoding::Utf16Be).expect("読める");
        assert!(text.starts_with('\u{FFFE}'), "{text:?}");
        assert!(!form.byte_order_mark);

        // UTF-8の印も、UTF-8だと言われたときだけ外れる。
        let mut utf8 = BYTE_ORDER_MARK.to_vec();
        utf8.extend_from_slice("本文".as_bytes());
        let (text, form) = decode_as(&utf8, LIMIT, Encoding::Utf8).expect("読める");
        assert_eq!(text, "本文");
        assert!(form.byte_order_mark);
    }

    /// E2の②: **CP932は印を持たない**ので、UTF-8のBOMも字として読もうとする
    /// ——`EF BB BF`はCP932の表に無い並びなので、そこで断られる。
    /// **断られたほうが良い**：読めない字を`?`にして開けば、書き手はBOMが
    /// あったことを二度と知れない。
    #[test]
    fn cp932_has_no_mark_to_take_off() {
        let mut bytes = BYTE_ORDER_MARK.to_vec();
        bytes.extend_from_slice("本文".as_bytes());
        let error = decode_as(&bytes, LIMIT, Encoding::Cp932).expect_err("断る");
        assert!(matches!(error, LoadError::Unreadable));
    }

    /// E2の②: **読めなければ断る。**呼ぶ側の本文はそのままで、開き直しは
    /// 失敗しても何も失わない操作である。
    #[test]
    fn a_named_encoding_that_cannot_read_the_bytes_refuses() {
        let bytes = crate::code_page::encode(crate::code_page::CP932, "日本語").expect("書ける");
        let error = decode_as(&bytes, LIMIT, Encoding::Utf8).expect_err("断る");
        assert!(matches!(error, LoadError::Unreadable));
    }

    /// E2の②: **開き直した文書と、開いた文書は同じ作りである。**改行の畳み方も
    /// 上限の数え方も[`decode`]と同じ道（`fold`）を通る——別々になれば、
    /// 開き直したあとの保存だけが違う形を書く。
    #[test]
    fn a_named_encoding_folds_line_breaks_the_same_way() {
        let bytes =
            crate::code_page::encode(crate::code_page::CP932, "春\r\n海\r\n").expect("書ける");

        let (text, form) = decode_as(&bytes, LIMIT, Encoding::Cp932).expect("読める");
        assert_eq!(text, "春\n海\n");
        assert_eq!(form.newline, Newline::Crlf);
        assert!(!form.mixed_newlines);

        // 上限も同じ数え方（畳んだあとの字数）。
        assert!(decode_as(&bytes, 4, Encoding::Cp932).is_ok());
        assert!(matches!(
            decode_as(&bytes, 3, Encoding::Cp932).expect_err("越える"),
            LoadError::TooLarge { characters: 4, .. }
        ));
    }

    /// E2の②: 見本のファイルを、**別の文字コードだと言って**開き直す。
    #[test]
    fn a_sample_file_can_be_opened_as_another_encoding() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let path = here.join("15_文字コード_CP932.txt");

        // 言わなければCP932。
        assert_eq!(
            read(&path, LIMIT).expect("読める").form.encoding,
            Encoding::Cp932
        );
        // UTF-8だと言えば、読めないので断られる——ファイルは元のままである。
        let error = read_as(&path, LIMIT, Encoding::Utf8).expect_err("断る");
        assert!(matches!(error, LoadError::Unreadable));
        assert_eq!(
            read(&path, LIMIT).expect("読める").form.encoding,
            Encoding::Cp932
        );
    }

    /// E2: **表せない字があれば、1バイトも書かない**——`?`に替えて保存しない。
    #[test]
    fn a_save_that_cannot_hold_the_text_writes_nothing() {
        let directory = scratch_directory("cp932-refuses");
        let path = directory.join("note.txt");
        let form = TextForm {
            encoding: Encoding::Cp932,
            ..TextForm::default()
        };

        let error = save(&path, "絵文字は🐈です", form).expect_err("断る");

        assert!(error.to_string().contains('🐈'), "{error}");
        assert!(!path.exists(), "ファイルは作られない");
        let _ = fs::remove_dir_all(&directory);
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
            newline: Newline::Crlf,
            ..TextForm::default()
        };
        assert_eq!(encode("a\nb\n", form).expect("書ける"), b"a\r\nb\r\n");
    }

    #[test]
    fn writes_back_the_byte_order_mark() {
        let form = TextForm {
            byte_order_mark: true,
            ..TextForm::default()
        };
        let bytes = encode("本文", form).expect("書ける");
        assert!(bytes.starts_with(&BYTE_ORDER_MARK));
        let body = &bytes[BYTE_ORDER_MARK.len()..];
        assert_eq!(body, "本文".as_bytes());
    }

    #[test]
    fn a_new_document_is_written_as_plain_utf8() {
        assert_eq!(
            encode("a\nb", TextForm::default()).expect("書ける"),
            b"a\nb"
        );
    }

    /// Reading a file and saving it without editing must produce the same
    /// bytes. This is 要件 8.2「不用意に変更しない」in practice.
    #[test]
    fn decoding_and_encoding_round_trips() {
        let mut with_mark = BYTE_ORDER_MARK.to_vec();
        with_mark.extend_from_slice("本文\r\n".as_bytes());
        // E2（2026-09-10）: 文字コードも往復する——読んだときの形で書き戻すのが、
        // 「無指定の保存では元の形式を維持する」（E2の完了の目安）である。
        let mut utf16_le = UTF16_LE_MARK.to_vec();
        for unit in "見出し\r\n".encode_utf16() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
        }
        let mut utf16_be = UTF16_BE_MARK.to_vec();
        for unit in "見出し\n".encode_utf16() {
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        let cp932 = crate::code_page::encode(crate::code_page::CP932, "本文\r\n").expect("書ける");
        let files: [&[u8]; 8] = [
            b"a\nb\n",
            b"a\r\nb\r\n",
            b"a\rb\r",
            "見出し\r\n本文\r\n".as_bytes(),
            &with_mark,
            &utf16_le,
            &utf16_be,
            &cp932,
        ];
        for original in files {
            let (text, form) = decode(original, LIMIT).expect("decodes");
            assert_eq!(encode(&text, form).expect("書ける"), original, "{form:?}");
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
            newline: Newline::Crlf,
            ..TextForm::default()
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
