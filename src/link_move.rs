//! A user-requested move updates references only in Auto Save documents.
//! Scanning/planning is read-only on a worker; writes are checked on the UI
//! thread so an opened or edited buffer can never be overwritten by that worker.
use crate::*;
use std::collections::{BTreeSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

struct Planned {
    path: PathBuf,
    original: file_io::LoadedFile,
    text: String,
}
struct Batch {
    entries: Vec<workspace_index::Entry>,
    plans: VecDeque<Planned>,
    skipped: usize,
}
pub(crate) struct Job {
    from: PathBuf,
    to: PathBuf,
    receiver: mpsc::Receiver<Result<Batch, String>>,
    cancelled: Arc<AtomicBool>,
    batch: Option<Batch>,
    updated: usize,
    roots: Vec<PathBuf>,
    processed: BTreeSet<PathBuf>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn start(window: &AppWindow, live: &Live, from: PathBuf, to: PathBuf) {
    let Some(runtime) = live.folder.borrow().workspace.clone() else {
        return;
    };
    let registry = runtime.borrow().registry().clone();
    if !registry
        .folders()
        .iter()
        .any(|f| f.mode == workspace::SaveMode::AutoSave)
    {
        return;
    }
    let roots = if runtime.borrow().active_workspace().is_some() {
        runtime.borrow().active_roots()
    } else {
        live.folder.borrow().root.clone().into_iter().collect()
    };
    if roots.is_empty() {
        return;
    }
    let job_roots = roots.clone();
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    let old = from.clone();
    let new = to.clone();
    let spawned = std::thread::Builder::new()
        .name("link-move-plan".into())
        .spawn(move || {
            let result = plan(&roots, &registry, &old, &new, &signal);
            let _ = sender.send(result);
        });
    if spawned.is_err() {
        window.tell(
            pick(
                "移動しましたが、リンク更新を開始できませんでした",
                "Moved, but could not start updating links",
            )
            .into(),
        );
        return;
    }
    live.folder.borrow_mut().link_move_job = Some(Job {
        from,
        to,
        receiver,
        cancelled,
        batch: None,
        updated: 0,
        roots: job_roots,
        processed: BTreeSet::new(),
    });
    window.tell(
        pick(
            "移動しました。Auto Save文書のリンクを確認しています…",
            "Moved. Checking links in Auto Save documents…",
        )
        .into(),
    );
}

fn plan(
    roots: &[PathBuf],
    registry: &workspace::Registry,
    from: &Path,
    to: &Path,
    cancel: &AtomicBool,
) -> Result<Batch, String> {
    let paths = inventory(roots, cancel)?;
    let mut entries = Vec::new();
    for path in &paths {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let current = path.canonicalize().unwrap_or_else(|_| path.clone());
        let before = file_tree::moved_path(to, from, &current).unwrap_or(current);
        // Every moved file becomes an entry, whatever its type. The index
        // itself keeps only notes and images, but a link to a `.pdf` or
        // `.txt` still has to follow that file when it is renamed or moved
        // (2026-09-21). Resolving those entries is `resolve_indexed_file`'s
        // job; the entries here are what it resolves against.
        let root = roots
            .iter()
            .filter(|r| before.starts_with(r))
            .max_by_key(|r| r.components().count())
            .cloned()
            .unwrap_or_default();
        let relative = before.strip_prefix(&root).unwrap_or(&before).to_path_buf();
        let fingerprint = file_io::FileStamp::read(path).map_err(|error| error.to_string())?;
        entries.push(workspace_index::Entry {
            root,
            relative,
            canonical: before,
            fingerprint,
            headings: Vec::new(),
            headings_complete: false,
        });
    }
    let mut plans = VecDeque::new();
    let mut skipped = 0;
    let mut retained_bytes = 0usize;
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if !file_tree::is_searchable(&path)
            || registry.save_mode_for(Some(&path)) != workspace::SaveMode::AutoSave
        {
            continue;
        }
        let path = path.canonicalize().unwrap_or(path);
        let original = match file_io::read(&path, MAX_DOCUMENT_CHARACTERS) {
            Ok(value) => value,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let before = file_tree::moved_path(to, from, &path).unwrap_or_else(|| path.clone());
        let text = link_rewrite::rewrite(&original.text, &before, &path, from, to, &entries);
        if text != original.text {
            if text.chars().count() > MAX_DOCUMENT_CHARACTERS {
                skipped += 1;
                continue;
            }
            retained_bytes = retained_bytes
                .saturating_add(original.text.len())
                .saturating_add(text.len());
            if retained_bytes > 64 * 1024 * 1024 {
                return Err("link update exceeds memory limit".into());
            }
            plans.push_back(Planned {
                path,
                original,
                text,
            });
        }
    }
    Ok(Batch {
        entries,
        plans,
        skipped,
    })
}

/// The same target population as the index: hidden files and attachments count,
/// linked directories/files do not. An incomplete inventory cannot prove uniqueness.
fn inventory(roots: &[PathBuf], cancel: &AtomicBool) -> Result<Vec<PathBuf>, String> {
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if linked(&metadata) {
            continue;
        }
        let path = path.canonicalize().map_err(|error| error.to_string())?;
        if !seen.insert(path.clone()) {
            continue;
        }
        if seen.len() > workspace_index::MAX_ENTRIES * 2 {
            return Err("too many entries".into());
        }
        if metadata.is_dir() {
            for child in std::fs::read_dir(&path).map_err(|error| error.to_string())? {
                let child = child.map_err(|error| error.to_string())?;
                if matches!(child.file_name().to_str(), Some(".git" | "target")) {
                    continue;
                }
                pending.push(child.path());
                if pending.len() > workspace_index::MAX_ENTRIES * 2 {
                    return Err("too many entries".into());
                }
            }
        } else if metadata.is_file() {
            files.push(path);
            if files.len() > workspace_index::MAX_ENTRIES {
                return Err("too many files".into());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn linked(metadata: &std::fs::Metadata) -> bool {
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

fn all_held_documents(live: &Live) -> Vec<Rc<OpenDocument>> {
    let mut result = Vec::new();
    for tab in live.tabs.borrow().panes.iter().flat_map(|pane| &pane.tabs) {
        if !result.iter().any(|held| Rc::ptr_eq(held, &tab.document)) {
            result.push(tab.document.clone());
        }
    }
    result
}

pub(crate) fn tick(window: &AppWindow, live: &Live) {
    let Some(mut job) = live.folder.borrow_mut().link_move_job.take() else {
        return;
    };
    if job.batch.is_none() {
        match job.receiver.try_recv() {
            Ok(Ok(batch)) => {
                job.batch = Some(batch);
            }
            Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                window.tell(
                    pick(
                        "移動しましたが、リンク更新を完了できませんでした",
                        "Moved, but link updates could not be completed",
                    )
                    .into(),
                );
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {
                live.folder.borrow_mut().link_move_job = Some(job);
                return;
            }
        }
    }
    update_open(window, live, &mut job);
    let runtime = live.folder.borrow().workspace.clone();
    let registry = runtime.map(|r| r.borrow().registry().clone());
    let batch = job.batch.as_mut().unwrap();
    // Bound each UI pass; source files opened since planning are handled with
    // their current buffer, and never through a stale disk replacement.
    for _ in 0..8 {
        let Some(item) = batch.plans.pop_front() else {
            break;
        };
        if all_held_documents(live)
            .iter()
            .any(|d| same_path(d.file.borrow().path(), &item.path))
        {
            continue;
        }
        if !job.processed.insert(item.path.clone()) {
            continue;
        }
        if registry
            .as_ref()
            .is_none_or(|r| r.save_mode_for(Some(&item.path)) != workspace::SaveMode::AutoSave)
        {
            continue;
        }
        if write_checked(&item).is_ok() {
            job.updated += 1;
        } else {
            batch.skipped += 1;
        }
    }
    if batch.plans.is_empty() {
        window.tell(
            format!(
                "{}: {} / {}: {}",
                pick("リンク更新", "Links updated"),
                job.updated,
                pick("未更新", "Skipped"),
                batch.skipped
            )
            .into(),
        );
        publish_tabs(window, live);
    } else {
        live.folder.borrow_mut().link_move_job = Some(job);
    }
}

fn same_path(path: Option<&Path>, expected: &Path) -> bool {
    path.is_some_and(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) == expected)
}

fn write_checked(item: &Planned) -> Result<(), ()> {
    if std::fs::metadata(&item.path)
        .map_err(|_| ())?
        .permissions()
        .readonly()
    {
        return Err(());
    }
    let current = file_io::read(&item.path, MAX_DOCUMENT_CHARACTERS).map_err(|_| ())?;
    if current.stamp != item.original.stamp
        || current.text != item.original.text
        || current.form != item.original.form
    {
        return Err(());
    }
    file_io::save(&item.path, &item.text, item.original.form).map_err(|_| ())?;
    Ok(())
}

fn update_open(window: &AppWindow, live: &Live, job: &mut Job) {
    let Some(runtime) = live.folder.borrow().workspace.clone() else {
        return;
    };
    let registry = runtime.borrow().registry().clone();
    let engine = live.folder.borrow().folder_autosave.clone();
    for document in all_held_documents(live) {
        let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
            continue;
        };
        let path = path.canonicalize().unwrap_or(path);
        if !job
            .roots
            .iter()
            .any(|root| path.starts_with(root.canonicalize().unwrap_or_else(|_| root.clone())))
            || !file_tree::is_searchable(&path)
            || !job.processed.insert(path.clone())
        {
            continue;
        }
        if registry.save_mode_for(Some(&path)) != workspace::SaveMode::AutoSave {
            continue;
        }
        if document.read_only()
            || has_reading_view(window, live, &document)
            || document.outside.get()
            || document.missing.get()
            || document.file.borrow().current_stamp() != document.file.borrow().agreed_stamp()
        {
            job.batch.as_mut().unwrap().skipped += 1;
            continue;
        }
        let before =
            file_tree::moved_path(&job.to, &job.from, &path).unwrap_or_else(|| path.clone());
        let old = document.text.borrow().clone();
        let next = link_rewrite::rewrite(
            &old,
            &before,
            &path,
            &job.from,
            &job.to,
            &job.batch.as_ref().unwrap().entries,
        );
        if old == next {
            continue;
        }
        if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
            job.batch.as_mut().unwrap().skipped += 1;
            continue;
        }
        engine
            .borrow_mut()
            .observe(&document, &registry, Instant::now());
        document.history.borrow_mut().separate_next = true;
        let (at, removed, inserted) = find::changed_span(&old, &next);
        let change = Change {
            at,
            removed,
            inserted,
        };
        document.record(
            at,
            old[at..at + removed].to_owned(),
            next[at..at + inserted].to_owned(),
        );
        *document.text.borrow_mut() = next.clone();
        document.history.borrow_mut().separate_next = true;
        // draw_edit already carries and draws its peers; only the originating
        // state's caret needs carrying here, and each peer must move only once.
        if let Some(id) = PaneId::all(window)
            .into_iter()
            .find(|id| Rc::ptr_eq(&live.states.document(*id), &document))
        {
            carry_state_across(&live.states.of(id), change, &next);
            id.draw_edit(
                window,
                &live.states,
                &live.cache,
                &document,
                &next,
                None,
                change,
            );
        }
        write_work_copy_of(window, live, &document);
        job.updated += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!(
            "rfn-link-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        folder.canonicalize().unwrap()
    }
    #[test]
    fn inventory_includes_attachments_and_hidden_paths_but_excludes_build_folders() {
        let root = scratch("inventory");
        for dir in [".hidden", "target", ".git"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        for name in [
            "diagram.pdf",
            ".note.md",
            ".hidden/target.png",
            "target/out.md",
            ".git/config",
        ] {
            std::fs::write(root.join(name), "content").unwrap();
        }
        let paths = inventory(std::slice::from_ref(&root), &AtomicBool::new(false)).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths.contains(&root.join("diagram.pdf")));
        assert!(paths.contains(&root.join(".note.md")));
        assert!(paths.contains(&root.join(".hidden/target.png")));
        assert!(inventory(&[root.join("missing")], &AtomicBool::new(false)).is_err());
        assert!(inventory(std::slice::from_ref(&root), &AtomicBool::new(true)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn attachment_rename_plans_only_on_sources_including_hidden_ones() {
        let root = scratch("policy");
        let off = root.join("off");
        std::fs::create_dir(&off).unwrap();
        std::fs::write(root.join("new.pdf"), b"not text").unwrap();
        for name in ["on.md", ".hidden.md", "off/keep.md"] {
            std::fs::write(root.join(name), "[[old.pdf|diagram]]").unwrap();
        }
        let mut registry = workspace::Registry::default();
        let id = registry.create_workspace("test".into()).unwrap();
        let folder = registry.add_root(id, &root).unwrap();
        registry
            .set_folder_mode(folder, workspace::SaveMode::AutoSave)
            .unwrap();
        registry.add_root(id, &off).unwrap();
        let batch = plan(
            std::slice::from_ref(&root),
            &registry,
            &root.join("old.pdf"),
            &root.join("new.pdf"),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(batch.plans.len(), 2);
        assert!(
            batch
                .plans
                .iter()
                .all(|item| item.text == "[[./new.pdf|diagram]]")
        );
        assert!(!batch.plans.iter().any(|item| item.path.starts_with(&off)));
        assert_eq!(
            std::fs::read_to_string(root.join("on.md")).unwrap(),
            "[[old.pdf|diagram]]",
            "planning must not write"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn checked_write_preserves_external_edits_and_encoding() {
        let folder = std::env::temp_dir().join(format!("rfn-link-write-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("source.md");
        std::fs::write(&path, "[x](old.md)\r\n").unwrap();
        let original = file_io::read(&path, 1000).unwrap();
        let item = Planned {
            path: path.clone(),
            original,
            text: "[x](new.md)\n".into(),
        };
        write_checked(&item).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[x](new.md)\r\n");
        std::fs::write(&path, "external content").unwrap();
        assert!(write_checked(&item).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external content");
        let _ = std::fs::remove_dir_all(folder);
    }
}
