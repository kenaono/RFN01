//! Phase 2 of Workspace設計.md: a cancellable background index of every
//! registered root's files and headings, with a small on-disk cache so link
//! completion has something to show before a scan finishes.
//!
//! **No Slint, no document writes, no save-mode decisions** — those are
//! [`crate::workspace`] (phase 1) and later phases.
//!
//! # Lifecycle (Codex review, round 2)
//!
//! [`Indexer`] owns exactly one persistent worker thread for its whole life,
//! fed by a command queue. This is what actually solves the cancel/reset-vs-
//! cache-write race the first draft had: because the worker processes one
//! [`Command`] at a time, a [`Command::ClearDir`] enqueued after a
//! [`Command::Scan`] cannot run — and so cannot delete or be raced by
//! anything — until that scan's own call to [`run_scan`] has already
//! returned. No two writers are ever touching a cache file at once, by
//! construction, without any lock.
//!
//! What the shared `Arc<AtomicU64>` generation is *for*, on top of that, is
//! purely responsiveness: without it, a scan superseded by a fresh
//! [`Indexer::restart`] would still run to completion — reading, parsing,
//! writing — before the worker even looked at the newer command sitting
//! behind it in the queue. [`still_current`] and [`send_checked`] let a scan
//! notice it has been superseded and stop quickly instead, including while
//! it is trying to publish into a full event channel — 仕様の実装依頼 "sends
//! cooperatively cancellable... without needing to drain".

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::document;
use crate::file_io;

pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("md"))
}
pub fn is_image(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        [
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "jxr", "heic",
        ]
        .iter()
        .any(|e| x.eq_ignore_ascii_case(e))
    })
}
pub fn is_indexable(path: &Path) -> bool {
    is_markdown(path) || is_image(path)
}

#[derive(Clone, Debug, Default)]
pub struct ScanMetrics {
    pub queue_ms: f64,
    pub first_ms: Option<f64>,
    pub parse_work_ms: f64,
    pub partial: bool,
    pub generation: u64,
    pub elapsed_ms: f64,
    pub cache_ms: f64,
    pub walk_ms: f64,
    pub parse_ms: f64,
    pub save_ms: f64,
    pub directories: usize,
    pub files: usize,
    pub reused: usize,
    pub parsed: usize,
    pub workers: usize,
    pub completed: bool,
}
impl ScanMetrics {
    pub fn log(&self) -> String {
        format!(
            "index generation={} elapsed_ms={:.3} queue_ms={:.3} first_ms={:?} cache_ms={:.3} walk_ms={:.3} parse_ms={:.3} parse_work_ms={:.3} save_ms={:.3} dirs={} files={} reused={} parsed={} workers={} completed={} partial={}",
            self.generation,
            self.elapsed_ms,
            self.queue_ms,
            self.first_ms,
            self.cache_ms,
            self.walk_ms,
            self.parse_ms,
            self.parse_work_ms,
            self.save_ms,
            self.directories,
            self.files,
            self.reused,
            self.parsed,
            self.workers,
            self.completed,
            self.partial
        )
    }
}

/// One file under a registered root, as the index knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Canonical, absolute — the registered root this file was found under.
    pub root: PathBuf,
    /// `canonical` with `root` taken off the front.
    pub relative: PathBuf,
    /// `root.join(relative)`.
    pub canonical: PathBuf,
    /// Modified time and length — what change detection compares against.
    pub fingerprint: file_io::FileStamp,
    /// Empty when this is not a markdown file at all ([`is_markdown`] — an
    /// image entry keeps only its name, per 索引対象の合意, and `.txt`/`.log`
    /// are not indexed), when parsing has not happened yet this run
    /// ([`Entry::headings_complete`] is `false` and the file was just
    /// published as a file-only stub), or when it is one but could not be
    /// read in full.
    pub headings: Vec<document::Heading>,
    /// `false` for a file-only stub not parsed yet, for a file over
    /// [`ScanOptions::max_heading_bytes`], or one that failed to decode. A
    /// file left incomplete is retried on the *next* scan even if its
    /// fingerprint has not changed — see [`Entry`]'s own doc and the
    /// `a_previously_incomplete_file_is_retried_even_unchanged` test.
    pub headings_complete: bool,
}

/// Limits a scan enforces on itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanOptions {
    pub force_reparse: bool,
    pub workers: usize,
    pub max_entries: usize,
    pub batch_size: usize,
    /// A markdown file past this many bytes is listed but not parsed for
    /// headings.
    pub max_heading_bytes: u64,
    pub excludes: Vec<String>,
}

pub const MAX_ENTRIES: usize = 100_000;
pub const MAX_BATCH: usize = 128;

/// A generation no real scan ever uses (`Indexer`'s own counter starts at 1
/// and only grows) — what a cache-maintenance operation that must not
/// supersede any in-flight scan tags its own `EventKind::Error` with, so a
/// caller can tell "this scan failed" from "cache maintenance for an
/// unrelated/inactive scope failed" without the two sharing a generation
/// number. See [`Indexer::clear_workspace_cache`].
pub const MAINTENANCE_GENERATION: u64 = u64::MAX;

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            force_reparse: false,
            workers: thread::available_parallelism().map_or(2, |n| n.get().min(4)),
            max_entries: MAX_ENTRIES,
            batch_size: MAX_BATCH,
            max_heading_bytes: 2 * 1024 * 1024,
            excludes: vec![".git".to_owned(), "target".to_owned()],
        }
    }
}

/// One thing a scan reports, tagged with the generation it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub generation: u64,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    InventoryComplete {
        complete: bool,
    },
    /// What a per-root cache file already held.
    Cached(Vec<Entry>),
    /// A file-only stub (just discovered, headings not parsed yet) or a
    /// fully parsed entry replacing one — a caller tells which by
    /// [`Entry::headings_complete`]; both arrive on this same variant so a
    /// [`Snapshot`] does not need to know the difference.
    Updated(Vec<Entry>),
    /// Canonical paths pruned. Never includes anything under a directory
    /// this scan could not read, and never sent at all when the scan was
    /// [`EventKind::Complete`]'s `truncated`.
    Removed(Vec<PathBuf>),
    /// A per-root cache write failed — reported, not swallowed. The scan
    /// itself still finished; only its cache is stale or missing for `root`.
    CacheWriteFailed {
        root: PathBuf,
        message: String,
    },
    /// The scan reached the end of every kept root.
    Complete {
        /// Directories that could not be read (permission, mostly).
        /// Whatever they held before is left exactly as it was.
        failed: Vec<PathBuf>,
        /// Requested roots that could not even be canonicalized (missing, or
        /// unreadable at the root itself) — scanned not at all this run.
        missing_roots: Vec<PathBuf>,
        /// `true` when [`ScanOptions::max_entries`] was reached and at least
        /// one new file was refused because of it. The walk itself still
        /// ran to completion either way, so pruning and the cache write are
        /// not affected — this only means the result may be missing some
        /// new files that existed beyond the cap.
        truncated: bool,
    },
    /// The scan could not proceed at all.
    Error(String),
}

/// A pure fold of [`Event`]s into the entries a caller is holding.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    generation: u64,
    entries: Vec<Entry>,
    /// `entries[i].canonical -> i`, kept in step with `entries` so a large
    /// snapshot's updates/removals stay near-linear instead of a per-event
    /// scan of the whole vector.
    index: HashMap<PathBuf, usize>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Applies `event`, unless it is from a generation older than the one
    /// already applied, in which case it is dropped and this returns
    /// `false`. A newer generation clears everything held so far.
    pub fn apply(&mut self, event: Event) -> bool {
        if event.generation < self.generation {
            return false;
        }
        if event.generation > self.generation {
            self.generation = event.generation;
            self.entries.clear();
            self.index.clear();
        }
        match event.kind {
            EventKind::Cached(found) | EventKind::Updated(found) => {
                for entry in found {
                    self.upsert(entry);
                }
            }
            EventKind::Removed(paths) => {
                for path in &paths {
                    self.remove(path);
                }
            }
            EventKind::InventoryComplete { .. }
            | EventKind::Complete { .. }
            | EventKind::Error(_)
            | EventKind::CacheWriteFailed { .. } => {}
        }
        true
    }

    fn upsert(&mut self, entry: Entry) {
        if let Some(&position) = self.index.get(&entry.canonical) {
            self.entries[position] = entry;
        } else {
            let position = self.entries.len();
            self.index.insert(entry.canonical.clone(), position);
            self.entries.push(entry);
        }
    }

    fn remove(&mut self, path: &Path) {
        let Some(position) = self.index.remove(path) else {
            return;
        };
        self.entries.swap_remove(position);
        if let Some(moved) = self.entries.get(position) {
            self.index.insert(moved.canonical.clone(), position);
        }
    }
}

/// Sent to the persistent worker; processed strictly one at a time, in the
/// order [`Indexer`] sent them — the whole basis for
/// [`Indexer::clear_workspace_cache`] never racing a [`Command::Scan`]'s cache
/// write.
enum Command {
    PruneShared {
        cache_dir: PathBuf,
        roots: Vec<PathBuf>,
    },
    Scan {
        queued: Instant,
        generation: u64,
        roots: Vec<PathBuf>,
        /// `None` for [`Indexer::restart_memory`]: scans without reading or
        /// writing any cache, for when appdata is unavailable.
        cache_dir: Option<PathBuf>,
        options: ScanOptions,
    },
    /// Every cache file this build's own naming produces under `cache_dir`,
    /// regardless of which roots are currently registered — see
    /// [`Indexer::clear_workspace_cache`]. `generation` only tags an
    /// `EventKind::Error` if deletion fails; it never gates whether the clear
    /// itself runs (see [`run_worker`]).
    ClearDir { generation: u64, cache_dir: PathBuf },
}

/// One persistent worker thread, a command queue, and the generation the
/// caller currently wants active. See the module doc for why this shape —
/// not one thread per scan — is what makes cancellation and reset safe.
pub struct Indexer {
    metrics: Arc<std::sync::Mutex<std::collections::VecDeque<ScanMetrics>>>,
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<Event>,
    /// Kept alongside `events`'s own receiver so `restart`/`clear_workspace_cache` can report
    /// "the worker is gone" as an [`EventKind::Error`] instead of an
    /// [`Indexer`] that silently stops doing anything.
    events_tx: mpsc::SyncSender<Event>,
    generation: Arc<AtomicU64>,
    next_generation: u64,
}

impl Default for Indexer {
    fn default() -> Self {
        Self::new()
    }
}

/// Event channel capacity, and the bound `Indexer::poll` returns per call —
/// keeping the two equal is what makes one `poll` call cost is bounded even
/// if the worker keeps refilling the channel as fast as it is drained.
const EVENT_CHANNEL_CAPACITY: usize = 8;

impl Indexer {
    pub fn retain_shared(&self, cache_dir: PathBuf, roots: Vec<PathBuf>) {
        self.report_if_disconnected(
            self.commands
                .send(Command::PruneShared { cache_dir, roots }),
            MAINTENANCE_GENERATION,
        );
    }
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel::<Command>();
        // Bounded: a scan cannot outrun a slow or absent consumer without
        // limit. `send_checked` is what keeps a full channel from blocking
        // the worker forever once it has been superseded.
        let (event_tx, event_rx) = mpsc::sync_channel::<Event>(EVENT_CHANNEL_CAPACITY);
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        // A clone goes to the worker; `event_tx` itself is kept so a failed
        // spawn (or, later, a disconnected worker) can still report an
        // `EventKind::Error` from here rather than going silently idle.
        let worker_events = event_tx.clone();
        let metrics = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let worker_metrics = metrics.clone();
        let spawned = thread::Builder::new()
            .name("workspace-index".into())
            .spawn(move || {
                run_worker(command_rx, worker_events, worker_generation, worker_metrics)
            });
        if spawned.is_err() {
            let _ = event_tx.try_send(Event {
                generation: 0,
                kind: EventKind::Error(
                    "failed to start the workspace index worker thread".to_owned(),
                ),
            });
        }
        Self {
            metrics,
            commands: command_tx,
            events: event_rx,
            events_tx: event_tx,
            generation,
            next_generation: 1,
        }
    }

    /// Reports `error` as an `EventKind::Error` tagged with `generation` when
    /// `sent` says the command never reached the worker — the persistent
    /// worker thread has exited (a panic, most plausibly) and nothing further
    /// will happen silently instead.
    fn report_if_disconnected(&self, sent: Result<(), mpsc::SendError<Command>>, generation: u64) {
        if sent.is_err() {
            let _ = self.events_tx.try_send(Event {
                generation,
                kind: EventKind::Error("the workspace index worker is not running".to_owned()),
            });
        }
    }

    /// Starts a new scan. Any scan already running is superseded — it will
    /// notice at its next cooperative check and stop publishing or writing —
    /// without this call waiting for it. Returns the new generation.
    pub fn restart(
        &mut self,
        roots: Vec<PathBuf>,
        cache_dir: PathBuf,
        options: ScanOptions,
    ) -> u64 {
        self.start_scan(roots, Some(cache_dir), options)
    }

    /// Same as [`Indexer::restart`], but never reads or writes a cache — for
    /// when appdata is unavailable and there is nowhere to put one.
    pub fn restart_memory(&mut self, roots: Vec<PathBuf>, options: ScanOptions) -> u64 {
        self.start_scan(roots, None, options)
    }

    fn start_scan(
        &mut self,
        roots: Vec<PathBuf>,
        cache_dir: Option<PathBuf>,
        options: ScanOptions,
    ) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.generation.store(generation, Ordering::SeqCst);
        let sent = self.commands.send(Command::Scan {
            queued: Instant::now(),
            generation,
            roots,
            cache_dir,
            options,
        });
        self.report_if_disconnected(sent, generation);
        generation
    }

    /// Supersedes any in-flight scan without starting a new one.
    pub fn cancel(&mut self) {
        self.generation
            .store(self.next_generation, Ordering::SeqCst);
        self.next_generation += 1;
    }

    /// Removes every per-root cache file this build's own naming produces
    /// (`index-<16hex>.rfnwsidx`, see [`cache_file_for`]) directly under
    /// `cache_dir`, then the directory itself if that leaves it empty —
    /// regardless of which roots are currently registered, so a root
    /// detached or relocated from a Workspace since its cache was written is
    /// not left behind (a caller does not have to track historical roots).
    ///
    /// **This never supersedes an in-flight scan** — it does not touch the shared generation at all, only takes
    /// its own turn on the same worker queue, after anything already queued
    /// ahead of it (see the module doc), so it cannot race a scan's own
    /// cache write either. This is what makes it safe to call for a
    /// Workspace other than the one currently being scanned.
    pub fn clear_workspace_cache(&mut self, cache_dir: PathBuf) {
        let sent = self.commands.send(Command::ClearDir {
            generation: MAINTENANCE_GENERATION,
            cache_dir,
        });
        self.report_if_disconnected(sent, MAINTENANCE_GENERATION);
    }

    /// Up to [`EVENT_CHANNEL_CAPACITY`] events waiting right now, without
    /// blocking — bounded even if the worker keeps refilling the channel, so
    /// one UI tick's cost stays bounded too.
    pub fn take_metrics(&self) -> Vec<ScanMetrics> {
        self.metrics.lock().unwrap().drain(..).collect()
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while events.len() < EVENT_CHANNEL_CAPACITY {
            match self.events.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }
        events
    }

    /// The next event, waiting at most `timeout` — for tests that must not
    /// hang if a fix regresses.
    #[cfg(test)]
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }
}

/// Supersedes any in-flight scan so it stops publishing/writing promptly,
/// the same as an explicit [`Indexer::cancel`] — dropping an `Indexer` is not
/// a reason for a scan already running to keep going to completion. Dropping
/// `self.commands` right after (an ordinary field drop) is what then lets
/// the persistent worker's `commands.recv()` return `Err` and the thread end
/// on its own — this never joins it.
impl Drop for Indexer {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run_worker(
    commands: mpsc::Receiver<Command>,
    events: mpsc::SyncSender<Event>,
    generation: Arc<AtomicU64>,
    metrics: Arc<std::sync::Mutex<std::collections::VecDeque<ScanMetrics>>>,
) {
    let inventory = crate::index_work::Inventory::default();
    let mut previous_scope = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::PruneShared { cache_dir, roots } => {
                if let Ok(read) = fs::read_dir(&cache_dir) {
                    for item in read.flatten() {
                        if !item.file_type().is_ok_and(|t| t.is_file())
                            || !is_own_cache_file_name(&item.file_name().to_string_lossy())
                        {
                            continue;
                        }
                        let Some(raw) = read_bounded(&item.path()) else {
                            continue;
                        };
                        let Some((root, _)) = decode_cache(&raw) else {
                            continue;
                        };
                        if roots
                            .iter()
                            .any(|r| root.starts_with(r) || r.starts_with(&root))
                        {
                            continue;
                        }
                        let lock = item.path().with_extension("lock");
                        let mut options = fs::OpenOptions::new();
                        options.write(true).create(true).truncate(false);
                        #[cfg(windows)]
                        {
                            use std::os::windows::fs::OpenOptionsExt;
                            options.share_mode(0);
                        }
                        // The lease is released *before* the lock file itself is removed:
                        // Windows refuses to delete a file whose handle is still open, so
                        // removing it inside the block would always fail. A writer that
                        // starts in the meantime simply recreates the file.
                        if let Ok(lease) = options.open(&lock) {
                            let _ = fs::remove_file(item.path());
                            drop(lease);
                            let _ = fs::remove_file(&lock);
                        }
                    }
                }
            }
            Command::Scan {
                queued,
                generation: my_generation,
                roots,
                cache_dir,
                options,
            } => {
                let key = (roots.clone(), options.excludes.clone());
                // Deliberately asymmetric, 2026-09-21: the direction is the
                // decision, not an inverted comparison.
                //
                // A *changed* scope keeps the directory listings this cache
                // exists for — switching from a parent folder to a child one
                // inside the same 30-second window must not list the shared
                // subtree a second time. A repeat scan of the *same* scope
                // drops them: scans of one scope are 30 seconds apart, so
                // those listings sit at the end of their life anyway, and
                // dropping them keeps the map bounded to the scope in use.
                // (`options.force_reparse`, a reset, always drops them.)
                if options.force_reparse
                    || previous_scope
                        .as_ref()
                        .is_none_or(|(old_roots, old_excludes)| {
                            old_roots == &roots || old_excludes != &options.excludes
                        })
                {
                    inventory.lock().unwrap().clear();
                }
                previous_scope = Some(key);
                let started = Instant::now();
                let mut measured = ScanMetrics {
                    queue_ms: started.duration_since(queued).as_secs_f64() * 1000.0,
                    generation: my_generation,
                    workers: options.workers,
                    ..Default::default()
                };
                run_scan(
                    my_generation,
                    roots,
                    cache_dir,
                    options,
                    &generation,
                    &events,
                    &mut measured,
                    &inventory,
                );
                measured.elapsed_ms = queued.elapsed().as_secs_f64() * 1000.0;
                let mut log = metrics.lock().unwrap();
                if log.len() == 64 {
                    log.pop_front();
                }
                log.push_back(measured);
            }
            Command::ClearDir {
                generation: my_generation,
                cache_dir,
            } => {
                // Always runs, whatever `generation` now holds — the queue's
                // own ordering is what protects this, not a generation check
                // (see the module doc); skipping it here would be exactly
                // the "clear gets silently dropped" bug this exists to avoid.
                // `try_send`, never a blocking `send`: a full, undrained queue
                // must not stall the worker's next command waiting on room for
                // an error nobody may ever read.
                if let Err(error) = clear_workspace_cache_dir(&cache_dir) {
                    let _ = events.try_send(Event {
                        generation: my_generation,
                        kind: EventKind::Error(format!(
                            "failed to clear the workspace index cache directory {}: {error}",
                            cache_dir.display()
                        )),
                    });
                }
            }
        }
    }
}

fn still_current(my_generation: u64, generation: &AtomicU64) -> bool {
    generation.load(Ordering::SeqCst) == my_generation
}

/// Publishes `kind`, retrying against a full channel rather than blocking on
/// it — every retry re-checks `generation`, so a superseded scan stuck
/// behind a full, undrained channel gives up within one retry interval
/// instead of hanging until someone drains it (仕様の実装依頼 #3).
fn send_checked(
    events: &mpsc::SyncSender<Event>,
    my_generation: u64,
    generation: &AtomicU64,
    kind: EventKind,
) -> bool {
    let mut pending = Event {
        generation: my_generation,
        kind,
    };
    loop {
        if !still_current(my_generation, generation) {
            return false;
        }
        match events.try_send(pending) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(back)) => {
                pending = back;
                thread::sleep(Duration::from_millis(2));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Whether `name` is a filename [`cache_file_for`] itself could have
/// produced — `index-`, exactly 16 lowercase hex digits, `.rfnwsidx`. What
/// [`clear_workspace_cache_dir`] uses to decide what it may remove.
fn is_own_cache_file_name(name: &str) -> bool {
    const PREFIX: &str = "index-";
    const SUFFIX: &str = ".rfnwsidx";
    let Some(middle) = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
    else {
        return false;
    };
    middle.len() == 16
        && middle
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Removes every cache file [`is_own_cache_file_name`] recognizes directly
/// under `cache_dir` (never recursing into a subdirectory), then the
/// directory itself if that leaves it empty. An absent `cache_dir` is not an
/// error — there was nothing to clear. Leaving a non-empty or otherwise
/// unremovable directory behind (something else is in there, or a removal
/// above hit a transient error) is not an error either: the cache files this
/// build owns are what matters, not the directory's own presence.
fn clear_workspace_cache_dir(cache_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !is_own_cache_file_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let _ = fs::remove_dir(cache_dir);
    Ok(())
}

/// Every root that is not equal to, or nested inside, one that sorted before
/// it. Pure and disk-free: `roots` is taken as already canonical.
pub fn dedupe_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut ordered: Vec<PathBuf> = roots.to_vec();
    ordered.sort_by_key(|path| path.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in ordered {
        if !kept.iter().any(|existing| path.starts_with(existing)) {
            kept.push(path);
        }
    }
    kept
}

/// Whether `metadata` names something a walk must not descend into or index
/// as a plain file: a symlink on any platform, or — since
/// [`std::fs::FileType::is_symlink`] is not guaranteed to catch every
/// Windows reparse point a junction can be — the `FILE_ATTRIBUTE_REPARSE_POINT`
/// bit itself, checked directly under `cfg(windows)` (仕様の実装依頼 #5).
pub(crate) fn is_unsafe_to_descend(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || is_reparse_point(metadata)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn fingerprint_of(metadata: &fs::Metadata) -> file_io::FileStamp {
    file_io::FileStamp {
        modified: metadata.modified().ok(),
        length: metadata.len(),
    }
}

pub(crate) fn read_entry(path: &Path, roots: &[PathBuf]) -> Option<Entry> {
    if !is_indexable(path) {
        return None;
    }
    // Inspect the original path before canonicalization can hide reparse points.
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part);
        if let Ok(meta) = fs::symlink_metadata(&prefix) {
            if is_unsafe_to_descend(&meta) {
                return None;
            }
        }
    }
    let canonical = path.canonicalize().ok()?;
    let root = roots
        .iter()
        .filter(|r| canonical.starts_with(r))
        .max_by_key(|r| r.components().count())?
        .clone();
    let relative = canonical.strip_prefix(&root).ok()?.to_path_buf();
    if relative
        .components()
        .any(|p| matches!(p.as_os_str().to_str(), Some(".git" | "target")))
    {
        return None;
    }
    let metadata = fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let fingerprint = fingerprint_of(&metadata);
    let (headings, headings_complete) = if is_markdown(&canonical) {
        parse_headings(&canonical, fingerprint.length, 2 * 1024 * 1024)
    } else {
        (Vec::new(), true)
    };
    Some(Entry {
        root,
        relative,
        canonical,
        fingerprint,
        headings,
        headings_complete,
    })
}

/// Headings for a file already known to be `length` bytes, bounded to
/// `max_bytes` — over that, or not decodable as text, and this is
/// `(Vec::new(), false)` rather than an error.
///
/// Reads through `File::take(max_bytes + 1)` rather than [`fs::read`]:
/// `length` is a metadata snapshot from moments earlier, and a file that grew
/// past it in the meantime must not turn into an unbounded read here — the
/// one extra byte is only so a file that lands *exactly* at the bound is not
/// mistaken for one that is over it.
fn parse_headings(path: &Path, length: u64, max_bytes: u64) -> (Vec<document::Heading>, bool) {
    if length > max_bytes {
        return (Vec::new(), false);
    }
    let Ok(file) = fs::File::open(path) else {
        return (Vec::new(), false);
    };
    let mut bytes = Vec::new();
    if file.take(max_bytes + 1).read_to_end(&mut bytes).is_err() {
        return (Vec::new(), false);
    }
    if bytes.len() as u64 > max_bytes {
        return (Vec::new(), false);
    }
    match file_io::decode(&bytes, max_bytes as usize) {
        Ok((text, _form)) => (document::outline(&text), true),
        Err(_) => (Vec::new(), false),
    }
}

/// A file Phase A found that needs (re-)parsing in Phase B.
struct ToParse {
    root: PathBuf,
    path: PathBuf,
    fingerprint: file_io::FileStamp,
}

/// The whole of one scan, run on the persistent worker's own thread.
///
/// Two phases, both interleaved with publishing rather than collected first
/// and sent after (仕様の実装依頼 #1): Phase A walks the filesystem and
/// publishes each new-or-changed file as a headings-empty stub the moment it
/// is found, so a caller has file candidates long before headings are ready;
/// Phase B then parses headings for exactly what Phase A flagged, and
/// publishes the completed entries.
///
/// Returns early, sending nothing further, the moment this scan is no
/// longer [`still_current`] — see the module doc for why that alone does not
/// have to protect the final cache write from a race.
fn run_scan(
    my_generation: u64,
    roots: Vec<PathBuf>,
    cache_dir: Option<PathBuf>,
    options: ScanOptions,
    generation: &AtomicU64,
    events: &mpsc::SyncSender<Event>,
    metrics: &mut ScanMetrics,
    inventory: &crate::index_work::Inventory,
) {
    if !still_current(my_generation, generation) {
        return;
    }
    let options = ScanOptions {
        max_entries: options.max_entries.min(MAX_ENTRIES),
        batch_size: options.batch_size.clamp(1, MAX_BATCH),
        ..options
    };
    let started = Instant::now();
    let first = std::cell::Cell::new(None);
    let send = |kind: EventKind| {
        let candidate = matches!(&kind,EventKind::Cached(e)|EventKind::Updated(e) if !e.is_empty());
        let sent = send_checked(events, my_generation, generation, kind);
        if sent && candidate && first.get().is_none() {
            first.set(Some(started.elapsed().as_secs_f64() * 1000.0));
        }
        sent
    };

    let mut missing_roots = Vec::new();
    let mut canonical_roots = Vec::new();
    for root in &roots {
        match root.canonicalize() {
            Ok(canonical) => canonical_roots.push(canonical),
            Err(_) => missing_roots.push(root.clone()),
        }
    }
    let kept_roots = dedupe_roots(&canonical_roots);

    // A single counter for the union of everything this scan holds —
    // cache-loaded and newly walked alike — so `options.max_entries` is one
    // cap on the result, not a cap on the walk that a big enough cache could
    // already have blown past on its own. `truncated` only ever means "a
    // genuinely new entry was refused for being over the cap" — it never
    // stops the walk itself, so already-known entries keep being checked for
    // updates and removals even once the cap is reached.
    let mut known: HashMap<PathBuf, Entry> = HashMap::new();
    let mut total = 0usize;
    let mut truncated = false;

    let phase = Instant::now();
    if let Some(cache_dir) = &cache_dir {
        for root in &kept_roots {
            if !still_current(my_generation, generation) {
                return;
            }
            let Some(entries) = load_cache(cache_dir, root) else {
                continue;
            };
            let mut batch = Vec::new();
            for entry in entries {
                if !is_indexable(&entry.canonical) {
                    continue;
                }
                if total >= options.max_entries {
                    truncated = true;
                    continue;
                }
                total += 1;
                known.insert(entry.canonical.clone(), entry.clone());
                batch.push(entry);
                if batch.len() >= options.batch_size
                    && !send(EventKind::Cached(std::mem::take(&mut batch)))
                {
                    return;
                }
            }
            if !batch.is_empty() && !send(EventKind::Cached(batch)) {
                return;
            }
        }
    }

    metrics.cache_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = Instant::now();
    // --- Phase A: enumerate, publishing file-only stubs as they're found. ---
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut failed_dirs: Vec<PathBuf> = Vec::new();
    let mut to_parse: Vec<ToParse> = Vec::new();
    let mut stub_batch: Vec<Entry> = Vec::new();

    crate::index_work::walk(
        &kept_roots,
        &options.excludes,
        options.workers,
        inventory,
        &|| still_current(my_generation, generation),
        |found| {
            let (root, path, metadata) = match found {
                crate::index_work::Found::Directory => {
                    metrics.directories += 1;
                    return;
                }
                crate::index_work::Found::Failed(path) => {
                    failed_dirs.push(path);
                    return;
                }
                crate::index_work::Found::File(root, path, meta) => (root, path, meta),
            };
            metrics.files += 1;
            if !is_indexable(&path) {
                return;
            }
            seen.insert(path.clone());
            let fingerprint = fingerprint_of(&metadata);
            let previous = known.get(&path);
            if !options.force_reparse
                && previous.is_some_and(|e| e.fingerprint == fingerprint && e.headings_complete)
            {
                metrics.reused += 1;
                return;
            }
            if previous.is_none() {
                if total >= options.max_entries {
                    truncated = true;
                    return;
                }
                total += 1;
            }
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            to_parse.push(ToParse {
                root: root.clone(),
                path: path.clone(),
                fingerprint,
            });
            stub_batch.push(Entry {
                root,
                relative,
                canonical: path,
                fingerprint,
                headings: Vec::new(),
                headings_complete: false,
            });
            if stub_batch.len() >= options.batch_size {
                send(EventKind::Updated(std::mem::take(&mut stub_batch)));
            }
        },
    );
    metrics.walk_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = Instant::now();
    if !stub_batch.is_empty() && !send(EventKind::Updated(stub_batch)) {
        return;
    }
    if !send(EventKind::InventoryComplete {
        complete: failed_dirs.is_empty() && missing_roots.is_empty() && !truncated,
    }) {
        return;
    }

    // --- Phase B: parse headings for exactly what Phase A flagged. ---
    let mut final_batch: Vec<Entry> = Vec::new();
    metrics.parsed = to_parse.iter().filter(|i| is_markdown(&i.path)).count();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let parse_work = AtomicU64::new(0);
    let (tx, rx) = mpsc::sync_channel(options.batch_size);
    thread::scope(|scope| {
        for _ in 0..options.workers.clamp(1, 8) {
            let (tx, items, next, parse_work) = (tx.clone(), &to_parse, &next, &parse_work);
            let options = &options;
            scope.spawn(move || {
                while still_current(my_generation, generation) {
                    let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let task_started = Instant::now();
                    let (headings, headings_complete) = if is_markdown(&item.path) {
                        parse_headings(
                            &item.path,
                            item.fingerprint.length,
                            options.max_heading_bytes,
                        )
                    } else {
                        (Vec::new(), true)
                    };
                    parse_work
                        .fetch_add(task_started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    let entry = Entry {
                        root: item.root.clone(),
                        relative: item
                            .path
                            .strip_prefix(&item.root)
                            .unwrap_or(&item.path)
                            .to_path_buf(),
                        canonical: item.path.clone(),
                        fingerprint: item.fingerprint,
                        headings,
                        headings_complete,
                    };
                    if tx.send(entry).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for entry in rx {
            if !still_current(my_generation, generation) {
                continue;
            }
            known.insert(entry.canonical.clone(), entry.clone());
            final_batch.push(entry);
            if final_batch.len() >= options.batch_size {
                send(EventKind::Updated(std::mem::take(&mut final_batch)));
            }
        }
    });
    if !final_batch.is_empty() && !send(EventKind::Updated(final_batch)) {
        return;
    }

    metrics.parse_ms = phase.elapsed().as_secs_f64() * 1000.0;
    metrics.parse_work_ms = parse_work.load(Ordering::Relaxed) as f64 / 1000.0;
    // Pruned only from a successfully scanned area. Runs whether or not this
    // scan was `truncated` — the walk above always covers every root in
    // full, so `seen`/`failed_dirs` are complete regardless; `truncated`
    // only ever means some new entry was refused for being over the cap, not
    // that anything here is stale.
    let stale: Vec<PathBuf> = known
        .keys()
        .filter(|path| {
            !seen.contains(*path) && !failed_dirs.iter().any(|failed| path.starts_with(failed))
        })
        .cloned()
        .collect();
    for path in &stale {
        known.remove(path);
    }
    for chunk in stale.chunks(options.batch_size) {
        if !still_current(my_generation, generation) {
            return;
        }
        if !send(EventKind::Removed(chunk.to_vec())) {
            return;
        }
    }

    let phase = Instant::now();
    if let Some(cache_dir) = &cache_dir {
        for root in &kept_roots {
            if !still_current(my_generation, generation) {
                break;
            }
            let for_root: Vec<Entry> = known
                .values()
                .filter(|entry| &entry.root == root)
                .cloned()
                .collect();
            if let Err(error) = write_cache(cache_dir, root, &for_root) {
                let _ = send(EventKind::CacheWriteFailed {
                    root: root.clone(),
                    message: error.to_string(),
                });
            }
        }
    }

    metrics.save_ms = phase.elapsed().as_secs_f64() * 1000.0;
    metrics.completed = still_current(my_generation, generation);
    metrics.first_ms = first.get().map(|n| n + metrics.queue_ms);
    metrics.partial = !failed_dirs.is_empty() || !missing_roots.is_empty() || truncated;
    let _ = send(EventKind::Complete {
        failed: failed_dirs,
        missing_roots,
        truncated,
    });
}

// --- Cache: a small versioned std-only format, one file per root. ---------

const CACHE_MAGIC: &str = "RFN-EDIT-WSINDEX 1";
const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// FNV-1a — small enough to write by hand and stable forever, unlike
/// `DefaultHasher`. Not for anything security-sensitive.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Where `root`'s cache lives under `cache_dir`. The filename is a hash of
/// the path text as given, never the path itself.
fn cache_file_for(cache_dir: &Path, root: &Path) -> PathBuf {
    let id = fnv1a64(root.to_string_lossy().as_bytes());
    cache_dir.join(format!("index-{id:016x}.rfnwsidx"))
}

fn read_bounded(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    String::from_utf8(buffer).ok()
}

fn load_cache(cache_dir: &Path, root: &Path) -> Option<Vec<Entry>> {
    if cache_dir.file_name().is_some_and(|n| n == "shared-v2") {
        return load_shared_cache(cache_dir, root);
    }
    let raw = read_bounded(&cache_file_for(cache_dir, root))?;
    let (stored_root, entries) = decode_cache(&raw)?;
    (stored_root == root).then_some(entries)
}

/// Shared roots (including parent/child roots) and legacy caches are provisional.
/// Rebase the view to the requesting root; never expose another Workspace.
fn load_shared_cache(cache_dir: &Path, root: &Path) -> Option<Vec<Entry>> {
    let mut directories = vec![cache_dir.to_path_buf()];
    if let Some(parent) = cache_dir.parent() {
        if let Ok(read) = fs::read_dir(parent) {
            directories.extend(
                read.flatten()
                    .filter(|e| {
                        e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink())
                            && e.file_name().to_string_lossy().parse::<u64>().is_ok()
                    })
                    .take(32)
                    .map(|e| e.path()),
            );
        }
    }
    let mut candidates = Vec::new();
    for dir in directories {
        if let Ok(read) = fs::read_dir(dir) {
            for item in read.flatten().take(256) {
                if !item.file_type().is_ok_and(|t| t.is_file())
                    || !is_own_cache_file_name(&item.file_name().to_string_lossy())
                {
                    continue;
                }
                if let Ok(meta) = item.metadata() {
                    candidates.push((meta.modified().ok(), meta.len(), item.path()));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let mut budget = 64 * 1024 * 1024u64;
    let mut entries = HashMap::new();
    for (_, size, path) in candidates {
        if size > budget {
            continue;
        }
        budget -= size;
        let Some(raw) = read_bounded(&path) else {
            continue;
        };
        let Some((_, loaded)) = decode_cache(&raw) else {
            continue;
        };
        for mut e in loaded {
            if !is_indexable(&e.canonical) || entries.len() >= MAX_ENTRIES {
                continue;
            }
            let Ok(relative) = e.canonical.strip_prefix(root) else {
                continue;
            };
            if relative
                .components()
                .any(|p| matches!(p.as_os_str().to_str(), Some(".git" | "target")))
            {
                continue;
            }
            e.relative = relative.to_path_buf();
            e.root = root.to_path_buf();
            entries.entry(e.canonical.clone()).or_insert(e);
        }
    }
    Some(entries.into_values().collect())
}

fn write_cache(cache_dir: &Path, root: &Path, entries: &[Entry]) -> io::Result<()> {
    let encoded = encode_cache(root, entries);
    if encoded.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace index cache exceeds the maximum encoded size",
        ));
    }
    fs::create_dir_all(cache_dir)?;
    // Serialize writers from different application instances too. The lock file
    // is persistent; Windows releases its exclusive handle on process exit.
    let mut lock = fs::OpenOptions::new();
    lock.write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        lock.share_mode(0);
    }
    let _lease = if cache_dir.file_name().is_some_and(|n| n == "shared-v2") {
        Some(lock.open(cache_file_for(cache_dir, root).with_extension("lock"))?)
    } else {
        None
    };
    file_io::write_atomically(&cache_file_for(cache_dir, root), encoded.as_bytes())?;
    Ok(())
}

fn split_line(raw: &str, at: usize) -> Option<(&str, usize)> {
    let rest = raw.get(at..)?;
    let (line, _) = rest.split_once('\n')?;
    Some((line, at + line.len() + 1))
}

fn encode_cache(root: &Path, entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str(CACHE_MAGIC);
    out.push('\n');
    let root_text = root.to_string_lossy();
    out.push_str(&format!("root: {}\n", root_text.len()));
    out.push_str(&root_text);
    out.push('\n');
    out.push_str(&format!("count: {}\n", entries.len()));
    for entry in entries {
        let relative_text = entry.relative.to_string_lossy();
        let (has_modified, secs, nanos) = match entry.fingerprint.modified {
            Some(time) => {
                let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
                (1u8, since_epoch.as_secs(), since_epoch.subsec_nanos())
            }
            None => (0u8, 0, 0),
        };
        out.push_str(&format!(
            "entry: {} {} {} {} {} {} {}\n",
            relative_text.len(),
            has_modified,
            secs,
            nanos,
            entry.fingerprint.length,
            entry.headings.len(),
            u8::from(entry.headings_complete),
        ));
        out.push_str(&relative_text);
        out.push('\n');
        for heading in &entry.headings {
            out.push_str(&format!(
                "heading: {} {} {}\n",
                heading.level,
                heading.at,
                heading.text.len()
            ));
            out.push_str(&heading.text);
            out.push('\n');
        }
    }
    out
}

fn decode_cache(raw: &str) -> Option<(PathBuf, Vec<Entry>)> {
    if raw.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    let (magic, at) = split_line(raw, 0)?;
    if magic != CACHE_MAGIC {
        return None;
    }

    let (root_line, next_at) = split_line(raw, at)?;
    let (_, root_len_text) = root_line.split_once(": ")?;
    let root_len: usize = root_len_text.parse().ok()?;
    let root_end = next_at.checked_add(root_len)?;
    let root_text = raw.get(next_at..root_end)?;
    if raw.get(root_end..root_end + 1)? != "\n" {
        return None;
    }
    let root = PathBuf::from(root_text);
    let mut at = root_end + 1;

    let (count_line, next_at) = split_line(raw, at)?;
    let (_, count_text) = count_line.split_once(": ")?;
    let count: usize = count_text.parse().ok()?;
    if count > MAX_ENTRIES {
        return None;
    }
    at = next_at;

    let mut entries = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let (entry_line, next_at) = split_line(raw, at)?;
        let (_, fields) = entry_line.split_once(": ")?;
        let mut parts = fields.split(' ');
        let relative_len: usize = parts.next()?.parse().ok()?;
        let has_modified: u8 = parts.next()?.parse().ok()?;
        let secs: u64 = parts.next()?.parse().ok()?;
        let nanos: u32 = parts.next()?.parse().ok()?;
        let length: u64 = parts.next()?.parse().ok()?;
        let heading_count: usize = parts.next()?.parse().ok()?;
        let complete: u8 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }

        let relative_end = next_at.checked_add(relative_len)?;
        let relative_text = raw.get(next_at..relative_end)?;
        if raw.get(relative_end..relative_end + 1)? != "\n" {
            return None;
        }
        at = relative_end + 1;

        let modified = (has_modified == 1).then(|| UNIX_EPOCH + Duration::new(secs, nanos));

        let mut headings = Vec::with_capacity(heading_count.min(256));
        for _ in 0..heading_count {
            let (heading_line, next_at) = split_line(raw, at)?;
            let (_, heading_fields) = heading_line.split_once(": ")?;
            let mut heading_parts = heading_fields.split(' ');
            let level: u8 = heading_parts.next()?.parse().ok()?;
            let heading_at: usize = heading_parts.next()?.parse().ok()?;
            let text_len: usize = heading_parts.next()?.parse().ok()?;
            if heading_parts.next().is_some() {
                return None;
            }
            let text_end = next_at.checked_add(text_len)?;
            let text = raw.get(next_at..text_end)?;
            if raw.get(text_end..text_end + 1)? != "\n" {
                return None;
            }
            at = text_end + 1;
            headings.push(document::Heading {
                level,
                text: text.to_owned(),
                at: heading_at,
            });
        }

        let relative = PathBuf::from(relative_text);
        let canonical = root.join(&relative);
        entries.push(Entry {
            root: root.clone(),
            relative,
            canonical,
            fingerprint: file_io::FileStamp { modified, length },
            headings,
            headings_complete: complete == 1,
        });
    }

    if at != raw.len() {
        return None;
    }
    Some((root, entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "release benchmark; explicitly invoked"]
    fn scan_benchmark() {
        let root = scratch_directory("benchmark-input");
        for folder in 0..32 {
            let dir = root.join(format!("d{folder}"));
            fs::create_dir_all(&dir).unwrap();
            for file in 0..64 {
                fs::write(
                    dir.join(format!("n{file}.md")),
                    "# Heading\ntext\n".repeat(100),
                )
                .unwrap();
                fs::write(dir.join(format!("n{file}.txt")), "unindexed").unwrap();
                fs::write(dir.join(format!("n{file}.png")), "image metadata only").unwrap();
            }
        }
        for workers in [1, 2, 4] {
            for sample in 0..3 {
                let cache = scratch_directory(&format!("benchmark-cache-{workers}-{sample}"));
                fs::write(root.join("d0/n0.md"), "# Heading\ntext\n".repeat(100)).unwrap();
                let mut indexer = Indexer::new();
                for round in 0..3 {
                    if round == 2 {
                        fs::write(root.join("d0/n0.md"), "# Changed\n").unwrap();
                    }
                    let start = Instant::now();
                    let generation = indexer.restart(
                        vec![root.clone()],
                        cache.clone(),
                        ScanOptions {
                            workers,
                            ..ScanOptions::default()
                        },
                    );
                    let (snapshot, _) = run_to_completion(&indexer, generation);
                    println!(
                        "scan_benchmark workers={workers} sample={sample} round={round} elapsed_ms={:.3} entries={}",
                        start.elapsed().as_secs_f64() * 1000.0,
                        snapshot.entries().len()
                    );
                }
            }
        }
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-wsindex-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    fn small_options() -> ScanOptions {
        ScanOptions {
            force_reparse: false,
            workers: 2,
            max_entries: 50,
            batch_size: 4,
            max_heading_bytes: 2 * 1024 * 1024,
            excludes: vec![".git".to_owned(), "target".to_owned()],
        }
    }

    #[test]
    fn only_notes_and_images_are_indexed_and_report_metrics() {
        let root = scratch_directory("types");
        for name in ["note.MD", "photo.PNG", "plain.txt", "app.log", "code.rs"] {
            fs::write(root.join(name), "# Heading\n").unwrap();
        }
        let mut index = Indexer::new();
        let generation = index.restart_memory(vec![root], small_options());
        let (snapshot, _) = run_to_completion(&index, generation);
        assert_eq!(snapshot.entries().len(), 2);
        assert!(
            snapshot
                .entries()
                .iter()
                .filter(|e| is_image(&e.canonical))
                .all(|e| e.headings.is_empty())
        );
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(m) = index.take_metrics().pop() {
                assert!(m.completed);
                assert_eq!(m.files, 5);
                assert_eq!(m.parsed, 1);
                assert!(!m.log().contains("note"));
                break;
            }
            assert!(Instant::now() < until);
            thread::yield_now();
        }
    }

    #[test]
    fn shared_cache_rebases_parent_and_filters_legacy_types() {
        let root = scratch_directory("shared-parent");
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("note.md"), "# Heading\n").unwrap();
        fs::write(root.join("other.md"), "# Other\n").unwrap();
        let cache = scratch_directory("shared-store").join("shared-v2");
        let mut index = Indexer::new();
        let g = index.restart(vec![root], cache.clone(), small_options());
        run_to_completion(&index, g);
        let child = child.canonicalize().unwrap();
        let entries = load_cache(&cache, &child).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].root, child);
        assert_eq!(entries[0].relative, PathBuf::from("note.md"));
    }

    fn kind_name(kind: &EventKind) -> String {
        match kind {
            EventKind::InventoryComplete { .. } => "inventory".to_owned(),
            EventKind::Cached(_) => "cached".to_owned(),
            EventKind::Updated(_) => "updated".to_owned(),
            EventKind::Removed(_) => "removed".to_owned(),
            EventKind::CacheWriteFailed { .. } => "cache-write-failed".to_owned(),
            EventKind::Complete { .. } => "complete".to_owned(),
            EventKind::Error(message) => format!("error:{message}"),
        }
    }

    /// Drains `indexer` to a `Complete` for `generation`, returning every
    /// event seen (of any generation) alongside a [`Snapshot`] folded from
    /// them. Bounded by a wall-clock deadline only as a hang guard for a
    /// regression — not used to assert timing.
    fn run_to_completion(indexer: &Indexer, generation: u64) -> (Snapshot, Vec<Event>) {
        let mut snapshot = Snapshot::new();
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = indexer
                .recv_timeout(Duration::from_secs(1))
                .expect("scan sends Complete before closing");
            let done =
                event.generation == generation && matches!(event.kind, EventKind::Complete { .. });
            events.push(event.clone());
            snapshot.apply(event);
            if done {
                break;
            }
            assert!(Instant::now() < deadline, "scan never completed");
        }
        (snapshot, events)
    }

    fn stub_entry(name: &str) -> Entry {
        Entry {
            root: PathBuf::from("/r"),
            relative: PathBuf::from(name),
            canonical: PathBuf::from(format!("/r/{name}")),
            fingerprint: file_io::FileStamp {
                modified: None,
                length: 0,
            },
            headings: Vec::new(),
            headings_complete: true,
        }
    }

    /// 仕様の実装依頼 #3: `Snapshot`'s internal index must follow
    /// `swap_remove`, so the entry moved into a freed slot still updates and
    /// removes correctly rather than the map going stale.
    #[test]
    fn snapshot_upsert_and_remove_stay_correct_after_a_swap_remove() {
        let mut snapshot = Snapshot::new();
        let entries = vec![
            stub_entry("f0.md"),
            stub_entry("f1.md"),
            stub_entry("f2.md"),
        ];
        snapshot.apply(Event {
            generation: 1,
            kind: EventKind::Updated(entries.clone()),
        });
        assert_eq!(snapshot.entries().len(), 3);

        // Removes index 0; internally this moves f2.md (the last entry) into
        // slot 0 via `swap_remove`.
        snapshot.apply(Event {
            generation: 1,
            kind: EventKind::Removed(vec![entries[0].canonical.clone()]),
        });
        assert_eq!(snapshot.entries().len(), 2);
        let mut names: Vec<&str> = snapshot
            .entries()
            .iter()
            .map(|e| e.relative.to_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["f1.md", "f2.md"]);

        // Updating the moved entry must replace it in place, not duplicate.
        let mut moved = entries[2].clone();
        moved.headings_complete = false;
        snapshot.apply(Event {
            generation: 1,
            kind: EventKind::Updated(vec![moved.clone()]),
        });
        assert_eq!(snapshot.entries().len(), 2);
        let found = snapshot
            .entries()
            .iter()
            .find(|e| e.canonical == moved.canonical)
            .expect("still present");
        assert!(!found.headings_complete);

        // Removing everything must leave an empty, still-consistent snapshot.
        snapshot.apply(Event {
            generation: 1,
            kind: EventKind::Removed(vec![
                entries[1].canonical.clone(),
                entries[2].canonical.clone(),
            ]),
        });
        assert!(snapshot.entries().is_empty());
    }

    /// 仕様の実装依頼 #4.
    #[test]
    fn restart_memory_scans_without_a_cache_directory() {
        let root = scratch_directory("memory-root");
        fs::write(root.join("a.md"), "# 一\n").expect("writes");

        let mut indexer = Indexer::new();
        let generation = indexer.restart_memory(vec![root], small_options());
        let (snapshot, _) = run_to_completion(&indexer, generation);

        assert_eq!(snapshot.entries().len(), 1);
    }

    /// 仕様の実装依頼 #4: one `poll` call never costs more than
    /// `EVENT_CHANNEL_CAPACITY` events, even if the worker keeps refilling.
    #[test]
    fn poll_never_returns_more_than_the_channel_capacity() {
        let root = scratch_directory("poll-bound-root");
        for index in 0..40 {
            fs::write(root.join(format!("f{index}.md")), "# h\n").expect("writes");
        }
        let cache_dir = scratch_directory("poll-bound-cache");
        let mut options = small_options();
        options.batch_size = 1;

        let mut indexer = Indexer::new();
        indexer.restart(vec![root], cache_dir, options);
        std::thread::sleep(Duration::from_millis(100));

        assert!(indexer.poll().len() <= EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn cached_entries_arrive_before_the_scan_finds_the_real_file() {
        let root = scratch_directory("cached-first-root");
        let cache_dir = scratch_directory("cached-first-cache");
        fs::write(root.join("本文.md"), "# 見出し\n").expect("writes");
        let canonical_root = root.canonicalize().expect("canonicalizes");

        let stale = Entry {
            root: canonical_root.clone(),
            relative: PathBuf::from("すでに無い.md"),
            canonical: canonical_root.join("すでに無い.md"),
            fingerprint: file_io::FileStamp {
                modified: None,
                length: 3,
            },
            headings: vec![document::Heading {
                level: 1,
                text: "むかしの見出し".to_owned(),
                at: 0,
            }],
            headings_complete: true,
        };
        write_cache(&cache_dir, &canonical_root, std::slice::from_ref(&stale))
            .expect("writes cache");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root.clone()], cache_dir, small_options());
        let (snapshot, events) = run_to_completion(&indexer, generation);

        assert_eq!(
            events.first().map(|e| e.kind.clone()),
            Some(EventKind::Cached(vec![stale.clone()]))
        );
        assert!(matches!(
            events.last().map(|e| &e.kind),
            Some(EventKind::Complete { .. })
        ));

        let names: Vec<&str> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.relative.to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["本文.md"]);
        assert_eq!(snapshot.entries()[0].headings[0].text, "見出し");
    }

    /// 仕様の実装依頼 #1: a file-only stub for a later file must not wait for
    /// the *entire* walk to finish — proven here by batch size 1 against
    /// several files, so the very first `Updated` event contains fewer
    /// entries than the walk will eventually find in total.
    #[test]
    fn file_stubs_are_published_incrementally_during_the_walk_not_after_it() {
        let root = scratch_directory("incremental-root");
        for index in 0..12 {
            fs::write(root.join(format!("f{index}.md")), "# 見出し\n").expect("writes");
        }
        let cache_dir = scratch_directory("incremental-cache");
        let mut options = small_options();
        options.batch_size = 1;

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, options);

        let first = indexer
            .recv_timeout(Duration::from_secs(5))
            .expect("an early event arrives");
        let EventKind::Updated(first_batch) = first.kind else {
            panic!("expected the first event to be a file-only stub batch");
        };
        assert_eq!(first_batch.len(), 1);
        assert!(
            !first_batch[0].headings_complete,
            "published before headings were parsed"
        );
        assert!(first_batch[0].headings.is_empty());

        let (snapshot, _) = run_to_completion(&indexer, generation);
        assert_eq!(snapshot.entries().len(), 12);
        assert!(snapshot.entries().iter().all(|e| e.headings_complete));
    }

    #[test]
    fn a_previously_incomplete_file_is_retried_even_unchanged() {
        let root = scratch_directory("retry-root");
        let cache_dir = scratch_directory("retry-cache");
        fs::write(root.join("大きい.md"), "# 見出し\n").expect("writes");
        let canonical_root = root.canonicalize().expect("canonicalizes");
        let metadata = fs::metadata(root.join("大きい.md")).unwrap();

        // Seed a cache claiming this exact file (same fingerprint the real
        // scan will see) but with `headings_complete: false` — as if an
        // earlier scan gave up on it.
        let stub = Entry {
            root: canonical_root.clone(),
            relative: PathBuf::from("大きい.md"),
            canonical: canonical_root.join("大きい.md"),
            fingerprint: fingerprint_of(&metadata),
            headings: Vec::new(),
            headings_complete: false,
        };
        write_cache(&cache_dir, &canonical_root, std::slice::from_ref(&stub))
            .expect("writes cache");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, small_options());
        let (snapshot, _) = run_to_completion(&indexer, generation);

        assert_eq!(snapshot.entries().len(), 1);
        assert!(snapshot.entries()[0].headings_complete);
        assert_eq!(snapshot.entries()[0].headings[0].text, "見出し");
    }

    #[test]
    fn canceling_supersedes_without_writing_a_cache() {
        let root = scratch_directory("cancel-root");
        for index in 0..40 {
            fs::write(root.join(format!("f{index}.md")), "# 見出し\n").expect("writes");
        }
        let cache_dir = scratch_directory("cancel-cache");

        let mut indexer = Indexer::new();
        indexer.restart(vec![root], cache_dir.clone(), small_options());
        indexer.cancel();

        // Drain whatever arrived before the cancel was noticed; none of it
        // may be a `Complete` for the canceled generation, and no cache file
        // may exist afterward — the deterministic proof is the absence, not
        // a race against how far the scan got.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if indexer.recv_timeout(Duration::from_millis(100)).is_none() {
                break;
            }
        }
        let entries = fs::read_dir(&cache_dir).map(|it| it.count()).unwrap_or(0);
        assert_eq!(entries, 0);
    }

    /// 仕様の実装依頼 #3: fills the bounded event channel and never drains
    /// it, so the superseded scan can only escape by noticing the
    /// generation change inside its own cooperative retry — not by a
    /// consumer making room.
    #[test]
    fn a_scan_stuck_on_a_full_channel_still_yields_to_a_newer_generation() {
        let root = scratch_directory("full-channel-root");
        for index in 0..50 {
            fs::write(root.join(format!("f{index}.md")), "# 見出し\n").expect("writes");
        }
        let cache_dir = scratch_directory("full-channel-cache");
        let mut options = small_options();
        options.batch_size = 1;

        let mut indexer = Indexer::new();
        let first = indexer.restart(vec![root], cache_dir.clone(), options);
        // No draining here on purpose.
        let second = indexer.restart(Vec::new(), cache_dir, small_options());
        assert!(second > first);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_second_complete = false;
        while Instant::now() < deadline && !saw_second_complete {
            if let Some(event) = indexer.recv_timeout(Duration::from_millis(200)) {
                if event.generation == second && matches!(event.kind, EventKind::Complete { .. }) {
                    saw_second_complete = true;
                }
            }
        }
        assert!(
            saw_second_complete,
            "a superseded scan must not block the worker on a full channel"
        );
    }

    /// 仕様の実装依頼 #1: a clear whose error report cannot fit in a full,
    /// undrained event queue must not block the worker — the clear loop
    /// keeps going and, crucially, so does the next queued `Scan`.
    #[test]
    fn a_failed_clear_does_not_block_the_worker_even_with_a_full_event_queue() {
        let cache_dir = scratch_directory("clear-fullqueue-cache");
        // A directory where an own cache file is expected: `fs::remove_file`
        // reliably fails against it, with an error other than `NotFound`.
        fs::create_dir_all(cache_file_for(&cache_dir, Path::new("/bad/root/for/clear")))
            .expect("blocks the cache file path with a directory");

        let root = scratch_directory("clear-fullqueue-root");
        for index in 0..50 {
            fs::write(root.join(format!("f{index}.md")), "# h\n").expect("writes");
        }
        let scan_cache = scratch_directory("clear-fullqueue-scan-cache");

        let mut indexer = Indexer::new();
        let mut options = small_options();
        options.batch_size = 1;
        indexer.restart(vec![root], scan_cache.clone(), options);
        // No draining: the event channel fills up and stays full.
        indexer.clear_workspace_cache(cache_dir);
        let last = indexer.restart(Vec::new(), scan_cache, small_options());

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut completed = false;
        while Instant::now() < deadline && !completed {
            if let Some(event) = indexer.recv_timeout(Duration::from_millis(200)) {
                if event.generation == last && matches!(event.kind, EventKind::Complete { .. }) {
                    completed = true;
                }
            }
        }
        assert!(
            completed,
            "a full queue plus a failed clear must not deadlock the worker"
        );
    }

    #[test]
    fn clear_runs_after_any_scan_already_queued_ahead_of_it() {
        let root = scratch_directory("clear-order-root");
        fs::write(root.join("a.md"), "# 一\n").expect("writes");
        let canonical_root = root.canonicalize().expect("canonicalizes");
        let cache_dir = scratch_directory("clear-order-cache");

        let mut indexer = Indexer::new();

        // Queue a scan that writes the cache, the clear, and a second
        // (empty-root) scan back to back with no draining in between. The
        // clear does not supersede the first scan, so that scan writes its
        // cache; the second scan's own `Complete` is an observable FIFO
        // barrier: since the worker processes commands strictly in order,
        // seeing it means the clear ahead of it has already run — after the
        // first scan's write, never racing it.
        indexer.restart(vec![root], cache_dir.clone(), small_options());
        indexer.clear_workspace_cache(cache_dir.clone());
        let last = indexer.restart(Vec::new(), cache_dir.clone(), small_options());
        run_to_completion(&indexer, last);

        assert!(load_cache(&cache_dir, &canonical_root).is_none());
    }

    #[test]
    fn reaching_the_entry_cap_reports_truncated_and_does_not_prune() {
        let root = scratch_directory("cap-root");
        for index in 0..10 {
            fs::write(root.join(format!("f{index}.md")), "# 見出し\n").expect("writes");
        }
        let cache_dir = scratch_directory("cap-cache");
        let mut options = small_options();
        options.max_entries = 3;

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, options);
        let (snapshot, events) = run_to_completion(&indexer, generation);

        let complete = events
            .iter()
            .find(|e| matches!(e.kind, EventKind::Complete { .. }))
            .expect("completes");
        let EventKind::Complete { truncated, .. } = &complete.kind else {
            unreachable!()
        };
        assert!(*truncated);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Removed(_)))
        );
        // Whatever was found before the cap is still reported, never
        // silently discarded.
        assert!(!snapshot.entries().is_empty());
    }

    /// 仕様の実装依頼 #1: cached entries from *two* roots must share the same
    /// cap as newly walked files, not bypass it — a reduced cap on a later
    /// scan must truncate against cache alone if the cache alone already
    /// reaches it. Nothing on disk was deleted, so pruning finds nothing
    /// stale regardless.
    #[test]
    fn cache_plus_new_entries_share_a_single_global_cap() {
        let root_a = scratch_directory("cap-union-a");
        let root_b = scratch_directory("cap-union-b");
        for index in 0..3 {
            fs::write(root_a.join(format!("a{index}.md")), "# h\n").expect("writes");
        }
        for index in 0..3 {
            fs::write(root_b.join(format!("b{index}.md")), "# h\n").expect("writes");
        }
        let cache_dir = scratch_directory("cap-union-cache");

        let mut indexer = Indexer::new();
        let mut generous = small_options();
        generous.max_entries = 10;
        let g1 = indexer.restart(
            vec![root_a.clone(), root_b.clone()],
            cache_dir.clone(),
            generous,
        );
        let (snapshot, _) = run_to_completion(&indexer, g1);
        assert_eq!(snapshot.entries().len(), 6);

        // A new file on top of a cache that alone already reaches a reduced cap.
        fs::write(root_a.join("new.md"), "# 新\n").expect("writes");
        let mut reduced = small_options();
        reduced.max_entries = 4;
        let g2 = indexer.restart(vec![root_a, root_b], cache_dir, reduced);
        let (snapshot2, events2) = run_to_completion(&indexer, g2);

        let complete = events2
            .iter()
            .find(|e| e.generation == g2 && matches!(e.kind, EventKind::Complete { .. }))
            .expect("completes");
        let EventKind::Complete { truncated, .. } = &complete.kind else {
            unreachable!()
        };
        assert!(*truncated);
        assert!(snapshot2.entries().len() <= 4);
        assert!(
            !events2
                .iter()
                .any(|e| e.generation == g2 && matches!(e.kind, EventKind::Removed(_)))
        );
    }

    /// 仕様の実装依頼 #2: reaching the cap must not stop the walk from
    /// checking already-known entries — a population sitting at exactly the
    /// cap still needs its changes and removals observed on the next scan.
    #[test]
    fn a_cached_population_at_exactly_the_cap_still_observes_updates_and_removals() {
        let root = scratch_directory("cap-exact-root");
        for index in 0..4 {
            fs::write(root.join(format!("f{index}.md")), "# h\n").expect("writes");
        }
        let cache_dir = scratch_directory("cap-exact-cache");
        let mut options = small_options();
        options.max_entries = 4;

        let mut indexer = Indexer::new();
        let g1 = indexer.restart(vec![root.clone()], cache_dir.clone(), options.clone());
        let (snapshot, events) = run_to_completion(&indexer, g1);
        assert_eq!(snapshot.entries().len(), 4);
        let complete = events
            .iter()
            .find(|e| matches!(e.kind, EventKind::Complete { .. }))
            .expect("completes");
        let EventKind::Complete { truncated, .. } = &complete.kind else {
            unreachable!()
        };
        assert!(!truncated);

        fs::write(root.join("f0.md"), "# 変わった\n").expect("edits");
        fs::remove_file(root.join("f1.md")).expect("removes");

        let g2 = indexer.restart(vec![root], cache_dir, options);
        let (snapshot2, events2) = run_to_completion(&indexer, g2);
        let kinds: Vec<String> = events2
            .iter()
            .filter(|e| e.generation == g2)
            .map(|e| kind_name(&e.kind))
            .collect();
        assert!(kinds.contains(&"updated".to_owned()));
        assert!(kinds.contains(&"removed".to_owned()));
        assert_eq!(snapshot2.entries().len(), 3);
    }

    /// 仕様の実装依頼 #3: `length` is only a metadata snapshot — a file that
    /// grew since must still be bounded by `max_bytes` itself, not read in
    /// full because a stale `length` looked fine.
    #[test]
    fn parse_headings_is_bounded_even_when_length_is_stale() {
        let root = scratch_directory("grew-root");
        let path = root.join("育った.md");
        fs::write(&path, "あ".repeat(10_000)).expect("writes");

        let (headings, complete) = parse_headings(&path, 10, 50);

        assert!(!complete);
        assert!(headings.is_empty());
    }

    /// 仕様の実装依頼 #4: dropping the `Indexer` must cancel an in-flight
    /// scan promptly, not let it run to completion and write a cache nobody
    /// asked for anymore.
    #[test]
    fn dropping_the_indexer_cancels_any_in_flight_scan() {
        let root = scratch_directory("drop-cancel-root");
        for index in 0..30 {
            fs::write(root.join(format!("f{index}.md")), "# h\n").expect("writes");
        }
        let cache_dir = scratch_directory("drop-cancel-cache");
        let mut options = small_options();
        options.batch_size = 1;

        {
            let mut indexer = Indexer::new();
            indexer.restart(vec![root], cache_dir.clone(), options);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && fs::read_dir(&cache_dir).map(|it| it.count()).unwrap_or(0) > 0
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            fs::read_dir(&cache_dir).map(|it| it.count()).unwrap_or(0),
            0
        );
    }

    /// 仕様の実装依頼 #5: a failed cache deletion is reported, not swallowed —
    /// tagged with [`MAINTENANCE_GENERATION`], never a scan's own number.
    #[test]
    fn clear_reports_a_deletion_failure_as_an_error_event() {
        let cache_dir = scratch_directory("clear-error-cache");
        // A directory where a file is expected: `fs::remove_file` reliably
        // fails against it, with an error other than `NotFound`.
        fs::create_dir_all(cache_file_for(
            &cache_dir,
            Path::new("/some/root/for/clear/error"),
        ))
        .expect("creates");

        let mut indexer = Indexer::new();
        indexer.clear_workspace_cache(cache_dir);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            if let Some(event) = indexer.recv_timeout(Duration::from_millis(200)) {
                if event.generation == MAINTENANCE_GENERATION
                    && matches!(event.kind, EventKind::Error(_))
                {
                    found = true;
                }
            }
        }
        assert!(found, "a failed cache deletion must be reported");
    }

    #[test]
    fn a_missing_root_is_reported_without_blocking_the_others() {
        let good = scratch_directory("missing-good-root");
        fs::write(good.join("a.md"), "# 一\n").expect("writes");
        let missing = std::env::temp_dir().join("rfnedit-wsindex-does-not-exist-at-all");
        let _ = fs::remove_dir_all(&missing);
        let cache_dir = scratch_directory("missing-root-cache");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![good, missing.clone()], cache_dir, small_options());
        let (snapshot, events) = run_to_completion(&indexer, generation);

        assert_eq!(snapshot.entries().len(), 1);
        let complete = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::Complete { missing_roots, .. } => Some(missing_roots.clone()),
                _ => None,
            })
            .expect("completes");
        assert_eq!(complete, vec![missing]);
    }

    #[test]
    fn a_cache_write_failure_is_reported_not_swallowed() {
        let root = scratch_directory("write-fail-root");
        fs::write(root.join("a.md"), "# 一\n").expect("writes");
        // A cache "directory" that is actually a file: `fs::create_dir_all`
        // inside `write_cache` will fail, and that failure must surface.
        let cache_dir = scratch_directory("write-fail-cache-parent").join("blocked");
        fs::write(&cache_dir, "not a directory").expect("writes a blocking file");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, small_options());
        let (_, events) = run_to_completion(&indexer, generation);

        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::CacheWriteFailed { .. }))
        );
    }

    #[test]
    fn duplicate_and_nested_roots_are_scanned_once() {
        let root = scratch_directory("overlap-root");
        let child = root.join("子");
        fs::create_dir_all(&child).expect("creates");
        fs::write(child.join("下.md"), "# 下\n").expect("writes");
        let canonical_root = root.canonicalize().expect("canonicalizes");
        let canonical_child = child.canonicalize().expect("canonicalizes");

        let kept = dedupe_roots(&[
            canonical_root.clone(),
            canonical_root.clone(),
            canonical_child,
        ]);
        assert_eq!(kept, vec![canonical_root]);
    }

    #[test]
    fn a_malformed_cache_is_ignored_rather_than_trusted() {
        assert!(decode_cache("not an index cache").is_none());
        assert!(decode_cache(CACHE_MAGIC).is_none());
    }

    #[test]
    fn an_oversize_cache_is_refused_without_reading_it_whole() {
        let cache_dir = scratch_directory("oversize-cache");
        let root = PathBuf::from("/anywhere");
        let path = cache_file_for(&cache_dir, &root);
        fs::create_dir_all(&cache_dir).expect("creates");
        let oversize = vec![b'a'; MAX_CACHE_BYTES as usize + 1];
        fs::write(&path, &oversize).expect("writes");

        assert!(load_cache(&cache_dir, &root).is_none());
    }

    #[test]
    fn the_cache_filename_never_spells_out_the_root_path() {
        let cache_dir = PathBuf::from("/cache");
        let root = PathBuf::from("/home/誰か/秘密の原稿フォルダ");
        let path = cache_file_for(&cache_dir, &root);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains("秘密"));
        assert!(!name.contains("誰か"));
    }

    #[test]
    fn is_own_cache_file_name_only_matches_this_builds_own_shape() {
        assert!(is_own_cache_file_name("index-0123456789abcdef.rfnwsidx"));
        assert!(!is_own_cache_file_name("index-0123456789ABCDEF.rfnwsidx"));
        assert!(!is_own_cache_file_name("index-short.rfnwsidx"));
        assert!(!is_own_cache_file_name("something-else.txt"));
        assert!(!is_own_cache_file_name(
            "index-0123456789abcdef.rfnwsidx.bak"
        ));
    }

    /// A root detached or relocated from a Workspace after its cache was
    /// written must not be left behind just because a caller only knows the
    /// roots currently registered — `clear_workspace_cache` removes every
    /// own-named file under the directory regardless.
    #[test]
    fn clear_workspace_cache_removes_every_own_cache_file_and_the_empty_directory() {
        let cache_dir = scratch_directory("clear-workspace-cache-dir");
        let still_registered = PathBuf::from("/still/registered");
        let detached = PathBuf::from("/detached/long/ago");
        write_cache(&cache_dir, &still_registered, &[]).expect("writes");
        write_cache(&cache_dir, &detached, &[]).expect("writes");
        assert_eq!(fs::read_dir(&cache_dir).unwrap().count(), 2);

        let mut indexer = Indexer::new();
        indexer.clear_workspace_cache(cache_dir.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        while cache_dir.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !cache_dir.exists(),
            "left empty afterward, the directory itself is removed too"
        );
    }

    /// A file this build did not itself write is left alone, and — since the
    /// directory is then not empty — so is the directory.
    #[test]
    fn clear_workspace_cache_never_removes_a_file_it_does_not_own() {
        let cache_dir = scratch_directory("clear-workspace-cache-foreign");
        let ours = PathBuf::from("/ours");
        write_cache(&cache_dir, &ours, &[]).expect("writes");
        fs::write(cache_dir.join("not-ours.txt"), b"keep me").expect("writes");

        let mut indexer = Indexer::new();
        indexer.clear_workspace_cache(cache_dir.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::metadata(cache_file_for(&cache_dir, &ours)).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fs::metadata(cache_file_for(&cache_dir, &ours)).is_err());
        assert!(cache_dir.join("not-ours.txt").exists());
        assert!(cache_dir.exists());
    }

    /// 仕様の実装依頼 (Codex preliminary review, engine corrections): clearing
    /// one Workspace's cache must never supersede a scan running for a
    /// *different* (e.g. the currently active) scope — proven here by a scan
    /// that still reaches `Complete` under its own original generation after
    /// a concurrent `clear_workspace_cache` for an unrelated directory.
    #[test]
    fn clear_workspace_cache_does_not_supersede_an_unrelated_in_flight_scan() {
        let root = scratch_directory("clear-workspace-no-supersede-root");
        fs::write(root.join("a.md"), "# 一\n").expect("writes");
        let cache_dir = scratch_directory("clear-workspace-no-supersede-cache");
        let other_cache_dir = scratch_directory("clear-workspace-no-supersede-other");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, small_options());
        indexer.clear_workspace_cache(other_cache_dir);

        let (snapshot, events) = run_to_completion(&indexer, generation);
        assert_eq!(snapshot.entries().len(), 1);
        assert!(events
            .iter()
            .any(|e| e.generation == generation && matches!(e.kind, EventKind::Complete { .. })));
    }

    #[test]
    fn a_file_over_the_byte_bound_is_listed_without_its_headings() {
        let root = scratch_directory("big-file-root");
        let cache_dir = scratch_directory("big-file-cache");
        let big = "あ".repeat(40) + "\n# 見出し\n";
        fs::write(root.join("大きい.md"), &big).expect("writes");

        let mut options = small_options();
        options.max_heading_bytes = 32;

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, options);
        let (snapshot, _) = run_to_completion(&indexer, generation);

        assert_eq!(snapshot.entries().len(), 1);
        let entry = &snapshot.entries()[0];
        assert!(!entry.headings_complete);
        assert!(entry.headings.is_empty());
    }

    #[test]
    fn same_named_files_in_two_roots_stay_distinct() {
        let root_a = scratch_directory("same-name-a");
        let root_b = scratch_directory("same-name-b");
        fs::write(root_a.join("メモ.md"), "# あ\n").expect("writes");
        fs::write(root_b.join("メモ.md"), "# い\n").expect("writes");
        let cache_dir = scratch_directory("same-name-cache");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(
            vec![root_a.clone(), root_b.clone()],
            cache_dir,
            small_options(),
        );
        let (snapshot, _) = run_to_completion(&indexer, generation);

        assert_eq!(snapshot.entries().len(), 2);
        let mut roots: Vec<&Path> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.root.as_path())
            .collect();
        roots.sort();
        let mut expected = [
            root_a.canonicalize().unwrap(),
            root_b.canonicalize().unwrap(),
        ];
        expected.sort();
        assert_eq!(
            roots,
            expected.iter().map(PathBuf::as_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_heading_inside_a_fenced_block_is_not_indexed() {
        let root = scratch_directory("fence-root");
        let cache_dir = scratch_directory("fence-cache");
        fs::write(
            root.join("柵.md"),
            "```\n# これは見出しではない\n```\n# これは見出し\n",
        )
        .expect("writes");

        let mut indexer = Indexer::new();
        let generation = indexer.restart(vec![root], cache_dir, small_options());
        let (snapshot, _) = run_to_completion(&indexer, generation);

        let headings = &snapshot.entries()[0].headings;
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "これは見出し");
    }

    #[test]
    fn add_change_and_remove_are_each_reported_across_two_scans() {
        let root = scratch_directory("delta-root");
        let cache_dir = scratch_directory("delta-cache");
        fs::write(root.join("残る.md"), "# 変わらない\n").expect("writes");
        fs::write(root.join("消える.md"), "# 消える\n").expect("writes");

        let mut indexer = Indexer::new();
        let g1 = indexer.restart(vec![root.clone()], cache_dir.clone(), small_options());
        let (snapshot, _) = run_to_completion(&indexer, g1);
        assert_eq!(snapshot.entries().len(), 2);

        fs::remove_file(root.join("消える.md")).expect("removes");
        fs::write(root.join("残る.md"), "# 変わった\n").expect("changes");
        fs::write(root.join("増える.md"), "# 増える\n").expect("adds");

        let g2 = indexer.restart(vec![root], cache_dir, small_options());
        let (snapshot, events) = run_to_completion(&indexer, g2);
        let kinds: Vec<String> = events
            .iter()
            .filter(|e| e.generation == g2)
            .map(|e| kind_name(&e.kind))
            .collect();
        assert!(kinds.contains(&"removed".to_owned()));
        assert!(kinds.contains(&"updated".to_owned()));

        let mut by_name: Vec<(String, String)> = snapshot
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.relative.to_str().unwrap().to_owned(),
                    entry
                        .headings
                        .first()
                        .map(|h| h.text.clone())
                        .unwrap_or_default(),
                )
            })
            .collect();
        by_name.sort();
        assert_eq!(
            by_name,
            vec![
                ("増える.md".to_owned(), "増える".to_owned()),
                ("残る.md".to_owned(), "変わった".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_read_is_not_pruned() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_directory("perm-root");
        let locked = root.join("鍵付き");
        fs::create_dir_all(&locked).expect("creates");
        fs::write(locked.join("中身.md"), "# 中\n").expect("writes");
        let cache_dir = scratch_directory("perm-cache");

        let mut indexer = Indexer::new();
        let g1 = indexer.restart(vec![root.clone()], cache_dir.clone(), small_options());
        let (snapshot, _) = run_to_completion(&indexer, g1);
        assert_eq!(snapshot.entries().len(), 1);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("locks");
        let g2 = indexer.restart(vec![root.clone()], cache_dir, small_options());
        let (snapshot, events) = run_to_completion(&indexer, g2);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlocks");

        let kinds: Vec<String> = events
            .iter()
            .filter(|e| e.generation == g2)
            .map(|e| kind_name(&e.kind))
            .collect();
        assert!(!kinds.contains(&"removed".to_owned()));
        assert_eq!(snapshot.entries().len(), 1);
    }

    #[test]
    fn a_cache_survives_the_disk_round_trip_with_japanese_and_tabs() {
        let root = PathBuf::from("/根");
        let entries = vec![Entry {
            root: root.clone(),
            relative: PathBuf::from("章\t一.md"),
            canonical: root.join("章\t一.md"),
            fingerprint: file_io::FileStamp {
                modified: Some(UNIX_EPOCH + Duration::new(1_700_000_000, 123)),
                length: 42,
            },
            headings: vec![document::Heading {
                level: 2,
                text: "改行\nを含む見出し".to_owned(),
                at: 7,
            }],
            headings_complete: true,
        }];

        let encoded = encode_cache(&root, &entries);
        let (decoded_root, decoded_entries) = decode_cache(&encoded).expect("decodes");
        assert_eq!(decoded_root, root);
        assert_eq!(decoded_entries, entries);
    }
}
