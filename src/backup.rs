//! RFN01-61（書き手と決めた 2026-09-27）: 自動バックアップの置き場。
//!
//! **保存でファイルを上書きする直前に、そのときディスクにある中身を残す。**秀丸・一太郎の
//! バックアップと同じ考え方で、版管理ではない——Gitを使う書き手はフォルダごとに切っておける
//! （`workspace::SaveMode::AutoBackup`）。ここは画面を知らないファイル操作だけを持つ。
//!
//! 置き場所は保存先の下に**元の絶対パスを写す**：`D:\原稿\長編A\第一章.md`のバックアップは
//! `<保存先>\D\原稿\長編A\第一章.2026-09-27_143012.md`。別のフォルダの同じ名前がぶつからず、
//! 書き手がエクスプローラーで覗いても、どのファイルのものか読める。
//!
//! **バックアップと読むのは、この名前の形をしたファイルだけ**である。保存先は書き手が選べる
//! （OneDriveのフォルダなど）ので、そこにある書き手自身のファイルを移したり消したりしない。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::timestamp::LocalTime;

/// 既定の保存先を置くアプリ専用領域の中のフォルダ名。
const DEFAULT_FOLDER: &str = "Backups";

/// 既定の残す数（書き手の決定 2026-09-27：5世代、設定で変えられる）。
pub const DEFAULT_KEEP: usize = 5;

/// 設定で選べる残す数の上限。
pub const MAX_KEEP: usize = 999;

/// 保存先を選んでいないときの置き場（アプリ専用領域）。
pub fn default_root() -> Option<PathBuf> {
    Some(crate::app_data::app_directory()?.join(DEFAULT_FOLDER))
}

/// 1つのバックアップ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    /// 取った時刻（名前から読む）。
    pub taken: LocalTime,
}

/// 元の絶対パスを、保存先の中の相対パスに写す。
///
/// ドライブは`D`、UNCは`UNC\server\share`になる。**相対パスや`..`を含むパスは写さない**
/// （`None`）——写した先が保存先の外や別のファイルの場所を指しうる。
fn mirror(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    out.push(char::from(letter).to_ascii_uppercase().to_string());
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out.push("UNC");
                    out.push(server);
                    out.push(share);
                }
                _ => return None,
            },
            Component::RootDir => rooted = true,
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => return None,
        }
    }
    (rooted && out.components().next().is_some()).then_some(out)
}

/// [`mirror`]の逆：保存先の中の相対パスから、元の絶対パスを戻す。
fn unmirror(relative: &Path) -> Option<PathBuf> {
    let mut parts = relative.components().map(|c| match c {
        Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
        _ => None,
    });
    let first = parts.next()??;
    let mut out = if first == "UNC" {
        let server = parts.next()??;
        let share = parts.next()??;
        PathBuf::from(format!(r"\\{server}\{share}\"))
    } else if first.len() == 1 && first.chars().all(|c| c.is_ascii_alphabetic()) {
        PathBuf::from(format!(r"{first}:\"))
    } else {
        return None;
    };
    for part in parts {
        out.push(part?);
    }
    Some(out)
}

/// `canonicalize`が付ける`\\?\`を外した、画面に出す形のパス。
///
/// Workspaceの台帳は正規化したパス（`\\?\C:\…`）を持ち、バックアップの置き場から戻した
/// パスは素の形（`C:\…`）なので、**比べる前にも、見せる前にも、この形にそろえる**。
pub fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{share}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// `original`のバックアップが入るフォルダ。
fn folder_of(root: &Path, original: &Path) -> Option<PathBuf> {
    Some(root.join(mirror(original.parent()?)?))
}

/// 既定のファイル名の書式（書き手の決定 2026-09-27：書式を書く形）。
///
/// `{name}`は元のファイル名から拡張子を除いたもの、`{ext}`は`.md`のような拡張子（無ければ空）。
/// 日時はTerminalのタイムスタンプと同じ文字（`yyyy` `MM` `dd` `HH` `mm` `ss`）で書く。
pub const DEFAULT_NAME_FORMAT: &str = "{name}.yyyy-MM-dd_HHmmss{ext}";

/// 書式の1片。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    Literal(char),
    Name,
    Ext,
    /// 日時の欄と、その桁数。
    Field(Field, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

/// 書式を片に分ける。**文字の並びの読み方はTerminalのタイムスタンプ（`timestamp::format`）と
/// 同じ**：同じ位置では`yyyy` `MM` `dd` `HH` `mm` `ss`の順に見る。
fn pieces(format: &str) -> Vec<Piece> {
    const WORDS: [(&str, Piece); 8] = [
        ("{name}", Piece::Name),
        ("{ext}", Piece::Ext),
        ("yyyy", Piece::Field(Field::Year, 4)),
        ("MM", Piece::Field(Field::Month, 2)),
        ("dd", Piece::Field(Field::Day, 2)),
        ("HH", Piece::Field(Field::Hour, 2)),
        ("mm", Piece::Field(Field::Minute, 2)),
        ("ss", Piece::Field(Field::Second, 2)),
    ];
    let mut out = Vec::new();
    let mut rest = format;
    'next: while let Some(c) = rest.chars().next() {
        for (word, piece) in WORDS {
            if let Some(after) = rest.strip_prefix(word) {
                out.push(piece);
                rest = after;
                continue 'next;
            }
        }
        out.push(Piece::Literal(c));
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// 書式が使えない理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatProblem {
    /// `{name}`がちょうど1つ無い。
    Name,
    /// 秒までの日時（年・月・日・時・分・秒）のどれかが無い。
    Time,
    /// `{ext}`が2つ以上ある。
    Ext,
    /// ファイル名に使えない字がある。
    Character(char),
}

/// 書式が使えるか。**名前と秒までの日時が要る**——どちらが欠けても、どのファイルの・いつの
/// バックアップかを名前から読み戻せない。
pub fn check_format(format: &str) -> Result<(), FormatProblem> {
    if let Some(bad) = format.chars().find(|c| {
        matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
    }) {
        return Err(FormatProblem::Character(bad));
    }
    let pieces = pieces(format);
    let count = |wanted: Piece| pieces.iter().filter(|p| **p == wanted).count();
    if count(Piece::Name) != 1 {
        return Err(FormatProblem::Name);
    }
    if count(Piece::Ext) > 1 {
        return Err(FormatProblem::Ext);
    }
    let fields = [
        Field::Year,
        Field::Month,
        Field::Day,
        Field::Hour,
        Field::Minute,
        Field::Second,
    ];
    let has = |field| {
        pieces
            .iter()
            .any(|p| matches!(p, Piece::Field(f, _) if *f == field))
    };
    if !fields.into_iter().all(has) {
        return Err(FormatProblem::Time);
    }
    Ok(())
}

/// 元のファイル名の、拡張子を除いたところと拡張子（`.`付き、無ければ空）。
fn name_parts(original: &Path) -> Option<(String, String)> {
    let stem = original.file_stem()?.to_string_lossy().into_owned();
    let extension = original
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    Some((stem, extension))
}

/// `format`で、`original`の`taken`のバックアップの名前を作る。
pub fn name_for(format: &str, original: &Path, taken: LocalTime) -> Option<String> {
    let (stem, extension) = name_parts(original)?;
    let mut out = String::new();
    for piece in pieces(format) {
        match piece {
            Piece::Literal(c) => out.push(c),
            Piece::Name => out.push_str(&stem),
            Piece::Ext => out.push_str(&extension),
            Piece::Field(field, width) => {
                let value = match field {
                    Field::Year => taken.year,
                    Field::Month => taken.month,
                    Field::Day => taken.day,
                    Field::Hour => taken.hour,
                    Field::Minute => taken.minute,
                    Field::Second => taken.second,
                };
                out.push_str(&format!("{value:0width$}"));
            }
        }
    }
    Some(out)
}

/// 名前から読み戻したもの。
#[derive(Default)]
struct Read<'a> {
    stem: &'a str,
    extension: &'a str,
    taken: LocalTime,
}

/// `name`を書式の片に当てはめる。`{name}`と`{ext}`は長いほうから試し、合わなければ短くする。
/// `want`があれば、`{name}`と`{ext}`はその字でなければならない。
fn fit<'a>(
    pieces: &[Piece],
    name: &'a str,
    want: Option<(&str, &str)>,
    read: &mut Read<'a>,
) -> bool {
    let Some((first, rest)) = pieces.split_first() else {
        return name.is_empty();
    };
    match *first {
        Piece::Literal(c) => name
            .strip_prefix(c)
            .is_some_and(|after| fit(rest, after, want, read)),
        Piece::Field(field, width) => {
            let Some(digits) = name.get(..width) else {
                return false;
            };
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            let value: u16 = digits.parse().unwrap_or(0);
            let slot = match field {
                Field::Year => &mut read.taken.year,
                Field::Month => &mut read.taken.month,
                Field::Day => &mut read.taken.day,
                Field::Hour => &mut read.taken.hour,
                Field::Minute => &mut read.taken.minute,
                Field::Second => &mut read.taken.second,
            };
            *slot = value;
            fit(rest, &name[width..], want, read)
        }
        Piece::Name => {
            if let Some((stem, _)) = want {
                return name.strip_prefix(stem).is_some_and(|after| {
                    read.stem = &name[..stem.len()];
                    fit(rest, after, want, read)
                });
            }
            let ends: Vec<usize> = name
                .char_indices()
                .map(|(at, c)| at + c.len_utf8())
                .collect();
            ends.into_iter().rev().any(|end| {
                read.stem = &name[..end];
                fit(rest, &name[end..], want, read)
            })
        }
        Piece::Ext => {
            if let Some((_, extension)) = want {
                return name.strip_prefix(extension).is_some_and(|after| {
                    read.extension = &name[..extension.len()];
                    fit(rest, after, want, read)
                });
            }
            // `.`で始まり、ほかに`.`を含まないもの（長いほうから）、または空。
            let mut ends = vec![0];
            if name.starts_with('.') {
                for (at, c) in name.char_indices().skip(1) {
                    if c == '.' {
                        break;
                    }
                    ends.push(at + c.len_utf8());
                }
            }
            ends.into_iter().rev().any(|end| {
                read.extension = &name[..end];
                fit(rest, &name[end..], want, read)
            })
        }
    }
}

/// `name`が`original`の、`format`で名付けたバックアップなら、その時刻。
fn stamp_in(format: &str, name: &str, original: &Path) -> Option<LocalTime> {
    let (stem, extension) = name_parts(original)?;
    let mut read = Read::default();
    fit(&pieces(format), name, Some((&stem, &extension)), &mut read).then_some(read.taken)
}

/// 名前だけから、`format`のバックアップの形をしているか見る（元のファイル名は問わない）。
///
/// 戻り値は元のファイル名。`第一章.2026-09-27_143012.md` → `第一章.md`。
fn original_name(format: &str, name: &str) -> Option<String> {
    let mut read = Read::default();
    let fits = fit(&pieces(format), name, None, &mut read);
    (fits && !read.stem.is_empty()).then(|| format!("{}{}", read.stem, read.extension))
}

/// バックアップの置き場：保存先と、ファイル名の書式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    pub root: PathBuf,
    pub format: String,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, format: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            format: format.into(),
        }
    }

    /// `original`のバックアップ、新しい順。
    pub fn list(&self, original: &Path) -> Vec<Backup> {
        let Some(folder) = folder_of(&self.root, original) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&folder) else {
            return Vec::new();
        };
        let mut found: Vec<Backup> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let taken = stamp_in(&self.format, &name, original)?;
                Some(Backup {
                    path: entry.path(),
                    taken,
                })
            })
            .collect();
        // 書式によって名前の並びと時刻の並びは一致しないので、時刻で並べる。
        let key = |b: &Backup| {
            let t = b.taken;
            (t.year, t.month, t.day, t.hour, t.minute, t.second)
        };
        found.sort_by_key(|b| std::cmp::Reverse(key(b)));
        found
    }

    /// 保存の直前に呼ぶ：`original`の今の中身をバックアップとして残す。
    ///
    /// - 元のファイルが無ければ（新しいファイルへの保存）何もしない。
    /// - **いちばん新しいバックアップと中身が同じなら書かない**——同じものが世代を押し出す
    ///   だけになる。
    /// - 書いたあと、`keep`を超えた古いものを消す。
    ///
    /// 戻り値は書いたバックアップ。書かなかったときは`None`。
    pub fn take(
        &self,
        original: &Path,
        keep: usize,
        taken: LocalTime,
    ) -> io::Result<Option<PathBuf>> {
        let bytes = match fs::read(original) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let folder = folder_of(&self.root, original).ok_or_else(|| unsupported(original))?;
        let name = name_for(&self.format, original, taken).ok_or_else(|| unsupported(original))?;
        let existing = self.list(original);
        let same = existing
            .first()
            .is_some_and(|newest| fs::read(&newest.path).is_ok_and(|held| held == bytes));
        let target = folder.join(name);
        // 同じ秒にもう取ってある（1秒に2回保存した）なら、その1つで足りる。
        let written = if same || target.exists() {
            None
        } else {
            fs::create_dir_all(&folder)?;
            crate::file_io::write_atomically(&target, &bytes)?;
            Some(target)
        };
        self.prune(original, keep.max(1));
        Ok(written)
    }

    /// `keep`を超えた古いバックアップを消す。消せなかったものは次の機会に回す。
    fn prune(&self, original: &Path, keep: usize) {
        for old in self.list(original).into_iter().skip(keep) {
            let _ = fs::remove_file(&old.path);
        }
    }

    /// バックアップを消す（Backup History画面・Backupsの面）。
    ///
    /// 1つでも消せなければ`Err`。消せたものは戻らない——消すことを選んだものだからである。
    pub fn delete(&self, paths: &[PathBuf]) -> io::Result<()> {
        let mut failed = None;
        for path in paths {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
            {
                failed.get_or_insert(error);
            }
            tidy(&self.root, path.parent());
        }
        failed.map_or(Ok(()), Err)
    }

    /// Editorの中で`from`を`to`へ名前変更・移動したとき、バックアップも付いていく。
    ///
    /// `from`はファイルでもフォルダでもよい。バックアップが無ければ何もしない。
    pub fn follow(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (Some(from_mirror), Some(to_mirror)) = (mirror(from), mirror(to)) else {
            return Ok(());
        };
        let (from_folder, to_folder) = (self.root.join(from_mirror), self.root.join(to_mirror));
        if from_folder.is_dir() {
            // フォルダを動かした：その下のバックアップを丸ごと写す先へ。
            for relative in self.files_under(&from_folder) {
                carry(&from_folder.join(&relative), &to_folder.join(&relative))?;
            }
            tidy(&self.root, Some(&from_folder));
            return Ok(());
        }
        // ファイルを動かした：時刻はそのまま、名前を新しいファイルのものに。
        let target = folder_of(&self.root, to).ok_or_else(|| unsupported(to))?;
        for backup in self.list(from) {
            let name = name_for(&self.format, to, backup.taken).ok_or_else(|| unsupported(to))?;
            carry(&backup.path, &target.join(name))?;
        }
        tidy(&self.root, folder_of(&self.root, from).as_deref());
        Ok(())
    }

    /// `folder`の下にあるバックアップの形をしたファイル（`folder`からの相対パス）。
    fn files_under(&self, folder: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let Ok(entries) = fs::read_dir(folder.join(&relative)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let child = relative.join(entry.file_name());
                if kind.is_dir() {
                    pending.push(child);
                } else if kind.is_file()
                    && original_name(&self.format, &entry.file_name().to_string_lossy()).is_some()
                {
                    found.push(child);
                }
            }
        }
        found.sort();
        found
    }

    /// 保存先にあるバックアップ（保存先からの相対パス）。
    ///
    /// **歩くのは写しのフォルダ（`C`・`D`などのドライブと`UNC`）の中だけ**（書き手の報告
    /// 2026-09-27）。保存先は書き手が選んだ普通のフォルダでありうるので、その中の書き手自身の
    /// フォルダやファイルには入らない——読みもしないし、移しも消しもしない。
    fn files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mirrored =
                name == "UNC" || (name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic()));
            if !mirrored || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let inside = self.files_under(&self.root.join(&name));
            found.extend(inside.into_iter().map(|r| Path::new(&name).join(r)));
        }
        found.sort();
        found
    }

    /// 保存先を`to`へ変えるとき、バックアップを全部移す。
    ///
    /// **途中で失敗したら何も変えない**（書き手の決定 2026-09-27）：全部を写し終えてから元を
    /// 消すので、写す途中の失敗は写したものを消せば元どおりになる。写し終えたあと元を消せ
    /// なかったものは、元の場所に残るだけで、失われるものは無い。
    ///
    /// 入れ子（今の保存先の中のフォルダへ、またはその逆）も移せる：先に一覧を取り、歩くのは
    /// 写しのフォルダだけなので、移し先が移し元の中にあっても数え直さない。
    pub fn move_to(&self, to: &Path) -> Result<(), MoveError> {
        let from = &self.root;
        if from == to {
            return Ok(());
        }
        let files = self.files();
        if let Some(clash) = files.iter().map(|f| to.join(f)).find(|t| t.exists()) {
            return Err(MoveError::Exists(clash));
        }
        let mut copied = Vec::new();
        for relative in &files {
            let target = to.join(relative);
            let copy = target
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::copy(from.join(relative), &target));
            if let Err(error) = copy {
                let _ = fs::remove_file(&target);
                for done in &copied {
                    let _ = fs::remove_file(done);
                    tidy(to, Path::new(done).parent());
                }
                tidy(to, target.parent());
                return Err(MoveError::Io(error));
            }
            copied.push(target);
        }
        for relative in &files {
            let source = from.join(relative);
            let _ = fs::remove_file(&source);
            tidy(from, source.parent());
        }
        Ok(())
    }

    /// ファイル名の書式を`format`へ変えるとき、バックアップの名前を全部付け替える。
    ///
    /// **途中で失敗したら何も変えない**（保存先の変更と同じ）：付け替えたものを元の名前へ戻す。
    pub fn rename_to(&self, format: &str) -> Result<(), MoveError> {
        if format == self.format {
            return Ok(());
        }
        let mut plan = Vec::new();
        for relative in self.files() {
            let from = self.root.join(&relative);
            let Some(name) = from.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let Some(original) = original_name(&self.format, &name) else {
                continue;
            };
            let original = from.with_file_name(original);
            let Some(taken) = stamp_in(&self.format, &name, &original) else {
                continue;
            };
            let Some(renamed) = name_for(format, &original, taken) else {
                continue;
            };
            let to = from.with_file_name(renamed);
            if to != from {
                plan.push((from, to));
            }
        }
        if let Some((_, clash)) = plan.iter().find(|(_, to)| to.exists()) {
            return Err(MoveError::Exists(clash.clone()));
        }
        let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
        for step in &plan {
            if let Err(error) = fs::rename(&step.0, &step.1) {
                for (from, to) in done.into_iter().rev() {
                    let _ = fs::rename(to, from);
                }
                return Err(MoveError::Io(error));
            }
            done.push(step);
        }
        Ok(())
    }

    /// 保存先にあるバックアップを、元のフォルダごとにまとめる（Backupsの面）。
    ///
    /// `registered`（Workspaceの登録フォルダ）の中にあったものは、**いちばん深い登録フォルダ**に
    /// まとめる。どこにも入らないもの（登録を外した、外で名前を変えた）は元の親フォルダで並ぶ。
    pub fn groups(&self, registered: &[PathBuf]) -> Vec<Group> {
        type Originals = BTreeMap<PathBuf, Vec<PathBuf>>;
        let mut grouped: BTreeMap<PathBuf, Originals> = BTreeMap::new();
        for relative in self.files() {
            let Some(original_parent) = relative.parent().and_then(unmirror) else {
                continue;
            };
            let Some(name) = relative
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };
            let Some(original) = original_name(&self.format, &name) else {
                continue;
            };
            let owner = registered
                .iter()
                .map(|folder| plain(folder))
                .filter(|folder| starts_with_ignoring_case(&original_parent, folder))
                .max_by_key(|folder| folder.components().count())
                .unwrap_or_else(|| original_parent.clone());
            grouped
                .entry(owner)
                .or_default()
                .entry(original_parent.join(original))
                .or_default()
                .push(self.root.join(relative));
        }
        grouped
            .into_iter()
            .map(|(folder, originals)| Group {
                folder,
                originals: originals.into_iter().collect(),
            })
            .collect()
    }
}

/// 空になったフォルダを、保存先の手前まで畳む。
fn tidy(root: &Path, mut folder: Option<&Path>) {
    while let Some(at) = folder {
        if at == root || !at.starts_with(root) || fs::remove_dir(at).is_err() {
            return;
        }
        folder = at.parent();
    }
}

fn unsupported(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("cannot back up {}", path.display()),
    )
}

/// 1つのファイルを動かす。同じ名前が先にあれば上書きしない。
fn carry(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // 別のドライブへは改名できない。写してから消す。
    fs::copy(from, to)?;
    fs::remove_file(from)
}

/// 保存先の変更・書式の変更の失敗。
#[derive(Debug)]
pub enum MoveError {
    /// 移し先（付け替え先）に同じ名前のファイルがある。
    Exists(PathBuf),
    Io(io::Error),
}

/// Backupsの面の1つのフォルダ：元のフォルダと、そこのファイルごとのバックアップ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub folder: PathBuf,
    /// 元のファイル（素の絶対パス）と、そのバックアップ。
    pub originals: Vec<(PathBuf, Vec<PathBuf>)>,
}

impl Group {
    /// このフォルダのバックアップ全部。
    pub fn files(&self) -> Vec<PathBuf> {
        self.originals.iter().flat_map(|(_, b)| b.clone()).collect()
    }
}

/// Windowsのパスは大文字・小文字を区別しない。
fn starts_with_ignoring_case(path: &Path, prefix: &Path) -> bool {
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    lower(path).starts_with(lower(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-backup-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    fn store(root: &Path) -> Store {
        Store::new(root, DEFAULT_NAME_FORMAT)
    }

    fn at(second: u16) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 9,
            day: 27,
            hour: 14,
            minute: 30,
            second,
            millis: 0,
        }
    }

    #[test]
    fn a_path_is_mirrored_under_the_root_and_back() {
        let drive = Path::new(r"D:\原稿\長編A");
        assert_eq!(mirror(drive), Some(PathBuf::from(r"D\原稿\長編A")));
        assert_eq!(
            unmirror(Path::new(r"D\原稿\長編A")),
            Some(drive.to_path_buf())
        );
        // ドライブ名は大文字にそろえる（同じドライブが2か所に分かれない）。
        assert_eq!(
            mirror(Path::new(r"d:\原稿")),
            Some(PathBuf::from(r"D\原稿"))
        );
        let share = Path::new(r"\\server\share\原稿");
        assert_eq!(mirror(share), Some(PathBuf::from(r"UNC\server\share\原稿")));
        assert_eq!(
            unmirror(Path::new(r"UNC\server\share\原稿")),
            Some(share.to_path_buf())
        );
        assert_eq!(mirror(Path::new(r"原稿\長編A")), None);
        assert_eq!(mirror(Path::new(r"D:\原稿\..\長編A")), None);
    }

    #[test]
    fn the_verbatim_prefix_is_taken_off_for_showing() {
        assert_eq!(plain(Path::new(r"\\?\C:\原稿")), PathBuf::from(r"C:\原稿"));
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\原稿")),
            PathBuf::from(r"\\server\share\原稿")
        );
        assert_eq!(plain(Path::new(r"C:\原稿")), PathBuf::from(r"C:\原稿"));
    }

    #[test]
    fn names_follow_the_format_and_read_back() {
        let taken = at(12);
        let name = |format: &str, p: &str| name_for(format, Path::new(p), taken);
        let f = DEFAULT_NAME_FORMAT;
        assert_eq!(
            name(f, r"D:\a\第一章.md").as_deref(),
            Some("第一章.2026-09-27_143012.md")
        );
        assert_eq!(
            name(f, r"D:\a\README").as_deref(),
            Some("README.2026-09-27_143012")
        );
        assert_eq!(
            name(f, r"D:\a\a.b.txt").as_deref(),
            Some("a.b.2026-09-27_143012.txt")
        );
        assert_eq!(
            stamp_in(f, "a.b.2026-09-27_143012.txt", Path::new(r"D:\a\a.b.txt")),
            Some(taken)
        );
        // 別のファイルの名前は、頭が同じでもそのファイルのものではない。
        assert_eq!(
            stamp_in(f, "a.b.2026-09-27_143012.txt", Path::new(r"D:\a\a.txt")),
            None
        );
        assert_eq!(
            original_name(f, "第一章.2026-09-27_143012.md").as_deref(),
            Some("第一章.md")
        );
        assert_eq!(
            original_name(f, "README.2026-09-27_143012").as_deref(),
            Some("README")
        );
        assert_eq!(
            original_name(f, "a.b.2026-09-27_143012.txt").as_deref(),
            Some("a.b.txt")
        );
        assert_eq!(original_name(f, "第一章.md"), None);
        assert_eq!(original_name(f, ".2026-09-27_143012.md"), None);
        // 書き手の報告 2026-09-27：長い日本語の名前で落ちた。
        assert_eq!(original_name(f, "あいうえおかきくけこさしすせそ.md"), None);
        assert_eq!(
            original_name(f, "長い日本語の名前の原稿.2026-09-27_143012.md").as_deref(),
            Some("長い日本語の名前の原稿.md")
        );
        // 別の書式：日時が先、拡張子の後ろに印。
        let g = "yyyyMMdd_HHmmss_{name}{ext}.bak";
        assert_eq!(
            name(g, r"D:\a\第一章.md").as_deref(),
            Some("20260927_143012_第一章.md.bak")
        );
        assert_eq!(
            original_name(g, "20260927_143012_第一章.md.bak").as_deref(),
            Some("第一章.md")
        );
        assert_eq!(
            stamp_in(
                g,
                "20260927_143012_第一章.md.bak",
                Path::new(r"D:\a\第一章.md")
            ),
            Some(taken)
        );
        assert_eq!(original_name(g, "第一章.2026-09-27_143012.md"), None);
    }

    #[test]
    fn a_format_needs_the_name_and_the_time_to_the_second() {
        assert_eq!(check_format(DEFAULT_NAME_FORMAT), Ok(()));
        assert_eq!(check_format("yyyyMMdd_HHmmss_{name}{ext}.bak"), Ok(()));
        assert_eq!(
            check_format("yyyy-MM-dd_HHmmss{ext}"),
            Err(FormatProblem::Name)
        );
        assert_eq!(
            check_format("{name}{name}.yyyyMMddHHmmss"),
            Err(FormatProblem::Name)
        );
        assert_eq!(
            check_format("{name}.yyyy-MM-dd_HHmm{ext}"),
            Err(FormatProblem::Time)
        );
        assert_eq!(
            check_format("{name}{ext}.yyyyMMddHHmmss{ext}"),
            Err(FormatProblem::Ext)
        );
        assert_eq!(
            check_format(r"{name}\yyyyMMddHHmmss"),
            Err(FormatProblem::Character('\\'))
        );
        assert_eq!(
            check_format("{name}:yyyyMMddHHmmss"),
            Err(FormatProblem::Character(':'))
        );
    }

    #[test]
    fn taking_keeps_the_newest_and_skips_the_same_contents() {
        let root = scratch("take-root");
        let work = scratch("take-work");
        let original = work.join("第一章.md");
        let store = store(&root);
        // 新しいファイル（上書きする相手が無い）では何も取らない。
        assert_eq!(store.take(&original, 5, at(0)).unwrap(), None);
        for (second, text) in (1..=7).zip(["一", "二", "三", "四", "五", "六", "七"]) {
            fs::write(&original, text).unwrap();
            assert!(store.take(&original, 5, at(second)).unwrap().is_some());
        }
        // 同じ中身は書かない。
        assert_eq!(store.take(&original, 5, at(8)).unwrap(), None);
        let kept = store.list(&original);
        assert_eq!(kept.len(), 5);
        assert_eq!(kept[0].taken, at(7));
        assert_eq!(fs::read_to_string(&kept[0].path).unwrap(), "七");
        assert_eq!(kept[4].taken, at(3));
        // 名前の形をしていない書き手のファイルは数えない。
        fs::write(kept[0].path.with_file_name("第一章.メモ.md"), "x").unwrap();
        assert_eq!(store.list(&original).len(), 5);
    }

    #[test]
    fn a_date_first_format_still_lists_newest_first() {
        let root = scratch("order-root");
        let work = scratch("order-work");
        let original = work.join("a.md");
        let store = Store::new(&root, "dd-MM-yyyy HHmmss {name}{ext}");
        let days = [(1u16, "一"), (2, "二"), (3, "三")];
        for (day, text) in days {
            fs::write(&original, text).unwrap();
            let taken = LocalTime {
                day,
                month: if day == 1 { 12 } else { 1 },
                year: if day == 1 { 2025 } else { 2026 },
                ..at(0)
            };
            store.take(&original, 5, taken).unwrap();
        }
        let kept = store.list(&original);
        assert_eq!(fs::read_to_string(&kept[0].path).unwrap(), "三");
        assert_eq!(fs::read_to_string(&kept[2].path).unwrap(), "一");
    }

    #[test]
    fn the_bytes_are_kept_as_they_were() {
        let root = scratch("bytes-root");
        let work = scratch("bytes-work");
        let original = work.join("sjis.txt");
        let bytes = [0x82u8, 0xa0, b'\r', b'\n', 0x82, 0xa2];
        fs::write(&original, bytes).unwrap();
        let written = store(&root).take(&original, 5, at(1)).unwrap().unwrap();
        assert_eq!(fs::read(written).unwrap(), bytes);
    }

    #[test]
    fn deleting_folds_empty_folders_up_to_the_root() {
        let root = scratch("delete-root");
        let work = scratch("delete-work");
        let original = work.join("a.md");
        let store = store(&root);
        fs::write(&original, "x").unwrap();
        store.take(&original, 5, at(1)).unwrap();
        fs::write(&original, "y").unwrap();
        store.take(&original, 5, at(2)).unwrap();
        let paths: Vec<_> = store.list(&original).into_iter().map(|b| b.path).collect();
        store.delete(&paths[..1]).unwrap();
        assert_eq!(store.list(&original).len(), 1);
        store.delete(&paths[1..]).unwrap();
        assert!(store.list(&original).is_empty());
        assert!(root.is_dir());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn backups_follow_a_renamed_file_and_a_moved_folder() {
        let root = scratch("follow-root");
        let work = scratch("follow-work");
        let store = store(&root);
        let chapter = work.join("章").join("第一章.md");
        fs::create_dir_all(chapter.parent().unwrap()).unwrap();
        fs::write(&chapter, "x").unwrap();
        store.take(&chapter, 5, at(1)).unwrap();
        let renamed = work.join("章").join("序章.md");
        store.follow(&chapter, &renamed).unwrap();
        assert!(store.list(&chapter).is_empty());
        let moved = store.list(&renamed);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].taken, at(1));
        let folder = work.join("第一部");
        store.follow(&work.join("章"), &folder).unwrap();
        assert_eq!(store.list(&folder.join("序章.md")).len(), 1);
        assert!(store.list(&renamed).is_empty());
        // 何も無いものを動かしても何も起きない。
        store
            .follow(&work.join("無い.md"), &work.join("別.md"))
            .unwrap();
    }

    #[test]
    fn moving_everything_is_all_or_nothing() {
        let from = scratch("move-from");
        let to = scratch("move-to");
        let work = scratch("move-work");
        for name in ["a.md", "b.md"] {
            let original = work.join(name);
            fs::write(&original, name).unwrap();
            store(&from).take(&original, 5, at(1)).unwrap();
        }
        // 移し先に同じ名前があれば、1つも動かさない。
        let clash = to
            .join(mirror(&work).unwrap())
            .join("b.2026-09-27_143001.md");
        fs::create_dir_all(clash.parent().unwrap()).unwrap();
        fs::write(&clash, "先客").unwrap();
        assert!(matches!(
            store(&from).move_to(&to),
            Err(MoveError::Exists(_))
        ));
        assert_eq!(store(&from).list(&work.join("a.md")).len(), 1);
        assert_eq!(store(&to).list(&work.join("a.md")).len(), 0);
        fs::remove_file(&clash).unwrap();
        store(&from).move_to(&to).unwrap();
        assert!(store(&from).list(&work.join("a.md")).is_empty());
        assert_eq!(store(&to).list(&work.join("a.md")).len(), 1);
        assert_eq!(store(&to).list(&work.join("b.md")).len(), 1);
        // 入れ子も移せる（書き手の報告 2026-09-27：今の保存先の中のフォルダを選んで断られた）。
        let inner = to.join("Temp");
        store(&to).move_to(&inner).unwrap();
        assert_eq!(store(&inner).list(&work.join("a.md")).len(), 1);
        assert!(store(&to).list(&work.join("a.md")).is_empty());
        store(&inner).move_to(&to).unwrap();
        assert_eq!(store(&to).list(&work.join("a.md")).len(), 1);
    }

    #[test]
    fn a_chosen_folder_keeps_its_own_files_to_itself() {
        let from = scratch("own-from");
        let to = scratch("own-to");
        let work = scratch("own-work");
        let original = work.join("a.md");
        fs::write(&original, "x").unwrap();
        store(&from).take(&original, 5, at(1)).unwrap();
        // 保存先に元からある書き手のもの：バックアップの形の名前でも、写しのフォルダの外なら触らない。
        let own = from.join("原稿").join("第一章.2026-09-27_143012.md");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, "書き手の").unwrap();
        fs::write(from.join("あいうえおかきくけこさしすせそ.md"), "書き手の").unwrap();
        store(&from).move_to(&to).unwrap();
        assert!(own.exists());
        assert!(from.join("あいうえおかきくけこさしすせそ.md").exists());
        assert!(!to.join("原稿").exists());
        assert_eq!(store(&to).list(&original).len(), 1);
        assert!(store(&from).groups(&[]).is_empty());
    }

    #[test]
    fn changing_the_format_renames_everything_or_nothing() {
        let root = scratch("rename-root");
        let work = scratch("rename-work");
        let a = work.join("第一章.md");
        let b = work.join("README");
        for (second, path) in [(1, &a), (2, &b)] {
            fs::write(path, "x").unwrap();
            store(&root).take(path, 5, at(second)).unwrap();
        }
        let new_format = "yyyyMMdd_HHmmss_{name}{ext}.bak";
        // 付け替え先に同じ名前があれば、1つも付け替えない。
        let folder = root.join(mirror(&work).unwrap());
        let clash = folder.join("20260927_143002_README.bak");
        fs::write(&clash, "先客").unwrap();
        assert!(matches!(
            store(&root).rename_to(new_format),
            Err(MoveError::Exists(_))
        ));
        assert_eq!(store(&root).list(&a).len(), 1);
        assert_eq!(store(&root).list(&b).len(), 1);
        fs::remove_file(&clash).unwrap();
        store(&root).rename_to(new_format).unwrap();
        let renamed = Store::new(&root, new_format);
        assert_eq!(
            renamed.list(&a)[0].path,
            folder.join("20260927_143001_第一章.md.bak")
        );
        assert_eq!(renamed.list(&b)[0].taken, at(2));
        assert!(store(&root).list(&a).is_empty());
    }

    #[test]
    fn groups_gather_under_the_deepest_registered_folder() {
        let root = scratch("groups-root");
        let work = scratch("groups-work");
        let novel = work.join("長編");
        let part = novel.join("第一部");
        let loose = work.join("外");
        for path in [
            novel.join("a.md"),
            novel.join("b.md"),
            part.join("b.md"),
            loose.join("c.md"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "x").unwrap();
            store(&root).take(&path, 5, at(1)).unwrap();
        }
        // 台帳のパスは正規化してある（`\\?\C:\…`）。それでも同じフォルダとしてまとまる。
        let verbatim = |p: &Path| PathBuf::from(format!(r"\\?\{}", p.display()));
        let found = store(&root).groups(&[verbatim(&novel), part.clone()]);
        let folders: Vec<_> = found.iter().map(|g| g.folder.clone()).collect();
        // 並びはパスの順（「外」は「長編」より前）。一時フォルダのドライブ名は大文字。
        assert_eq!(folders, vec![loose.clone(), novel.clone(), part.clone()]);
        let originals: Vec<_> = found[1]
            .originals
            .iter()
            .map(|(o, b)| (o.clone(), b.len()))
            .collect();
        assert_eq!(
            originals,
            vec![(novel.join("a.md"), 1), (novel.join("b.md"), 1)]
        );
        assert_eq!(found[1].files().len(), 2);
    }
}
