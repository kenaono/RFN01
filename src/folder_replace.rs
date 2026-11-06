//! RFN01-70: 左Paneの検索から、フォルダのファイルをまとめて置換する。
//!
//! **探し方は本文の検索と同じ**（`find::Search`）、**書き換えるファイルは検索と同じ並び**
//! （`searcher::searched_files`）。数えてから訊き、答えが来たら読み直して置き換える。
//!
//! * 開いている文書は、保存していてもいなくても**TABの本文を置き換える**。未保存のまま
//!   残り、1回のUndoで戻る（書き手の判断 2026-10-24）。
//! * 開いていないファイルは、**バックアップを取ってから**書き換える。文字コードと改行は
//!   元のまま。バックアップが取れなければ書き換えない。
use crate::*;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    needle: String,
    replacement: String,
    rules: find::Rules,
    roots: Vec<PathBuf>,
    exclusions: String,
    files: VecDeque<Candidate>,
}

#[derive(Clone, Debug)]
struct Candidate {
    path: PathBuf,
    identity: PathBuf,
}

impl Candidate {
    fn new(path: PathBuf) -> Self {
        let identity = file_io::document_path_identity(&path);
        Self { path, identity }
    }

    fn matches(&self, file: &DocumentFile) -> bool {
        file.path() == Some(self.path.as_path())
            || file.path_identity() == Some(self.identity.as_path())
    }
}

enum Operation {
    Scan(Scan),
    Apply(Apply),
}

struct Scan {
    stop: Arc<AtomicBool>,
    checked: Arc<AtomicUsize>,
    result: Receiver<Option<Tally>>,
}

struct Tally {
    plan: Plan,
    matches: usize,
    open: usize,
}

struct Apply {
    plan: Plan,
    stop: Arc<AtomicBool>,
    tasks: Sender<Task>,
    result: Receiver<Outcome>,
    pending: Option<PendingFile>,
    total: usize,
    tried: usize,
    files: usize,
    places: usize,
    changed: usize,
    refused: usize,
}

enum Task {
    Open(String),
    Closed(Candidate),
}

enum PendingFile {
    Open {
        candidate: Candidate,
        document: Rc<OpenDocument>,
        revision: u64,
    },
    Closed,
}

struct Outcome {
    result: Result<Replacement, Refused>,
    diagnostic: Option<String>,
}

enum Replacement {
    Open(Option<OpenEdit>),
    Closed(usize),
}

struct OpenEdit {
    text: String,
    count: usize,
    change: Change,
    removed: String,
    inserted: String,
}

thread_local! {
    static OPERATION: RefCell<Option<Operation>> = const { RefCell::new(None) };
    static RESERVED: RefCell<Option<Candidate>> = const { RefCell::new(None) };
}

pub(crate) fn closed_write_in_progress() -> bool {
    RESERVED.with(|held| held.borrow().is_some())
}

/// Open and Save As must not adopt the disk file while the worker is replacing it.
pub(crate) fn path_available(window: &AppWindow, path: &Path) -> bool {
    let blocked = RESERVED.with(|held| {
        held.borrow().as_ref().is_some_and(|file| {
            file_tree::path_key(path) == file_tree::path_key(&file.path)
                || file_io::document_path_identity(path) == file.identity
        })
    });
    if blocked {
        window.tell(
            pick(
                "一括置換で更新中のファイルです。完了後にもう一度お試しください",
                "Replace All is updating this file. Try again when it finishes",
            )
            .into(),
        );
    }
    !blocked
}

fn show_progress(window: &AppWindow, text: String) {
    window.set_folder_replace_progress(text.into());
    window.set_folder_replacing(true);
}

fn stop_progress(window: &AppWindow) {
    window.set_folder_replacing(false);
    window.set_folder_replace_progress("".into());
}

fn cancel_active(window: &AppWindow) -> bool {
    let cancelled = OPERATION.with(|state| {
        let state = state.borrow();
        let stop = match state.as_ref() {
            Some(Operation::Scan(scan)) => &scan.stop,
            Some(Operation::Apply(apply)) => &apply.stop,
            None => return false,
        };
        stop.store(true, Ordering::Release);
        true
    });
    if cancelled {
        show_progress(window, pick("中止しています…", "Cancelling…").to_owned());
    }
    cancelled
}

pub(crate) fn close_allowed(window: &AppWindow) -> bool {
    if !closed_write_in_progress() {
        return true;
    }
    cancel_active(window);
    window.tell(
        pick(
            "一括置換を中止しています。現在のファイルの処理後に閉じられます",
            "Cancelling Replace All. Close again after the current file finishes",
        )
        .into(),
    );
    false
}

/// The button cancels the active operation or requests a new tally.
pub(crate) fn requested(window: &AppWindow, live: &Live) {
    if cancel_active(window) {
        return;
    }

    let needle = window.get_folder_needle().to_string();
    if needle.is_empty() {
        return;
    }
    if !tags::split_query(&needle).0.is_empty() {
        window.set_folder_status(
            pick(
                "tag: の検索では置換できません",
                "Cannot replace in a tag: search",
            )
            .into(),
        );
        return;
    }
    let rules = folder_rules(window);
    let search = match find::Search::new(&needle, rules) {
        Ok(search) => search,
        Err(trouble) => {
            window.set_folder_status(trouble.into());
            return;
        }
    };
    let roots = folder_search_roots(live, &needle);
    if roots.is_empty() {
        window.set_folder_status(no_work_folder().into());
        return;
    }

    let plan = Plan {
        needle,
        replacement: window.get_folder_replacement().to_string(),
        rules,
        roots,
        exclusions: window.get_search_exclusions().to_string(),
        files: VecDeque::new(),
    };
    let open = open_documents(live)
        .into_iter()
        .filter_map(|document| {
            let file = document.file.borrow();
            let identity = file.path_identity()?;
            Some((identity.to_path_buf(), document.text.borrow().clone()))
        })
        .collect::<HashMap<_, _>>();
    let stop = Arc::new(AtomicBool::new(false));
    let checked = Arc::new(AtomicUsize::new(0));
    let (send, result) = mpsc::channel();
    let worker_stop = stop.clone();
    let worker_checked = checked.clone();
    let spawn = std::thread::Builder::new()
        .name("folder-replace-scan".into())
        .spawn(move || {
            let _ = send.send(tally(plan, search, &open, &worker_stop, &worker_checked));
        });
    if spawn.is_err() {
        window.set_folder_status(
            pick(
                "置換の集計を開始できませんでした",
                "Could not start the replacement scan",
            )
            .into(),
        );
        return;
    }
    OPERATION.with(|state| {
        *state.borrow_mut() = Some(Operation::Scan(Scan {
            stop,
            checked,
            result,
        }))
    });
    show_progress(
        window,
        pick(
            "置換する箇所を数えています…",
            "Counting replacement matches…",
        )
        .to_owned(),
    );
}

fn tally(
    mut plan: Plan,
    search: find::Search,
    open: &HashMap<PathBuf, String>,
    stop: &AtomicBool,
    checked: &AtomicUsize,
) -> Option<Tally> {
    let cancelled = || stop.load(Ordering::Acquire);
    let walked =
        searcher::searched_files(&plan.roots, &plan.exclusions, SEARCHED_FILES, &mut || {
            cancelled()
        })?;
    let (mut matches, mut open_count) = (0, 0);
    for path in walked {
        if cancelled() {
            return None;
        }
        let candidate = Candidate::new(path);
        let held = open.get(&candidate.identity);
        let count = match held {
            Some(text) => search.tally(text, None).0,
            None => match file_io::read(&candidate.path, MAX_DOCUMENT_CHARACTERS) {
                Ok(loaded) => search.tally(&loaded.text, None).0,
                Err(_) => 0,
            },
        };
        checked.fetch_add(1, Ordering::Relaxed);
        if count != 0 {
            matches += count;
            open_count += usize::from(held.is_some());
            plan.files.push_back(candidate);
        }
    }
    (!cancelled()).then_some(Tally {
        plan,
        matches,
        open: open_count,
    })
}

fn show_confirmation(window: &AppWindow, live: &Live, tally: Tally) {
    let Tally {
        plan,
        matches: places,
        open,
    } = tally;
    let Plan {
        needle,
        replacement,
        rules,
        roots,
        exclusions,
        files,
    } = &plan;
    if live.pending.borrow().is_some()
        || *roots != folder_search_roots(live, needle)
        || *rules != folder_rules(window)
        || exclusions.as_str() != window.get_search_exclusions().as_str()
        || needle.as_str() != window.get_folder_needle().as_str()
        || replacement.as_str() != window.get_folder_replacement().as_str()
    {
        window.set_folder_status(
            pick(
                "検索条件が変わったため集計を取り消しました",
                "Replacement scan discarded because the search changed",
            )
            .into(),
        );
        return;
    }
    if files.is_empty() {
        window.set_folder_status(
            say!("「{needle}」は見つかりません", "\"{needle}\" not found").into(),
        );
        return;
    }
    let count = files.len();
    let text = say!(
        "「{needle}」を「{replacement}」に置換します。\n\n\
         {count}ファイル・{places}箇所（開いているTAB {open}ファイルを含む）。\n\
         開いているTABは本文を置き換え、保存せずに残します。\n\
         開いていないファイルは、バックアップを取ってから書き換えます。",
        "Replace \"{needle}\" with \"{replacement}\".\n\n\
         {count} files, {places} places ({open} files open in tabs).\n\
         Open tabs are changed and left unsaved.\n\
         Files that are not open are backed up, then rewritten."
    );
    ask_question(
        window,
        live,
        Question::FolderReplace(plan),
        text,
        &[pick("置換", "Replace"), cancel()],
        0,
    );
}

/// Use the confirmed file set, but read each document as it comes up for replacement.
pub(crate) fn confirmed(window: &AppWindow, live: &Live, plan: Plan) {
    if OPERATION.with(|state| state.borrow().is_some())
        || plan.roots != folder_search_roots(live, &plan.needle)
    {
        return;
    }
    let Ok(search) = find::Search::new(&plan.needle, plan.rules) else {
        return;
    };
    let store = saving::backup_store(window);
    let keep = window.get_backup_keep().clamp(1, backup::MAX_KEEP as i32) as usize;
    let replacement = plan.replacement.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (tasks, work) = mpsc::channel();
    let (send, result) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("folder-replace-apply".into())
        .spawn(move || {
            while let Ok(task) = work.recv() {
                let outcome = match task {
                    Task::Open(old) => Outcome {
                        result: prepare_open(&old, &search, &replacement).map(Replacement::Open),
                        diagnostic: None,
                    },
                    Task::Closed(file) => replace_closed(
                        file,
                        &search,
                        &replacement,
                        store.as_ref(),
                        keep,
                        &worker_stop,
                    ),
                };
                if send.send(outcome).is_err() {
                    break;
                }
            }
        });
    if spawn.is_err() {
        window.set_folder_status(
            pick(
                "一括置換を開始できませんでした",
                "Could not start Replace All",
            )
            .into(),
        );
        return;
    }
    let total = plan.files.len();
    OPERATION.with(|state| {
        *state.borrow_mut() = Some(Operation::Apply(Apply {
            plan,
            stop,
            tasks,
            result,
            pending: None,
            total,
            tried: 0,
            files: 0,
            places: 0,
            changed: 0,
            refused: 0,
        }))
    });
    collect(window, live);
}

pub(crate) fn collect(window: &AppWindow, live: &Live) {
    let operation = OPERATION.with(|state| state.borrow_mut().take());
    let next = match operation {
        Some(Operation::Scan(scan)) => match scan.result.try_recv() {
            Err(TryRecvError::Empty) => {
                if !scan.stop.load(Ordering::Acquire) {
                    let checked = scan.checked.load(Ordering::Relaxed);
                    show_progress(
                        window,
                        say!(
                            "集計中… {checked}ファイル確認済み",
                            "Counting… {checked} files checked"
                        ),
                    );
                }
                Some(Operation::Scan(scan))
            }
            Ok(Some(tally)) if !scan.stop.load(Ordering::Acquire) => {
                stop_progress(window);
                show_confirmation(window, live, tally);
                None
            }
            outcome => {
                stop_progress(window);
                let message = if matches!(outcome, Err(TryRecvError::Disconnected)) {
                    pick("置換の集計が失敗しました", "Replacement scan failed")
                } else {
                    pick("置換の集計を中止しました", "Replacement scan cancelled")
                };
                window.set_folder_status(message.into());
                None
            }
        },
        Some(Operation::Apply(mut apply)) => {
            if apply.advance(window, live) {
                Some(Operation::Apply(apply))
            } else {
                apply.finish(window, live);
                None
            }
        }
        None => return,
    };
    OPERATION.with(|state| *state.borrow_mut() = next);
}

impl Apply {
    fn advance(&mut self, window: &AppWindow, live: &Live) -> bool {
        if self.plan.roots != folder_search_roots(live, &self.plan.needle) {
            self.stop.store(true, Ordering::Release);
        }
        if self.pending.is_some() {
            let outcome = match self.result.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Outcome {
                    result: Err(Refused::Other),
                    diagnostic: None,
                },
            };
            let pending = self.pending.take().unwrap();
            RESERVED.with(|held| held.borrow_mut().take());
            if let Some(diagnostic) = outcome.diagnostic {
                live.cache.borrow_mut().log_diag("file", &diagnostic);
            }
            let result = match (pending, outcome.result) {
                (PendingFile::Closed, Ok(Replacement::Closed(count))) => Ok(count),
                (
                    PendingFile::Open {
                        candidate,
                        document,
                        revision,
                    },
                    Ok(Replacement::Open(edit)),
                ) => {
                    if self.stop.load(Ordering::Acquire) {
                        Ok(0)
                    } else if document.text.revision() != revision
                        || !candidate.matches(&document.file.borrow())
                        || !open_documents(live)
                            .iter()
                            .any(|held| Rc::ptr_eq(held, &document))
                    {
                        Err(Refused::Changed)
                    } else if document.read_only() || has_reading_view(window, live, &document) {
                        Err(Refused::Other)
                    } else {
                        Ok(edit.map_or(0, |edit| install_open(window, live, &document, edit)))
                    }
                }
                (_, Err(error)) => Err(error),
                _ => Err(Refused::Other),
            };
            self.record(result);
        }
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        if live.pending.borrow().is_some() {
            return true;
        }
        let Some(candidate) = self.plan.files.pop_front() else {
            return false;
        };
        let done = self.tried;
        let total = self.total;
        show_progress(
            window,
            say!(
                "置換中… {done}/{total} ファイル処理済み",
                "Replacing… {done}/{total} files processed"
            ),
        );
        let (task, pending) = if let Some(document) = open_documents(live)
            .into_iter()
            .find(|document| candidate.matches(&document.file.borrow()))
        {
            let revision = document.text.revision();
            let text = document.text.borrow().clone();
            (
                Task::Open(text),
                PendingFile::Open {
                    candidate,
                    document,
                    revision,
                },
            )
        } else if terminal_panels::entries(live)
            .iter()
            .any(|entry| candidate.matches(&entry.borrow().document.file.borrow()))
            || terminal_panels::logging_to(live, &candidate.path)
        {
            self.record(Err(Refused::Other));
            return true;
        } else {
            RESERVED.with(|held| *held.borrow_mut() = Some(candidate.clone()));
            (Task::Closed(candidate), PendingFile::Closed)
        };
        if self.tasks.send(task).is_err() {
            RESERVED.with(|held| held.borrow_mut().take());
            self.record(Err(Refused::Other));
            self.stop.store(true, Ordering::Release);
            return false;
        }
        self.pending = Some(pending);
        true
    }

    fn record(&mut self, result: Result<usize, Refused>) {
        self.tried += 1;
        match result {
            Ok(0) => {}
            Ok(count) => {
                self.files += 1;
                self.places += count;
            }
            Err(Refused::Changed) => self.changed += 1,
            Err(Refused::Other) => self.refused += 1,
        }
    }

    fn finish(self, window: &AppWindow, live: &Live) {
        stop_progress(window);
        let Self {
            tried,
            total,
            files,
            places,
            refused,
            changed,
            stop,
            ..
        } = self;
        let mut told = say!(
            "{files}ファイル・{places}箇所を置換しました",
            "Replaced {places} places in {files} files"
        );
        if stop.load(Ordering::Acquire) {
            told = say!(
                "中止しました（{tried}/{total} ファイル処理済み）。{told}",
                "Cancelled after {tried}/{total} files. {told}"
            );
        }
        if refused != 0 {
            told.push_str(&say!(
                "（{refused}ファイルは書き換えられませんでした）",
                " ({refused} files could not be changed)"
            ));
        }
        if changed != 0 {
            told.push_str(&say!(
                "。処理中に変更された {changed} ファイルは置換しませんでした",
                ". Skipped {changed} files changed during replacement"
            ));
        }
        live.cache.borrow_mut().log_diag(
            "search",
            &format!("replace files={files} places={places} refused={refused} changed={changed}"),
        );
        publish_tabs(window, live);
        crate::backup_ui::refresh_pane(window, live);
        window.tell(told.clone().into());
        live.folder.borrow_mut().replaced_note = Some(told);
        search_work_folder(window, live);
    }
}

fn prepare_open(
    old: &str,
    search: &find::Search,
    replacement: &str,
) -> Result<Option<OpenEdit>, Refused> {
    let (text, count) = search
        .replace_all_with_limit(old, replacement, MAX_DOCUMENT_CHARACTERS)
        .ok_or(Refused::Other)?;
    if count == 0 {
        return Ok(None);
    }
    let (at, removed, inserted) = find::changed_span(old, &text);
    let removed_text = old[at..at + removed].to_owned();
    let inserted_text = text[at..at + inserted].to_owned();
    Ok(Some(OpenEdit {
        text,
        count,
        change: Change {
            at,
            removed,
            inserted,
        },
        removed: removed_text,
        inserted: inserted_text,
    }))
}

fn install_open(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    edit: OpenEdit,
) -> usize {
    let OpenEdit {
        text,
        count,
        change,
        removed,
        inserted,
    } = edit;
    document.history.borrow_mut().separate_next = true;
    document.record(change.at, removed, inserted);
    *document.text.borrow_mut() = text;
    document.history.borrow_mut().separate_next = true;
    if let Some(id) = PaneId::all(window)
        .into_iter()
        .find(|id| Rc::ptr_eq(&live.states.document(*id), document))
    {
        let text = document.text.borrow();
        carry_state_across(&live.states.of(id), change, &text);
        id.draw_edit(
            window,
            &live.states,
            &live.cache,
            document,
            &text,
            None,
            change,
        );
    }
    saving::write_work_copy_of(window, live, document);
    count
}

#[derive(Debug, PartialEq, Eq)]
enum Refused {
    Changed,
    Other,
}

fn replace_closed(
    file: Candidate,
    search: &find::Search,
    replacement: &str,
    store: Option<&backup::Store>,
    keep: usize,
    stop: &AtomicBool,
) -> Outcome {
    let path = &file.path;
    let mut diagnostic = None;
    let result = (|| {
        if file_io::document_path_identity(path) != file.identity {
            return Err(Refused::Changed);
        }
        let source = ClosedSource::read(path)?;
        let (next, count) = search
            .replace_all_with_limit(&source.text, replacement, MAX_DOCUMENT_CHARACTERS)
            .ok_or(Refused::Other)?;
        if count == 0 || stop.load(Ordering::Acquire) {
            return Ok(0);
        }
        let store = store.ok_or(Refused::Other)?;
        match source.install(path, &next, store, keep) {
            Ok(retained) => {
                diagnostic =
                    retained.map(|path| format!("replace retained old file at {}", path.display()));
                Ok(count)
            }
            Err(error) => {
                diagnostic = Some(format!(
                    "replace failed path={} error={error}",
                    path.display()
                ));
                Err(refusal(error))
            }
        }
    })();
    Outcome {
        result: result.map(Replacement::Closed),
        diagnostic,
    }
}
struct ClosedSource {
    file: std::fs::File,
    version: disk_entry::Version,
    text: String,
    form: file_io::TextForm,
    bytes: Vec<u8>,
}

impl ClosedSource {
    fn read(path: &Path) -> Result<Self, Refused> {
        // Deny writes to this file while reading and preparing the result.
        // Other editors can rename a new entry onto the path, so install
        // also checks the identity of the entry that would be replaced.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_DELETE).0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(refusal)?;
        let metadata = file.metadata().map_err(refusal)?;
        if !metadata.is_file()
            || metadata.permissions().readonly()
            || file_tree::is_reparse_point(&metadata)
        {
            return Err(Refused::Other);
        }
        let version = disk_entry::Version::of(&file).map_err(refusal)?;
        let (text, form, bytes) = file_io::read_for_replacement(&mut file, MAX_DOCUMENT_CHARACTERS)
            .map_err(|_| Refused::Other)?;
        Ok(Self {
            file,
            version,
            text,
            form,
            bytes,
        })
    }

    fn install(
        self,
        path: &Path,
        next: &str,
        store: &backup::Store,
        keep: usize,
    ) -> io::Result<Option<PathBuf>> {
        if disk_entry::Version::read(path)? != self.version {
            return Err(disk_entry::changed());
        }
        let stage = disk_entry::Stage::beside(path)?;
        let incoming = stage.incoming();
        let outcome = (|| {
            file_io::save(&incoming, next, self.form)
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            store.take_exact(path, &self.bytes, keep, timestamp::now())?;
            stage.replace(&incoming, path, self.version)
        })();
        // Release the read handle before removing the displaced entry.
        drop(self.file);
        match outcome {
            Ok(previous) => Ok(std::fs::remove_file(&previous).is_err().then_some(previous)),
            Err(error) => {
                let _ = std::fs::remove_file(incoming);
                Err(error)
            }
        }
    }
}

fn refusal(error: io::Error) -> Refused {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::NotFound
    ) || matches!(error.raw_os_error(), Some(32 | 33))
    {
        Refused::Changed
    } else {
        Refused::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_file_replaced_after_the_read_keeps_its_external_contents() {
        let root = saving::scratch_directory("replace-version");
        let path = root.join("memo.md");
        let original = root.join("previous.md");
        fs::write(&path, "置換前\r\n").unwrap();
        let source = ClosedSource::read(&path).unwrap();
        fs::rename(&path, &original).unwrap();
        fs::write(&path, "外部からの更新").unwrap();
        let store = backup::Store::new(root.join("backups"), backup::DEFAULT_NAME_FORMAT);

        let error = source.install(&path, "置換後\n", &store, 10).unwrap_err();

        assert_eq!(refusal(error), Refused::Changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "外部からの更新");
        assert_eq!(fs::read_to_string(original).unwrap(), "置換前\r\n");
    }
}
