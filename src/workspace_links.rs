//! Phase 5 (engine subset) of Workspace設計.md: the layer between
//! [`crate::workspace_index::Indexer`] and a later UI worker — active-scope
//! lifecycle, link completion as a small state machine, and pure link
//! resolution.
//!
//! No Slint or document writes. Index scans and link-validity presence checks
//! run on cancellable background workers; presentation consumes snapshots.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::document;
use crate::link_completion::{self, Candidate, Context};
use crate::workspace::WorkspaceId;
use crate::workspace_index::{
    Entry, Event, EventKind, Indexer, MAINTENANCE_GENERATION, ScanOptions, Snapshot,
};

#[derive(Clone)]
pub struct ValidityDocument {
    pub id: usize,
    pub path: Option<PathBuf>,
    pub text: String,
}

type InvalidTargets = Vec<(usize, Vec<std::ops::Range<usize>>)>;
struct ValidityRequest {
    complete: bool,
    generation: u64,
    roots: Vec<PathBuf>,
    entries: std::sync::Arc<Vec<Entry>>,
    documents: Vec<ValidityDocument>,
}

/// One bounded worker. Missing-path probes never run during layout or on the UI thread.
pub struct ValidityChecker {
    sender: std::sync::mpsc::SyncSender<ValidityRequest>,
    receiver: std::sync::mpsc::Receiver<(u64, InvalidTargets)>,
    current: std::sync::Arc<std::sync::atomic::AtomicU64>,
    next: u64,
}

impl ValidityChecker {
    pub fn new() -> Self {
        let (sender, requests) = std::sync::mpsc::sync_channel::<ValidityRequest>(1);
        let (results, receiver) = std::sync::mpsc::channel();
        let current = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let running = current.clone();
        let _ = std::thread::Builder::new()
            .name("link-validity".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    let matches =
                        || running.load(std::sync::atomic::Ordering::Relaxed) == request.generation;
                    if !matches() {
                        continue;
                    }
                    let mut invalid = Vec::new();
                    let healthy = request.roots.iter().all(|root| readable_root(root));
                    for doc in &request.documents {
                        if !matches() {
                            break;
                        }
                        let mut ranges = if healthy {
                            invalid_destinations(
                                doc,
                                &request.roots,
                                &request.entries,
                                &request.documents,
                                &|path| {
                                    if matches() {
                                        probe_local(path, &request.roots)
                                    } else {
                                        Presence::Unknown
                                    }
                                },
                                &matches,
                            )
                        } else {
                            Vec::new()
                        };
                        if !request.complete {
                            // Partial inventories cannot prove absence or uniqueness of a name.
                            ranges.retain(|r| {
                                let raw = doc.text[r.clone()].trim().trim_matches(['<', '>']);
                                raw.starts_with('#')
                                    || raw.starts_with("./")
                                    || raw.starts_with("../")
                                    || Path::new(raw).is_absolute()
                            });
                        }
                        invalid.push((doc.id, ranges));
                    }
                    if matches() {
                        let _ = results.send((request.generation, invalid));
                    }
                }
            });
        Self {
            sender,
            receiver,
            current,
            next: 0,
        }
    }

    pub fn cancel(&mut self) {
        self.next = self.next.wrapping_add(1);
        self.current
            .store(self.next, std::sync::atomic::Ordering::Relaxed);
    }

    /// `complete` says whether the name list behind `entries` covers every
    /// root in `roots`. A partial list can still suggest a target but cannot
    /// prove one is missing or unique, so an incomplete request keeps only the
    /// targets the document itself spells out literally (`#`, `./`, `../`, or
    /// an absolute path) — see the retain below. Every caller (the tick, the
    /// scope reset, a rename) goes through here.
    pub fn submit_scoped(
        &mut self,
        roots: Vec<PathBuf>,
        entries: std::sync::Arc<Vec<Entry>>,
        documents: Vec<ValidityDocument>,
        complete: bool,
    ) -> Option<u64> {
        self.cancel();
        let generation = self.next;
        self.sender
            .try_send(ValidityRequest {
                complete,
                generation,
                roots,
                entries,
                documents,
            })
            .ok()
            .map(|_| generation)
    }

    pub fn poll(&self) -> Option<(u64, InvalidTargets)> {
        self.receiver
            .try_iter()
            .filter(|(generation, _)| {
                *generation == self.current.load(std::sync::atomic::Ordering::Relaxed)
            })
            .last()
    }
}

impl Drop for ValidityChecker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Copy)]
enum Presence {
    File(crate::file_io::FileStamp),
    Missing,
    Unknown,
}

fn lexical_path(path: &Path) -> PathBuf {
    let mut spelling = link_completion::path_to_string(path);
    #[cfg(windows)]
    if spelling.as_bytes().get(1) == Some(&b':') {
        spelling.replace_range(..1, &spelling[..1].to_ascii_uppercase());
    }
    let mut result = PathBuf::new();
    for component in Path::new(&spelling).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
                if result.file_name().is_some_and(|name| name != "..") =>
            {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

fn contained(path: &Path, roots: &[PathBuf]) -> bool {
    let path = lexical_path(path);
    roots
        .iter()
        .any(|root| path.starts_with(lexical_path(root)))
}

fn readable_root(root: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() || std::fs::read_dir(root).is_err() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

fn uncertain_traversal(path: &Path, roots: &[PathBuf], inspect: &impl Fn(&Path) -> bool) -> bool {
    let spelling = link_completion::path_to_string(path);
    let mut prefix = PathBuf::new();
    for part in Path::new(&spelling).components() {
        prefix.push(part);
        if contained(&prefix, roots) && inspect(&prefix) {
            return true;
        }
    }
    false
}

fn unsafe_metadata(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return true;
                }
            }
            metadata.file_type().is_symlink()
        }
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn probe_local(path: &Path, roots: &[PathBuf]) -> Presence {
    // Inspect the original walk first: collapsing junction/.. would erase the
    // evidence that the filesystem traversal leaves the indexed tree.
    if uncertain_traversal(path, roots, &unsafe_metadata) {
        return Presence::Unknown;
    }
    let path = lexical_path(path);
    let Some(root) = roots
        .iter()
        .map(|root| lexical_path(root))
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
    else {
        return Presence::Unknown;
    };
    if !readable_root(&root) {
        return Presence::Unknown;
    }
    let mut checked = root.clone();
    // Even a cache-complete index intentionally skips excluded/reparse trees.
    for part in path.strip_prefix(&root).unwrap().components() {
        if matches!(part.as_os_str().to_str(), Some(".git" | "target")) {
            return Presence::Unknown;
        }
        checked.push(part);
        match std::fs::symlink_metadata(&checked) {
            Ok(meta) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Presence::Unknown;
                    }
                }
                if meta.file_type().is_symlink() {
                    return Presence::Unknown;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Presence::Missing,
            Err(_) => return Presence::Unknown,
        }
    }
    if !path.is_file() {
        return Presence::Unknown;
    }
    crate::file_io::FileStamp::read(&path)
        .map(Presence::File)
        .unwrap_or(Presence::Unknown)
}

fn invalid_destinations(
    doc: &ValidityDocument,
    roots: &[PathBuf],
    entries: &[Entry],
    overlays: &[ValidityDocument],
    probe: &impl Fn(&Path) -> Presence,
    current: &impl Fn() -> bool,
) -> Vec<std::ops::Range<usize>> {
    let Some(source_file) = doc.path.as_deref().filter(|path| contained(path, roots)) else {
        return Vec::new();
    };
    let mut bad = Vec::new();
    for (range, wiki) in document::link_target_ranges(&doc.text) {
        if !current() {
            break;
        }
        let target = &doc.text[range.clone()];
        let trimmed = target.trim().trim_start_matches('<').trim_end_matches('>');
        let (file, _) = link_completion::split_target_heading(trimmed);
        let decoded = link_completion::percent_decode(file);
        if decoded.contains(':') && !Path::new(&decoded).is_absolute() {
            continue;
        }
        let invalid = match resolve_link(target, wiki, Some(source_file), &doc.text, entries) {
            Err(ResolveError::NotFound)
                if wiki && !decoded.is_empty() && !decoded.contains(['/', '\\', ':']) =>
            {
                true
            }
            Err(ResolveError::Ambiguous(paths)) => paths
                .iter()
                .all(|path| contained(path, roots) && matches!(probe(path), Presence::File(_))),
            Ok(ResolvedLink::SameFileHeading { result }) => {
                matches!(result, HeadingLookup::Missing | HeadingLookup::Ambiguous(_))
            }
            Ok(ResolvedLink::Target { path, heading }) if contained(&path, roots) => {
                match probe(&path) {
                    Presence::Missing => true,
                    Presence::Unknown => false,
                    Presence::File(stamp) => {
                        if let Some(heading) = heading {
                            if let Some(live) = overlays.iter().find(|other| {
                                other
                                    .path
                                    .as_ref()
                                    .is_some_and(|p| lexical_path(p) == lexical_path(&path))
                            }) {
                                matches!(
                                    find_heading_occurrence(
                                        &live.text,
                                        &heading.text,
                                        heading.occurrence
                                    ),
                                    HeadingLookup::Missing | HeadingLookup::Ambiguous(_)
                                )
                            } else if let Some(entry) = entries.iter().find(|entry| {
                                lexical_path(&entry.canonical) == lexical_path(&path)
                                    && entry.fingerprint == stamp
                                    && entry.headings_complete
                            }) {
                                let count = entry
                                    .headings
                                    .iter()
                                    .filter(|found| found.text == heading.text)
                                    .count();
                                heading
                                    .occurrence
                                    .map_or(count != 1, |n| n == 0 || n > count)
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    }
                }
            }
            _ => false,
        };
        if invalid {
            bad.push(range);
        }
    }
    bad
}

#[cfg(test)]
mod validity_tests {
    use super::*;
    fn path(name: &str) -> PathBuf {
        Path::new(if cfg!(windows) {
            "C:/active"
        } else {
            "/active"
        })
        .join(name)
    }
    fn doc(text: &str) -> ValidityDocument {
        ValidityDocument {
            id: 1,
            path: Some(path("source.md")),
            text: text.into(),
        }
    }
    fn stamp() -> crate::file_io::FileStamp {
        crate::file_io::FileStamp {
            modified: None,
            length: 1,
        }
    }
    fn entry(name: &str) -> Entry {
        Entry {
            root: path(""),
            relative: name.into(),
            canonical: path(name),
            fingerprint: stamp(),
            headings: Vec::new(),
            headings_complete: true,
        }
    }
    #[test]
    fn validation_requires_a_healthy_completed_active_scope() {
        let mut index = WorkspaceLinks::new(None);
        index.identity = Some(ScopeIdentity {
            workspace: Some(1),
            roots: vec![path("")],
            reset_generation: 0,
        });
        assert!(index.validation_roots().is_none());
        index.completed = true;
        assert!(index.validation_roots().is_some());
        for status in [
            Status {
                busy: true,
                ..Default::default()
            },
            Status {
                failed_dirs: 1,
                ..Default::default()
            },
            Status {
                missing_roots: 1,
                ..Default::default()
            },
            Status {
                truncated: true,
                ..Default::default()
            },
            Status {
                cache_write_failed: 1,
                ..Default::default()
            },
            Status {
                error: Some("denied".into()),
                ..Default::default()
            },
        ] {
            index.status = status;
            assert!(index.validation_roots().is_none());
        }
        index.status = Status::default();
        index.identity.as_mut().unwrap().workspace = None;
        assert!(
            index.validation_roots().is_none(),
            "another Workspace's cache does not establish current scope"
        );
    }
    #[test]
    fn invalid_only_when_missing_or_ambiguous_inside_active_roots() {
        let input = doc("[[./missing.md|alias]] [[exists.md]] [[duplicate.md]] [[unknown.md]]");
        let entries = [
            entry("exists.md"),
            entry("a/duplicate.md"),
            entry("b/duplicate.md"),
        ];
        let ranges = invalid_destinations(
            &input,
            &[path("")],
            &entries,
            &[],
            &|p| {
                if p.ends_with("missing.md") {
                    Presence::Missing
                } else {
                    Presence::File(stamp())
                }
            },
            &|| true,
        );
        let targets: Vec<_> = ranges.iter().map(|r| &input.text[r.clone()]).collect();
        assert_eq!(targets, ["./missing.md", "duplicate.md", "unknown.md"]);
        let uncertain = doc("[[./missing.md]]");
        assert!(
            invalid_destinations(
                &uncertain,
                &[path("")],
                &[],
                &[],
                &|_| Presence::Unknown,
                &|| true
            )
            .is_empty()
        );
        assert!(
            invalid_destinations(
                &uncertain,
                &[path("")],
                &[],
                &[],
                &|_| Presence::Missing,
                &|| false
            )
            .is_empty()
        );
    }
    #[test]
    fn other_workspace_and_outside_targets_remain_unknown_without_probes() {
        let outside = Path::new(if cfg!(windows) { "D:/other" } else { "/other" });
        let mut input = doc(&format!(
            "[[{}/missing.md]] [web](https://example.com)",
            outside.display()
        ));
        assert!(
            invalid_destinations(
                &input,
                &[path("")],
                &[],
                &[],
                &|_| panic!("must not probe outside roots"),
                &|| true
            )
            .is_empty()
        );
        input.path = Some(outside.join("source.md"));
        input.text = "[[unknown.md]]".into();
        assert!(
            invalid_destinations(
                &input,
                &[path("")],
                &[entry("known.md")],
                &[],
                &|_| panic!("source outside scope"),
                &|| true
            )
            .is_empty()
        );
    }
    #[test]
    fn heading_validation_uses_live_overlay_and_does_not_trust_stale_index_text() {
        let input = doc("[[./target.md#heading]]");
        let stale = entry("target.md");
        let live = ValidityDocument {
            id: 2,
            path: Some(path("target.md")),
            text: "# heading\n".into(),
        };
        assert!(
            invalid_destinations(
                &input,
                &[path("")],
                std::slice::from_ref(&stale),
                &[live],
                &|_| Presence::File(stamp()),
                &|| true
            )
            .is_empty()
        );
        assert_eq!(
            invalid_destinations(
                &input,
                &[path("")],
                std::slice::from_ref(&stale),
                &[],
                &|_| Presence::File(stamp()),
                &|| true
            )
            .len(),
            1
        );
        let changed = crate::file_io::FileStamp {
            modified: None,
            length: 2,
        };
        assert!(
            invalid_destinations(
                &input,
                &[path("")],
                &[stale],
                &[],
                &|_| Presence::File(changed),
                &|| true
            )
            .is_empty()
        );
    }
    #[test]
    fn original_parent_traversal_cannot_hide_a_junction() {
        let roots = [path("")];
        let traversal = path("junction/../missing.md");
        assert!(uncertain_traversal(&traversal, &roots, &|p| p == path("junction")));
        let siblings = [path("one"), path("two")];
        assert!(!uncertain_traversal(
            &path("one/../two/file.md"),
            &siblings,
            &|_| false
        ));
    }
    #[test]
    fn worker_discards_superseded_results_and_missing_roots_are_unknown() {
        let root = std::env::temp_dir().join(format!(
            "rfn-validity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut worker = ValidityChecker::new();
        let documents = vec![ValidityDocument {
            id: 1,
            path: Some(root.join("source.md")),
            text: "[[missing.md]]".repeat(2000),
        }];
        worker.submit_scoped(
            vec![root.clone()],
            std::sync::Arc::new(Vec::new()),
            documents,
            true,
        );
        worker.cancel();
        let until = Instant::now() + Duration::from_secs(3);
        let newest = loop {
            if let Some(id) = worker.submit_scoped(
                vec![root.clone()],
                std::sync::Arc::new(Vec::new()),
                vec![ValidityDocument {
                    id: 2,
                    path: Some(root.join("source.md")),
                    text: "plain".into(),
                }],
                true,
            ) {
                break id;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        };
        loop {
            if let Some((generation, values)) = worker.poll() {
                assert_eq!(generation, newest);
                assert_eq!(values, vec![(2, Vec::new())]);
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
        std::fs::remove_dir(&root).unwrap();
        assert!(matches!(
            probe_local(&root.join("missing.md"), &[root]),
            Presence::Unknown
        ));
    }
}

// --- Scope lifecycle --------------------------------------------------------

/// What scope is currently indexed — everything that must stay equal for a
/// previous scan's results to still apply. Read each tick from
/// `workspace_ui::Runtime`'s own `active_workspace`/`active_roots`/
/// `reset_generation` — this module never reads the ledger itself.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeIdentity {
    workspace: Option<WorkspaceId>,
    roots: Vec<PathBuf>,
    reset_generation: u64,
}

/// Coarse state a caller shows instead of blocking the editor on a scan —
/// 仕様 "索引更新中/partial error and number, not a spinner that blocks the
/// editor".
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub busy: bool,
    /// Directories the running/last scan could not read.
    pub failed_dirs: usize,
    /// Requested roots that could not even be canonicalized.
    pub missing_roots: usize,
    /// Whether `ScanOptions::max_entries` cut the last scan short.
    pub truncated: bool,
    /// How many per-root cache writes have failed since the current scope
    /// started — the index itself is still fine; only its on-disk cache is
    /// stale for those roots.
    pub cache_write_failed: usize,
    /// The scan could not proceed at all, or the worker thread itself is
    /// gone. Set alongside `busy: false`.
    pub error: Option<String>,
}

/// Owns one [`Indexer`] and the [`Snapshot`] folded from its events, scoped
/// to whichever Workspace (or none) is currently active. A caller polls this
/// on a timer; nothing here blocks.
pub struct WorkspaceLinks {
    names_complete: bool,
    force_reparse: bool,
    recent: Vec<(Vec<PathBuf>, Vec<Entry>, Instant)>,
    /// The one view every caller reads: the shared index with open documents
    /// overlaid. Held behind [`std::sync::Arc`] so a caller that needs to keep
    /// or hand it to a worker shares it instead of copying every entry (the
    /// index holds up to [`crate::workspace_index::MAX_ENTRIES`] files).
    visible: std::sync::Arc<Vec<Entry>>,
    /// Set by anything that can change `visible` (a folded entry event, a new
    /// priority overlay, a scope change) so a poll that merely reported
    /// progress — `Complete`, `InventoryComplete`, an error — does not pay for
    /// rebuilding and re-comparing the whole view.
    visible_dirty: bool,
    view_metrics: ViewMetrics,
    priority_entries: Vec<Entry>,
    background_revision: u64,
    completed: bool,
    revision: u64,
    appdata_dir: Option<PathBuf>,
    indexer: Indexer,
    snapshot: Snapshot,
    identity: Option<ScopeIdentity>,
    /// The generation `Indexer::restart`/`restart_memory` returned for the
    /// scan currently backing `identity` — events tagged with any other
    /// generation are stale (a scan superseded by `sync_scope` before it
    /// noticed) and are dropped rather than folded, so a scope change never
    /// shows even a transient frame of the previous scope's files.
    expected_generation: Option<u64>,
    status: Status,
    last_scan_started: Option<Instant>,
    /// The most recent error from `clear_cache`, kept separate from
    /// `Status::error` — a maintenance operation for any Workspace (active or
    /// not) must never be folded into the *active scan's* own status.
    last_maintenance_error: Option<String>,
}

/// What keeping the shared view fresh cost since the last drain — counts and
/// times only, never a name or a path (要件: 本文・ファイル名を記録しない).
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewMetrics {
    pub rebuilds: u64,
    /// Building the view (map, filter, sort).
    pub rebuild_ms: f64,
    /// Deciding whether the rebuilt view differs from the published one.
    pub compare_ms: f64,
    /// Entries in the published view, as of the last rebuild.
    pub entries: usize,
}

impl ViewMetrics {
    pub fn log(&self) -> String {
        format!(
            "index_view rebuilds={} rebuild_ms={:.3} compare_ms={:.3} entries={}",
            self.rebuilds, self.rebuild_ms, self.compare_ms, self.entries
        )
    }
}

impl WorkspaceLinks {
    pub fn retain_shared(&mut self, roots: Vec<PathBuf>) {
        self.recent.retain(|(covered, _, _)| {
            covered.iter().any(|r| {
                roots.iter().any(|keep| {
                    contained(r, std::slice::from_ref(keep))
                        || contained(keep, std::slice::from_ref(r))
                })
            })
        });
        if let Some(appdata) = &self.appdata_dir {
            self.indexer
                .retain_shared(appdata.join("workspace-index/shared-v2"), roots);
        }
    }
    /// `appdata_dir` is the editor's own AppData directory, or `None` when it
    /// is unavailable — every scan then runs through `Indexer::restart_memory`
    /// instead, and `clear_cache` does nothing (仕様 "Appdata unavailable =>
    /// memory-only indexing, no temp cache under manuscript cwd").
    pub fn new(appdata_dir: Option<PathBuf>) -> Self {
        Self {
            names_complete: false,
            force_reparse: false,
            recent: Vec::new(),
            visible: std::sync::Arc::new(Vec::new()),
            visible_dirty: false,
            view_metrics: ViewMetrics::default(),
            priority_entries: Vec::new(),
            background_revision: 0,
            completed: false,
            revision: 0,
            appdata_dir,
            indexer: Indexer::new(),
            snapshot: Snapshot::new(),
            identity: None,
            expected_generation: None,
            status: Status::default(),
            last_scan_started: None,
            last_maintenance_error: None,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.visible
    }

    /// The same view as [`WorkspaceLinks::entries`], shared rather than
    /// copied — what callers that keep it (a validity request, a priority
    /// request, the picture index) should take. It is replaced, never
    /// mutated in place, so a holder always sees one consistent snapshot.
    pub fn entries_shared(&self) -> std::sync::Arc<Vec<Entry>> {
        self.visible.clone()
    }

    /// Drains what [`WorkspaceLinks::rebuild_visible`] cost since the last
    /// call, for the caller that writes it to the diagnostics log.
    pub fn take_view_metrics(&mut self) -> ViewMetrics {
        std::mem::take(&mut self.view_metrics)
    }

    pub fn background_revision(&self) -> u64 {
        self.background_revision
    }
    pub fn names_complete(&self) -> bool {
        self.names_complete
    }
    /// Called only by the explicit open operation, never by layout or candidate generation.
    pub fn resolve_for_open(
        &self,
        target: &str,
        wiki: bool,
        source: Option<&Path>,
        text: &str,
    ) -> Result<ResolvedLink, ResolveError> {
        let raw = target.trim().trim_matches(['<', '>']);
        let (file, heading) = link_completion::split_target_heading(raw);
        let decoded = link_completion::percent_decode(file);
        let direct = resolve_explicit_path(&decoded, source);
        if !file.is_empty() && direct.as_ref().is_some_and(|p| p.is_file()) {
            return Ok(ResolvedLink::Target {
                path: direct.unwrap(),
                heading: heading.map(parse_heading_spec),
            });
        }
        let resolved = resolve_link(target, wiki, source, text, self.entries())?;
        if !self.names_complete {
            if let ResolvedLink::Target { path, .. } = &resolved {
                let local = direct.as_ref().is_some_and(|p| {
                    lexical_path(p) == lexical_path(path)
                        || lexical_path(&PathBuf::from(format!("{}.md", p.display())))
                            == lexical_path(path)
                });
                if !local {
                    return Err(ResolveError::NotFound);
                }
            }
        }
        Ok(resolved)
    }
    pub fn roots(&self) -> Vec<PathBuf> {
        self.identity
            .as_ref()
            .map(|s| s.roots.clone())
            .unwrap_or_default()
    }
    pub fn take_metrics(&self) -> Vec<crate::workspace_index::ScanMetrics> {
        self.indexer.take_metrics()
    }
    pub fn set_priority(&mut self, entries: Vec<Entry>) {
        if self.priority_entries != entries {
            self.priority_entries = entries;
            self.visible_dirty = true;
            self.rebuild_visible();
        }
    }
    fn rebuild_visible(&mut self) {
        if !self.visible_dirty {
            return;
        }
        self.visible_dirty = false;
        let started = Instant::now();
        let roots = self.roots();
        let mut entries: std::collections::HashMap<PathBuf, Entry> = self
            .snapshot
            .entries()
            .iter()
            .cloned()
            .map(|e| (e.canonical.clone(), e))
            .collect();
        for entry in &self.priority_entries {
            entries.insert(entry.canonical.clone(), entry.clone());
        }
        let mut next: Vec<_> = entries
            .into_values()
            .filter(|e| {
                crate::workspace_index::is_indexable(&e.canonical)
                    && contained(&e.canonical, &roots)
            })
            .collect();
        next.sort_by(|a, b| a.canonical.cmp(&b.canonical));
        let rebuilt = started.elapsed();
        let compared = Instant::now();
        let changed = self.visible.as_slice() != next.as_slice();
        let compared = compared.elapsed();
        if changed {
            self.visible = std::sync::Arc::new(next);
            self.revision = self.revision.wrapping_add(1);
        }
        self.view_metrics.rebuilds += 1;
        self.view_metrics.rebuild_ms += rebuilt.as_secs_f64() * 1000.0;
        self.view_metrics.compare_ms += compared.as_secs_f64() * 1000.0;
        self.view_metrics.entries = self.visible.len();
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn validation_roots(&self) -> Option<&[PathBuf]> {
        let status = &self.status;
        let scope = self.identity.as_ref()?;
        (scope.workspace.is_some()
            && !scope.roots.is_empty()
            && self.completed
            && !status.busy
            && status.error.is_none()
            && status.failed_dirs == 0
            && status.missing_roots == 0
            && !status.truncated
            && status.cache_write_failed == 0)
            .then_some(scope.roots.as_slice())
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn last_maintenance_error(&self) -> Option<&str> {
        self.last_maintenance_error.as_deref()
    }

    /// Points this at a scope — the identity a caller reads off
    /// `workspace_ui::Runtime` each tick. A no-op when `workspace`, `roots`,
    /// and `reset_generation` all match what is already active, so a caller
    /// can call this unconditionally every tick without restarting a scan
    /// still in flight or already `Complete`. Any change invalidates the
    /// snapshot immediately, cancels whatever scan was running, and starts a
    /// fresh one; empty `roots` (including `workspace: None`) starts none.
    pub fn sync_scope(
        &mut self,
        workspace: Option<WorkspaceId>,
        roots: Vec<PathBuf>,
        reset_generation: u64,
    ) {
        // Defensive: `roots` without a Workspace must never be indexed —
        // 仕様 "no active Workspace => no workspace index". A caller passing
        // some leftover/temporary folder list alongside `workspace: None` is
        // honored as "no scope" rather than silently scanning it.
        let roots = if workspace.is_none() {
            Vec::new()
        } else {
            roots
        };
        let candidate = ScopeIdentity {
            workspace,
            roots,
            reset_generation,
        };
        if self.identity.as_ref() == Some(&candidate) {
            return;
        }
        self.force_reparse = self.identity.as_ref().is_some_and(|old| {
            old.workspace == candidate.workspace
                && old.reset_generation != candidate.reset_generation
        });
        self.indexer.cancel();
        self.names_complete = false;
        self.visible = std::sync::Arc::new(Vec::new());
        self.visible_dirty = true;
        self.priority_entries.clear();
        self.background_revision = self.background_revision.wrapping_add(1);
        self.completed = false;
        self.revision = self.revision.wrapping_add(1);
        self.snapshot = Snapshot::new();
        self.status = Status::default();
        self.expected_generation = None;
        self.last_scan_started = None;
        let started = !candidate.roots.is_empty();
        self.identity = Some(candidate);
        if started {
            let roots = self.roots();
            if let Some((_, entries, at)) = self.recent.iter().rev().find(|(covered, _, at)| {
                !self.force_reparse
                    && at.elapsed() < Duration::from_secs(30)
                    && roots.iter().all(|r| contained(r, covered))
            }) {
                let entries = entries
                    .iter()
                    .filter_map(|e| {
                        let root = roots
                            .iter()
                            .filter(|r| contained(&e.canonical, std::slice::from_ref(r)))
                            .max_by_key(|r| r.components().count())?;
                        let mut e = e.clone();
                        e.root = lexical_path(root);
                        e.relative = lexical_path(&e.canonical)
                            .strip_prefix(&e.root)
                            .ok()?
                            .to_path_buf();
                        Some(e)
                    })
                    .collect();
                let at = *at;
                self.snapshot.apply(Event {
                    generation: 0,
                    kind: EventKind::Cached(entries),
                });
                self.completed = true;
                self.names_complete = true;
                self.last_scan_started = Some(at);
                self.rebuild_visible();
                return;
            }
            self.start_scan();
        }
    }

    fn start_scan(&mut self) {
        self.names_complete = false;
        self.completed = false;
        let identity = self
            .identity
            .clone()
            .expect("start_scan is only called right after identity is set");
        // Reset per-scan status fresh on every start — including a periodic
        // rescan via `maybe_rescan`, which does not go through `sync_scope` —
        // so a prior run's error/truncated/etc never lingers past a
        // successful refresh.
        self.status = Status {
            busy: true,
            ..Status::default()
        };
        self.last_scan_started = Some(Instant::now());
        let options = ScanOptions {
            force_reparse: std::mem::take(&mut self.force_reparse),
            ..ScanOptions::default()
        };
        let generation = match (identity.workspace, &self.appdata_dir) {
            (Some(id), Some(appdata)) => {
                let _ = id;
                let cache_dir = appdata.join("workspace-index").join("shared-v2");
                self.indexer.restart(identity.roots, cache_dir, options)
            }
            _ => self.indexer.restart_memory(identity.roots, options),
        };
        self.expected_generation = Some(generation);
    }

    /// Starts a fresh scan of the current scope if the previous one is
    /// `Complete` (or ended in `Error`) and at least `interval` has passed
    /// since it began — 仕様 "periodic refresh only after prior scan
    /// completes; don't restart repeatedly before a large scan can finish".
    /// A no-op with no active scope, or while one is still running. Returns
    /// whether a rescan was started.
    pub fn maybe_rescan(&mut self, interval: Duration, now: Instant) -> bool {
        if self.status.busy {
            return false;
        }
        let Some(identity) = &self.identity else {
            return false;
        };
        if identity.roots.is_empty() {
            return false;
        }
        let Some(started) = self.last_scan_started else {
            return false;
        };
        if now.duration_since(started) < interval {
            return false;
        }
        self.start_scan();
        true
    }

    /// Folds up to `Indexer::poll`'s own bounded batch of waiting events into
    /// the snapshot and status — cheap enough for a UI timer tick regardless
    /// of how fast the worker is producing them.
    pub fn poll(&mut self) {
        let before = self.background_revision;
        for event in self.indexer.poll() {
            self.fold_event(event);
        }
        if before != self.background_revision {
            self.publish();
        }
    }

    #[cfg(test)]
    fn apply(&mut self, event: Event) {
        self.fold_event(event);
        self.publish();
    }
    fn fold_event(&mut self, event: Event) {
        if event.generation == MAINTENANCE_GENERATION {
            // A `clear_cache` error — for any Workspace, active or not.
            // Deliberately kept out of `Status`: it must never read as "the
            // active scan failed" (Codex preliminary review, engine
            // corrections).
            if let EventKind::Error(message) = event.kind {
                self.last_maintenance_error = Some(message);
            }
            return;
        }
        if Some(event.generation) != self.expected_generation {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        match &event.kind {
            EventKind::InventoryComplete { complete } => {
                self.names_complete = *complete;
            }
            EventKind::Complete {
                failed,
                missing_roots,
                truncated,
            } => {
                self.completed = true;
                self.status.busy = false;
                self.status.failed_dirs = failed.len();
                self.status.missing_roots = missing_roots.len();
                self.status.truncated = *truncated;
            }
            EventKind::Error(message) => {
                self.status.busy = false;
                self.status.error = Some(message.clone());
            }
            EventKind::CacheWriteFailed { .. } => {
                self.status.cache_write_failed += 1;
            }
            // Only these three can move an entry in or out of the view; the
            // progress reports above never can, so they must not dirty it.
            EventKind::Cached(found) | EventKind::Updated(found) => {
                self.visible_dirty |= !found.is_empty();
            }
            EventKind::Removed(paths) => {
                self.visible_dirty |= !paths.is_empty();
            }
        }
        self.snapshot.apply(event);
        self.background_revision = self.background_revision.wrapping_add(1);
    }
    fn publish(&mut self) {
        self.rebuild_visible();
        if self.completed && self.validation_roots().is_some() {
            let roots = self.roots();
            self.recent.retain(|(r, _, _)| r != &roots);
            if self.recent.len() >= 4 {
                self.recent.remove(0);
            }
            self.recent
                .push((roots, self.snapshot.entries().to_vec(), Instant::now()));
        }
    }

    /// Removes `workspace`'s entire on-disk index cache directory — every
    /// per-root cache file, whichever root wrote it, plus the directory
    /// itself if that leaves it empty — for a Workspace reset ("索引を再構築
    /// する") or removal ("unregister"). Enumerates by this build's own
    /// filename pattern rather than a `roots` list, so a root detached or
    /// relocated from the Workspace since its cache was written is still
    /// cleaned up, not just the currently registered ones.
    ///
    /// **Safe to call for a Workspace that is not the current scope, even
    /// while a scan for a different (e.g. the active) scope is running**:
    /// this goes through [`Indexer::clear_workspace_cache`], which — unlike
    /// the plain per-root `Indexer::clear` — never touches the shared scan
    /// generation, so it cannot supersede or stall on an unrelated scan; it
    /// only takes its own turn on the same worker queue. Any failure surfaces
    /// through [`WorkspaceLinks::last_maintenance_error`], not `status`. Does
    /// nothing when there is no AppData directory.
    pub fn clear_cache(&mut self, workspace: WorkspaceId) {
        if self
            .identity
            .as_ref()
            .is_some_and(|i| i.workspace == Some(workspace))
        {
            let roots = self.roots();
            self.recent.retain(|(covered, _, _)| {
                !covered.iter().any(|r| contained(r, &roots))
                    && !roots.iter().any(|r| contained(r, covered))
            });
        }
        let Some(appdata) = &self.appdata_dir else {
            return;
        };
        let cache_dir = cache_dir_for(appdata, workspace);
        self.indexer.clear_workspace_cache(cache_dir);
    }
}

fn cache_dir_for(appdata_dir: &Path, workspace: WorkspaceId) -> PathBuf {
    appdata_dir
        .join("workspace-index")
        .join(workspace.to_string())
}

// --- Completion state machine -----------------------------------------------

/// A caller-supplied stamp identifying which pane, document, and edit a
/// [`Context`] was detected against — carried unchanged through to
/// [`Completion::accept`] so a stale completion (the writer switched panes,
/// reopened the document, or kept editing without another `detect`) is
/// rejected rather than replacing text that no longer matches what the popup
/// showed. What `pane`/`document` actually are is entirely the caller's own
/// identifiers; this module never inspects them beyond equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionStamp {
    pub pane: u64,
    pub document: u64,
    /// The caller's own edit/version counter, bumped on every source
    /// mutation — catches an edit between `detect` and `accept` even when
    /// `pane`/`document` stayed the same.
    pub source_generation: u64,
}

/// What accepting a candidate would write — a caller applies this with
/// `open_document::replace_source_range` (or equivalent) rather than this
/// module ever touching a document itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub range: std::ops::Range<usize>,
    pub text: String,
    pub caret: usize,
}

#[derive(Debug)]
struct Open {
    stamp: CompletionStamp,
    context: Context,
    candidates: Vec<Candidate>,
    selected: usize,
}

/// A link-completion popup's own state: whether one is open, its candidates,
/// and which is selected — built from [`link_completion::detect`]/
/// [`link_completion::candidates`], never from a raw document mutation.
#[derive(Debug, Default)]
pub struct Completion {
    open: Option<Open>,
}

impl Completion {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether the open popup is offering a file's headings rather than files
    /// — for the diagnostics log (`link.popup`), so a report about heading
    /// candidates can be told from one about the file list without recording
    /// what either said.
    pub fn is_heading(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| open.context.kind.is_heading())
    }

    pub fn candidates(&self) -> &[Candidate] {
        self.open
            .as_ref()
            .map(|open| open.candidates.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected(&self) -> Option<usize> {
        self.open.as_ref().map(|open| open.selected)
    }

    pub fn hide(&mut self) {
        self.open = None;
    }

    /// Detects a trigger at `caret` in `source` and, if one is found and has
    /// at least one candidate, opens (or refreshes) the popup for it —
    /// otherwise closes whatever was open. Returns whether a popup is open
    /// after the call. `entries` should already carry the live outline of every
    /// open document with unsaved edits (the caller's `dirty_headings`).
    pub fn detect(
        &mut self,
        stamp: CompletionStamp,
        source: &str,
        caret: usize,
        source_file: Option<&Path>,
        entries: &[Entry],
        limit: usize,
    ) -> bool {
        let Some(context) = link_completion::detect(source, caret) else {
            self.open = None;
            return false;
        };
        let candidates = link_completion::candidates(entries, source_file, source, &context, limit);
        if candidates.is_empty() {
            self.open = None;
            return false;
        }
        self.open = Some(Open {
            stamp,
            context,
            candidates,
            selected: 0,
        });
        true
    }

    /// Moves the selection by `delta`, wrapping within the candidate list.
    /// A no-op with nothing open or an empty list.
    pub fn move_selection(&mut self, delta: i32) {
        let Some(open) = &mut self.open else { return };
        let count = open.candidates.len();
        if count == 0 {
            return;
        }
        let next = (open.selected as i64 + i64::from(delta)).rem_euclid(count as i64);
        open.selected = next as usize;
    }

    /// Selects a candidate by index directly — for a click. Out-of-range is
    /// ignored rather than panicking.
    pub fn select(&mut self, index: usize) {
        if let Some(open) = &mut self.open {
            if index < open.candidates.len() {
                open.selected = index;
            }
        }
    }

    /// What accepting the selected candidate would write, if `stamp` and
    /// `caret` still match what the popup was built against — `None` for a
    /// stale accept (an edit, a pane/document switch, or the caret having
    /// moved since `detect`), which a caller should treat as "do nothing,
    /// just close". Does not itself close the popup or mutate anything;
    /// `hide` is the caller's own next step either way.
    pub fn accept(&self, stamp: CompletionStamp, caret: usize) -> Option<Accepted> {
        let open = self.open.as_ref()?;
        if open.stamp != stamp {
            return None;
        }
        let expected_caret = open.context.query_at + open.context.query.len();
        if caret != expected_caret {
            return None;
        }
        let candidate = open.candidates.get(open.selected)?;
        Some(Accepted {
            // To the end of the target, not to the caret: a caret placed
            // inside an already-written target replaces the whole of it.
            range: open.context.query_at..open.context.target_end.unwrap_or(caret),
            text: candidate.insert.clone(),
            caret: open.context.query_at + candidate.caret_after_insert,
        })
    }
}

// --- Pure link resolution ----------------------------------------------------

/// A heading target still to be located in a document's live outline — never
/// a byte offset carried across a save/edit, which is what would go stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadingTarget {
    pub text: String,
    /// 1-based occurrence explicitly named by the link's own `#heading#2`
    /// suffix (仕様の実装依頼: "Additional reviewed integration decisions").
    /// `None` when the link named no suffix at all — **not** the same as
    /// "occurrence 1": [`find_heading_occurrence`] reports [`HeadingLookup::Ambiguous`]
    /// rather than silently guessing when this is `None` and `text` is not
    /// unique.
    pub occurrence: Option<usize>,
}

/// What locating a [`HeadingTarget`] against a document's live outline found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadingLookup {
    /// The byte offset of the one heading this unambiguously names.
    Found(usize),
    /// No heading in the document has this text (or this exact explicit
    /// occurrence of it).
    Missing,
    /// No occurrence was named, and more than one heading shares this text —
    /// `count` of them. Not resolved to any one of them; a caller should ask
    /// the writer to pick, or offer the link's own `#heading#N` suffix.
    Ambiguous(usize),
}

/// What [`resolve_link`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedLink {
    /// `[[#heading]]` or `[shown](#heading)` — the current document. Resolved
    /// immediately against `current_source`, since the caller already holds
    /// that text.
    SameFileHeading { result: HeadingLookup },
    /// Any other target. `heading`, if present, is **not** resolved to a byte
    /// offset here — a pure resolver must not read another file from disk, so
    /// a caller opens `path` first and then calls [`find_heading_occurrence`]
    /// against that document's own live text.
    Target {
        path: PathBuf,
        heading: Option<HeadingTarget>,
    },
}

/// Everything [`resolve_link`] can refuse with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The target was empty (or only whitespace) after trimming.
    Empty,
    /// A bare wiki filename matched nothing in the index.
    NotFound,
    /// A bare wiki filename matched more than one indexed file — every
    /// match's canonical path, for a caller to offer as a choice the writer
    /// can pick a qualified path from instead.
    Ambiguous(Vec<PathBuf>),
    /// An explicit relative path was given with no source file to resolve it
    /// against (an unsaved document with no path of its own).
    NoSourceFile,
}

/// Resolves `target` — the raw grammar text between a link's own delimiters,
/// exactly as `document::link_target_at` hands it back, still percent-encoded
/// per [`link_completion::percent_encode_reserved`] and still carrying a
/// literal `#` as this design's heading separator — against `entries` (an
/// index snapshot, optionally carrying unsaved documents' live outlines) and, for a
/// same-document heading, `current_source`'s own live text. Decodes the file
/// portion exactly once, so a filename that legitimately contains `#`
/// (percent-encoded as `%23` per grammar) is never mistaken for a heading
/// separator or rejected as one.
///
/// - An absolute path, or one that contains a path separator — including the
///   `./` a same-directory candidate's own `insert` now carries (see
///   `link_completion::relative_between`) — is resolved directly against
///   `source_file`'s folder (or used as-is if absolute), lexically
///   normalized, **never** through the index, wiki or not: 仕様 "resolve
///   relative wiki against source parent for explicit paths", and what makes
///   accepting a specific candidate always resolve that exact file even when
///   another root shares its basename.
/// - A bare filename (no separator) in a *wiki* link (`wiki: true`) is looked
///   up by basename among `entries`; anything else (a plain Markdown link's
///   bare filename) resolves the same way an explicit relative path does,
///   which is the existing, index-free behaviour non-wiki links already had.
///
/// No disk read, no Slint, no window API — see [`ResolvedLink::Target`] for
/// what that means for a cross-file heading.
pub fn resolve_link(
    target: &str,
    wiki: bool,
    source_file: Option<&Path>,
    current_source: &str,
    entries: &[Entry],
) -> Result<ResolvedLink, ResolveError> {
    let target = target.trim();
    let target = target
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(target);
    if target.is_empty() {
        return Err(ResolveError::Empty);
    }
    let (file_part, heading_part) = link_completion::split_target_heading(target);

    if file_part.trim().is_empty() {
        let Some(heading_part) = heading_part else {
            return Err(ResolveError::Empty);
        };
        let spec = parse_heading_spec(heading_part);
        let result = find_heading_occurrence(current_source, &spec.text, spec.occurrence);
        return Ok(ResolvedLink::SameFileHeading { result });
    }

    let heading = heading_part.map(parse_heading_spec);
    let decoded_file = link_completion::percent_decode(file_part);
    let path = resolve_indexed_file(&decoded_file, wiki, source_file, entries, false)?;

    Ok(ResolvedLink::Target { path, heading })
}

/// Shared, disk-free name resolution for notes, heading completion and images.
pub fn resolve_indexed_file(
    decoded: &str,
    wiki: bool,
    source: Option<&Path>,
    entries: &[Entry],
    image: bool,
) -> Result<PathBuf, ResolveError> {
    let direct = resolve_explicit_path(decoded, source);
    let qualified = decoded.contains(['/', '\\']);
    let explicit = Path::new(decoded).is_absolute()
        || decoded.starts_with("./")
        || decoded.starts_with("../")
        || decoded.starts_with(".\\")
        || decoded.starts_with("..\\");
    let same = |a: &Path, b: &Path| {
        let (a, b) = (lexical_path(a), lexical_path(b));
        if cfg!(windows) {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        } else {
            a == b
        }
    };
    // Every entry list reaching here is already restricted to the index's own
    // types: `run_scan` filters cached and walked files, `load_shared_cache`
    // filters what it re-projects, `WorkspaceLinks::rebuild_visible` filters
    // the published view, and the priority/completion paths filter their own
    // copies. Re-testing the type here would instead *break* the rename and
    // move path, whose entries are the moved files themselves: a `.pdf` or
    // `.txt` is out of scope for name resolution through the index, but a
    // link to one still follows the file when it moves (2026-09-21).
    let allowed = |e: &&Entry| !image || crate::workspace_index::is_image(&e.canonical);
    if let Some(path) = &direct {
        if let Some(e) = entries
            .iter()
            .filter(allowed)
            .find(|e| same(&e.canonical, path))
        {
            return Ok(e.canonical.clone());
        }
    }
    let can_md = !image
        && !Path::new(decoded)
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| {
                [
                    "md", "txt", "log", "pdf", "rs", "json", "toml", "html", "png", "jpg", "jpeg",
                    "gif", "webp", "bmp", "svg",
                ]
                .iter()
                .any(|e| x.eq_ignore_ascii_case(e))
            });
    if can_md {
        if let Some(path) = &direct {
            let appended = PathBuf::from(format!("{}.md", path.display()));
            if let Some(e) = entries
                .iter()
                .filter(allowed)
                .find(|e| same(&e.canonical, &appended))
            {
                return Ok(e.canonical.clone());
            }
        }
    }
    if !explicit && (wiki || image || qualified) {
        for name in
            std::iter::once(decoded.to_owned()).chain(can_md.then(|| format!("{decoded}.md")))
        {
            let mut found: Vec<PathBuf> = entries
                .iter()
                .filter(allowed)
                .filter(|e| {
                    if qualified {
                        same(&e.relative, Path::new(&name))
                    } else {
                        e.canonical.file_name().is_some_and(|n| {
                            if cfg!(windows) {
                                n.to_string_lossy().eq_ignore_ascii_case(&name)
                            } else {
                                n == std::ffi::OsStr::new(&name)
                            }
                        })
                    }
                })
                .map(|e| e.canonical.clone())
                .collect();
            found.sort();
            found.dedup();
            match found.len() {
                0 => {}
                1 => return Ok(found.remove(0)),
                _ => return Err(ResolveError::Ambiguous(found)),
            }
        }
    }
    if wiki && !qualified && !Path::new(decoded).is_absolute() && !image {
        return Err(ResolveError::NotFound);
    }
    direct.ok_or(ResolveError::NoSourceFile)
}

/// An already-decoded file path (never re-decoded — the caller decodes
/// exactly once) resolved against `source_file`'s folder, or used directly if
/// absolute. Trims, strips a `<...>` verbatim wrapper, and rejects external
/// URLs and newlines. **No `#` guard**: by this point `#` has already been
/// split off as this design's own heading separator by [`resolve_link`], so a
/// `#` surviving into `decoded` can only be a literal character of the
/// filename itself (from a decoded `%23`), not a stray separator.
fn resolve_explicit_path(decoded: &str, source_file: Option<&Path>) -> Option<PathBuf> {
    let target = decoded
        .trim()
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(decoded.trim());
    if target.is_empty() || target.contains(['\n', '\r']) || target.contains("://") {
        return None;
    }
    let path = Path::new(target);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        source_file?.parent()?.join(path)
    };
    Some(normalize_current_dir(&joined))
}

/// Lexically drops every `.` (`Component::CurDir`) component — what a
/// same-directory candidate's own `./` join would otherwise leave in the
/// result, so it compares equal to the entry's own `canonical` path rather
/// than a syntactically different string naming the same file. Never touches
/// the filesystem (no symlink awareness, no `..` resolution) — purely
/// syntactic, which is all a pure resolver may do.
fn normalize_current_dir(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        if component == std::path::Component::CurDir {
            continue;
        }
        out.push(component.as_os_str());
    }
    out
}

/// Splits a trailing literal `#digits` occurrence marker off `raw` (仕様の
///実装依頼 "Additional reviewed integration decisions": `#percent_encoded_heading#2`),
/// then percent-decodes what remains as the heading text. A literal `#`
/// inside a heading's own text can never survive this far — it is always
/// `%23` by construction (`WIKI_RESERVED`/`MARKDOWN_RESERVED`) — so any
/// literal `#` still present can only be this marker, unambiguously.
fn parse_heading_spec(raw: &str) -> HeadingTarget {
    if let Some(at) = raw.rfind('#') {
        let (text_part, occurrence_part) = (&raw[..at], &raw[at + 1..]);
        if !occurrence_part.is_empty() && occurrence_part.bytes().all(|byte| byte.is_ascii_digit())
        {
            if let Ok(occurrence) = occurrence_part.parse::<usize>() {
                if occurrence >= 1 {
                    return HeadingTarget {
                        text: link_completion::percent_decode(text_part),
                        occurrence: Some(occurrence),
                    };
                }
            }
        }
    }
    HeadingTarget {
        text: link_completion::percent_decode(raw),
        occurrence: None,
    }
}

/// Locates `text` in `source`'s live outline ([`document::outline`], never a
/// stale offset carried from elsewhere). `occurrence` is 1-based when
/// `Some`, from the link's own explicit `#heading#N` suffix; `None` means the
/// link named no suffix — [`HeadingLookup::Ambiguous`] rather than an
/// arbitrary guess when more than one heading shares `text` in that case.
pub fn find_heading_occurrence(
    source: &str,
    text: &str,
    occurrence: Option<usize>,
) -> HeadingLookup {
    let matches: Vec<usize> = document::outline(source)
        .into_iter()
        .filter(|heading| heading.text == text)
        .map(|heading| heading.at)
        .collect();
    match occurrence {
        Some(n) => matches
            .get(n.max(1) - 1)
            .copied()
            .map_or(HeadingLookup::Missing, HeadingLookup::Found),
        None => match matches.len() {
            0 => HeadingLookup::Missing,
            1 => HeadingLookup::Found(matches[0]),
            count => HeadingLookup::Ambiguous(count),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::FileStamp;
    use std::time::Duration;

    fn entry(root: &str, relative: &str, headings: Vec<document::Heading>) -> Entry {
        let root = PathBuf::from(root);
        let relative = PathBuf::from(relative);
        let canonical = root.join(&relative);
        Entry {
            root,
            relative,
            canonical,
            fingerprint: FileStamp {
                modified: None,
                length: 0,
            },
            headings,
            headings_complete: true,
        }
    }

    fn heading(level: u8, text: &str, at: usize) -> document::Heading {
        document::Heading {
            level,
            text: text.to_owned(),
            at,
        }
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-wslinks-{name}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("creates");
        directory
    }

    /// 未保存の見出しが補完候補まで届くことを固定する。索引はディスク上の古い
    /// 見出しを持ち、優先更新だけが開いている本文の新しい見出しを持つ状況を作る
    /// （RFN01-24 の確認手順4が実画面で通らなかった、2026-09-21 の報告を受けて）。
    #[test]
    fn an_unsaved_heading_reaches_the_completion_candidates() {
        let root = scratch_directory("unsaved-heading");
        let notes = root.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("Target.md"), "# 古い見出し\n").unwrap();
        let review = root.join("Review.md");
        std::fs::write(&review, "[[Target#\n").unwrap();
        let root_text = root.to_str().unwrap();

        let mut links = WorkspaceLinks::new(None);
        links.identity = Some(ScopeIdentity {
            workspace: Some(1),
            roots: vec![root.clone()],
            reset_generation: 0,
        });
        links.expected_generation = Some(7);
        links.apply(Event {
            generation: 7,
            kind: EventKind::Cached(vec![entry(
                root_text,
                "notes/Target.md",
                vec![heading(1, "古い見出し", 0)],
            )]),
        });
        assert_eq!(links.entries()[0].headings[0].text, "古い見出し");

        // What the priority worker publishes for a document open with unsaved
        // edits: its live outline, never the file's.
        links.set_priority(vec![entry(
            root_text,
            "notes/Target.md",
            vec![heading(1, "新しい見出し", 0)],
        )]);
        assert_eq!(
            links.entries()[0].headings[0].text,
            "新しい見出し",
            "the priority overlay has to win over the indexed text"
        );

        let source = "[[Target#";
        let context = link_completion::detect(source, source.len()).unwrap();
        let candidates =
            link_completion::candidates(links.entries(), Some(&review), source, &context, 100);
        let shown: Vec<&str> = candidates
            .iter()
            .map(|item| item.display.as_str())
            .collect();
        assert!(
            shown.iter().any(|text| text.contains("新しい見出し")),
            "the unsaved heading should be offered, got {shown:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 2026-09-21: what keeping the shared view costs the UI thread at the
    /// size a real manuscript reaches. The first line is the whole first
    /// publish; each later line is one event batch carrying a single changed
    /// file — what a keystroke or a scan update actually pays. Release
    /// benchmark: run alone with `--release -- --ignored --nocapture`.
    #[test]
    #[ignore = "release benchmark; explicitly invoked"]
    fn view_rebuild_benchmark() {
        const FILES: usize = 10_000;
        const HEADINGS: usize = 30;
        let root = scratch_directory("view-benchmark");
        let mut links = WorkspaceLinks::new(None);
        links.identity = Some(ScopeIdentity {
            workspace: Some(1),
            roots: vec![root.clone()],
            reset_generation: 0,
        });
        links.expected_generation = Some(7);
        let entry_for = |index: usize, revision: usize| Entry {
            root: root.clone(),
            relative: PathBuf::from(format!("n{index}.md")),
            canonical: root.join(format!("n{index}.md")),
            fingerprint: FileStamp {
                modified: None,
                length: 0,
            },
            headings: (0..HEADINGS)
                .map(|heading| document::Heading {
                    level: 2,
                    text: format!("見出し{index}-{heading}-{revision}"),
                    at: heading * 8,
                })
                .collect(),
            headings_complete: true,
        };

        let started = Instant::now();
        links.apply(Event {
            generation: 7,
            kind: EventKind::Updated((0..FILES).map(|index| entry_for(index, 0)).collect()),
        });
        let published = links.take_view_metrics();
        println!(
            "view_rebuild_benchmark files={FILES} headings={HEADINGS} phase=first rebuild_ms={:.3} compare_ms={:.3} entries={} wall_ms={:.3}",
            published.rebuild_ms,
            published.compare_ms,
            published.entries,
            started.elapsed().as_secs_f64() * 1000.0
        );

        let mut samples = Vec::new();
        for round in 1..=5 {
            let started = Instant::now();
            links.apply(Event {
                generation: 7,
                kind: EventKind::Updated(vec![entry_for(round, round)]),
            });
            let metrics = links.take_view_metrics();
            samples.push(metrics.rebuild_ms + metrics.compare_ms);
            println!(
                "view_rebuild_benchmark files={FILES} phase=one-file round={round} rebuild_ms={:.3} compare_ms={:.3} entries={} wall_ms={:.3}",
                metrics.rebuild_ms,
                metrics.compare_ms,
                metrics.entries,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "view_rebuild_benchmark files={FILES} median_rebuild_plus_compare_ms={:.3}",
            samples[samples.len() / 2]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // --- WorkspaceLinks scope lifecycle -------------------------------

    #[test]
    fn syncing_the_same_scope_twice_does_not_restart_the_scan() {
        let root = scratch_directory("same-scope-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root.clone()], 0);
        let first_generation = links.expected_generation;

        links.sync_scope(Some(1), vec![root], 0);
        assert_eq!(
            links.expected_generation, first_generation,
            "identical scope must not restart the scan"
        );
    }

    #[test]
    fn changing_scope_invalidates_the_snapshot_and_ignores_stale_events() {
        let old_root = scratch_directory("switch-old-root");
        std::fs::write(old_root.join("old.md"), "# 旧\n").expect("writes");
        let new_root = scratch_directory("switch-new-root");
        std::fs::write(new_root.join("new.md"), "# 新\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![old_root.clone()], 0);
        // A stale event tagged with a generation from before the switch must
        // never appear in the new scope's entries.
        links.apply(Event {
            generation: links.expected_generation.unwrap(),
            kind: EventKind::Updated(vec![entry(old_root.to_str().unwrap(), "x.md", Vec::new())]),
        });
        assert_eq!(links.entries().len(), 1);

        links.sync_scope(Some(1), vec![new_root], 0);
        assert!(
            links.entries().is_empty(),
            "switching scope clears the snapshot immediately"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while links.entries().is_empty() && Instant::now() < deadline {
            links.poll();
        }
        let names: Vec<&str> = links
            .entries()
            .iter()
            .map(|e| e.relative.to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["new.md"]);
    }

    #[test]
    fn empty_roots_starts_no_scan_and_reports_idle() {
        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(None, Vec::new(), 0);
        assert!(links.expected_generation.is_none());
        assert!(!links.status().busy);
        assert!(links.entries().is_empty());
    }

    #[test]
    fn a_higher_reset_generation_with_the_same_roots_still_restarts() {
        let root = scratch_directory("reset-gen-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root.clone()], 0);
        let first_generation = links.expected_generation;

        links.sync_scope(Some(1), vec![root], 1);
        assert_ne!(links.expected_generation, first_generation);
    }

    #[test]
    fn poll_reaches_complete_and_reports_status() {
        let root = scratch_directory("poll-complete-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root], 0);
        assert!(links.status().busy);

        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        assert!(!links.status().busy);
        assert_eq!(links.entries().len(), 1);
    }

    #[test]
    fn maybe_rescan_waits_for_completion_and_the_interval() {
        let root = scratch_directory("rescan-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root], 0);
        // Still busy: must refuse regardless of how much time has passed.
        assert!(!links.maybe_rescan(
            Duration::from_secs(0),
            Instant::now() + Duration::from_secs(60)
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        let started = links.last_scan_started.unwrap();
        // Complete, but the interval has not elapsed yet.
        assert!(!links.maybe_rescan(Duration::from_secs(60), started));
        // Complete, and the interval has elapsed.
        assert!(links.maybe_rescan(Duration::from_secs(1), started + Duration::from_secs(2)));
        assert!(links.status().busy);
    }

    #[test]
    fn clear_cache_does_nothing_without_an_appdata_directory() {
        let mut links = WorkspaceLinks::new(None);
        // Must not panic and must not touch anything real.
        links.clear_cache(1);
    }

    #[test]
    fn clear_cache_removes_the_named_workspaces_cache_directory() {
        let appdata = scratch_directory("clear-cache-appdata");
        let root = scratch_directory("clear-cache-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(Some(appdata.clone()));
        links.sync_scope(Some(7), vec![root], 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        let cache_dir = cache_dir_for(&appdata, 7);
        // New scans use the shared store; legacy per-Workspace cleanup stays supported.
        assert!(appdata.join("workspace-index/shared-v2").exists());
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("index-0000000000000001.rfnwsidx"), "legacy").unwrap();
        assert_eq!(
            std::fs::read_dir(&cache_dir)
                .map(|it| it.count())
                .unwrap_or(0),
            1
        );

        links.clear_cache(7);
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache_dir.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!cache_dir.exists());
        assert!(appdata.join("workspace-index/shared-v2").exists());
    }

    #[test]
    fn notes_and_images_resolve_through_one_rule() {
        let entries = vec![
            entry("/root", "notes/person.md", vec![]),
            entry("/root", "assets/photo.png", vec![]),
            entry("/root", "other/person.md", vec![]),
        ];
        let source = Path::new("/root/notes/source.md");
        assert_eq!(
            resolve_indexed_file("person", true, Some(source), &entries, false).unwrap(),
            PathBuf::from("/root/notes/person.md")
        );
        assert_eq!(
            resolve_indexed_file("photo.png", true, Some(source), &entries, true).unwrap(),
            PathBuf::from("/root/assets/photo.png")
        );
        assert_eq!(
            resolve_indexed_file("assets/photo.png", true, Some(source), &entries, true).unwrap(),
            PathBuf::from("/root/assets/photo.png")
        );
        assert!(matches!(
            resolve_indexed_file("person", true, None, &entries, false),
            Err(ResolveError::Ambiguous(_))
        ));
        // Explicit paths must never be redirected to a matching name elsewhere.
        assert_eq!(
            resolve_indexed_file("./photo.png", true, Some(source), &entries, true).unwrap(),
            PathBuf::from("/root/notes/photo.png")
        );
        assert_eq!(
            resolve_indexed_file("plain.txt", true, None, &entries, false),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn completed_parent_snapshot_is_reused_by_another_workspaces_child() {
        let root = scratch_directory("share-parent-live");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("one.md"), "# One\n").unwrap();
        std::fs::write(root.join("outside.md"), "# Outside\n").unwrap();
        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root], 0);
        let until = Instant::now() + Duration::from_secs(5);
        while links.status.busy {
            links.poll();
            assert!(Instant::now() < until);
        }
        links.sync_scope(Some(2), vec![child.clone()], 0);
        assert!(!links.status.busy);
        assert_eq!(links.entries().len(), 1);
        assert!(links.entries()[0].canonical.ends_with("one.md"));
        assert_eq!(links.entries()[0].relative, PathBuf::from("one.md"));
        links.sync_scope(Some(2), vec![child], 1);
        assert!(
            links.status.busy,
            "reset must bypass a fresh shared snapshot"
        );
    }

    /// Codex preliminary review, engine corrections: clearing an *inactive*
    /// Workspace's cache must never stall or poison the *active* scope's own
    /// scan — proven here by the active scan still reaching `Complete` and
    /// its entries still showing up, even though the clear was issued while
    /// it was still running.
    #[test]
    fn clearing_an_inactive_workspaces_cache_does_not_poison_the_active_scan() {
        let appdata = scratch_directory("clear-cache-inactive-appdata");
        let active_root = scratch_directory("clear-cache-inactive-active-root");
        std::fs::write(active_root.join("a.md"), "# 一\n").expect("writes");
        // Seed the *other* (inactive) Workspace's own cache directory so
        // there is something for `clear_cache` to actually remove.
        let inactive_cache_dir = cache_dir_for(&appdata, 99);
        std::fs::create_dir_all(&inactive_cache_dir).expect("creates");
        std::fs::write(
            inactive_cache_dir.join("index-0000000000000001.rfnwsidx"),
            b"stale",
        )
        .expect("writes");

        let mut links = WorkspaceLinks::new(Some(appdata));
        links.sync_scope(Some(1), vec![active_root], 0);
        assert!(links.status().busy);

        // Issued while the active scan may still be running.
        links.clear_cache(99);

        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        assert!(
            !links.status().busy,
            "the active scan must still reach Complete"
        );
        assert!(links.status().error.is_none());
        assert_eq!(links.entries().len(), 1);

        let deadline = Instant::now() + Duration::from_secs(5);
        while inactive_cache_dir.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !inactive_cache_dir.exists(),
            "the inactive workspace's cache is still cleared"
        );
    }

    /// Codex preliminary review, engine corrections: `start_scan` must reset
    /// `Status` fresh even when reached through `maybe_rescan`, which (unlike
    /// `sync_scope`) does not go through its own explicit reset — otherwise a
    /// stale `missing_roots`/`error`/etc lingers past a later successful run
    /// of the very same scope.
    #[test]
    fn maybe_rescan_clears_a_previous_runs_stale_status() {
        let root = scratch_directory("rescan-clears-error-root").join("not-yet-there");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(Some(1), vec![root.clone()], 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        assert_eq!(links.status().missing_roots, 1);

        std::fs::create_dir_all(&root).expect("creates");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");
        let started = links.last_scan_started.unwrap();
        assert!(links.maybe_rescan(Duration::from_millis(0), started + Duration::from_secs(1)));

        let deadline = Instant::now() + Duration::from_secs(5);
        while links.status().busy && Instant::now() < deadline {
            links.poll();
        }
        assert_eq!(
            links.status().missing_roots,
            0,
            "a successful rescan must not carry the prior error forward"
        );
        assert_eq!(links.entries().len(), 1);
    }

    #[test]
    fn sync_scope_ignores_roots_given_without_a_workspace() {
        let root = scratch_directory("no-workspace-defensive-root");
        std::fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut links = WorkspaceLinks::new(None);
        links.sync_scope(None, vec![root], 0);

        assert!(links.expected_generation.is_none());
        assert!(!links.status().busy);
        assert!(links.entries().is_empty());
    }

    // --- Completion state machine ---------------------------------------

    fn stamp(pane: u64, document: u64, source_generation: u64) -> CompletionStamp {
        CompletionStamp {
            pane,
            document,
            source_generation,
        }
    }

    /// 書き手の報告 2026-09-21: キャレットを既存リンクの対象の途中に置いて
    /// 選び直すと、`[[Target#Target%20heading%20Test]]ing]]` のように残りと
    /// `]]` が二重になっていた。受け入れは対象の終わりまでを置き換える。
    #[test]
    fn accepting_inside_a_written_target_replaces_the_whole_target() {
        let entries = vec![entry(
            "/根",
            "Target.md",
            vec![heading(1, "Target heading", 0)],
        )];
        let source_file = Path::new("/根/Review.md");
        let replace = |source: &str| {
            let caret = source.find("ing").expect("typing inside the heading");
            let mut completion = Completion::new();
            assert!(completion.detect(
                stamp(1, 1, 0),
                source,
                caret,
                Some(source_file),
                &entries,
                10
            ));
            let accepted = completion
                .accept(stamp(1, 1, 0), caret)
                .expect("the caret still matches the open popup");
            let mut next = source.to_owned();
            next.replace_range(accepted.range.clone(), &accepted.text);
            next
        };
        assert_eq!(
            replace("[[Target#Target heading]]"),
            "[[Target#Target%20heading]]"
        );
        assert_eq!(
            replace("[[Target#Target heading|表示名]]"),
            "[[Target#Target%20heading|表示名]]"
        );
    }

    #[test]
    fn detecting_with_no_trigger_closes_an_open_popup() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let mut completion = Completion::new();
        assert!(completion.detect(stamp(1, 1, 0), "[[次", "[[次".len(), None, &entries, 10));
        assert!(completion.is_open());

        assert!(!completion.detect(stamp(1, 1, 1), "本文だけ", 3, None, &entries, 10));
        assert!(!completion.is_open());
    }

    #[test]
    fn accept_returns_none_when_the_stamp_does_not_match() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let mut completion = Completion::new();
        let source = "[[次";
        completion.detect(stamp(1, 1, 0), source, source.len(), None, &entries, 10);

        assert!(
            completion.accept(stamp(1, 1, 1), source.len()).is_none(),
            "an edit bumping source_generation must invalidate accept"
        );
        assert!(
            completion.accept(stamp(2, 1, 0), source.len()).is_none(),
            "a different pane must invalidate accept"
        );
        assert!(
            completion.accept(stamp(1, 2, 0), source.len()).is_none(),
            "a different document must invalidate accept"
        );
    }

    #[test]
    fn accept_returns_none_when_the_caret_has_moved() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let mut completion = Completion::new();
        let source = "[[次";
        completion.detect(stamp(1, 1, 0), source, source.len(), None, &entries, 10);

        assert!(completion.accept(stamp(1, 1, 0), 0).is_none());
    }

    #[test]
    fn accepting_a_valid_context_returns_the_replacement_range_text_and_caret() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let mut completion = Completion::new();
        let source = "[[次";
        completion.detect(stamp(1, 1, 0), source, source.len(), None, &entries, 10);

        let accepted = completion
            .accept(stamp(1, 1, 0), source.len())
            .expect("still valid");
        assert_eq!(accepted.range, 2..source.len());
        assert_eq!(accepted.text, "/根/次.md]]");
        assert_eq!(accepted.caret, 2 + "/根/次.md".len());
    }

    #[test]
    fn move_selection_wraps_within_the_candidate_list() {
        let entries = vec![
            entry("/根", "一.md", Vec::new()),
            entry("/根", "二.md", Vec::new()),
        ];
        let mut completion = Completion::new();
        completion.detect(stamp(1, 1, 0), "[[", 2, None, &entries, 10);
        assert_eq!(completion.selected(), Some(0));

        completion.move_selection(-1);
        assert_eq!(completion.selected(), Some(1));
        completion.move_selection(1);
        assert_eq!(completion.selected(), Some(0));
    }

    #[test]
    fn hide_closes_regardless_of_state() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let mut completion = Completion::new();
        completion.detect(stamp(1, 1, 0), "[[次", "[[次".len(), None, &entries, 10);
        assert!(completion.is_open());
        completion.hide();
        assert!(!completion.is_open());
        assert!(completion.candidates().is_empty());
    }

    // --- resolve_link ------------------------------------------------------

    #[test]
    fn a_bare_wiki_filename_resolves_through_the_index() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let resolved = resolve_link("次.md", true, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/次.md"),
                heading: None
            }
        );
    }

    #[test]
    fn a_bare_wiki_filename_matching_nothing_is_not_found() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        assert_eq!(
            resolve_link("いない.md", true, None, "", &entries),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn a_bare_wiki_filename_matching_two_roots_is_ambiguous() {
        let entries = vec![
            entry("/一", "同じ.md", Vec::new()),
            entry("/二", "同じ.md", Vec::new()),
        ];
        let error = resolve_link("同じ.md", true, None, "", &entries).unwrap_err();
        let ResolveError::Ambiguous(mut paths) = error else {
            panic!("expected Ambiguous")
        };
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("/一/同じ.md"), PathBuf::from("/二/同じ.md")]
        );
    }

    #[test]
    fn an_explicit_relative_wiki_path_resolves_against_the_source_folder_without_the_index() {
        let entries: Vec<Entry> = Vec::new();
        let source_file = Path::new("/根/現在.md");
        let resolved =
            resolve_link("章/次.md", true, Some(source_file), "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/章/次.md"),
                heading: None
            }
        );
    }

    #[test]
    fn an_explicit_relative_path_with_no_source_file_is_refused() {
        let entries: Vec<Entry> = Vec::new();
        assert_eq!(
            resolve_link("章/次.md", true, None, "", &entries),
            Err(ResolveError::NoSourceFile)
        );
    }

    #[test]
    fn an_absolute_path_resolves_directly_even_for_a_wiki_link() {
        let entries: Vec<Entry> = Vec::new();
        let target = if cfg!(windows) {
            "C:/よそ/先.md"
        } else {
            "/よそ/先.md"
        };
        let resolved = resolve_link(target, true, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from(target),
                heading: None
            }
        );
        assert_eq!(
            resolve_link(&format!("<{target}>"), false, None, "", &entries).unwrap(),
            resolved
        );
    }

    #[test]
    fn a_plain_markdown_bare_filename_resolves_relative_to_source_like_before() {
        let entries: Vec<Entry> = Vec::new();
        let source_file = Path::new("/根/現在.md");
        let resolved =
            resolve_link("次.md", false, Some(source_file), "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/次.md"),
                heading: None
            }
        );
    }

    #[test]
    fn a_same_file_heading_resolves_against_the_live_current_source() {
        let entries: Vec<Entry> = Vec::new();
        let current_source = "# 新しい見出し\n本文";
        let encoded = link_completion::percent_encode_reserved("新しい見出し", b"#");
        let target = format!("#{encoded}");
        let resolved =
            resolve_link(&target, true, None, current_source, &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::SameFileHeading {
                result: HeadingLookup::Found(0)
            }
        );
    }

    #[test]
    fn a_missing_same_file_heading_resolves_to_missing_not_an_error() {
        let entries: Vec<Entry> = Vec::new();
        let resolved = resolve_link("#いない", true, None, "本文だけ", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::SameFileHeading {
                result: HeadingLookup::Missing
            }
        );
    }

    /// Codex preliminary review, engine corrections: omitting the `#N`
    /// suffix when the same heading text repeats must be reported as
    /// ambiguous, not silently resolved to the first occurrence.
    #[test]
    fn a_same_file_heading_with_no_suffix_and_two_matches_is_ambiguous() {
        let entries: Vec<Entry> = Vec::new();
        let current_source = "# まとめ\n本文\n## まとめ\nまた本文";
        let resolved =
            resolve_link("#まとめ", true, None, current_source, &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::SameFileHeading {
                result: HeadingLookup::Ambiguous(2)
            }
        );
    }

    #[test]
    fn a_cross_file_heading_with_no_suffix_carries_no_occurrence() {
        let entries = vec![entry("/根", "次.md", vec![heading(1, "第一章", 0)])];
        let resolved = resolve_link("次.md#第一章", true, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/次.md"),
                heading: Some(HeadingTarget {
                    text: "第一章".to_owned(),
                    occurrence: None
                }),
            }
        );
    }

    #[test]
    fn a_duplicate_heading_occurrence_suffix_is_parsed_and_stripped() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let resolved = resolve_link("次.md#まとめ#2", true, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/次.md"),
                heading: Some(HeadingTarget {
                    text: "まとめ".to_owned(),
                    occurrence: Some(2)
                }),
            }
        );
    }

    #[test]
    fn find_heading_occurrence_distinguishes_missing_ambiguous_and_found() {
        let source = "# まとめ\n本文\n## まとめ\nまた本文";
        assert_eq!(
            find_heading_occurrence(source, "まとめ", None),
            HeadingLookup::Ambiguous(2)
        );
        assert_eq!(
            find_heading_occurrence(source, "まとめ", Some(1)),
            HeadingLookup::Found(0)
        );
        let second_at = source.find("## まとめ").unwrap();
        assert_eq!(
            find_heading_occurrence(source, "まとめ", Some(2)),
            HeadingLookup::Found(second_at)
        );
        assert_eq!(
            find_heading_occurrence(source, "まとめ", Some(3)),
            HeadingLookup::Missing
        );
        assert_eq!(
            find_heading_occurrence(source, "いない", None),
            HeadingLookup::Missing
        );

        let unique_source = "# 単独\n本文";
        assert_eq!(
            find_heading_occurrence(unique_source, "単独", None),
            HeadingLookup::Found(0)
        );
    }

    #[test]
    fn an_empty_target_is_refused() {
        let entries: Vec<Entry> = Vec::new();
        assert_eq!(
            resolve_link("   ", true, None, "", &entries),
            Err(ResolveError::Empty)
        );
    }

    /// A filename that literally contains `#` (percent-encoded as `%23` per
    /// grammar) must resolve, not be rejected as if it had a stray heading
    /// separator — the whole point of decoding exactly once and not
    /// delegating to a `#`-rejecting path parser after the fact (Codex
    /// preliminary review, engine corrections).
    #[test]
    fn a_filename_containing_a_literal_hash_resolves() {
        let entries: Vec<Entry> = Vec::new();
        let target = if cfg!(windows) {
            "C:/よそ/第%23一章.md"
        } else {
            "/よそ/第%23一章.md"
        };
        let resolved = resolve_link(target, true, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from(target.replace("%23", "#")),
                heading: None
            }
        );
    }

    #[test]
    fn generated_links_round_trip_real_files_with_reserved_characters() {
        let root = scratch_directory("reserved-roundtrip")
            .canonicalize()
            .unwrap();
        let source = root.join("current.md");
        std::fs::write(&source, "").unwrap();
        for name in [
            "第#一.md",
            "literal%23.md",
            "close)bracket].md",
            "space name.md",
        ] {
            let path = root.join(name);
            std::fs::write(&path, "# heading\n").unwrap();
            let entries = vec![Entry {
                root: root.clone(),
                relative: PathBuf::from(name),
                canonical: path.clone(),
                fingerprint: FileStamp {
                    modified: None,
                    length: 10,
                },
                headings: vec![],
                headings_complete: true,
            }];
            for typed in ["[[", "[link]("] {
                let context = link_completion::detect(typed, typed.len()).unwrap();
                let candidates =
                    link_completion::candidates(&entries, Some(&source), "", &context, 10);
                let line = format!("{typed}{}", candidates[0].insert);
                let (target, wiki) = document::link_target_at(&line, typed.len()).unwrap();
                let ResolvedLink::Target {
                    path: resolved,
                    heading: None,
                } = resolve_link(target, wiki, Some(&source), "", &entries).unwrap()
                else {
                    panic!("expected file target")
                };
                assert_eq!(
                    resolved.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Round-trip through the real grammar: build a candidate for a file
    /// whose name needs escaping (`#`, `)`, `]`, a space, Japanese), accept
    /// it into a line, reparse that line's target with
    /// `document::link_target_at` the same way an editor click would, and
    /// resolve the *parsed* target — not just the boundaries, the actual
    /// resolution.
    #[test]
    fn a_generated_wiki_link_with_reserved_characters_parses_and_resolves() {
        let root = if cfg!(windows) { "C:/根" } else { "/根" };
        let entries = vec![entry(root, "第#一)章].md", Vec::new())];
        let typed = "[[";
        let context = link_completion::detect(typed, typed.len()).unwrap();
        let candidate = &link_completion::candidates(&entries, None, "", &context, 10)[0];
        let full_line = format!("{typed}{}", candidate.insert);

        let (target, is_wiki) = document::link_target_at(&full_line, 3).expect("parses as a link");
        assert!(is_wiki);
        let resolved = resolve_link(target, is_wiki, None, "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: Path::new(root).join("第#一)章].md"),
                heading: None
            }
        );
    }

    /// Same round-trip, but for a cross-file heading with a literal `#` in
    /// the heading text itself, through a Markdown link.
    #[test]
    fn a_generated_markdown_heading_link_with_a_literal_hash_parses_and_resolves() {
        let entries = vec![entry("/根", "次.md", vec![heading(1, "第#一章", 0)])];
        let source_file = Path::new("/根/現在.md");
        let typed = "[見よ](次.md#";
        let context = link_completion::detect(typed, typed.len()).unwrap();
        let candidate =
            &link_completion::candidates(&entries, Some(source_file), "", &context, 10)[0];
        let full_line = format!("{typed}{}", candidate.insert);

        let byte = typed.len();
        let (target, is_wiki) =
            document::link_target_at(&full_line, byte).expect("parses as a link");
        assert!(!is_wiki);
        let resolved =
            resolve_link(target, is_wiki, Some(source_file), "", &entries).expect("resolves");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/次.md"),
                heading: Some(HeadingTarget {
                    text: "第#一章".to_owned(),
                    occurrence: None
                }),
            }
        );
    }

    /// Codex preliminary review, engine corrections: accepting a specific
    /// same-directory candidate must resolve to *that* file, never an
    /// ambiguous global basename lookup, even when another root has a file
    /// of the same name.
    #[test]
    fn a_same_directory_candidates_insert_resolves_to_that_exact_file_despite_a_duplicate_basename_elsewhere()
     {
        let entries = vec![
            entry("/根", "note.md", Vec::new()),
            entry("/よそ", "note.md", Vec::new()),
        ];
        let source_file = Path::new("/根/現在.md");
        let typed = "[[";
        let context = link_completion::detect(typed, typed.len()).unwrap();
        let found = link_completion::candidates(&entries, Some(source_file), "", &context, 10);
        let same_directory = found
            .iter()
            .find(|c| c.path == *"/根/note.md")
            .expect("has one");

        let full_line = format!("{typed}{}", same_directory.insert);
        let (target, is_wiki) = document::link_target_at(&full_line, 3).expect("parses as a link");
        assert!(is_wiki);

        let resolved = resolve_link(target, is_wiki, Some(source_file), "", &entries)
            .expect("resolves, not ambiguous");
        assert_eq!(
            resolved,
            ResolvedLink::Target {
                path: PathBuf::from("/根/note.md"),
                heading: None
            }
        );
    }

    /// A bare filename hand-typed with no candidate selection (so no `./`
    /// qualifier) still goes through the index and is refused as ambiguous
    /// when more than one root shares the basename — the deliberate case
    /// `resolve_explicit_path` is *not* meant to shortcut.
    #[test]
    fn a_hand_typed_bare_basename_with_a_duplicate_elsewhere_is_still_ambiguous() {
        let entries = vec![
            entry("/根", "note.md", Vec::new()),
            entry("/よそ", "note.md", Vec::new()),
        ];
        let error = resolve_link("note.md", true, None, "", &entries).unwrap_err();
        assert!(matches!(error, ResolveError::Ambiguous(_)));
    }
}
