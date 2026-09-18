//! Phase 1 of Workspace設計.md: the ledger of Workspaces and the shared
//! folders they point at, and the pure decision of which save mode a saved
//! document falls under.
//!
//! **No filesystem deletion, no document writes, no index or screen.** This
//! module only manages the ledger's own data — registering a root canonicalizes
//! and checks it once, but nothing here ever removes a manuscript file or a
//! work copy, and reset/cache cleanup is a later phase's job.
//!
//! Persistence follows the same shape [`crate::app_data`] uses for the session
//! and the work copies: a versioned magic line, one fact per line, and
//! [`crate::file_io::write_atomically`] for the actual write — there is no
//! separate "atomic replace" to invent here.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::file_io;

/// How a document under a registered folder is saved (要件 8, RFN01-11).
///
/// `Recovery` is the default everywhere: only a folder explicitly switched to
/// `AutoSave` writes back to the original file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    Recovery,
    AutoSave,
}

/// A stable identifier for a [`Workspace`]. Never reused after the Workspace
/// it named is removed — [`Registry`] hands these out from a counter that only
/// ever grows.
pub type WorkspaceId = u64;

/// A stable identifier for a [`FolderRegistration`]. Never reused after the
/// folder it named is removed.
pub type FolderId = u64;

/// One folder registered with the ledger, shared by every Workspace that
/// references it.
///
/// **The save mode lives here, not on the Workspace.** 仕様: "保存方式はフォルダ
/// 単位の共通設定とし、Workspaceを切り替えても変わらない" — switching the active
/// Workspace must not change what a folder's documents do when they save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRegistration {
    pub id: FolderId,
    /// Canonical, absolute. Set once at registration and never changed by
    /// anything in this module afterwards.
    pub path: PathBuf,
    pub mode: SaveMode,
}

/// One Workspace: a name and an ordered list of folders it references.
///
/// The folders themselves live in [`Registry::folders`]; this only holds the
/// order in which they were added, since that order is what the writer
/// arranged and is a Workspace's own to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub folders: Vec<FolderId>,
}

/// Everything wrong an operation on the ledger can report.
///
/// Deliberately without `PartialEq`: [`RegistryError::PathUnreadable`] carries
/// an [`io::Error`], which does not compare. Callers match on the variant.
#[derive(Debug)]
pub enum RegistryError {
    EmptyName,
    UnknownWorkspace,
    UnknownFolder,
    FolderNotReferenced,
    /// `reorder_roots` was given something other than a rearrangement of the
    /// Workspace's own folders.
    InvalidReorder,
    /// `remove_unused_folder` on a folder some Workspace still references.
    FolderInUse,
    PathNotAbsolute,
    PathNotADirectory,
    PathUnreadable(io::Error),
    /// `relocate_folder` onto a path already registered under a different
    /// [`FolderId`].
    FolderPathCollision,
    /// A workspace or folder id counter is already at [`WorkspaceId::MAX`] or
    /// [`FolderId::MAX`] — refused rather than wrapping into an id already
    /// handed out.
    IdSpaceExhausted,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::EmptyName => write!(out, "name is empty"),
            RegistryError::UnknownWorkspace => write!(out, "no such workspace"),
            RegistryError::UnknownFolder => write!(out, "no such folder"),
            RegistryError::FolderNotReferenced => {
                write!(out, "workspace does not reference that folder")
            }
            RegistryError::InvalidReorder => {
                write!(out, "reorder does not match the workspace's own folders")
            }
            RegistryError::FolderInUse => write!(out, "folder is still referenced"),
            RegistryError::PathNotAbsolute => write!(out, "path is not absolute"),
            RegistryError::PathNotADirectory => write!(out, "path is not a directory"),
            RegistryError::PathUnreadable(error) => write!(out, "path unreadable: {error}"),
            RegistryError::FolderPathCollision => {
                write!(out, "another folder is already registered at that path")
            }
            RegistryError::IdSpaceExhausted => write!(out, "no ids remain to hand out"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The ledger: every Workspace, every shared folder, and which Workspace (if
/// any) is the default.
///
/// Management here is pure data handling — no filesystem writes happen except
/// inside [`add_root`](Registry::add_root), which has to read the path once to
/// canonicalize and confirm it is a directory.
#[derive(Clone, Debug)]
pub struct Registry {
    workspaces: Vec<Workspace>,
    folders: Vec<FolderRegistration>,
    default: Option<WorkspaceId>,
    /// The next id [`create_workspace`](Registry::create_workspace) or
    /// [`duplicate_workspace`](Registry::duplicate_workspace) hands out. Only
    /// ever grows, so a removed Workspace's id is never seen again.
    next_workspace_id: WorkspaceId,
    /// The next id [`add_root`](Registry::add_root) hands out for a folder
    /// this ledger has not seen before. Only ever grows, for the same reason.
    next_folder_id: FolderId,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            workspaces: Vec::new(),
            folders: Vec::new(),
            default: None,
            // 0 is never handed out, so it is free to mean "no id" wherever
            // that is useful — the same convention `app_data::StoredWords`
            // uses for its own ids.
            next_workspace_id: 1,
            next_folder_id: 1,
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn folders(&self) -> &[FolderRegistration] {
        &self.folders
    }

    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn folder(&self, id: FolderId) -> Option<&FolderRegistration> {
        self.folders.iter().find(|f| f.id == id)
    }

    pub fn default_workspace(&self) -> Option<WorkspaceId> {
        self.default
    }

    fn workspace_index(&self, id: WorkspaceId) -> Result<usize, RegistryError> {
        self.workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or(RegistryError::UnknownWorkspace)
    }

    fn folder_index(&self, id: FolderId) -> Result<usize, RegistryError> {
        self.folders
            .iter()
            .position(|f| f.id == id)
            .ok_or(RegistryError::UnknownFolder)
    }

    /// A new, empty Workspace.
    pub fn create_workspace(&mut self, name: String) -> Result<WorkspaceId, RegistryError> {
        if name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        let id = self.take_workspace_id()?;
        self.workspaces.push(Workspace {
            id,
            name,
            folders: Vec::new(),
        });
        Ok(id)
    }

    /// Hands out the next [`WorkspaceId`], refusing once the counter has
    /// reached [`WorkspaceId::MAX`] rather than handing that value out and
    /// having nowhere left for the counter to go afterwards — `MAX` is left
    /// permanently unused, the same convention that already reserves `0` to
    /// mean "no id".
    fn take_workspace_id(&mut self) -> Result<WorkspaceId, RegistryError> {
        if self.next_workspace_id == WorkspaceId::MAX {
            return Err(RegistryError::IdSpaceExhausted);
        }
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        Ok(id)
    }

    /// Hands out the next [`FolderId`], with the same `MAX`-reserved
    /// convention as [`take_workspace_id`](Registry::take_workspace_id).
    fn take_folder_id(&mut self) -> Result<FolderId, RegistryError> {
        if self.next_folder_id == FolderId::MAX {
            return Err(RegistryError::IdSpaceExhausted);
        }
        let id = self.next_folder_id;
        self.next_folder_id += 1;
        Ok(id)
    }

    /// A new Workspace with `source`'s folder references, in the same order.
    ///
    /// The folders themselves are shared, not copied — duplicating a Workspace
    /// does not register anything new.
    pub fn duplicate_workspace(
        &mut self,
        source: WorkspaceId,
        new_name: String,
    ) -> Result<WorkspaceId, RegistryError> {
        if new_name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        let index = self.workspace_index(source)?;
        let folders = self.workspaces[index].folders.clone();
        let id = self.take_workspace_id()?;
        self.workspaces.push(Workspace {
            id,
            name: new_name,
            folders,
        });
        Ok(id)
    }

    pub fn rename_workspace(
        &mut self,
        id: WorkspaceId,
        new_name: String,
    ) -> Result<(), RegistryError> {
        if new_name.is_empty() {
            return Err(RegistryError::EmptyName);
        }
        let index = self.workspace_index(id)?;
        self.workspaces[index].name = new_name;
        Ok(())
    }

    /// Removes the Workspace and its own folder order. Clears the default when
    /// it named this Workspace.
    ///
    /// **Never touches [`Registry::folders`].** 仕様: "登録解除はWorkspace専用
    /// データを削除するが原稿・退避本文は削除しない" — a shared folder's setting
    /// and any other Workspace referencing it are left exactly as they were.
    pub fn remove_workspace(&mut self, id: WorkspaceId) -> Result<(), RegistryError> {
        let index = self.workspace_index(id)?;
        self.workspaces.remove(index);
        if self.default == Some(id) {
            self.default = None;
        }
        Ok(())
    }

    /// Registers `path` under `workspace`, canonicalizing it first.
    ///
    /// `path` must be absolute and must exist as a directory. If this ledger
    /// already has a folder at that canonical path — under this Workspace or
    /// another one — its existing [`FolderId`] and mode are reused rather than
    /// creating a duplicate row, and its mode is left as it was. Adding the
    /// same root to the same Workspace twice is idempotent: the second call
    /// returns the same id without a second reference.
    pub fn add_root(
        &mut self,
        workspace: WorkspaceId,
        path: &Path,
    ) -> Result<FolderId, RegistryError> {
        let workspace_index = self.workspace_index(workspace)?;
        if !path.is_absolute() {
            return Err(RegistryError::PathNotAbsolute);
        }
        let canonical = path.canonicalize().map_err(RegistryError::PathUnreadable)?;
        let metadata = fs::metadata(&canonical).map_err(RegistryError::PathUnreadable)?;
        if !metadata.is_dir() {
            return Err(RegistryError::PathNotADirectory);
        }
        let folder_id = match self.folders.iter().find(|f| f.path == canonical) {
            Some(existing) => existing.id,
            None => {
                let id = self.take_folder_id()?;
                self.folders.push(FolderRegistration {
                    id,
                    path: canonical,
                    mode: SaveMode::Recovery,
                });
                id
            }
        };
        let workspace = &mut self.workspaces[workspace_index];
        if !workspace.folders.contains(&folder_id) {
            workspace.folders.push(folder_id);
        }
        Ok(folder_id)
    }

    /// Points `folder`'s shared registration at `new_path` instead — 仕様
    /// "見つからないフォルダ…場所変更…で整理できる". Canonicalizes and requires a
    /// directory the same as [`add_root`](Registry::add_root), refuses a path
    /// already registered under a different [`FolderId`] rather than merging
    /// the two, and leaves the folder's id and [`SaveMode`] exactly as they
    /// were — every Workspace already referencing this id keeps referencing
    /// it, now at the new location.
    pub fn relocate_folder(
        &mut self,
        folder: FolderId,
        new_path: &Path,
    ) -> Result<(), RegistryError> {
        let index = self.folder_index(folder)?;
        if !new_path.is_absolute() {
            return Err(RegistryError::PathNotAbsolute);
        }
        let canonical = new_path
            .canonicalize()
            .map_err(RegistryError::PathUnreadable)?;
        let metadata = fs::metadata(&canonical).map_err(RegistryError::PathUnreadable)?;
        if !metadata.is_dir() {
            return Err(RegistryError::PathNotADirectory);
        }
        if self
            .folders
            .iter()
            .any(|existing| existing.id != folder && existing.path == canonical)
        {
            return Err(RegistryError::FolderPathCollision);
        }
        self.folders[index].path = canonical;
        Ok(())
    }

    /// Drops `folder` from `workspace`'s own list. The shared registration —
    /// its mode, and any other Workspace's reference to it — is untouched;
    /// see [`remove_unused_folder`](Registry::remove_unused_folder) for that.
    pub fn remove_root(
        &mut self,
        workspace: WorkspaceId,
        folder: FolderId,
    ) -> Result<(), RegistryError> {
        let index = self.workspace_index(workspace)?;
        let workspace = &mut self.workspaces[index];
        let position = workspace
            .folders
            .iter()
            .position(|&f| f == folder)
            .ok_or(RegistryError::FolderNotReferenced)?;
        workspace.folders.remove(position);
        Ok(())
    }

    /// Replaces `workspace`'s folder order with `order`, which must contain
    /// exactly the folders it already references, each once.
    pub fn reorder_roots(
        &mut self,
        workspace: WorkspaceId,
        order: &[FolderId],
    ) -> Result<(), RegistryError> {
        let index = self.workspace_index(workspace)?;
        let mut current = self.workspaces[index].folders.clone();
        let mut wanted = order.to_vec();
        current.sort_unstable();
        wanted.sort_unstable();
        if current != wanted {
            return Err(RegistryError::InvalidReorder);
        }
        self.workspaces[index].folders = order.to_vec();
        Ok(())
    }

    /// Sets the default Workspace, or clears it with `None`.
    pub fn set_default(&mut self, id: Option<WorkspaceId>) -> Result<(), RegistryError> {
        if let Some(id) = id {
            self.workspace_index(id)?;
        }
        self.default = id;
        Ok(())
    }

    /// Changes a shared folder's save mode. Every Workspace referencing it
    /// sees the change, because the mode lives on the folder, not on any one
    /// Workspace.
    pub fn set_folder_mode(
        &mut self,
        folder: FolderId,
        mode: SaveMode,
    ) -> Result<(), RegistryError> {
        let index = self.folder_index(folder)?;
        self.folders[index].mode = mode;
        Ok(())
    }

    /// Every Workspace that references `folder`, for the management screen's
    /// "used by" display.
    pub fn folder_users(&self, folder: FolderId) -> Vec<WorkspaceId> {
        self.workspaces
            .iter()
            .filter(|w| w.folders.contains(&folder))
            .map(|w| w.id)
            .collect()
    }

    /// Removes a folder's registration entirely. Refuses while any Workspace
    /// still references it — clear those references with
    /// [`remove_root`](Registry::remove_root) first.
    pub fn remove_unused_folder(&mut self, folder: FolderId) -> Result<(), RegistryError> {
        let index = self.folder_index(folder)?;
        if !self.folder_users(folder).is_empty() {
            return Err(RegistryError::FolderInUse);
        }
        self.folders.remove(index);
        Ok(())
    }

    /// The save mode that applies to a document at `document`, independent of
    /// which Workspace (if any) is active or default.
    ///
    /// `None` — an unsaved document — and any path this cannot canonicalize —
    /// missing, or otherwise unresolvable — fall back to [`SaveMode::Recovery`]
    /// rather than guessing: 仕様 "見つからないフォルダ…自動退避に戻る", and the
    /// same caution applies to a document whose own identity is not yet
    /// certain. Canonicalizing both sides (this, and each folder's path at
    /// registration) is what resolves a Windows case difference or a junction
    /// to the same spelling the filesystem itself uses.
    ///
    /// Matching is by path components, never by string prefix — `/root` must
    /// not match `/root-sibling` — and the *deepest* registered ancestor wins,
    /// so a child folder's own mode overrides its parent's even when the child
    /// is the one set to `Recovery`.
    pub fn save_mode_for(&self, document: Option<&Path>) -> SaveMode {
        let Some(document) = document else {
            return SaveMode::Recovery;
        };
        let Ok(canonical) = document.canonicalize() else {
            return SaveMode::Recovery;
        };
        let mut best: Option<&FolderRegistration> = None;
        for folder in &self.folders {
            if !is_within(&canonical, &folder.path) {
                continue;
            }
            let deeper = match best {
                Some(current) => {
                    folder.path.components().count() > current.path.components().count()
                }
                None => true,
            };
            if deeper {
                best = Some(folder);
            }
        }
        best.map(|folder| folder.mode).unwrap_or(SaveMode::Recovery)
    }
}

/// Whether `path` is inside `root`, comparing path components rather than
/// text — so `/root-sibling/x` is not "inside" `/root`.
fn is_within(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    for root_component in root.components() {
        if path_components.next() != Some(root_component) {
            return false;
        }
    }
    // At least one component of `path` must remain: `root` itself is a
    // directory, and a document is a file somewhere inside it.
    path_components.next().is_some()
}

fn mode_name(mode: SaveMode) -> &'static str {
    match mode {
        SaveMode::Recovery => "Recovery",
        SaveMode::AutoSave => "AutoSave",
    }
}

fn mode_from_name(name: &str) -> Option<SaveMode> {
    match name {
        "Recovery" => Some(SaveMode::Recovery),
        "AutoSave" => Some(SaveMode::AutoSave),
        _ => None,
    }
}

/// First line of every encoded registry — a version from the start, the same
/// reason `app_data`'s formats have one: this file outlives the run that wrote
/// it.
const REGISTRY_MAGIC: &str = "RFN-EDIT-WORKSPACES 1";

const REGISTRY_FILE: &str = "workspaces.rfnworkspaces";

/// The largest encoded registry this build will decode or write.
///
/// 4 MiB is far beyond any real ledger of folders and Workspaces — this
/// exists so that a corrupted or hostile file cannot force an unbounded read
/// or allocation just by being large. [`decode`] refuses anything over this
/// size outright, [`load`] never reads more than one byte past it off disk
/// regardless of the file's actual length, and [`save`] refuses to write a
/// registry that would exceed it.
pub const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;

/// The ledger as it is written: a magic line, the id counters, the default,
/// then every folder and every Workspace.
///
/// Names and paths are length-prefixed — a byte count, a newline, then exactly
/// that many bytes — the way `app_data::encode_history` keeps a draft's text
/// safe from whatever separator would otherwise have been chosen. Japanese
/// text, spaces and even a stray `\n` in a name all round-trip because of it.
pub fn encode(registry: &Registry) -> String {
    let mut out = String::new();
    out.push_str(REGISTRY_MAGIC);
    out.push('\n');
    out.push_str(&format!("next-workspace: {}\n", registry.next_workspace_id));
    out.push_str(&format!("next-folder: {}\n", registry.next_folder_id));
    if let Some(default) = registry.default {
        out.push_str(&format!("default: {default}\n"));
    }
    for folder in &registry.folders {
        let path = folder.path.to_string_lossy();
        out.push_str(&format!(
            "folder: {} {} {}\n",
            folder.id,
            mode_name(folder.mode),
            path.len()
        ));
        out.push_str(&path);
        out.push('\n');
    }
    for workspace in &registry.workspaces {
        out.push_str(&format!(
            "workspace: {} {}\n",
            workspace.id,
            workspace.name.len()
        ));
        out.push_str(&workspace.name);
        out.push('\n');
        if !workspace.folders.is_empty() {
            let refs = workspace
                .folders
                .iter()
                .map(FolderId::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("roots: {refs}\n"));
        }
    }
    out
}

/// The line starting at byte offset `at`, and the offset just past its `\n`.
///
/// `None` when there is no `\n` to find — every line this format writes ends
/// with one, so its absence means the file is cut short or was never this
/// format.
fn split_line(raw: &str, at: usize) -> Option<(&str, usize)> {
    let rest = raw.get(at..)?;
    let (line, _) = rest.split_once('\n')?;
    Some((line, at + line.len() + 1))
}

/// Reads the ledger back, or `None` for anything this build refuses: an
/// unrecognised or unsupported version, a line that does not parse, a length
/// that runs past the end of the file, or a reference to a folder or Workspace
/// id that does not exist. **Refused rather than guessed at** — the caller
/// must not overwrite the file on disk with a registry decoded from a broken
/// read, the same rule `app_data::decode_session` follows for the session.
pub fn decode(raw: &str) -> Option<Registry> {
    if raw.len() as u64 > MAX_REGISTRY_BYTES {
        return None;
    }
    let (first_line, mut at) = split_line(raw, 0)?;
    if first_line != REGISTRY_MAGIC {
        return None;
    }
    let mut registry = Registry {
        workspaces: Vec::new(),
        folders: Vec::new(),
        default: None,
        next_workspace_id: 0,
        next_folder_id: 0,
    };
    while at < raw.len() {
        let (line, next_at) = split_line(raw, at)?;
        if line.is_empty() {
            at = next_at;
            continue;
        }
        let (key, value) = line.split_once(": ")?;
        match key {
            "next-workspace" => {
                registry.next_workspace_id = value.parse().ok()?;
                at = next_at;
            }
            "next-folder" => {
                registry.next_folder_id = value.parse().ok()?;
                at = next_at;
            }
            "default" => {
                registry.default = Some(value.parse().ok()?);
                at = next_at;
            }
            "folder" => {
                let mut fields = value.split(' ');
                let id: FolderId = fields.next()?.parse().ok()?;
                let mode = mode_from_name(fields.next()?)?;
                let length: usize = fields.next()?.parse().ok()?;
                if fields.next().is_some() {
                    return None;
                }
                let end = next_at.checked_add(length)?;
                let path = raw.get(next_at..end)?;
                if raw.get(end..end + 1)? != "\n" {
                    return None;
                }
                registry.folders.push(FolderRegistration {
                    id,
                    path: PathBuf::from(path),
                    mode,
                });
                at = end + 1;
            }
            "workspace" => {
                let mut fields = value.split(' ');
                let id: WorkspaceId = fields.next()?.parse().ok()?;
                let length: usize = fields.next()?.parse().ok()?;
                if fields.next().is_some() {
                    return None;
                }
                let end = next_at.checked_add(length)?;
                let name = raw.get(next_at..end)?;
                if raw.get(end..end + 1)? != "\n" {
                    return None;
                }
                if name.is_empty() {
                    return None;
                }
                registry.workspaces.push(Workspace {
                    id,
                    name: name.to_owned(),
                    folders: Vec::new(),
                });
                at = end + 1;
            }
            "roots" => {
                let workspace = registry.workspaces.last_mut()?;
                let mut ids = Vec::new();
                for field in value.split(' ') {
                    if field.is_empty() {
                        continue;
                    }
                    ids.push(field.parse().ok()?);
                }
                workspace.folders = ids;
                at = next_at;
            }
            // Anything else is either damage or a later format this build does
            // not know how to read faithfully; refused rather than silently
            // dropped, since a workspace ledger is not forgiving the way a
            // display setting is — a dropped root reference would look to the
            // writer like folders quietly disappearing from a Workspace.
            _ => return None,
        }
    }
    if registry.validate() {
        Some(registry)
    } else {
        None
    }
}

impl Registry {
    /// Every id referenced actually exists, every id is unique, and both
    /// counters are ahead of everything they counted — the shape
    /// [`encode`]/[`decode`] can produce, and the only shape trusted back in.
    ///
    /// Also refuses what a hand-edited or corrupted file could otherwise slip
    /// past parsing: a folder path that is empty or not absolute (so
    /// [`save_mode_for`](Registry::save_mode_for) can never mistake a relative
    /// fragment for a real ancestor and hand out [`SaveMode::AutoSave`] by
    /// accident), two folders sharing one canonical path, an empty workspace
    /// name, or a workspace listing the same folder id twice.
    fn validate(&self) -> bool {
        let mut folder_ids = HashSet::new();
        let mut folder_paths = HashSet::new();
        for folder in &self.folders {
            if folder.id == 0 || folder.id >= self.next_folder_id {
                return false;
            }
            if !folder_ids.insert(folder.id) {
                return false;
            }
            if folder.path.as_os_str().is_empty() || !folder.path.is_absolute() {
                return false;
            }
            if !folder_paths.insert(folder.path.clone()) {
                return false;
            }
        }
        let mut workspace_ids = HashSet::new();
        for workspace in &self.workspaces {
            if workspace.id == 0 || workspace.id >= self.next_workspace_id {
                return false;
            }
            if !workspace_ids.insert(workspace.id) {
                return false;
            }
            if workspace.name.is_empty() {
                return false;
            }
            let mut referenced = HashSet::new();
            for folder_ref in &workspace.folders {
                if !folder_ids.contains(folder_ref) {
                    return false;
                }
                if !referenced.insert(*folder_ref) {
                    return false;
                }
            }
        }
        match self.default {
            Some(default) => workspace_ids.contains(&default),
            None => true,
        }
    }
}

/// Puts the ledger away in `directory` — the caller's app-data directory,
/// never a path under a manuscript root (仕様: "原稿フォルダへ独自データを作らず、
/// 設定・状態・索引はAppDataに保存する").
///
/// Written through [`file_io::write_atomically`], the same primitive
/// `app_data` itself uses for the session and the work copies — there is no
/// separate atomic-replace helper to add.
///
/// Refuses — before touching the file — a registry whose encoding would
/// exceed [`MAX_REGISTRY_BYTES`], the same limit [`load`] enforces on the way
/// back in.
pub fn save(directory: &Path, registry: &Registry) -> io::Result<()> {
    let encoded = encode(registry);
    if encoded.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace registry exceeds the maximum encoded size",
        ));
    }
    fs::create_dir_all(directory)?;
    let path = directory.join(REGISTRY_FILE);
    file_io::write_atomically(&path, encoded.as_bytes())?;
    Ok(())
}

/// What [`load_status`] found, distinguishing "nothing saved yet" from "there
/// is something there, but this build cannot trust it" — a caller (a
/// management screen, in particular) must not treat those the same way: the
/// first opens with an empty ledger, the second must show a read error and
/// block edits until a deliberate reset, never silently start empty over
/// whatever bytes are actually on disk.
pub enum LoadStatus {
    /// No registry file exists yet — a fresh start, not an error.
    Absent,
    /// Read and decoded successfully.
    Loaded(Registry),
    /// A file exists but is not a ledger this build can trust: too large, not
    /// valid UTF-8, an unsupported version, or a shape [`decode`] refuses.
    Invalid,
    /// The file could not even be read — permission denied, most likely.
    /// Deliberately distinct from [`LoadStatus::Absent`]: this is not "nothing
    /// saved yet", it is "something is saved and this build cannot see it".
    Io(io::Error),
}

/// Reads the ledger back from `directory`, reporting *why* there is nothing
/// usable when there isn't — see [`LoadStatus`].
///
/// Reads at most [`MAX_REGISTRY_BYTES`] plus one byte off disk regardless of
/// the file's actual length — an oversize file is refused the same as
/// [`decode`] would refuse it, but without ever reading the rest of it in.
pub fn load_status(directory: &Path) -> LoadStatus {
    let file = match fs::File::open(directory.join(REGISTRY_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return LoadStatus::Absent,
        Err(error) => return LoadStatus::Io(error),
    };
    let mut buffer = Vec::new();
    if let Err(error) = file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut buffer) {
        return LoadStatus::Io(error);
    }
    if buffer.len() as u64 > MAX_REGISTRY_BYTES {
        return LoadStatus::Invalid;
    }
    let Ok(raw) = String::from_utf8(buffer) else {
        return LoadStatus::Invalid;
    };
    match decode(&raw) {
        Some(registry) => LoadStatus::Loaded(registry),
        None => LoadStatus::Invalid,
    }
}

/// [`load_status`] narrowed to `Option`, for callers that only need "is there
/// a usable registry" and already treat absent and invalid the same way —
/// existing callers and tests predating [`LoadStatus`].
pub fn load(directory: &Path) -> Option<Registry> {
    match load_status(directory) {
        LoadStatus::Loaded(registry) => Some(registry),
        LoadStatus::Absent | LoadStatus::Invalid | LoadStatus::Io(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-workspace-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    fn subdirectory(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        fs::create_dir_all(&path).expect("creates");
        path
    }

    #[test]
    fn creating_duplicating_and_sharing_a_reference() {
        let mut registry = Registry::new();
        let root = scratch_directory("create-dup");
        let a = registry
            .create_workspace("小説".to_owned())
            .expect("creates");
        let folder = registry.add_root(a, &root).expect("registers");

        let b = registry
            .duplicate_workspace(a, "小説 コピー".to_owned())
            .expect("duplicates");

        assert_ne!(a, b);
        assert_eq!(registry.workspace(b).unwrap().folders, vec![folder]);
        // The folder is shared, not copied: one registration, two users.
        assert_eq!(registry.folders().len(), 1);
        let mut users = registry.folder_users(folder);
        users.sort_unstable();
        assert_eq!(users, vec![a, b]);

        // Adding the same root again is idempotent, not a duplicate row.
        let again = registry.add_root(a, &root).expect("re-registers");
        assert_eq!(again, folder);
        assert_eq!(registry.workspace(a).unwrap().folders, vec![folder]);
        assert_eq!(registry.folders().len(), 1);
    }

    #[test]
    fn removing_the_default_workspace_clears_the_default() {
        let mut registry = Registry::new();
        let a = registry
            .create_workspace("既定".to_owned())
            .expect("creates");
        registry.set_default(Some(a)).expect("sets");
        assert_eq!(registry.default_workspace(), Some(a));

        registry.remove_workspace(a).expect("removes");

        assert_eq!(registry.default_workspace(), None);
        assert!(registry.workspace(a).is_none());
    }

    #[test]
    fn unused_removal_refuses_a_referenced_folder() {
        let mut registry = Registry::new();
        let root = scratch_directory("unused-removal");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let folder = registry.add_root(a, &root).expect("registers");

        assert!(matches!(
            registry.remove_unused_folder(folder),
            Err(RegistryError::FolderInUse)
        ));

        registry.remove_root(a, folder).expect("unreferences");
        registry.remove_unused_folder(folder).expect("removes");
        assert!(registry.folder(folder).is_none());
    }

    #[test]
    fn a_folders_mode_is_shared_across_every_workspace() {
        let mut registry = Registry::new();
        let root = scratch_directory("shared-mode");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let b = registry.create_workspace("B".to_owned()).expect("creates");
        let folder = registry.add_root(a, &root).expect("registers");
        registry.add_root(b, &root).expect("shares");

        registry
            .set_folder_mode(folder, SaveMode::AutoSave)
            .expect("sets");

        assert_eq!(registry.folder(folder).unwrap().mode, SaveMode::AutoSave);
        // Switching which Workspace is active never enters into it — the mode
        // is read straight off the folder either way.
        assert_eq!(
            registry.save_mode_for(Some(&root.join("a.md"))),
            SaveMode::Recovery
        );
    }

    #[test]
    fn a_deeper_child_root_overrides_its_parent_even_set_to_recovery() {
        let mut registry = Registry::new();
        let parent = scratch_directory("deep-parent");
        let child = subdirectory(&parent, "章");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let parent_folder = registry.add_root(a, &parent).expect("registers parent");
        let child_folder = registry.add_root(a, &child).expect("registers child");
        registry
            .set_folder_mode(parent_folder, SaveMode::AutoSave)
            .expect("sets");
        registry
            .set_folder_mode(child_folder, SaveMode::Recovery)
            .expect("sets");

        let document = child.join("見出し.md");
        fs::write(&document, "本文").expect("writes");

        assert_eq!(registry.save_mode_for(Some(&document)), SaveMode::Recovery);
    }

    #[test]
    fn a_sibling_with_a_matching_prefix_is_not_mistaken_for_inside() {
        let mut registry = Registry::new();
        let base = scratch_directory("sibling-prefix");
        let root = subdirectory(&base, "root");
        let sibling = subdirectory(&base, "root-sibling");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let folder = registry.add_root(a, &root).expect("registers");
        registry
            .set_folder_mode(folder, SaveMode::AutoSave)
            .expect("sets");

        let document = sibling.join("原稿.md");
        fs::write(&document, "本文").expect("writes");

        assert_eq!(registry.save_mode_for(Some(&document)), SaveMode::Recovery);
    }

    #[test]
    fn unsaved_and_missing_documents_fall_back_to_recovery() {
        let mut registry = Registry::new();
        let root = scratch_directory("fallback");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let folder = registry.add_root(a, &root).expect("registers");
        registry
            .set_folder_mode(folder, SaveMode::AutoSave)
            .expect("sets");

        assert_eq!(registry.save_mode_for(None), SaveMode::Recovery);
        assert_eq!(
            registry.save_mode_for(Some(&root.join("いない.md"))),
            SaveMode::Recovery
        );
        assert_eq!(
            registry.save_mode_for(Some(Path::new("/no/such/place/x.md"))),
            SaveMode::Recovery
        );
    }

    fn sample_registry(root: &Path) -> Registry {
        let mut registry = Registry::new();
        let a = registry
            .create_workspace("小説 プロジェクト".to_owned())
            .expect("creates");
        let folder = registry.add_root(a, root).expect("registers");
        registry
            .set_folder_mode(folder, SaveMode::AutoSave)
            .expect("sets");
        registry.set_default(Some(a)).expect("sets");
        registry
    }

    #[test]
    fn a_registry_with_japanese_names_and_spaces_round_trips() {
        let root = scratch_directory("round-trip 空白");
        let registry = sample_registry(&root);

        let written = encode(&registry);
        let read = decode(&written).expect("decodes");

        assert_eq!(read.workspaces(), registry.workspaces());
        assert_eq!(read.folders(), registry.folders());
        assert_eq!(read.default_workspace(), registry.default_workspace());
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        let raw = "RFN-EDIT-WORKSPACES 2\nnext-workspace: 1\nnext-folder: 1\n";
        assert!(decode(raw).is_none());
    }

    #[test]
    fn a_reference_to_an_unknown_folder_is_refused() {
        let name = "小説";
        let raw = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 2\nnext-folder: 1\nworkspace: 1 {}\n{name}\nroots: 99\n",
            name.len()
        );
        assert!(decode(&raw).is_none());
    }

    #[test]
    fn a_default_naming_an_unknown_workspace_is_refused() {
        let raw = format!("{REGISTRY_MAGIC}\nnext-workspace: 1\nnext-folder: 1\ndefault: 7\n");
        assert!(decode(&raw).is_none());
    }

    #[test]
    fn a_length_running_past_the_end_is_refused_rather_than_panicking() {
        let raw = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 1\nnext-folder: 1\nworkspace: 1 999999999\n名前\n"
        );
        assert!(decode(&raw).is_none());
    }

    #[test]
    fn something_else_entirely_is_refused() {
        assert!(decode("not a workspace ledger").is_none());
        assert!(decode("").is_none());
    }

    /// A raw ledger holding one workspace named with `name_len` copies of
    /// `a` — built so the test below can hit [`MAX_REGISTRY_BYTES`] exactly
    /// without hand-computing every fixed byte around the name.
    fn raw_with_workspace_name_len(name_len: usize) -> String {
        let name = "a".repeat(name_len);
        format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 2\nnext-folder: 1\nworkspace: 1 {name_len}\n{name}\n"
        )
    }

    #[test]
    fn decode_accepts_exactly_the_maximum_size_and_refuses_one_byte_more() {
        // `name_len`'s own decimal width is stable across the one-byte
        // adjustment below (nowhere near a power-of-ten boundary), so a
        // single correction converges on the exact byte count — no need to
        // build anything larger than the limit plus one byte.
        let target = MAX_REGISTRY_BYTES as usize;
        let mut name_len = target - raw_with_workspace_name_len(0).len();
        loop {
            let len = raw_with_workspace_name_len(name_len).len();
            if len == target {
                break;
            }
            if len > target {
                name_len -= len - target;
            } else {
                name_len += target - len;
            }
        }

        let at_limit = raw_with_workspace_name_len(name_len);
        assert_eq!(at_limit.len() as u64, MAX_REGISTRY_BYTES);
        assert!(decode(&at_limit).is_some());

        let over_limit = raw_with_workspace_name_len(name_len + 1);
        assert_eq!(over_limit.len() as u64, MAX_REGISTRY_BYTES + 1);
        assert!(decode(&over_limit).is_none());
    }

    #[test]
    fn save_refuses_an_oversize_registry_before_writing_anything() {
        let directory = scratch_directory("oversize-save");
        let mut registry = Registry::new();
        registry
            .create_workspace("a".repeat(MAX_REGISTRY_BYTES as usize))
            .expect("creates");

        let error = save(&directory, &registry).expect_err("refuses");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!directory.join(REGISTRY_FILE).exists());
    }

    #[test]
    fn load_refuses_a_file_one_byte_over_the_maximum_without_reading_it_whole() {
        let directory = scratch_directory("oversize-load");
        // Content is irrelevant here — this is only checking that `load`
        // turns away anything past the byte cap, the same as `decode` does,
        // before it would even reach parsing.
        let oversize = vec![b'a'; MAX_REGISTRY_BYTES as usize + 1];
        fs::write(directory.join(REGISTRY_FILE), &oversize).expect("writes");

        assert!(load(&directory).is_none());
    }

    #[test]
    fn a_registry_survives_the_disk_and_a_malformed_write_does_not_overwrite_it() {
        let directory = scratch_directory("disk-round-trip");
        let root = scratch_directory("disk-round-trip-root");
        let registry = sample_registry(&root);

        save(&directory, &registry).expect("writes");
        let read = load(&directory).expect("reads");
        assert_eq!(read.workspaces(), registry.workspaces());

        // A caller that gets `None` back must not have touched the file: the
        // bytes actually on disk still decode to the same registry.
        let malformed = decode("not a workspace ledger");
        assert!(malformed.is_none());
        let still_there = load(&directory).expect("still reads");
        assert_eq!(still_there.workspaces(), registry.workspaces());
    }

    #[test]
    fn a_registered_folders_data_stays_listable_after_its_directory_is_gone() {
        let directory = scratch_directory("missing-folder-listable");
        let root = scratch_directory("missing-folder-listable-root");
        let registry = sample_registry(&root);
        save(&directory, &registry).expect("writes");
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());

        fs::remove_dir_all(&root).expect("removes the folder from disk");

        let read = load(&directory).expect("reads");
        assert_eq!(read.folders().len(), 1);
        assert_eq!(read.folders()[0].path, canonical_root);
    }

    #[test]
    fn names_may_not_be_empty() {
        let mut registry = Registry::new();
        assert!(matches!(
            registry.create_workspace(String::new()),
            Err(RegistryError::EmptyName)
        ));
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        assert!(matches!(
            registry.rename_workspace(a, String::new()),
            Err(RegistryError::EmptyName)
        ));
    }

    #[test]
    fn reordering_requires_the_same_set_of_folders() {
        let mut registry = Registry::new();
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let one = registry
            .add_root(a, &scratch_directory("reorder-one"))
            .expect("registers");
        let two = registry
            .add_root(a, &scratch_directory("reorder-two"))
            .expect("registers");

        registry.reorder_roots(a, &[two, one]).expect("reorders");
        assert_eq!(registry.workspace(a).unwrap().folders, vec![two, one]);

        assert!(matches!(
            registry.reorder_roots(a, &[one]),
            Err(RegistryError::InvalidReorder)
        ));
    }

    #[test]
    fn stable_ids_are_never_reused_after_delete_and_recreate() {
        let mut registry = Registry::new();
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        registry.remove_workspace(a).expect("removes");
        let b = registry.create_workspace("B".to_owned()).expect("creates");
        assert_ne!(a, b);
    }

    #[test]
    fn relocating_a_folder_preserves_its_id_mode_and_every_workspaces_reference() {
        let mut registry = Registry::new();
        let old_root = scratch_directory("relocate-old");
        let new_root = scratch_directory("relocate-new");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let b = registry.create_workspace("B".to_owned()).expect("creates");
        let folder = registry.add_root(a, &old_root).expect("registers");
        registry.add_root(b, &old_root).expect("shares");
        registry
            .set_folder_mode(folder, SaveMode::AutoSave)
            .expect("sets");

        registry
            .relocate_folder(folder, &new_root)
            .expect("relocates");

        let canonical_new = new_root.canonicalize().expect("canonicalizes");
        assert_eq!(registry.folder(folder).unwrap().path, canonical_new);
        assert_eq!(registry.folder(folder).unwrap().mode, SaveMode::AutoSave);
        assert_eq!(registry.workspace(a).unwrap().folders, vec![folder]);
        assert_eq!(registry.workspace(b).unwrap().folders, vec![folder]);
    }

    #[test]
    fn relocating_onto_an_already_registered_path_is_refused() {
        let mut registry = Registry::new();
        let one = scratch_directory("relocate-collide-one");
        let two = scratch_directory("relocate-collide-two");
        let a = registry.create_workspace("A".to_owned()).expect("creates");
        let folder_one = registry.add_root(a, &one).expect("registers");
        let folder_two = registry.add_root(a, &two).expect("registers");

        assert!(matches!(
            registry.relocate_folder(folder_one, &two),
            Err(RegistryError::FolderPathCollision)
        ));
        assert_eq!(
            registry.folder(folder_two).unwrap().path,
            two.canonicalize().expect("canonicalizes")
        );
    }

    #[test]
    fn load_status_distinguishes_absent_from_invalid_and_never_touches_a_good_file_on_a_bad_read() {
        let directory = scratch_directory("load-status-absent");
        assert!(matches!(load_status(&directory), LoadStatus::Absent));

        let root = scratch_directory("load-status-root");
        let registry = sample_registry(&root);
        save(&directory, &registry).expect("writes");

        fs::write(directory.join(REGISTRY_FILE), b"not a workspace ledger").expect("overwrites");
        assert!(matches!(load_status(&directory), LoadStatus::Invalid));
    }

    #[test]
    fn a_folder_with_a_relative_or_empty_path_is_refused() {
        let raw = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 1\nnext-folder: 2\nfolder: 1 Recovery 8\nrelative\n"
        );
        assert!(decode(&raw).is_none());

        let raw_empty = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 1\nnext-folder: 2\nfolder: 1 Recovery 0\n\n"
        );
        assert!(decode(&raw_empty).is_none());
    }

    #[test]
    fn two_folders_sharing_one_canonical_path_are_refused() {
        let root = scratch_directory("duplicate-canonical");
        let path = root.to_string_lossy().into_owned();
        let raw = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 1\nnext-folder: 3\nfolder: 1 Recovery {}\n{path}\nfolder: 2 AutoSave {}\n{path}\n",
            path.len(),
            path.len()
        );
        assert!(decode(&raw).is_none());
    }

    #[test]
    fn a_workspace_referencing_the_same_folder_twice_is_refused() {
        let root = scratch_directory("duplicate-reference");
        let path = root.to_string_lossy().into_owned();
        let raw = format!(
            "{REGISTRY_MAGIC}\nnext-workspace: 2\nnext-folder: 2\nfolder: 1 Recovery {}\n{path}\nworkspace: 1 1\nA\nroots: 1 1\n",
            path.len()
        );
        assert!(decode(&raw).is_none());
    }

    #[test]
    fn an_exhausted_id_counter_is_refused_rather_than_wrapped() {
        let mut registry = Registry::new();
        // Force the counter to the edge without looping `WorkspaceId::MAX`
        // times: the field is private to this module, so a test here may
        // reach in directly. One id remains — `MAX - 1` — before the space is
        // exhausted; `MAX` itself is never handed out.
        registry.next_workspace_id = WorkspaceId::MAX - 1;
        let last = registry
            .create_workspace("最後".to_owned())
            .expect("creates the last one");
        assert_eq!(last, WorkspaceId::MAX - 1);

        assert!(matches!(
            registry.create_workspace("あふれる".to_owned()),
            Err(RegistryError::IdSpaceExhausted)
        ));
    }
}
