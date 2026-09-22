//! Phase 3 of Workspace設計.md: the running state a management screen and the
//! tree read from and write through — which Workspace is active, what each
//! Workspace's tree had expanded the last time it was shown, and the ledger
//! itself, kept in one place so a caller never has to reconcile two owners of
//! the same registry.
//!
//! **No Slint, no tree rows, no index scanning.** This module holds state and
//! the rules for changing it safely; a caller (`main.rs`) is what connects it
//! to a window.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::bookmarks;
use crate::file_io;
use crate::workspace::{self, LoadStatus, Registry, RegistryError, WorkspaceId};

/// Why [`Runtime::registry`] may not be trusted for editing yet.
///
/// 仕様: "malformed 状態のレジストリを黙って空にしない" — a corrupted file on disk
/// must surface as a read error, not silently become an empty ledger a writer
/// could then save over the very bytes that might still be recovered by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryReadError {
    /// The file could not be parsed, or was too large — see
    /// [`workspace::LoadStatus::Invalid`].
    Invalid,
    /// The file could not even be opened — see [`workspace::LoadStatus::Io`].
    /// Carries only the kind: [`io::Error`] itself is not `Clone`/`PartialEq`,
    /// and nothing here needs more than that to decide what to show.
    Io(io::ErrorKind),
}

/// Everything the running editor keeps about Workspaces, on top of the ledger
/// itself.
pub struct Runtime {
    /// Where the ledger is read from and written to — the editor's own
    /// AppData directory, passed in explicitly rather than looked up here, so
    /// a test (or a future portable-mode build) never has to fight a global.
    appdata_dir: PathBuf,
    /// The ledger as last successfully loaded or saved. When
    /// [`Runtime::read_error`] is `Some`, this is an **empty** registry —
    /// never a partially-decoded guess — and every editing method refuses
    /// until [`Runtime::reset_after_read_error`] is called deliberately.
    registry: Registry,
    /// Set when the file on disk could not be trusted at open time. Cleared
    /// only by [`Runtime::reset_after_read_error`], never by a successful
    /// `edit` — there is nothing to clear once editing is already refused.
    read_error: Option<RegistryReadError>,
    /// The Workspace the tree and link completion currently scope to. `None`
    /// is a valid, deliberate state ("Workspaceなし"), distinct from "not
    /// decided yet" — there is no third state here.
    active: Option<WorkspaceId>,
    /// Which Workspace the management screen's list has selected — separate
    /// from `active`, since opening the manager to look at (or edit) a
    /// Workspace must not switch the tree out from under the writer.
    manager_selection: Option<WorkspaceId>,
    /// What each Workspace's tree had expanded, saved on
    /// [`Runtime::switch_active`] and restored the next time that Workspace
    /// becomes active. A Workspace with no entry here has never been shown
    /// this run and nothing has been loaded for it yet — see
    /// [`Runtime::expanded_for`], which is the only reader.
    ///
    /// An in-memory cache of what [`save_expanded`]/[`load_expanded`] keep on
    /// disk under this Workspace's numeric id — 仕様 "State under numeric
    /// Workspace IDs in appdata, bounded/versioned" — so a restart restores
    /// the same tree shape a switch would have, without reading the file
    /// again for a Workspace already visited this run.
    expanded: HashMap<WorkspaceId, BTreeSet<PathBuf>>,
    /// Bumped by [`Runtime::reset_view`], so a later index integration can
    /// tell "this Workspace's cache was just cleared" from "same as last
    /// time" without inventing its own counter.
    reset_generation: HashMap<WorkspaceId, u64>,
    /// 書き手の求め 2026-09-22: each Workspace's bookmarks, read from its own
    /// file the first time that Workspace's are asked for.
    bookmarks: HashMap<WorkspaceId, bookmarks::Tree>,
}

impl Runtime {
    /// Loads the ledger from `appdata_dir`. An absent file opens with an
    /// empty registry — 仕様 "registry absent の場合は空を使う" — a read error
    /// also opens empty, but is remembered in [`Runtime::read_error`] so a
    /// caller shows it rather than treating the two the same way.
    pub fn open(appdata_dir: PathBuf) -> Self {
        let (registry, read_error) = match workspace::load_status(&appdata_dir) {
            LoadStatus::Absent => (Registry::new(), None),
            LoadStatus::Loaded(registry) => (registry, None),
            LoadStatus::Invalid => (Registry::new(), Some(RegistryReadError::Invalid)),
            LoadStatus::Io(error) => (Registry::new(), Some(RegistryReadError::Io(error.kind()))),
        };
        Self {
            appdata_dir,
            registry,
            read_error,
            active: None,
            manager_selection: None,
            expanded: HashMap::new(),
            reset_generation: HashMap::new(),
            bookmarks: HashMap::new(),
        }
    }

    /// The active Workspace's bookmarks, or `None` when no Workspace is active
    /// — bookmarks belong to a Workspace.
    pub fn bookmarks(&mut self) -> Option<&mut bookmarks::Tree> {
        let active = self.active?;
        let appdata_dir = &self.appdata_dir;
        Some(
            self.bookmarks
                .entry(active)
                .or_insert_with(|| bookmarks::load(appdata_dir, active)),
        )
    }

    /// Write the active Workspace's bookmarks to its file.
    pub fn save_bookmarks(&self) -> io::Result<()> {
        let Some(active) = self.active else {
            return Ok(());
        };
        match self.bookmarks.get(&active) {
            Some(tree) => bookmarks::save(&self.appdata_dir, active, tree),
            None => Ok(()),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn read_error(&self) -> Option<&RegistryReadError> {
        self.read_error.as_ref()
    }

    /// Discards whatever could not be trusted on disk and starts over with an
    /// empty ledger — the deliberate reset 仕様 asks for, distinct from the
    /// ordinary per-Workspace [`Runtime::reset_view`]. Requires a caller to
    /// have already confirmed with the writer; this function itself does not
    /// ask.
    pub fn reset_after_read_error(&mut self) -> io::Result<()> {
        // Clear stale numeric IDs before publishing a registry which can reuse
        // them. A failed cleanup leaves the damaged registry blocked.
        clear_all_view_state(&self.appdata_dir)?;
        bookmarks::remove_all(&self.appdata_dir)?;
        self.bookmarks.clear();
        let empty = Registry::new();
        workspace::save(&self.appdata_dir, &empty)?;
        self.registry = empty;
        self.read_error = None;
        self.active = None;
        self.manager_selection = None;
        self.expanded.clear();
        self.reset_generation.clear();
        // The ledger just went back to empty, so no numeric Workspace id
        // means anything any more — a leftover view file from before the
        // corruption must not silently reattach itself to whatever new
        // Workspace happens to be created next and reuses that id.
        Ok(())
    }

    pub fn active_workspace(&self) -> Option<WorkspaceId> {
        self.active
    }

    pub fn manager_selection(&self) -> Option<WorkspaceId> {
        self.manager_selection
    }

    /// Points the management screen's own selection at `id`, without
    /// touching the active Workspace or its tree.
    pub fn select_in_manager(&mut self, id: Option<WorkspaceId>) {
        self.manager_selection = id;
    }

    /// Switches the active Workspace, saving `current_expanded` under whichever
    /// Workspace was active before and returning what the newly active one
    /// had saved (empty if it has none yet). 仕様 "切り替えで既存TAB・未保存本文
    /// を閉じない" — this only ever hands back tree state; a caller's tabs and
    /// dirty buffers are untouched because this function never sees them.
    ///
    /// The switch itself always succeeds — an unwritable AppData directory
    /// must not stop a Workspace switch, only leave the next restart without
    /// this particular tree shape. The write's own outcome is the second
    /// element, so a caller can report the failure rather than claim success
    /// it does not have; `None` when there was nothing to write (no Workspace
    /// was active before this call).
    pub fn switch_active(
        &mut self,
        id: Option<WorkspaceId>,
        current_expanded: BTreeSet<PathBuf>,
    ) -> (BTreeSet<PathBuf>, Option<io::Error>) {
        let mut write_error = None;
        if let Some(previous) = self.active {
            if let Err(error) = save_expanded(&self.appdata_dir, previous, &current_expanded) {
                write_error = Some(error);
            }
            self.expanded.insert(previous, current_expanded);
        }
        self.active = id;
        let restored = match id {
            Some(id) => self.expanded_for(id),
            None => BTreeSet::new(),
        };
        (restored, write_error)
    }

    /// Writes `expanded` to disk under the active Workspace's own id, without
    /// changing which Workspace is active or reading anything back — what a
    /// session save calls so a Workspace's tree state is not lost between
    /// switches, the same way it is not lost on an explicit one.
    ///
    /// `Ok(())` with nothing written when no Workspace is active: there is no
    /// id to save this under, which is not a failure.
    pub fn persist_active_expanded(&mut self, expanded: BTreeSet<PathBuf>) -> io::Result<()> {
        let Some(active) = self.active else {
            return Ok(());
        };
        save_expanded(&self.appdata_dir, active, &expanded)?;
        self.expanded.insert(active, expanded);
        Ok(())
    }

    /// What `id`'s tree had expanded, from the in-memory cache if this run
    /// has already loaded or saved it, otherwise from disk — populating the
    /// cache either way so a second call in the same run never re-reads the
    /// file.
    fn expanded_for(&mut self, id: WorkspaceId) -> BTreeSet<PathBuf> {
        if let Some(cached) = self.expanded.get(&id) {
            return cached.clone();
        }
        let loaded = load_expanded(&self.appdata_dir, id).unwrap_or_default();
        self.expanded.insert(id, loaded.clone());
        loaded
    }

    /// Sets the active Workspace directly, without saving or restoring any
    /// tree state — for startup, where there is no prior tree to save and the
    /// caller decides separately what the tree should show (要件: explicit
    /// folder arguments must not disturb the registered default).
    pub fn set_active_silently(&mut self, id: Option<WorkspaceId>) {
        self.active = id;
    }

    /// The active Workspace's registered roots, in the order they were added
    /// — empty when no Workspace is active, or when it references nothing.
    /// The single source of truth multi-root code reads instead of
    /// `WorkFolder::root`.
    pub fn active_roots(&self) -> Vec<PathBuf> {
        let Some(active) = self.active else {
            return Vec::new();
        };
        let Some(workspace) = self.registry.workspace(active) else {
            return Vec::new();
        };
        workspace
            .folders
            .iter()
            .filter_map(|id| self.registry.folder(*id))
            .map(|folder| folder.path.clone())
            .collect()
    }

    /// Clears a Workspace's saved tree state and bumps its reset generation —
    /// 仕様 "リセットは表示状態を初期化し、索引を再構築する。フォルダ構成・保存方式は
    /// 維持する". Never touches the ledger itself: the Workspace and its
    /// folders are exactly as they were.
    pub fn reset_view(&mut self, id: WorkspaceId) -> io::Result<u64> {
        remove_expanded(&self.appdata_dir, id)?;
        self.expanded.remove(&id);
        let generation = self.reset_generation.entry(id).or_insert(0);
        *generation += 1;
        Ok(*generation)
    }

    pub fn reset_generation(&self, id: WorkspaceId) -> u64 {
        self.reset_generation.get(&id).copied().unwrap_or(0)
    }

    /// Applies `edit` to a clone of the current registry, and only replaces
    /// [`Runtime::registry`] and persists it if both `edit` and the save
    /// succeed. 仕様の実装依頼 "clone/edit/save then publish, error must not
    /// mutate live state invisibly" — a caller sees either the whole change
    /// applied and saved, or the original registry untouched.
    ///
    /// Refused outright while [`Runtime::read_error`] is set: editing an
    /// empty stand-in for a file that could not be trusted would save over
    /// whatever might still be recovered from it by hand.
    pub fn edit<T>(
        &mut self,
        edit: impl FnOnce(&mut Registry) -> Result<T, RegistryError>,
    ) -> Result<T, EditError> {
        if self.read_error.is_some() {
            return Err(EditError::BlockedByReadError);
        }
        let mut candidate = self.registry.clone();
        let value = edit(&mut candidate).map_err(EditError::Registry)?;
        workspace::save(&self.appdata_dir, &candidate).map_err(EditError::Io)?;
        self.registry = candidate;
        Ok(value)
    }

    /// Drops a Workspace's own saved tree/reset state — what a "登録解除"
    /// (remove Workspace) control calls after [`Runtime::edit`] has removed it
    /// from the ledger. If it was the active Workspace, the active selection
    /// falls back to none, never to guessing another Workspace instead.
    pub fn forget_workspace(&mut self, id: WorkspaceId) -> io::Result<()> {
        self.expanded.remove(&id);
        self.reset_generation.remove(&id);
        self.bookmarks.remove(&id);
        let cleanup =
            remove_expanded(&self.appdata_dir, id).and(bookmarks::remove(&self.appdata_dir, id));
        if self.active == Some(id) {
            self.active = None;
        }
        if self.manager_selection == Some(id) {
            self.manager_selection = None;
        }
        cleanup
    }
}

/// Everything [`Runtime::edit`] can fail with.
#[derive(Debug)]
pub enum EditError {
    /// Refused because [`Runtime::read_error`] is set — see [`Runtime::edit`].
    BlockedByReadError,
    /// The edit closure itself refused, before anything was saved.
    Registry(RegistryError),
    /// The edit succeeded but could not be saved; the live registry was left
    /// exactly as it was before the call.
    Io(io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::BlockedByReadError => {
                write!(out, "registry could not be read; reset required")
            }
            EditError::Registry(error) => write!(out, "{error}"),
            EditError::Io(error) => write!(out, "{error}"),
        }
    }
}

impl std::error::Error for EditError {}

// --- Per-Workspace view state: a small versioned std-only format, one file --
// --- per numeric Workspace id, the same shape `workspace::encode`/`decode` --
// --- and `workspace_index`'s cache already use for their own AppData files. -

/// First line of every view-state file this build writes or reads.
const VIEW_STATE_MAGIC: &str = "RFN-EDIT-WSVIEW 1";

/// The largest view-state file this build will decode or write — far beyond
/// any real tree's worth of expanded folders, so a corrupted or hostile file
/// cannot force an unbounded read just by being large.
const MAX_VIEW_STATE_BYTES: u64 = 1024 * 1024;

fn view_state_prefix() -> &'static str {
    "workspace-view-"
}

fn view_state_suffix() -> &'static str {
    ".rfnwsview"
}

/// Where `id`'s view state lives under `appdata_dir`. The numeric id in the
/// filename is exactly what 仕様 asks the state to be keyed by — never a
/// document or folder path.
fn view_state_file(appdata_dir: &Path, id: WorkspaceId) -> PathBuf {
    appdata_dir.join(format!(
        "{}{id}{}",
        view_state_prefix(),
        view_state_suffix()
    ))
}

/// The line starting at byte offset `at`, and the offset just past its `\n` —
/// the same length-prefixed framing `workspace::encode`/`decode` use.
fn split_line(raw: &str, at: usize) -> Option<(&str, usize)> {
    let rest = raw.get(at..)?;
    let (line, _) = rest.split_once('\n')?;
    Some((line, at + line.len() + 1))
}

fn encode_expanded(paths: &BTreeSet<PathBuf>) -> String {
    let mut out = String::new();
    out.push_str(VIEW_STATE_MAGIC);
    out.push('\n');
    out.push_str(&format!("count: {}\n", paths.len()));
    for path in paths {
        let text = path.to_string_lossy();
        out.push_str(&format!("path: {}\n", text.len()));
        out.push_str(&text);
        out.push('\n');
    }
    out
}

fn decode_expanded(raw: &str) -> Option<BTreeSet<PathBuf>> {
    if raw.len() as u64 > MAX_VIEW_STATE_BYTES {
        return None;
    }
    let (magic, at) = split_line(raw, 0)?;
    if magic != VIEW_STATE_MAGIC {
        return None;
    }
    let (count_line, mut at) = split_line(raw, at)?;
    let (_, count_text) = count_line.split_once(": ")?;
    let count: usize = count_text.parse().ok()?;
    let mut paths = BTreeSet::new();
    for _ in 0..count {
        let (line, next_at) = split_line(raw, at)?;
        let (_, length_text) = line.split_once(": ")?;
        let length: usize = length_text.parse().ok()?;
        let end = next_at.checked_add(length)?;
        let text = raw.get(next_at..end)?;
        if raw.get(end..end + 1)? != "\n" {
            return None;
        }
        paths.insert(PathBuf::from(text));
        at = end + 1;
    }
    Some(paths)
}

fn save_expanded(appdata_dir: &Path, id: WorkspaceId, paths: &BTreeSet<PathBuf>) -> io::Result<()> {
    let encoded = encode_expanded(paths);
    if encoded.len() as u64 > MAX_VIEW_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace view state exceeds the maximum encoded size",
        ));
    }
    fs::create_dir_all(appdata_dir)?;
    file_io::write_atomically(&view_state_file(appdata_dir, id), encoded.as_bytes())?;
    Ok(())
}

/// Reads `id`'s view state back. `None` for anything this build refuses:
/// nothing saved yet, an oversize file, or a shape [`decode_expanded`]
/// refuses — a caller falls back to an empty (fully collapsed) tree rather
/// than treating this as fatal, the same as a missing session.
fn load_expanded(appdata_dir: &Path, id: WorkspaceId) -> Option<BTreeSet<PathBuf>> {
    let file = fs::File::open(view_state_file(appdata_dir, id)).ok()?;
    let mut buffer = Vec::new();
    file.take(MAX_VIEW_STATE_BYTES + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() as u64 > MAX_VIEW_STATE_BYTES {
        return None;
    }
    let raw = String::from_utf8(buffer).ok()?;
    decode_expanded(&raw)
}

fn remove_expanded(appdata_dir: &Path, id: WorkspaceId) -> io::Result<()> {
    match fs::remove_file(view_state_file(appdata_dir, id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes every Workspace's view state under `appdata_dir` — never a
/// directory, and never anything whose name does not match this build's own
/// naming for these files. What [`Runtime::reset_after_read_error`] calls;
/// nothing else needs to touch every Workspace's state at once.
fn clear_all_view_state(appdata_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(appdata_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(view_state_prefix())
            .and_then(|body| body.strip_suffix(view_state_suffix()))
            .is_some_and(|id| id.parse::<u64>().is_ok())
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-workspace-ui-{name}"));
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
    fn opening_with_no_file_yet_is_not_a_read_error() {
        let directory = scratch_directory("open-absent");
        let runtime = Runtime::open(directory);
        assert!(runtime.read_error().is_none());
        assert!(runtime.registry().workspaces().is_empty());
    }

    #[test]
    fn opening_a_malformed_file_reports_a_read_error_and_blocks_editing() {
        let directory = scratch_directory("open-malformed");
        fs::write(
            directory.join("workspaces.rfnworkspaces"),
            b"not a workspace ledger",
        )
        .expect("writes");

        let mut runtime = Runtime::open(directory);
        assert_eq!(runtime.read_error(), Some(&RegistryReadError::Invalid));
        assert!(runtime.registry().workspaces().is_empty());

        let result = runtime.edit(|registry| registry.create_workspace("小説".to_owned()));
        assert!(matches!(result, Err(EditError::BlockedByReadError)));
    }

    #[test]
    fn reset_after_read_error_allows_editing_again() {
        let directory = scratch_directory("reset-after-error");
        fs::write(
            directory.join("workspaces.rfnworkspaces"),
            b"not a workspace ledger",
        )
        .expect("writes");

        let mut runtime = Runtime::open(directory);
        runtime.reset_after_read_error().unwrap();
        assert!(runtime.read_error().is_none());

        let id = runtime
            .edit(|registry| registry.create_workspace("小説".to_owned()))
            .expect("edits");
        assert_eq!(runtime.registry().workspaces().len(), 1);
        assert_eq!(runtime.registry().workspace(id).unwrap().name, "小説");
    }

    #[test]
    fn registry_reset_is_persistent_without_a_subsequent_edit() {
        let directory = scratch_directory("reset-persisted");
        fs::write(directory.join("workspaces.rfnworkspaces"), b"broken").unwrap();
        let mut runtime = Runtime::open(directory.clone());
        runtime.reset_after_read_error().unwrap();
        let reopened = Runtime::open(directory);
        assert!(reopened.read_error().is_none());
        assert!(matches!(
            workspace::load_status(&reopened.appdata_dir),
            LoadStatus::Loaded(_)
        ));
    }

    #[test]
    fn failed_state_cleanup_keeps_registry_blocked_and_reset_generation_unchanged() {
        let directory = scratch_directory("reset-cleanup-failure");
        let ledger = directory.join("workspaces.rfnworkspaces");
        fs::write(&ledger, b"broken").unwrap();
        fs::create_dir(view_state_file(&directory, 1)).unwrap();
        let mut runtime = Runtime::open(directory.clone());
        assert!(runtime.reset_after_read_error().is_err());
        assert!(runtime.read_error().is_some());
        assert_eq!(fs::read(&ledger).unwrap(), b"broken");
        assert!(runtime.reset_view(1).is_err());
        assert_eq!(runtime.reset_generation(1), 0);
    }

    #[test]
    fn failed_registry_reset_write_does_not_clear_the_read_error() {
        let directory = scratch_directory("reset-write-failure");
        fs::create_dir(directory.join("workspaces.rfnworkspaces")).unwrap();
        let mut runtime = Runtime::open(directory);
        assert!(runtime.read_error().is_some());
        assert!(runtime.reset_after_read_error().is_err());
        assert!(runtime.read_error().is_some());
        assert!(matches!(
            runtime.edit(|registry| registry.create_workspace("A".into())),
            Err(EditError::BlockedByReadError)
        ));
    }

    #[test]
    fn a_failed_edit_never_mutates_the_live_registry() {
        let directory = scratch_directory("edit-failure");
        let mut runtime = Runtime::open(directory);

        let result = runtime.edit(|registry| registry.rename_workspace(999, "x".to_owned()));
        assert!(matches!(
            result,
            Err(EditError::Registry(RegistryError::UnknownWorkspace))
        ));
        assert!(runtime.registry().workspaces().is_empty());
    }

    #[test]
    fn a_successful_edit_persists_to_disk() {
        let directory = scratch_directory("edit-success");
        let mut runtime = Runtime::open(directory.clone());

        runtime
            .edit(|registry| registry.create_workspace("小説".to_owned()))
            .expect("edits");

        let reopened = Runtime::open(directory);
        assert_eq!(reopened.registry().workspaces().len(), 1);
        assert_eq!(reopened.registry().workspaces()[0].name, "小説");
    }

    #[test]
    fn switching_active_saves_and_restores_expanded_state_per_workspace() {
        let directory = scratch_directory("switch-expanded");
        let mut runtime = Runtime::open(directory);
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        let b = runtime
            .edit(|registry| registry.create_workspace("B".to_owned()))
            .expect("creates");

        runtime.set_active_silently(Some(a));
        let mut a_expanded = BTreeSet::new();
        a_expanded.insert(PathBuf::from("/a/one"));

        let (restored_for_b, error) = runtime.switch_active(Some(b), a_expanded.clone());
        assert!(error.is_none());
        assert!(restored_for_b.is_empty());

        let (restored_for_a, error) = runtime.switch_active(Some(a), BTreeSet::new());
        assert!(error.is_none());
        assert_eq!(restored_for_a, a_expanded);
    }

    #[test]
    fn active_roots_reflects_the_active_workspaces_own_folders_in_order() {
        let directory = scratch_directory("active-roots");
        let base = scratch_directory("active-roots-base");
        let one = subdirectory(&base, "one");
        let two = subdirectory(&base, "two");
        let mut runtime = Runtime::open(directory);
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime
            .edit(|registry| registry.add_root(a, &one))
            .expect("registers");
        runtime
            .edit(|registry| registry.add_root(a, &two))
            .expect("registers");

        assert!(runtime.active_roots().is_empty());
        runtime.set_active_silently(Some(a));
        assert_eq!(
            runtime.active_roots(),
            vec![one.canonicalize().unwrap(), two.canonicalize().unwrap()]
        );
    }

    #[test]
    fn reset_view_clears_expanded_state_and_bumps_generation_without_touching_the_ledger() {
        let directory = scratch_directory("reset-view");
        let mut runtime = Runtime::open(directory);
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/x"));
        let _ = runtime.switch_active(Some(a), expanded);
        let _ = runtime.switch_active(None, BTreeSet::new());

        assert_eq!(runtime.reset_generation(a), 0);
        let generation = runtime.reset_view(a).unwrap();
        assert_eq!(generation, 1);
        assert_eq!(runtime.reset_generation(a), 1);

        runtime.set_active_silently(Some(a));
        let (restored, error) = runtime.switch_active(None, BTreeSet::new());
        assert!(error.is_none());
        assert!(restored.is_empty());

        assert_eq!(runtime.registry().workspaces().len(), 1);
    }

    #[test]
    fn expanded_state_survives_a_fresh_open_of_the_same_directory() {
        let directory = scratch_directory("view-state-persists");
        let mut runtime = Runtime::open(directory.clone());
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/root/章"));
        // Switching away is what writes `a`'s state to disk.
        let (_, error) = runtime.switch_active(None, expanded.clone());
        assert!(error.is_none());

        let mut reopened = Runtime::open(directory);
        let (restored, error) = reopened.switch_active(Some(a), BTreeSet::new());
        assert!(error.is_none());
        assert_eq!(restored, expanded);
    }

    #[test]
    fn reset_view_removes_the_state_file_from_disk() {
        let directory = scratch_directory("view-state-reset-removes-file");
        let mut runtime = Runtime::open(directory.clone());
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/x"));
        let _ = runtime.switch_active(None, expanded);
        assert!(load_expanded(&directory, a).is_some());

        runtime.reset_view(a).unwrap();
        assert!(load_expanded(&directory, a).is_none());
    }

    #[test]
    fn resetting_after_a_read_error_clears_every_workspaces_view_state() {
        let directory = scratch_directory("view-state-cleared-on-reset");
        let mut runtime = Runtime::open(directory.clone());
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/x"));
        let _ = runtime.switch_active(None, expanded);
        assert!(load_expanded(&directory, a).is_some());

        fs::write(
            directory.join("workspaces.rfnworkspaces"),
            b"not a workspace ledger",
        )
        .expect("overwrites");
        let mut reopened = Runtime::open(directory.clone());
        assert!(reopened.read_error().is_some());
        reopened.reset_after_read_error().unwrap();

        assert!(load_expanded(&directory, a).is_none());
    }

    #[test]
    fn persist_active_expanded_writes_under_the_active_workspace_without_switching() {
        let directory = scratch_directory("persist-active-expanded");
        let mut runtime = Runtime::open(directory.clone());
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));

        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/session/save"));
        runtime
            .persist_active_expanded(expanded.clone())
            .expect("persists");

        assert_eq!(runtime.active_workspace(), Some(a));
        let mut reopened = Runtime::open(directory);
        let (restored, error) = reopened.switch_active(Some(a), BTreeSet::new());
        assert!(error.is_none());
        assert_eq!(restored, expanded);
    }

    #[test]
    fn persist_active_expanded_is_a_no_op_when_nothing_is_active() {
        let directory = scratch_directory("persist-active-expanded-none");
        let mut runtime = Runtime::open(directory);
        let mut expanded = BTreeSet::new();
        expanded.insert(PathBuf::from("/x"));
        assert!(runtime.persist_active_expanded(expanded).is_ok());
    }

    #[test]
    fn forgetting_the_active_workspace_falls_back_to_none() {
        let directory = scratch_directory("forget-active");
        let mut runtime = Runtime::open(directory);
        let a = runtime
            .edit(|registry| registry.create_workspace("A".to_owned()))
            .expect("creates");
        runtime.set_active_silently(Some(a));
        runtime.select_in_manager(Some(a));

        runtime.forget_workspace(a).unwrap();

        assert_eq!(runtime.active_workspace(), None);
        assert_eq!(runtime.manager_selection(), None);
    }
}
