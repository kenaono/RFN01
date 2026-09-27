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

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::timestamp::{self, LocalTime};

/// 既定の保存先を置くアプリ専用領域の中のフォルダ名。
const DEFAULT_FOLDER: &str = "Backups";

/// 名前に入れる時刻の形。**秒まで**入れる——分までだと、1分に2回保存したとき片方が消える。
const STAMP_FORMAT: &str = "yyyy-MM-dd_HHmmss";

/// `yyyy-MM-dd_HHmmss`の長さ。
const STAMP_LENGTH: usize = 17;

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

/// 名前の頭と尻：`第一章.md`なら`第一章.`と`.md`、拡張子の無い`README`なら`README.`と空。
fn name_parts(original: &Path) -> Option<(String, String)> {
    let stem = original.file_stem()?.to_string_lossy().into_owned();
    let extension = original
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    Some((format!("{stem}."), extension))
}

fn backup_name(original: &Path, taken: LocalTime) -> Option<String> {
    let (head, tail) = name_parts(original)?;
    let stamp = timestamp::format(STAMP_FORMAT, taken);
    Some(format!("{head}{stamp}{tail}"))
}

/// `yyyy-MM-dd_HHmmss`を時刻として読む。形が違えば`None`。
fn parse_stamp(stamp: &str) -> Option<LocalTime> {
    let bytes = stamp.as_bytes();
    if bytes.len() != STAMP_LENGTH {
        return None;
    }
    let shape_ok = bytes.iter().enumerate().all(|(at, byte)| match at {
        4 | 7 => *byte == b'-',
        10 => *byte == b'_',
        _ => byte.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    let number = |range: std::ops::Range<usize>| stamp[range].parse::<u16>().ok();
    Some(LocalTime {
        year: number(0..4)?,
        month: number(5..7)?,
        day: number(8..10)?,
        hour: number(11..13)?,
        minute: number(13..15)?,
        second: number(15..17)?,
        millis: 0,
    })
}

/// `name`が`original`のバックアップの名前なら、その時刻。
fn stamp_in(name: &str, original: &Path) -> Option<LocalTime> {
    let (head, tail) = name_parts(original)?;
    let middle = name.strip_prefix(&head)?.strip_suffix(&tail)?;
    parse_stamp(middle)
}

/// 名前だけから、バックアップの形をしているか見る（元のファイル名は問わない）。
///
/// 戻り値は元のファイル名。`第一章.2026-09-27_143012.md` → `第一章.md`。
fn original_name(name: &str) -> Option<String> {
    // 時刻の後ろは拡張子（`.`で始まり、それ以上`.`を含まない）か、何も無い。
    let stamp_ends_at = |dot: usize| {
        let start = dot.checked_sub(STAMP_LENGTH)?;
        parse_stamp(name.get(start..dot)?)
    };
    let (before, extension) = match name.rfind('.') {
        Some(dot) if stamp_ends_at(dot).is_some() => name.split_at(dot),
        _ => (name, ""),
    };
    // **字の途中では切らない**（書き手の報告 2026-09-27：長い日本語の名前で落ちた）。
    // 時刻は18バイトのASCIIなので、そこが字の境目でなければバックアップの名前ではない。
    let split = before.len().checked_sub(STAMP_LENGTH + 1)?;
    let (stem, rest) = (before.get(..split)?, before.get(split..)?);
    if !rest.starts_with('.') || stem.is_empty() || parse_stamp(&rest[1..]).is_none() {
        return None;
    }
    Some(format!("{stem}{extension}"))
}

/// `original`のバックアップ、新しい順。
pub fn list(root: &Path, original: &Path) -> Vec<Backup> {
    let Some(folder) = folder_of(root, original) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&folder) else {
        return Vec::new();
    };
    let mut found: Vec<(String, Backup)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let taken = stamp_in(&name, original)?;
            let path = entry.path();
            Some((name, Backup { path, taken }))
        })
        .collect();
    // 名前の時刻は桁がそろっているので、文字列の順がそのまま時刻の順である。
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, backup)| backup).collect()
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
    root: &Path,
    original: &Path,
    keep: usize,
    taken: LocalTime,
) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(original) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let folder = folder_of(root, original).ok_or_else(|| unsupported(original))?;
    let name = backup_name(original, taken).ok_or_else(|| unsupported(original))?;
    let existing = list(root, original);
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
    prune(root, original, keep.max(1));
    Ok(written)
}

fn unsupported(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("cannot back up {}", path.display()),
    )
}

/// `keep`を超えた古いバックアップを消す。消せなかったものは次の機会に回す。
fn prune(root: &Path, original: &Path, keep: usize) {
    for old in list(root, original).into_iter().skip(keep) {
        let _ = fs::remove_file(&old.path);
    }
}

/// バックアップを消す（Backup History画面・Delete Backups…）。
///
/// 1つでも消せなければ`Err`。消せたものは戻らない——消すことを選んだものだからである。
pub fn delete(root: &Path, paths: &[PathBuf]) -> io::Result<()> {
    let mut failed = None;
    for path in paths {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            failed.get_or_insert(error);
        }
        tidy(root, path.parent());
    }
    failed.map_or(Ok(()), Err)
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

/// Editorの中で`from`を`to`へ名前変更・移動したとき、バックアップも付いていく。
///
/// `from`はファイルでもフォルダでもよい。バックアップが無ければ何もしない。
pub fn follow(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
    let (Some(from_mirror), Some(to_mirror)) = (mirror(from), mirror(to)) else {
        return Ok(());
    };
    let (from_folder, to_folder) = (root.join(from_mirror), root.join(to_mirror));
    if from_folder.is_dir() {
        // フォルダを動かした：その下のバックアップを丸ごと写す先へ。
        for relative in backup_files_under(&from_folder) {
            carry(&from_folder.join(&relative), &to_folder.join(&relative))?;
        }
        tidy(root, Some(&from_folder));
        return Ok(());
    }
    // ファイルを動かした：時刻はそのまま、名前を新しいファイルのものに。
    let target = folder_of(root, to).ok_or_else(|| unsupported(to))?;
    for backup in list(root, from) {
        let name = backup_name(to, backup.taken).ok_or_else(|| unsupported(to))?;
        carry(&backup.path, &target.join(name))?;
    }
    tidy(root, folder_of(root, from).as_deref());
    Ok(())
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

/// `folder`の下にあるバックアップの形をしたファイル（`folder`からの相対パス）。
fn backup_files_under(folder: &Path) -> Vec<PathBuf> {
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
                && original_name(&entry.file_name().to_string_lossy()).is_some()
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
fn store_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
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
        let inside = backup_files_under(&root.join(&name));
        found.extend(
            inside
                .into_iter()
                .map(|relative| Path::new(&name).join(relative)),
        );
    }
    found.sort();
    found
}

/// 保存先を変えたときの失敗。
#[derive(Debug)]
pub enum MoveError {
    /// 移し先に同じ名前のバックアップがある。
    Exists(PathBuf),
    Io(io::Error),
}

/// 保存先を`from`から`to`へ変えるとき、バックアップを全部移す。
///
/// **途中で失敗したら何も変えない**（書き手の決定 2026-09-27）：全部を写し終えてから元を
/// 消すので、写す途中の失敗は写したものを消せば元どおりになる。写し終えたあと元を消せ
/// なかったものは、元の場所に残るだけで、失われるものは無い。
pub fn move_all(from: &Path, to: &Path) -> Result<(), MoveError> {
    if from == to {
        return Ok(());
    }
    // 入れ子（今の保存先の中のフォルダへ、またはその逆）も移せる：先に一覧を取り、
    // 歩くのは写しのフォルダだけなので、移し先が移し元の中にあっても数え直さない。
    let files = store_files(from);
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

/// Delete Backups…の1行：元のフォルダと、そこに属するバックアップ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub folder: PathBuf,
    pub files: Vec<PathBuf>,
}

/// 保存先にあるバックアップを、元のフォルダごとにまとめる。
///
/// `registered`（Workspaceの登録フォルダ）の中にあったものは、**いちばん深い登録フォルダ**に
/// まとめる。どこにも入らないもの（登録を外した、外で名前を変えた）は元の親フォルダで並ぶ。
pub fn groups(root: &Path, registered: &[PathBuf]) -> Vec<Group> {
    let mut grouped: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> = Default::default();
    for relative in store_files(root) {
        let Some(original_parent) = relative.parent().and_then(unmirror) else {
            continue;
        };
        let owner = registered
            .iter()
            .map(|folder| plain(folder))
            .filter(|folder| starts_with_ignoring_case(&original_parent, folder))
            .max_by_key(|folder| folder.components().count())
            .unwrap_or(original_parent);
        grouped.entry(owner).or_default().push(root.join(relative));
    }
    grouped
        .into_iter()
        .map(|(folder, files)| Group { folder, files })
        .collect()
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
    fn names_carry_the_second_and_keep_the_extension() {
        let taken = at(12);
        let name = |p: &str| backup_name(Path::new(p), taken);
        assert_eq!(
            name(r"D:\a\第一章.md").as_deref(),
            Some("第一章.2026-09-27_143012.md")
        );
        assert_eq!(
            name(r"D:\a\README").as_deref(),
            Some("README.2026-09-27_143012")
        );
        assert_eq!(
            name(r"D:\a\a.b.txt").as_deref(),
            Some("a.b.2026-09-27_143012.txt")
        );
        assert_eq!(
            stamp_in("a.b.2026-09-27_143012.txt", Path::new(r"D:\a\a.b.txt")),
            Some(taken)
        );
        // 別のファイルの名前は、頭が同じでもそのファイルのものではない。
        assert_eq!(
            stamp_in("a.b.2026-09-27_143012.txt", Path::new(r"D:\a\a.txt")),
            None
        );
        assert_eq!(
            original_name("第一章.2026-09-27_143012.md").as_deref(),
            Some("第一章.md")
        );
        assert_eq!(
            original_name("README.2026-09-27_143012").as_deref(),
            Some("README")
        );
        assert_eq!(original_name("第一章.md"), None);
        assert_eq!(original_name(".2026-09-27_143012.md"), None);
    }

    #[test]
    fn taking_keeps_the_newest_and_skips_the_same_contents() {
        let root = scratch("take-root");
        let work = scratch("take-work");
        let original = work.join("第一章.md");
        // 新しいファイル（上書きする相手が無い）では何も取らない。
        assert_eq!(take(&root, &original, 5, at(0)).unwrap(), None);
        for (second, text) in (1..=7).zip(["一", "二", "三", "四", "五", "六", "七"]) {
            fs::write(&original, text).unwrap();
            assert!(take(&root, &original, 5, at(second)).unwrap().is_some());
        }
        // 同じ中身は書かない。
        assert_eq!(take(&root, &original, 5, at(8)).unwrap(), None);
        let kept = list(&root, &original);
        assert_eq!(kept.len(), 5);
        assert_eq!(kept[0].taken, at(7));
        assert_eq!(fs::read_to_string(&kept[0].path).unwrap(), "七");
        assert_eq!(kept[4].taken, at(3));
        // 名前の形をしていない書き手のファイルは数えない。
        fs::write(kept[0].path.with_file_name("第一章.メモ.md"), "x").unwrap();
        assert_eq!(list(&root, &original).len(), 5);
    }

    #[test]
    fn the_bytes_are_kept_as_they_were() {
        let root = scratch("bytes-root");
        let work = scratch("bytes-work");
        let original = work.join("sjis.txt");
        let bytes = [0x82u8, 0xa0, b'\r', b'\n', 0x82, 0xa2];
        fs::write(&original, bytes).unwrap();
        let written = take(&root, &original, 5, at(1)).unwrap().unwrap();
        assert_eq!(fs::read(written).unwrap(), bytes);
    }

    #[test]
    fn deleting_folds_empty_folders_up_to_the_root() {
        let root = scratch("delete-root");
        let work = scratch("delete-work");
        let original = work.join("a.md");
        fs::write(&original, "x").unwrap();
        take(&root, &original, 5, at(1)).unwrap();
        fs::write(&original, "y").unwrap();
        take(&root, &original, 5, at(2)).unwrap();
        let paths: Vec<_> = list(&root, &original).into_iter().map(|b| b.path).collect();
        delete(&root, &paths[..1]).unwrap();
        assert_eq!(list(&root, &original).len(), 1);
        delete(&root, &paths[1..]).unwrap();
        assert!(list(&root, &original).is_empty());
        assert!(root.is_dir());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn backups_follow_a_renamed_file_and_a_moved_folder() {
        let root = scratch("follow-root");
        let work = scratch("follow-work");
        let chapter = work.join("章").join("第一章.md");
        fs::create_dir_all(chapter.parent().unwrap()).unwrap();
        fs::write(&chapter, "x").unwrap();
        take(&root, &chapter, 5, at(1)).unwrap();
        let renamed = work.join("章").join("序章.md");
        follow(&root, &chapter, &renamed).unwrap();
        assert!(list(&root, &chapter).is_empty());
        let moved = list(&root, &renamed);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].taken, at(1));
        let folder = work.join("第一部");
        follow(&root, &work.join("章"), &folder).unwrap();
        assert_eq!(list(&root, &folder.join("序章.md")).len(), 1);
        assert!(list(&root, &renamed).is_empty());
        // 何も無いものを動かしても何も起きない。
        follow(&root, &work.join("無い.md"), &work.join("別.md")).unwrap();
    }

    #[test]
    fn moving_everything_is_all_or_nothing() {
        let from = scratch("move-from");
        let to = scratch("move-to");
        let work = scratch("move-work");
        for name in ["a.md", "b.md"] {
            let original = work.join(name);
            fs::write(&original, name).unwrap();
            take(&from, &original, 5, at(1)).unwrap();
        }
        // 移し先に同じ名前があれば、1つも動かさない。
        let clash = to
            .join(mirror(&work).unwrap())
            .join("b.2026-09-27_143001.md");
        fs::create_dir_all(clash.parent().unwrap()).unwrap();
        fs::write(&clash, "先客").unwrap();
        assert!(matches!(move_all(&from, &to), Err(MoveError::Exists(_))));
        assert_eq!(list(&from, &work.join("a.md")).len(), 1);
        assert_eq!(list(&to, &work.join("a.md")).len(), 0);
        fs::remove_file(&clash).unwrap();
        move_all(&from, &to).unwrap();
        assert!(list(&from, &work.join("a.md")).is_empty());
        assert_eq!(list(&to, &work.join("a.md")).len(), 1);
        assert_eq!(list(&to, &work.join("b.md")).len(), 1);
        // 入れ子も移せる（書き手の報告 2026-09-27：今の保存先の中のフォルダを選んで断られた）。
        let inner = to.join("Temp");
        move_all(&to, &inner).unwrap();
        assert_eq!(list(&inner, &work.join("a.md")).len(), 1);
        assert!(list(&to, &work.join("a.md")).is_empty());
        move_all(&inner, &to).unwrap();
        assert_eq!(list(&to, &work.join("a.md")).len(), 1);
        assert!(list(&inner, &work.join("a.md")).is_empty());
    }

    #[test]
    fn a_chosen_folder_keeps_its_own_files_to_itself() {
        // 書き手の報告 2026-09-27：保存先に選んだフォルダの長い日本語の名前で落ちた。
        assert_eq!(original_name("あいうえおかきくけこさしすせそ.md"), None);
        assert_eq!(original_name("あいうえおかきくけこさしすせそ"), None);
        assert_eq!(
            original_name("長い日本語の名前の原稿.2026-09-27_143012.md").as_deref(),
            Some("長い日本語の名前の原稿.md")
        );
        let from = scratch("own-from");
        let to = scratch("own-to");
        let work = scratch("own-work");
        let original = work.join("a.md");
        fs::write(&original, "x").unwrap();
        take(&from, &original, 5, at(1)).unwrap();
        // 保存先に元からある書き手のもの：バックアップの形の名前でも、写しのフォルダの外なら触らない。
        let own = from.join("原稿").join("第一章.2026-09-27_143012.md");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, "書き手の").unwrap();
        fs::write(from.join("あいうえおかきくけこさしすせそ.md"), "書き手の").unwrap();
        move_all(&from, &to).unwrap();
        assert!(own.exists());
        assert!(from.join("あいうえおかきくけこさしすせそ.md").exists());
        assert!(!to.join("原稿").exists());
        assert_eq!(list(&to, &original).len(), 1);
        assert!(groups(&from, &[]).is_empty());
    }

    #[test]
    fn groups_gather_under_the_deepest_registered_folder() {
        let root = scratch("groups-root");
        let work = scratch("groups-work");
        let novel = work.join("長編");
        let part = novel.join("第一部");
        let loose = work.join("外");
        for path in [novel.join("a.md"), part.join("b.md"), loose.join("c.md")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "x").unwrap();
            take(&root, &path, 5, at(1)).unwrap();
        }
        // 台帳のパスは正規化してある（`\\?\C:\…`）。それでも同じフォルダとしてまとまる。
        let verbatim = |p: &Path| PathBuf::from(format!(r"\\?\{}", p.display()));
        let found = groups(&root, &[verbatim(&novel), part.clone()]);
        let folders: Vec<_> = found.iter().map(|g| g.folder.clone()).collect();
        // 一時フォルダのドライブ名は大文字（`C:\`）なので、戻したパスがそのまま比べられる。
        // 並びはパスの順（「外」は「長編」より前）。
        assert_eq!(folders, vec![loose.clone(), novel.clone(), part.clone()]);
        assert!(found.iter().all(|g| g.files.len() == 1));
    }
}
