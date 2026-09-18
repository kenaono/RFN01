//! The work folder's file tree (要件 5.2, 6.2).
//!
//! **Flattened, and for the same reason the pane layout is** (`pane_layout`):
//! Slint cannot draw a tree of unknown depth, because a component may not
//! instantiate itself. So the shape is worked out here and handed over as a
//! list of rows that each know how deep they are.
//!
//! Reading a directory is the only part that touches the disk, and it is one
//! function. Everything else — which folders are open, what order the rows come
//! in, how deep each one is — is arithmetic over what that function returned,
//! and is tested without a disk at all.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One thing in a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    pub folder: bool,
}

/// One row of the tree as it is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub folder: bool,
    /// How far in this row sits, in steps from the work folder.
    pub depth: usize,
    /// Whether this folder's contents are among the rows below it.
    pub open: bool,
    /// This row stands for one of the active Workspace's own registered
    /// roots — see [`multi_rows`]. Always `false` from [`rows`], which never
    /// draws a row for the folder it was itself called on.
    pub is_root: bool,
}

/// What one folder holds, in the order it is shown.
///
/// **Folders first, then files, each by name.** Case is ignored in the
/// comparison because a folder called `Notes` and one called `notes` sitting
/// apart in the list would look like a mistake rather than an ordering.
///
/// Entries whose name begins with a dot are left out: they are configuration
/// for other programs, and 要件 5.2 asks for the writer's documents.
pub fn read_folder(directory: &Path) -> Vec<Node> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let folder = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            Some(Node {
                name,
                path: entry.path(),
                folder,
            })
        })
        .collect();
    sort_nodes(&mut nodes);
    nodes
}

/// Folders first, then files, each by name and ignoring case.
pub fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|one, two| {
        two.folder
            .cmp(&one.folder)
            .then_with(|| one.name.to_lowercase().cmp(&two.name.to_lowercase()))
    });
}

/// The rows to draw, from the work folder down.
///
/// A folder's contents appear directly below it when it is in `expanded`, and
/// not at all when it is not — which is the whole of what "open" and "closed"
/// mean here. `read` is the only way this reaches a disk, so a test hands it a
/// folder made of nothing.
pub fn rows(
    root: &Path,
    expanded: &BTreeSet<PathBuf>,
    read: &dyn Fn(&Path) -> Vec<Node>,
) -> Vec<Row> {
    let mut out = Vec::new();
    push_rows(root, 0, expanded, read, &mut out);
    out
}

/// The rows to draw for a Workspace with more than one registered root
/// (Workspace設計.md phase 3) — each root gets its own row, at depth 0, naming
/// the folder itself rather than starting directly with its contents the way
/// [`rows`] does. A root's own row is open exactly when the root's own path is
/// in `expanded`, the same rule every other folder row follows.
///
/// **Never call this for a single classic work folder.** A Workspace with one
/// root still gets a row standing for that root; a plain "Open Folder" with no
/// Workspace does not, and must keep going through [`rows`] so the existing
/// single-folder shape (and everything built on `depth 0` meaning "child of
/// the work folder") is unchanged.
pub fn multi_rows(
    roots: &[PathBuf],
    expanded: &BTreeSet<PathBuf>,
    read: &dyn Fn(&Path) -> Vec<Node>,
) -> Vec<Row> {
    let mut out = Vec::new();
    for root in roots {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let open = expanded.contains(root);
        out.push(Row {
            name,
            path: root.clone(),
            folder: true,
            depth: 0,
            open,
            is_root: true,
        });
        if open {
            push_rows(root, 1, expanded, read, &mut out);
        }
    }
    out
}

fn push_rows(
    directory: &Path,
    depth: usize,
    expanded: &BTreeSet<PathBuf>,
    read: &dyn Fn(&Path) -> Vec<Node>,
    out: &mut Vec<Row>,
) {
    // **A folder that is not open is not read.** The rows are what is on
    // screen, and a work folder of any size costs one `read_dir` per open
    // folder rather than one per folder in it.
    for node in read(directory) {
        let open = node.folder && expanded.contains(&node.path);
        out.push(Row {
            name: node.name,
            path: node.path.clone(),
            folder: node.folder,
            depth,
            open,
            is_root: false,
        });
        if open {
            push_rows(&node.path, depth + 1, expanded, read, out);
        }
    }
}

/// Which row holds each row, by number, and `-1` for the ones the work folder
/// holds itself (要件 5.2).
///
/// **So that a row can name where a thing let go on it would land.** Pointing
/// at a file means the folder that file is in — the same rule that decides
/// where a new file is made ([`destination_folder`]) — and the row has to be
/// able to say which row that is without asking a path anything, because the
/// rows are drawn by Slint and Slint cannot ask a path what holds it.
///
/// The rows are a flattened walk, so the folder holding a row is the nearest
/// row above it that stands one step further out.
pub fn holders(rows: &[Row]) -> Vec<i32> {
    let mut out = Vec::with_capacity(rows.len());
    let mut above: Vec<usize> = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        above.truncate(row.depth);
        out.push(above.last().map_or(-1, |&row| row as i32));
        above.push(at);
    }
    out
}

/// Why a name cannot be used (要件 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    /// Nothing, or nothing but spaces.
    Empty,
    /// A character Windows does not allow in a name.
    Character(char),
    /// Windows drops a trailing dot without saying so, and the name that
    /// appeared would not be the one asked for.
    TrailingDot,
    /// A name Windows keeps for a device.
    Reserved,
}

impl NameProblem {
    /// What to tell the writer.
    pub fn message(self) -> String {
        match self {
            Self::Empty => crate::say!("名前を入れてください", "Enter a name"),
            Self::Character(bad) => {
                crate::say!("名前に {bad} は使えません", "A name cannot contain {bad}")
            }
            Self::TrailingDot => {
                crate::say!(
                    "終わりのピリオドは使えません",
                    "A name cannot end with a period"
                )
            }
            Self::Reserved => crate::say!("Windowsが使う名前です", "Windows reserves this name"),
        }
    }
}

/// The characters Windows does not allow in a name.
const FORBIDDEN: &str = "<>:\"/\\|?*";

/// Whether `name` can be a file or folder name, and the name to use if it can.
///
/// The spaces around a typed name are dropped rather than refused — nobody
/// means them — but a name that is *only* spaces is nothing.
///
/// **Asked before the disk is touched**, so the writer is told what is wrong
/// with the name they typed rather than what the filesystem made of it: the
/// error for `CON.md` is a bare "アクセスが拒否されました" from far below.
pub fn check_name(name: &str) -> Result<&str, NameProblem> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NameProblem::Empty);
    }
    let bad = name
        .chars()
        .find(|letter| letter.is_control() || FORBIDDEN.contains(*letter));
    if let Some(bad) = bad {
        return Err(NameProblem::Character(bad));
    }
    if name.ends_with('.') {
        return Err(NameProblem::TrailingDot);
    }
    if is_device_name(name) {
        return Err(NameProblem::Reserved);
    }
    Ok(name)
}

/// The names Windows keeps for devices, whatever extension is put on them.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str()) {
        return true;
    }
    if !upper.starts_with("COM") && !upper.starts_with("LPT") {
        return false;
    }
    let letters = upper.as_bytes();
    letters.len() == 4 && (b'1'..=b'9').contains(&letters[3])
}

/// A name nothing in `taken` already has, from the one asked for.
///
/// The number goes before the extension — `メモ 2.md`, never `メモ.md 2` —
/// because the extension is what says how the file opens.
pub fn unique_name(taken: &[String], desired: &str) -> String {
    if !holds(taken, desired) {
        return desired.to_string();
    }
    let (stem, extension) = split_extension(desired);
    for number in 2..1000 {
        let candidate = format!("{stem} {number}{extension}");
        if !holds(taken, &candidate) {
            return candidate;
        }
    }
    desired.to_string()
}

/// **Ignoring case, because Windows does.** Two names that differ only in case
/// are one name to the filesystem, and offering the second is offering a
/// collision.
fn holds(taken: &[String], name: &str) -> bool {
    let wanted = name.to_lowercase();
    taken.iter().any(|held| held.to_lowercase() == wanted)
}

/// The name and its extension, the dot going with the extension.
///
/// The **last** dot, so `年表.tar.gz` keeps `.gz`; a name that is all extension
/// (`.gitignore`) has none, which is the same reading the tree already takes of
/// a leading dot.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(dot) => name.split_at(dot),
    }
}

/// Where `path` ends up when `from` is renamed to `to`, if it moves at all.
///
/// **A folder carries everything under it.** This is why a rename is a path
/// calculation rather than a comparison: an open tab three folders down still
/// points at its file afterwards, which is what 要件 5.2 asks for by "タブの
/// 状態を維持する".
pub fn moved_path(from: &Path, to: &Path, path: &Path) -> Option<PathBuf> {
    if path == from {
        return Some(to.to_path_buf());
    }
    let rest = path.strip_prefix(from).ok()?;
    Some(to.join(rest))
}

/// Where `source` lands when it is carried into `into` (要件 5.2).
///
/// **Nothing, when the move would not be one.** A folder cannot go inside
/// itself or inside anything under it — what came away would be unreachable
/// from the root it was carried out of — and something let go in the folder it
/// is already in has not moved at all. Both are `None` rather than an error:
/// neither is a mistake, and neither is a thing to tell the writer about.
pub fn move_target(source: &Path, into: &Path) -> Option<PathBuf> {
    if into.starts_with(source) {
        return None;
    }
    if source.parent() == Some(into) {
        return None;
    }
    Some(into.join(source.file_name()?))
}

/// Where the rows under the one at `at` stop (要件 5.2).
///
/// The rows are a flattened walk (see [`rows`]), so everything under a folder
/// is the run directly below it — which turns "is this row inside that one"
/// into a comparison of two numbers. **It has to be one**: the rows are drawn
/// by Slint, and Slint cannot ask a path what it is under. A file, and a row
/// that is not there at all, hold nothing and stop at the next row.
pub fn subtree_end(paths: &[PathBuf], at: usize) -> usize {
    let Some(source) = paths.get(at) else {
        return at + 1;
    };
    let rest = &paths[at + 1..];
    let inside = |path: &&PathBuf| path.starts_with(source);
    at + 1 + rest.iter().take_while(inside).count()
}

/// The folder a new file or folder is made in (要件 5.2).
///
/// The selected folder itself, the selected file's folder, or the work folder
/// when nothing is selected — which is what "new" means standing in each of
/// those three places. `folder` says which of the two the selection is, and is
/// nothing to do with anything when there is no selection.
pub fn destination_folder(selected: Option<&Path>, folder: bool, root: &Path) -> PathBuf {
    let Some(path) = selected else {
        return root.to_path_buf();
    };
    if folder {
        return path.to_path_buf();
    }
    let parent = path.parent().map(Path::to_path_buf);
    parent.unwrap_or_else(|| root.to_path_buf())
}

/// Every name a folder holds, for [`unique_name`].
///
/// Unlike [`read_folder`] this keeps the names beginning with a dot: they are
/// not shown, but they are still names a new file would collide with.
pub fn names_in(directory: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Make an empty file (要件 5.2).
///
/// Fails rather than empties one that is there: the writer asked for something
/// new, and a file already at that name is somebody's work.
pub fn create_file(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    Ok(())
}

/// Make a folder (要件 5.2).
pub fn create_folder(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Give something a different name or place (要件 5.2).
///
/// **Refuses to write over what is already there.** `fs::rename` replaces a
/// file without a word, and a rename is not a way to delete a note. A name that
/// differs only in case is the same name to Windows, and is allowed through so
/// that `memo.md` can become `Memo.md`.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let same = from.to_string_lossy().to_lowercase();
    let wanted = to.to_string_lossy().to_lowercase();
    if same != wanted && to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            crate::i18n::pick(
                "同じ名前のものがあります",
                "Something with that name already exists",
            ),
        ));
    }
    fs::rename(from, to)
}

/// Copy something beside itself under a name nothing else has (要件 5.2).
///
/// Returns where the copy landed, so the tree can put the writer on it.
pub fn duplicate(path: &Path) -> io::Result<PathBuf> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::i18n::pick(
                "複製できる場所がありません",
                "There is nowhere to put a copy",
            ),
        ));
    };
    let name = path.file_name().unwrap_or_default();
    let name = name.to_string_lossy().into_owned();
    let copy = parent.join(unique_name(&names_in(parent), &name));
    if path.is_dir() {
        copy_folder(path, &copy)?;
    } else {
        fs::copy(path, &copy)?;
    }
    Ok(copy)
}

/// Copy a folder and everything under it.
fn copy_folder(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)?.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_folder(&source, &target)?;
        } else {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// The extensions a folder-wide search reads (要件 4.1, 4.2, 7.7).
///
/// **A closed list.** 要件 5.2 has the tree show attachments as well, and
/// reading an image looking for a word costs the writer time for nothing.
const SEARCHABLE: [&str; 4] = ["md", "markdown", "txt", "text"];

/// Whether a folder-wide search reads this file.
pub fn is_searchable(path: &Path) -> bool {
    let Some(extension) = path.extension() else {
        return false;
    };
    let extension = extension.to_string_lossy().to_lowercase();
    SEARCHABLE.contains(&extension.as_str())
}

/// Every searchable file under `root`, in the order the tree draws them.
///
/// **Every folder, not only the open ones**: the writer is asking about the
/// work folder, not about what is on screen — which is the one place this
/// module departs from "the rows are what is on screen".
///
/// `limit` bounds it, because a work folder can have anything dropped into it
/// and a search must not be a way to make the editor stop answering.
pub fn files_under(
    root: &Path,
    read_nodes: &dyn Fn(&Path) -> Vec<Node>,
    limit: usize,
) -> Vec<PathBuf> {
    files_under_many(&[root.to_path_buf()], read_nodes, limit)
}

/// The same as [`files_under`], across every one of `roots` under one shared
/// `limit` — a whole-Workspace search (Workspace設計.md phase 3), 仕様 "現在の
/// Workspaceの全登録フォルダが対象".
///
/// Files and directories are deduplicated by canonical identity, regardless
/// of the registration order of overlapping roots.
pub fn files_under_many(
    roots: &[PathBuf],
    read_nodes: &dyn Fn(&Path) -> Vec<Node>,
    limit: usize,
) -> Vec<PathBuf> {
    files_under_many_cancellable(roots, read_nodes, limit, &mut || false).unwrap_or_default()
}

/// A bounded, iterative walk which can abandon even a tree of empty folders.
/// Canonical identities prevent overlapping roots and directory aliases from
/// consuming the file budget twice. `None` means cancellation, never no matches.
pub fn files_under_many_cancellable(
    roots: &[PathBuf],
    read_nodes: &dyn Fn(&Path) -> Vec<Node>,
    limit: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Option<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut directories = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut pending: Vec<Node> = roots
        .iter()
        .rev()
        .map(|path| Node {
            name: String::new(),
            path: path.clone(),
            folder: true,
        })
        .collect();
    while found.len() < limit {
        if cancelled() {
            return None;
        }
        let Some(node) = pending.pop() else {
            break;
        };
        let identity = node
            .path
            .canonicalize()
            .unwrap_or_else(|_| node.path.clone());
        if node.folder {
            if directories.insert(identity) {
                pending.extend(
                    read_nodes(&node.path)
                        .into_iter()
                        .rev()
                        .filter(|child| !child.folder || !is_directory_link(&child.path)),
                );
            }
        } else if is_searchable(&node.path) && files.insert(identity) {
            found.push(node.path);
        }
    }
    Some(found)
}

fn is_directory_link(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str, folder: bool) -> Node {
        let path = PathBuf::from(path);
        Node {
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            folder,
        }
    }

    /// A folder made of nothing, so the shape can be tested without a disk.
    fn imagined(path: &Path) -> Vec<Node> {
        match path.to_string_lossy().replace('\\', "/").as_str() {
            "/work" => vec![
                node("/work/章", true),
                node("/work/資料", true),
                node("/work/はじめに.md", false),
            ],
            "/work/章" => vec![
                node("/work/章/第一章.md", false),
                node("/work/章/下書き", true),
            ],
            "/work/章/下書き" => vec![node("/work/章/下書き/断片.md", false)],
            "/work/資料" => vec![node("/work/資料/年表.txt", false)],
            "/other" => vec![node("/other/資料.md", false)],
            _ => Vec::new(),
        }
    }

    fn expanded(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// A folder nobody has opened shows only its own row.
    #[test]
    fn a_closed_folder_hides_what_is_in_it() {
        let rows = rows(Path::new("/work"), &expanded(&[]), &imagined);

        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["章", "資料", "はじめに.md"]);
        assert!(rows.iter().all(|row| row.depth == 0));
        assert!(rows.iter().all(|row| !row.open));
    }

    /// An open folder's contents come directly below it, one step further in.
    #[test]
    fn an_open_folder_puts_its_contents_below_it() {
        let rows = rows(Path::new("/work"), &expanded(&["/work/章"]), &imagined);

        let shape: Vec<(&str, usize)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("章", 0),
                ("第一章.md", 1),
                ("下書き", 1),
                ("資料", 0),
                ("はじめに.md", 0),
            ],
        );
    }

    /// Opening one deep does not open the folders on the way to it: the rows
    /// are what is on screen, and nothing else is read.
    #[test]
    fn only_the_folders_that_are_open_are_read() {
        let inner = expanded(&["/work/章", "/work/章/下書き"]);
        let rows = rows(Path::new("/work"), &inner, &imagined);

        let shape: Vec<(&str, usize)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("章", 0),
                ("第一章.md", 1),
                ("下書き", 1),
                ("断片.md", 2),
                ("資料", 0),
                ("はじめに.md", 0),
            ],
        );
        // A folder listed as open that nobody can reach changes nothing.
        let unreachable = expanded(&["/work/資料/どこか"]);
        let unchanged = super::rows(Path::new("/work"), &unreachable, &imagined);
        assert_eq!(unchanged.len(), 3);
    }

    /// Folders first, then files, each by name and ignoring case.
    #[test]
    fn folders_come_first_and_names_ignore_case() {
        let mut nodes = vec![
            node("/work/b.md", false),
            node("/work/Notes", true),
            node("/work/A.md", false),
            node("/work/archive", true),
        ];

        sort_nodes(&mut nodes);

        let names: Vec<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, vec!["archive", "Notes", "A.md", "b.md"]);
    }

    /// A name the writer typed is taken as they meant it, minus the spaces
    /// around it — and the ones Windows cannot store are refused here rather
    /// than by the filesystem.
    #[test]
    fn a_name_windows_cannot_store_is_refused_before_the_disk() {
        assert_eq!(check_name("  メモ.md  "), Ok("メモ.md"));
        assert_eq!(check_name("   "), Err(NameProblem::Empty));
        assert_eq!(check_name(""), Err(NameProblem::Empty));
        assert_eq!(check_name("a/b.md"), Err(NameProblem::Character('/')));
        assert_eq!(check_name("問い?.md"), Err(NameProblem::Character('?')));
        assert_eq!(check_name("メモ."), Err(NameProblem::TrailingDot));
        assert_eq!(check_name("con"), Err(NameProblem::Reserved));
        assert_eq!(check_name("CON.md"), Err(NameProblem::Reserved));
        assert_eq!(check_name("COM3.txt"), Err(NameProblem::Reserved));
        // Named like a device but not one of them.
        assert_eq!(check_name("COM.md"), Ok("COM.md"));
        assert_eq!(check_name("CONTENTS.md"), Ok("CONTENTS.md"));
    }

    /// The number goes before the extension, and a name already taken in
    /// another case is taken.
    #[test]
    fn a_copy_is_numbered_before_its_extension() {
        let taken = vec![
            "メモ.md".to_string(),
            "メモ 2.md".to_string(),
            "ノート".to_string(),
        ];

        assert_eq!(unique_name(&taken, "メモ.md"), "メモ 3.md");
        assert_eq!(unique_name(&taken, "ノート"), "ノート 2");
        assert_eq!(unique_name(&taken, "年表.md"), "年表.md");
        // The last dot, so a doubled extension keeps its tail.
        let doubled = ["年表.tar.gz".to_string()];
        assert_eq!(unique_name(&doubled, "年表.tar.gz"), "年表.tar 2.gz");
        // A name that is all extension has none to put the number before.
        assert_eq!(unique_name(&[".env".to_string()], ".env"), ".env 2");
    }

    /// Renaming a folder moves everything under it, which is how an open tab
    /// keeps pointing at its file (要件 5.2).
    #[test]
    fn a_renamed_folder_carries_what_is_under_it() {
        let from = Path::new("/work/章");
        let to = Path::new("/work/第一部");

        let moved = moved_path(from, to, Path::new("/work/章/下書き/断片.md"));
        assert_eq!(moved, Some(PathBuf::from("/work/第一部/下書き/断片.md")));
        assert_eq!(moved_path(from, to, from), Some(to.to_path_buf()));
        // Something the rename has nothing to do with does not move.
        let elsewhere = Path::new("/work/資料/年表.txt");
        assert_eq!(moved_path(from, to, elsewhere), None);
        // A folder whose name merely starts the same is not underneath it.
        assert_eq!(moved_path(from, to, Path::new("/work/章立て.md")), None);
    }

    /// A new file lands where the writer is standing: in the folder they have
    /// selected, beside the file they have selected, or in the work folder.
    #[test]
    fn every_row_names_the_row_that_holds_it() {
        let open = expanded(&["/work/章", "/work/章/下書き"]);
        let rows = rows(Path::new("/work"), &open, &imagined);

        // 章, 第一章.md, 下書き, 断片.md, 資料, はじめに.md — so the work folder
        // holds the first, the fifth and the sixth; 章 holds the two below it;
        // 下書き holds the one inside it.
        assert_eq!(holders(&rows), vec![-1, 0, 0, 2, -1, -1]);
    }

    #[test]
    fn a_folder_does_not_go_inside_itself_or_under_itself() {
        let source = Path::new("/w/notes");
        assert_eq!(move_target(source, Path::new("/w/notes")), None);
        assert_eq!(move_target(source, Path::new("/w/notes/2026")), None);
        // A name that merely begins with the same letters is another folder.
        let beside = move_target(source, Path::new("/w/notes2"));
        assert_eq!(beside, Some(PathBuf::from("/w/notes2/notes")));
    }

    #[test]
    fn let_go_in_the_folder_it_is_already_in_is_not_a_move() {
        let source = Path::new("/w/notes/memo.md");
        assert_eq!(move_target(source, Path::new("/w/notes")), None);
    }

    #[test]
    fn a_move_keeps_the_name_and_takes_the_folder_it_was_dropped_in() {
        let source = Path::new("/w/memo.md");
        let landed = move_target(source, Path::new("/w/notes/2026"));
        assert_eq!(landed, Some(PathBuf::from("/w/notes/2026/memo.md")));
    }

    #[test]
    fn what_is_under_a_folder_is_the_run_of_rows_below_it() {
        let paths: Vec<PathBuf> = ["/w/a", "/w/a/x.md", "/w/a/b", "/w/a/b/y.md", "/w/c.md"]
            .iter()
            .map(PathBuf::from)
            .collect();
        // The folder holds the three rows after it; the one inside it holds
        // one; a file holds none, and neither does a row nobody drew.
        assert_eq!(subtree_end(&paths, 0), 4);
        assert_eq!(subtree_end(&paths, 2), 4);
        assert_eq!(subtree_end(&paths, 4), 5);
        assert_eq!(subtree_end(&paths, 9), 10);
    }

    #[test]
    fn a_new_file_lands_where_the_writer_is_standing() {
        let root = Path::new("/work");
        let chapters = Path::new("/work/章");
        let chapter = Path::new("/work/章/第一章.md");

        let nowhere = destination_folder(None, false, root);
        assert_eq!(nowhere, PathBuf::from("/work"));
        let inside = destination_folder(Some(chapters), true, root);
        assert_eq!(inside, PathBuf::from("/work/章"));
        let beside = destination_folder(Some(chapter), false, root);
        assert_eq!(beside, PathBuf::from("/work/章"));
    }

    /// A folder-wide search reads every folder, open or not, and only the
    /// files it can read as text.
    #[test]
    fn a_search_walks_every_folder_and_only_the_text_files() {
        let found = files_under(Path::new("/work"), &imagined, 100);

        let names: Vec<String> = found
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            names,
            vec![
                "/work/章/第一章.md",
                "/work/章/下書き/断片.md",
                "/work/資料/年表.txt",
                "/work/はじめに.md",
            ],
        );
        // A limit stops the walk rather than the list.
        assert_eq!(files_under(Path::new("/work"), &imagined, 2).len(), 2);
    }

    /// A whole-Workspace search walks every registered root.
    #[test]
    fn files_under_many_walks_every_root() {
        let roots = [PathBuf::from("/work"), PathBuf::from("/other")];
        let found = files_under_many(&roots, &imagined, 100);

        let names: Vec<String> = found
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(names.contains(&"/work/はじめに.md".to_owned()));
        assert!(names.contains(&"/other/資料.md".to_owned()));
    }

    /// A root nested inside one already walked is not walked a second time —
    /// 仕様 "同じフォルダを複数Workspaceから参照できる" must not double-count it.
    #[test]
    fn files_under_many_skips_a_root_nested_inside_an_earlier_one() {
        let roots = [PathBuf::from("/work"), PathBuf::from("/work/章")];
        let found = files_under_many(&roots, &imagined, 100);

        let names: Vec<String> = found
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        // `/work/章/第一章.md` appears once, from walking `/work` — not twice
        // from also walking `/work/章` on its own.
        assert_eq!(
            names
                .iter()
                .filter(|name| *name == "/work/章/第一章.md")
                .count(),
            1
        );
    }

    /// The shared limit stops the whole walk, not just one root's share of it.
    #[test]
    fn files_under_many_shares_one_limit_across_roots() {
        let roots = [PathBuf::from("/work"), PathBuf::from("/other")];
        assert_eq!(files_under_many(&roots, &imagined, 2).len(), 2);
    }

    #[test]
    fn overlapping_roots_in_either_order_count_unique_files() {
        for roots in [
            vec![PathBuf::from("/work"), PathBuf::from("/work/章")],
            vec![
                PathBuf::from("/work/章"),
                PathBuf::from("/work"),
                PathBuf::from("/work"),
            ],
        ] {
            let found = files_under_many(&roots, &imagined, 4);
            assert_eq!(found.len(), 4);
            assert_eq!(found.iter().collect::<BTreeSet<_>>().len(), 4);
            assert!(found.contains(&PathBuf::from("/work/はじめに.md")));
        }
    }

    #[test]
    fn cyclic_directory_provider_is_walked_once() {
        let found = files_under_many(
            &[PathBuf::from("/cycle")],
            &|_| vec![node("/cycle", true), node("/cycle/note.md", false)],
            10,
        );
        assert_eq!(found, vec![PathBuf::from("/cycle/note.md")]);
    }

    #[test]
    fn empty_directory_walk_can_be_cancelled_before_enumeration_finishes() {
        let reads = std::cell::Cell::new(0);
        let found = files_under_many_cancellable(
            &[PathBuf::from("/empty")],
            &|path| {
                reads.set(reads.get() + 1);
                vec![Node {
                    name: "child".into(),
                    path: path.join("child"),
                    folder: true,
                }]
            },
            100,
            &mut || reads.get() >= 3,
        );
        assert!(found.is_none());
        assert_eq!(reads.get(), 3);
    }

    /// Only the extensions a search reads.
    #[test]
    fn an_attachment_is_not_searched() {
        assert!(is_searchable(Path::new("/work/メモ.md")));
        assert!(is_searchable(Path::new("/work/メモ.MD")));
        assert!(is_searchable(Path::new("/work/年表.txt")));
        assert!(!is_searchable(Path::new("/work/図.png")));
        assert!(!is_searchable(Path::new("/work/なまえだけ")));
    }

    /// Workspace設計.md phase 3: each active root gets its own row, closed by
    /// default, unlike `rows` which never draws a row for the folder it was
    /// itself called on.
    #[test]
    fn multi_rows_gives_each_root_its_own_closed_row() {
        let roots = [PathBuf::from("/work"), PathBuf::from("/other")];
        let rows = multi_rows(&roots, &expanded(&[]), &imagined);

        let shape: Vec<(&str, usize, bool, bool)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.depth, row.open, row.is_root))
            .collect();
        assert_eq!(
            shape,
            vec![("work", 0, false, true), ("other", 0, false, true)],
        );
    }

    /// Opening one root's row reads only that root, at depth 1 — the other
    /// root stays a single closed row of its own.
    #[test]
    fn multi_rows_opens_only_the_expanded_root() {
        let roots = [PathBuf::from("/work"), PathBuf::from("/other")];
        let rows = multi_rows(&roots, &expanded(&["/work"]), &imagined);

        let shape: Vec<(&str, usize, bool)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.depth, row.is_root))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("work", 0, true),
                ("章", 1, false),
                ("資料", 1, false),
                ("はじめに.md", 1, false),
                ("other", 0, true),
            ],
        );
    }

    /// A row from `rows` (the classic single-folder shape) is never a root
    /// row — only `multi_rows` ever sets that.
    #[test]
    fn rows_never_marks_anything_as_a_root() {
        let rows = rows(Path::new("/work"), &expanded(&["/work/章"]), &imagined);
        assert!(rows.iter().all(|row| !row.is_root));
    }
}
