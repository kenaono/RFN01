//! RFN01-67（書き手と決めた 2026-10-03）: 左ペインの Git Changes。
//!
//! Gitを呼ぶのは[`crate::git`]で、ここは画面とつなぐだけにする：
//!
//! - **対象は使用中の Workspace の登録フォルダ**（Backups と同じ数え方）。初めは今のTABの
//!   ファイルを含むフォルダ。Gitの管理下になければ「Create Git Repository」だけを出す。
//! - **Gitは裏のスレッドで呼ぶ。**読み直し（status）も操作も。操作は1度に1つで、そのあいだ
//!   釦は押せない。Fetch・Pull・Push・Sync は Cancel で止められる。
//! - **ファイルを替える操作の前に、未保存のTABを確かめる**（Pull・Sync・切替・Stashの適用は
//!   保存／破棄／キャンセル、Commit・Stash All は保存／キャンセル）。終わったら、そのリポジトリ
//!   で開いているTABのうち中身が変わったものを読み直す。
//! - 面が出ているあいだは5秒ごとに読み直す——書き手はTerminalでもgitを打つ。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::git::{self, Cancel, Change, GitError, Kind, Stash, Status};
use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::{AppWindow, GitRow, Live, Opening, Question, StatusBar, say};

/// 面の番号（`left-tab`）。
const TAB: i32 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Staged,
    Changes,
    Stashes,
}

/// 一覧の行が指すもの（行の順）。
enum Line {
    Header(Section),
    File { staged: bool, change: Change },
    Stash(Stash),
    Note,
}

/// 選んでいるフォルダの、読み直した結果。
struct Snapshot {
    folder: PathBuf,
    /// `None`はGitの管理下にない。
    root: Option<PathBuf>,
    status: Status,
    stashes: Vec<Stash>,
    branches: Vec<String>,
}

/// 裏で走っている操作。
struct Job {
    done: Receiver<Result<String, GitError>>,
    cancel: Cancel,
    root: PathBuf,
    /// 終わったら読み直すTABの範囲。
    reload: Reload,
    /// 書き手が「破棄して続ける」を選んだ文書。終わったら必ず読み直す。
    discarded: Vec<Rc<OpenDocument>>,
    /// 通ったときにすること（Commitならメッセージ欄を空にする）。
    after: After,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    Nothing,
    ClearMessage,
}

enum Reload {
    None,
    Repository,
    Files(Vec<PathBuf>),
}

#[derive(Default)]
struct Pane {
    /// 使用中の Workspace の登録フォルダ（並びのまま）。
    folders: Vec<PathBuf>,
    chosen: Option<PathBuf>,
    snapshot: Option<Snapshot>,
    /// 読み直しが断られたときの文。
    error: Option<String>,
    lines: Vec<Line>,
    closed: Vec<Section>,
    job: Option<Job>,
    reading: Option<(u64, Receiver<Result<Snapshot, GitError>>)>,
    /// 読み直しの番号。操作を始めるたびに進め、古い読み直しの結果を捨てる。
    generation: u64,
    /// Amend をチェックする前のメッセージ（外したら戻す）。
    kept_message: Option<String>,
}

thread_local! {
    static PANE: RefCell<Pane> = RefCell::new(Pane::default());
    /// 裏の結果を受け取る時計。待つものがあるあいだだけ動く。
    static POLL: Timer = Timer::default();
    /// 面が出ているあいだの5秒ごとの読み直し。
    static TICK: Timer = Timer::default();
}

/// 操作の前に確かめることと、確かめたあとですること。`Question`が答えまで持ち歩く。
#[derive(Clone, Debug)]
pub struct Pending {
    root: PathBuf,
    action: Action,
    /// 確認の問い（Undo・Drop・Push済みのAmend）に、もう答えた。
    confirmed: bool,
}

#[derive(Clone, Debug)]
enum Action {
    Switch(String),
    Pull,
    Sync,
    StashApply {
        name: String,
        pop: bool,
    },
    StashDrop(String),
    Commit {
        message: String,
        all: bool,
        amend: bool,
    },
    StashAll(String),
    Undo {
        changes: Vec<Change>,
        staged: bool,
    },
}

impl Action {
    /// 未保存のTABをどう確かめるか。
    fn guard(&self) -> Guard {
        match self {
            Action::Switch(_) | Action::Pull | Action::Sync | Action::StashApply { .. } => {
                Guard::SaveOrDiscard
            }
            Action::Commit { .. } | Action::StashAll(_) => Guard::SaveOnly,
            // 書き手は捨てると答えている（確認の問いで言う）。
            Action::Undo { .. } => Guard::Discard,
            Action::StashDrop(_) => Guard::None,
        }
    }
}

enum Guard {
    None,
    SaveOrDiscard,
    SaveOnly,
    Discard,
}

fn visible(window: &AppWindow) -> bool {
    window.get_tree_open() && window.get_left_tab() == TAB
}

/// 道を比べられる形に（大文字小文字・区切りを揃え、`\\?\`を外す）。
fn key(path: &Path) -> String {
    let plain = crate::backup::plain(path);
    let mut text = plain.to_string_lossy().replace('/', "\\").to_lowercase();
    while text.ends_with('\\') {
        text.pop();
    }
    text
}

fn inside(path: &Path, folder: &Path) -> bool {
    let (path, folder) = (key(path), key(folder));
    path == folder || path.starts_with(&(folder + "\\"))
}

/// 使用中の Workspace の登録フォルダ。
fn workspace_folders(live: &Live) -> Vec<PathBuf> {
    let folder = live.folder.borrow();
    let Some(runtime) = folder.workspace.as_ref() else {
        return Vec::new();
    };
    let runtime = runtime.borrow();
    let registry = runtime.registry();
    let ids = runtime
        .active_workspace()
        .and_then(|id| registry.workspace(id))
        .map(|w| w.folders.clone())
        .unwrap_or_default();
    ids.iter()
        .filter_map(|id| registry.folder(*id))
        .map(|f| crate::backup::plain(&f.path))
        .collect()
}

fn root_of(pane: &Pane) -> Option<PathBuf> {
    let snapshot = pane.snapshot.as_ref()?;
    if Some(&snapshot.folder) != pane.chosen.as_ref() {
        return None;
    }
    snapshot.root.clone()
}

/// 面を開いたとき・Workspace が替わったとき（`publish_left`）。
pub fn publish(window: &AppWindow, live: &Live) {
    let folders = workspace_folders(live);
    let active = live
        .active(window)
        .file
        .borrow()
        .path()
        .map(Path::to_path_buf);
    PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        let kept = pane
            .chosen
            .as_ref()
            .is_some_and(|chosen| folders.iter().any(|f| key(f) == key(chosen)));
        if !kept {
            pane.chosen = active
                .as_ref()
                .and_then(|path| folders.iter().find(|f| inside(path, f)))
                .or(folders.first())
                .cloned();
            pane.snapshot = None;
            pane.error = None;
        }
        pane.folders = folders;
    });
    render(window);
    refresh(window, live);
}

/// 保存のあと（`saving::write_document_in`）。面が出ていれば読み直す。
pub fn refresh_soon(window: &AppWindow, live: &Live) {
    if visible(window) {
        refresh(window, live);
    }
}

/// 選んでいるフォルダを裏で読み直す。操作の最中なら、終わったあとの読み直しに任せる。
fn refresh(window: &AppWindow, live: &Live) {
    let start = PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        if pane.job.is_some() || pane.reading.is_some() {
            return None;
        }
        let folder = pane.chosen.clone()?;
        let (sender, receiver) = mpsc::channel();
        pane.reading = Some((pane.generation, receiver));
        Some((folder, sender))
    });
    let Some((folder, sender)) = start else {
        return;
    };
    std::thread::spawn(move || {
        let _ = sender.send(read(folder));
    });
    poll(window, live);
}

fn read(folder: PathBuf) -> Result<Snapshot, GitError> {
    let root = git::repository_root(&folder)?;
    let Some(root) = root else {
        return Ok(Snapshot {
            folder,
            root: None,
            status: Status::default(),
            stashes: Vec::new(),
            branches: Vec::new(),
        });
    };
    Ok(Snapshot {
        folder,
        status: git::status(&root)?,
        stashes: git::stashes(&root)?,
        branches: git::branches(&root)?,
        root: Some(root),
    })
}

/// 裏の結果を受け取る時計を動かす（もう動いていれば何もしない）。
fn poll(window: &AppWindow, live: &Live) {
    POLL.with(|timer| {
        if timer.running() {
            return;
        }
        let weak = window.as_weak();
        let live = live.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            if let Some(window) = weak.upgrade() {
                collect(&window, &live);
            }
        });
    });
}

fn collect(window: &AppWindow, live: &Live) {
    // 読み直しの結果。
    let read = PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        let (generation, receiver) = pane.reading.as_ref()?;
        let generation = *generation;
        match receiver.try_recv() {
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                pane.reading = None;
                None
            }
            Ok(result) => {
                pane.reading = None;
                Some((generation == pane.generation, result))
            }
        }
    });
    if let Some((current, result)) = read {
        if current {
            PANE.with(|pane| {
                let mut pane = pane.borrow_mut();
                match result {
                    Ok(snapshot) if Some(&snapshot.folder) == pane.chosen.as_ref() => {
                        pane.snapshot = Some(snapshot);
                        pane.error = None;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        pane.snapshot = None;
                        pane.error = Some(error.to_string());
                    }
                }
            });
            render(window);
        } else {
            // 操作が始まる前に始めた読み直し。今の姿を読み直す。
            refresh(window, live);
        }
    }
    // 操作の結果。
    let finished = PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        let job = pane.job.as_ref()?;
        match job.done.try_recv() {
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                let job = pane.job.take()?;
                Some((job, Err(GitError::Failed("Git stopped".into()))))
            }
            Ok(result) => {
                let job = pane.job.take()?;
                Some((job, result))
            }
        }
    });
    if let Some((job, result)) = finished {
        finish(window, live, job, result);
    }
    let idle = PANE.with(|pane| {
        let pane = pane.borrow();
        pane.job.is_none() && pane.reading.is_none()
    });
    if idle {
        POLL.with(Timer::stop);
    }
}

fn finish(window: &AppWindow, live: &Live, job: Job, result: Result<String, GitError>) {
    window.set_git_busy(false);
    window.set_git_busy_text(SharedString::new());
    window.set_git_can_cancel(false);
    let outcome = match &result {
        Ok(_) => "ok".to_owned(),
        Err(GitError::Cancelled) => "cancelled".to_owned(),
        Err(GitError::Conflict) => "conflict".to_owned(),
        Err(_) => "failed".to_owned(),
    };
    live.cache
        .borrow_mut()
        .log_diag("spec.git", &format!("done {outcome}"));
    reload_documents(window, live, &job);
    match result {
        Ok(told) => {
            if job.after == After::ClearMessage {
                window.set_git_message(SharedString::new());
                window.set_git_amend(false);
                PANE.with(|pane| pane.borrow_mut().kept_message = None);
            }
            if !told.is_empty() {
                window.tell(told.into());
            }
        }
        Err(GitError::Cancelled) => window.tell(GitError::Cancelled.to_string().into()),
        Err(error) => notice(window, live, error.to_string()),
    }
    refresh(window, live);
}

/// 知らせるだけの問い。Gitの文は長いので、帯ではなくダイアログで見せる。
fn notice(window: &AppWindow, live: &Live, text: String) {
    if live.pending.borrow().is_some() {
        window.tell(text.into());
        return;
    }
    crate::ask_question(
        window,
        live,
        Question::GitNotice,
        text,
        &[pick("閉じる", "Close")],
        -1,
    );
}

/// 操作のあと、開いているTABを読み直す：破棄すると答えたものは必ず、それ以外は
/// **未保存の変更が無く、ファイルが変わったものだけ**。
fn reload_documents(window: &AppWindow, live: &Live, job: &Job) {
    for document in &job.discarded {
        crate::saving::reload_document(window, live, document);
    }
    let in_scope = |path: &Path| match &job.reload {
        Reload::None => false,
        Reload::Repository => inside(path, &job.root),
        Reload::Files(files) => files.iter().any(|file| key(file) == key(path)),
    };
    for document in crate::open_documents(live) {
        if job.discarded.iter().any(|d| Rc::ptr_eq(d, &document)) || document.text.edited() {
            continue;
        }
        let path = document.file.borrow().path().map(Path::to_path_buf);
        let Some(path) = path else {
            continue;
        };
        let changed =
            document.file.borrow().external_change() == crate::buffer::ExternalChange::Modified;
        if changed && in_scope(&path) {
            crate::saving::reload_document(window, live, &document);
        }
    }
}

/// 一覧と釦を、いまの状態から組む。
fn render(window: &AppWindow) {
    PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        let names: Vec<SharedString> = pane
            .folders
            .iter()
            .map(|f| {
                f.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| f.display().to_string())
                    .into()
            })
            .collect();
        let index = pane
            .chosen
            .as_ref()
            .and_then(|chosen| pane.folders.iter().position(|f| key(f) == key(chosen)))
            .unwrap_or(0);
        window.set_git_repos(ModelRc::new(VecModel::from(names)));
        window.set_git_repo_index(index as i32);
        let chosen = pane.chosen.clone();
        window.set_git_repo_tip(
            chosen
                .as_ref()
                .map(|f| f.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        let (mode, note) = match (&chosen, &pane.error, &pane.snapshot) {
            (None, ..) => (
                0,
                say!(
                    "Workspaceを開くと、その登録フォルダのGitを操作できます。",
                    "Open a Workspace to use Git with its folders."
                ),
            ),
            (Some(_), Some(error), _) => (0, error.clone()),
            (Some(folder), None, Some(snapshot)) if &snapshot.folder == folder => {
                match &snapshot.root {
                    None => (
                        1,
                        say!(
                            "{}はGitで管理されていません。",
                            "{} is not a Git repository.",
                            folder.display()
                        ),
                    ),
                    Some(_) => {
                        build_rows(&pane.closed, snapshot, &mut rows, &mut lines);
                        (2, String::new())
                    }
                }
            }
            _ => (0, say!("読み込んでいます…", "Reading the repository…")),
        };
        window.set_git_mode(mode);
        window.set_git_note(note.into());
        let snapshot = pane.snapshot.as_ref().filter(|_| mode == 2);
        let status = snapshot.map(|s| &s.status);
        let branch = match status {
            Some(Status {
                branch: Some(branch),
                ..
            }) => branch.clone(),
            Some(_) => pick("（ブランチの外）", "(detached HEAD)").to_owned(),
            None => String::new(),
        };
        window.set_git_branch(branch.into());
        let branches: Vec<SharedString> = snapshot
            .map(|s| s.branches.iter().map(SharedString::from).collect())
            .unwrap_or_default();
        window.set_git_branches(ModelRc::new(VecModel::from(branches)));
        window.set_git_on_branch(status.is_some_and(|s| s.branch.is_some()));
        window.set_git_has_upstream(status.is_some_and(|s| s.upstream.is_some()));
        window.set_git_has_commit(status.is_some_and(|s| !s.unborn));
        window.set_git_incoming(status.map_or(0, |s| s.behind as i32));
        window.set_git_outgoing(status.map_or(0, |s| s.ahead as i32));
        window.set_git_staged_count(status.map_or(0, |s| s.staged.len() as i32));
        window.set_git_change_count(status.map_or(0, |s| s.changes.len() as i32));
        window.set_git_rows(ModelRc::new(VecModel::from(rows)));
        pane.lines = lines;
    });
}

fn build_rows(
    closed: &[Section],
    snapshot: &Snapshot,
    rows: &mut Vec<GitRow>,
    lines: &mut Vec<Line>,
) {
    let status = &snapshot.status;
    let header =
        |section: Section, title: String, rows: &mut Vec<GitRow>, lines: &mut Vec<Line>| {
            rows.push(GitRow {
                kind: match section {
                    Section::Staged => 0,
                    Section::Changes => 1,
                    Section::Stashes => 2,
                },
                label: title.into(),
                open: !closed.contains(&section),
                ..Default::default()
            });
            lines.push(Line::Header(section));
            !closed.contains(&section)
        };
    let files =
        |staged: bool, changes: &[Change], rows: &mut Vec<GitRow>, lines: &mut Vec<Line>| {
            for change in changes {
                let (folder, name) = match change.path.rsplit_once('/') {
                    Some((folder, name)) => (folder.to_owned(), name.to_owned()),
                    None => (String::new(), change.path.clone()),
                };
                let tip = match &change.from {
                    Some(from) => format!("{from} → {}", change.path),
                    None => change.path.clone(),
                };
                rows.push(GitRow {
                    kind: if staged { 3 } else { 4 },
                    label: name.into(),
                    folder: folder.into(),
                    letter: change.kind.letter().into(),
                    tip: tip.into(),
                    open: false,
                });
                lines.push(Line::File {
                    staged,
                    change: change.clone(),
                });
            }
        };
    if !status.staged.is_empty() {
        let title = format!(
            "{} ({})",
            pick("Stage済みの変更", "Staged Changes"),
            status.staged.len()
        );
        if header(Section::Staged, title, rows, lines) {
            files(true, &status.staged, rows, lines);
        }
    }
    let title = format!("{} ({})", pick("変更", "Changes"), status.changes.len());
    if header(Section::Changes, title, rows, lines) {
        if status.changes.is_empty() {
            rows.push(GitRow {
                kind: 6,
                label: pick("変更はありません", "No changes").into(),
                ..Default::default()
            });
            lines.push(Line::Note);
        } else {
            files(false, &status.changes, rows, lines);
        }
    }
    if !snapshot.stashes.is_empty() {
        let title = format!("Stashes ({})", snapshot.stashes.len());
        if header(Section::Stashes, title, rows, lines) {
            for stash in &snapshot.stashes {
                rows.push(GitRow {
                    kind: 5,
                    label: stash.message.clone().into(),
                    folder: stash.name.clone().into(),
                    tip: format!("{}: {}", stash.name, stash.message).into(),
                    ..Default::default()
                });
                lines.push(Line::Stash(stash.clone()));
            }
        }
    }
}

/// 裏で操作を始める。`work`はリポジトリの根と止める合図を受け取り、通れば知らせる文を返す。
#[allow(clippy::too_many_arguments)]
fn start(
    window: &AppWindow,
    live: &Live,
    root: PathBuf,
    busy: &str,
    cancellable: bool,
    reload: Reload,
    discarded: Vec<Rc<OpenDocument>>,
    after: After,
    name: &str,
    work: impl FnOnce(&Path, &Cancel) -> Result<String, GitError> + Send + 'static,
) {
    let cancel = Cancel::default();
    let (sender, done) = mpsc::channel();
    let started = PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        if pane.job.is_some() {
            return false;
        }
        pane.generation += 1;
        pane.job = Some(Job {
            done,
            cancel: cancel.clone(),
            root: root.clone(),
            reload,
            discarded,
            after,
        });
        true
    });
    if !started {
        return;
    }
    live.cache
        .borrow_mut()
        .log_diag("spec.git", &format!("start {name}"));
    window.set_git_busy(true);
    window.set_git_busy_text(busy.into());
    window.set_git_can_cancel(cancellable);
    std::thread::spawn(move || {
        let _ = sender.send(work(&root, &cancel));
    });
    poll(window, live);
}

/// すぐ済む操作（Stage など）。読み直しのほかに何もしない。
fn quick(
    window: &AppWindow,
    live: &Live,
    name: &str,
    work: impl FnOnce(&Path) -> Result<(), GitError> + Send + 'static,
) {
    let Some(root) = PANE.with(|pane| root_of(&pane.borrow())) else {
        return;
    };
    start(
        window,
        live,
        root,
        "",
        false,
        Reload::None,
        Vec::new(),
        After::Nothing,
        name,
        move |root, _| work(root).map(|()| String::new()),
    );
}

/// 確認・未保存の確かめを済ませてから、操作を走らせる。
fn request(window: &AppWindow, live: &Live, pending: Pending) {
    if live.pending.borrow().is_some() {
        return;
    }
    if !pending.confirmed
        && let Some(text) = confirmation(&pending)
    {
        let (yes, danger) = match &pending.action {
            Action::Commit { .. } => (pick("Amendする", "Amend"), -1),
            Action::StashDrop(_) => (pick("Dropする", "Drop"), 0),
            _ => (pick("変更を元に戻す", "Undo Changes"), 0),
        };
        crate::ask_question(
            window,
            live,
            Question::GitConfirm(pending),
            text,
            &[yes, crate::cancel()],
            danger,
        );
        return;
    }
    let edited = edited_in_scope(live, &pending);
    match (pending.action.guard(), edited.is_empty()) {
        (Guard::None, _) | (_, true) => run(window, live, pending, Vec::new()),
        (Guard::Discard, false) => run(window, live, pending, edited),
        (Guard::SaveOnly, false) => crate::ask_question(
            window,
            live,
            Question::GitUnsaved(pending),
            say!(
                "このリポジトリに未保存の文書があります。保存してから続けますか？\n\n保存しないとCommitに入りません。",
                "There are unsaved documents in this repository. Save them and continue?\n\nUnsaved changes are not part of the commit."
            ),
            &[pick("保存して続ける", "Save and Continue"), crate::cancel()],
            -1,
        ),
        (Guard::SaveOrDiscard, false) => crate::ask_question(
            window,
            live,
            Question::GitUnsaved(pending),
            say!(
                "このリポジトリに未保存の文書があります。\n\nGitがファイルを書き換えるため、先に保存するか破棄してください。",
                "There are unsaved documents in this repository.\n\nGit rewrites the files, so save or discard them first."
            ),
            &[
                pick("保存して続ける", "Save and Continue"),
                pick("破棄して続ける", "Discard and Continue"),
                crate::cancel(),
            ],
            1,
        ),
    }
}

/// 確認の文（要らなければ`None`）。
fn confirmation(pending: &Pending) -> Option<String> {
    match &pending.action {
        Action::Undo { changes, staged } => {
            let names: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
            let new = changes.iter().any(|c| {
                c.kind == Kind::Untracked
                    || (*staged && matches!(c.kind, Kind::Added | Kind::Renamed))
            });
            let mut text = say!(
                "{}の変更を元に戻しますか？\n\n元に戻せません。開いているTABの未保存の変更も破棄します。",
                "Undo the changes to {}?\n\nThis cannot be undone. Unsaved changes in an open tab are discarded too.",
                names.join(", ")
            );
            if new {
                text.push_str(&say!(
                    "新しく足したファイルは削除します。",
                    " A file that was added is deleted."
                ));
            }
            Some(text)
        }
        Action::StashDrop(name) => Some(say!(
            "{name}を捨てますか？\n\n元に戻せません。",
            "Drop {name}?\n\nThis cannot be undone."
        )),
        Action::Commit { amend: true, .. } if git::head_is_pushed(&pending.root) => Some(say!(
            "直前のCommitはもうPushしています。Amendしますか？\n\nAmendすると、送ったCommitと食い違います（送り直すにはforce pushが要ります）。",
            "The last commit has already been pushed. Amend it?\n\nAmending makes it differ from the pushed commit (pushing again needs a force push)."
        )),
        _ => None,
    }
}

fn edited_in_scope(live: &Live, pending: &Pending) -> Vec<Rc<OpenDocument>> {
    let files: Vec<PathBuf> = match &pending.action {
        Action::Undo { changes, .. } => changes
            .iter()
            .flat_map(|c| std::iter::once(&c.path).chain(c.from.as_ref()))
            .map(|p| pending.root.join(p))
            .collect(),
        _ => Vec::new(),
    };
    crate::open_documents(live)
        .into_iter()
        .filter(|document| document.text.edited())
        .filter(|document| {
            let file = document.file.borrow();
            let Some(path) = file.path() else {
                return false;
            };
            if files.is_empty() {
                inside(path, &pending.root)
            } else {
                files.iter().any(|f| key(f) == key(path))
            }
        })
        .collect()
}

/// 「保存して続ける」「破棄して続ける」（`Question::GitUnsaved`）。
pub fn unsaved_answered(window: &AppWindow, live: &Live, pending: Pending, choice: i32) {
    let save_only = matches!(pending.action.guard(), Guard::SaveOnly);
    let edited = edited_in_scope(live, &pending);
    match (choice, save_only) {
        (0, _) => {
            for document in &edited {
                let path = document.file.borrow().path().map(Path::to_path_buf);
                let outside = document.file.borrow().external_change()
                    != crate::buffer::ExternalChange::None
                    || document.outside.get();
                if let Some(path) = path
                    && !outside
                {
                    crate::saving::write_document_to(window, live, document, path);
                }
            }
            if edited.iter().any(|document| document.text.edited()) {
                notice(
                    window,
                    live,
                    say!(
                        "保存できなかった文書があるため、何もしませんでした。外で変更されたファイルは、TABから個別に保存してください。",
                        "Nothing was done because some documents could not be saved. Save a file changed outside from its own tab."
                    ),
                );
                return;
            }
            run(window, live, pending, Vec::new());
        }
        (1, false) => run(window, live, pending, edited),
        _ => {}
    }
}

/// 確認の問いに「はい」（`Question::GitConfirm`）。
pub fn confirmed(window: &AppWindow, live: &Live, mut pending: Pending) {
    pending.confirmed = true;
    request(window, live, pending);
}

/// 新しいブランチの名前が来た（`Question::GitNewBranch`）。
pub fn branch_named(window: &AppWindow, live: &Live, root: PathBuf, name: String) {
    let told = say!(
        "ブランチ{name}を作って移りました",
        "Created and switched to {name}"
    );
    start(
        window,
        live,
        root,
        "",
        false,
        Reload::None,
        Vec::new(),
        After::Nothing,
        "branch",
        move |root, _| git::create_branch(root, &name).map(|()| told),
    );
}

/// 確かめ終わった操作を走らせる。`discarded`は破棄すると答えた文書。
fn run(window: &AppWindow, live: &Live, pending: Pending, discarded: Vec<Rc<OpenDocument>>) {
    // 破棄：退避も捨てる。中身は操作のあとで読み直す（`reload_documents`）。
    for document in &discarded {
        crate::saving::discard_work_copy(
            live,
            &crate::saving::work_identity(&document.file.borrow()),
        );
    }
    let Pending { root, action, .. } = pending;
    match action {
        Action::Switch(branch) => {
            let told = say!("{branch}へ切り替えました", "Switched to {branch}");
            start(
                window,
                live,
                root,
                "",
                false,
                Reload::Repository,
                discarded,
                After::Nothing,
                "switch",
                move |root, _| git::switch(root, &branch).map(|()| told),
            );
        }
        Action::Pull => start(
            window,
            live,
            root,
            pick("Pullしています…", "Pulling…"),
            true,
            Reload::Repository,
            discarded,
            After::Nothing,
            "pull",
            |root, cancel| {
                git::pull(root, cancel).map(|()| pick("Pullしました", "Pulled").to_owned())
            },
        ),
        Action::Sync => start(
            window,
            live,
            root,
            pick("Syncしています…", "Syncing…"),
            true,
            Reload::Repository,
            discarded,
            After::Nothing,
            "sync",
            |root, cancel| {
                let status = git::status(root)?;
                if status.upstream.is_some() {
                    git::pull(root, cancel)?;
                }
                git::push(root, &git::status(root)?, cancel)?;
                Ok(pick("Syncしました", "Synced").to_owned())
            },
        ),
        Action::StashApply { name, pop } => {
            let told = if pop {
                say!("{name}をPopしました", "Popped {name}")
            } else {
                say!("{name}をApplyしました", "Applied {name}")
            };
            start(
                window,
                live,
                root,
                "",
                false,
                Reload::Repository,
                discarded,
                After::Nothing,
                "stash-apply",
                move |root, _| git::stash_apply(root, &name, pop).map(|()| told),
            );
        }
        Action::StashDrop(name) => {
            let told = say!("{name}をDropしました", "Dropped {name}");
            start(
                window,
                live,
                root,
                "",
                false,
                Reload::None,
                discarded,
                After::Nothing,
                "stash-drop",
                move |root, _| git::stash_drop(root, &name).map(|()| told),
            );
        }
        Action::Commit {
            message,
            all,
            amend,
        } => start(
            window,
            live,
            root,
            pick("Commitしています…", "Committing…"),
            false,
            Reload::None,
            discarded,
            After::ClearMessage,
            "commit",
            move |root, _| {
                git::commit(root, &message, all, amend)?;
                Ok(if amend {
                    pick("Amendしました", "Amended").to_owned()
                } else {
                    pick("Commitしました", "Committed").to_owned()
                })
            },
        ),
        Action::StashAll(message) => start(
            window,
            live,
            root,
            "",
            false,
            Reload::Repository,
            discarded,
            After::ClearMessage,
            "stash",
            move |root, _| {
                git::stash_all(root, &message).map(|()| pick("Stashしました", "Stashed").to_owned())
            },
        ),
        Action::Undo { changes, staged } => {
            let files: Vec<PathBuf> = changes
                .iter()
                .flat_map(|c| std::iter::once(&c.path).chain(c.from.as_ref()))
                .map(|p| root.join(p))
                .collect();
            start(
                window,
                live,
                root,
                "",
                false,
                Reload::Files(files),
                discarded,
                After::Nothing,
                "undo",
                move |root, _| {
                    git::undo(root, &changes, staged)
                        .map(|()| pick("変更を元に戻しました", "Changes undone").to_owned())
                },
            );
        }
    }
}

fn pending(action: Action) -> Option<Pending> {
    let root = PANE.with(|pane| root_of(&pane.borrow()))?;
    Some(Pending {
        root,
        action,
        confirmed: false,
    })
}

fn act(window: &AppWindow, live: &Live, action: Action) {
    if let Some(pending) = pending(action) {
        request(window, live, pending);
    }
}

fn status_now() -> Option<Status> {
    PANE.with(|pane| {
        let pane = pane.borrow();
        root_of(&pane)?;
        pane.snapshot.as_ref().map(|s| s.status.clone())
    })
}

fn repo_chosen(window: &AppWindow, live: &Live, index: usize) {
    let changed = PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        let Some(folder) = pane.folders.get(index).cloned() else {
            return false;
        };
        if pane.chosen.as_ref().is_some_and(|c| key(c) == key(&folder)) {
            return false;
        }
        pane.chosen = Some(folder);
        pane.snapshot = None;
        pane.error = None;
        pane.generation += 1;
        true
    });
    if changed {
        window.set_git_amend(false);
        render(window);
        refresh(window, live);
    }
}

fn create_repository(window: &AppWindow, live: &Live) {
    let Some(folder) = PANE.with(|pane| pane.borrow().chosen.clone()) else {
        return;
    };
    let told = say!(
        "{}をGitのリポジトリにしました",
        "Created a Git repository in {}",
        folder.display()
    );
    start(
        window,
        live,
        folder,
        "",
        false,
        Reload::None,
        Vec::new(),
        After::Nothing,
        "init",
        move |folder, _| git::init(folder).map(|()| told),
    );
}

fn branch_chosen(window: &AppWindow, live: &Live, index: usize) {
    let target = PANE.with(|pane| {
        let pane = pane.borrow();
        let snapshot = pane.snapshot.as_ref()?;
        let name = snapshot.branches.get(index)?;
        (snapshot.status.branch.as_ref() != Some(name)).then(|| name.clone())
    });
    if let Some(name) = target {
        act(window, live, Action::Switch(name));
    }
}

fn new_branch(window: &AppWindow, live: &Live) {
    let Some(root) = PANE.with(|pane| root_of(&pane.borrow())) else {
        return;
    };
    if live.pending.borrow().is_some() {
        return;
    }
    crate::ask_for_name(
        window,
        live,
        Question::GitNewBranch(root),
        say!(
            "新しいブランチの名前を入れてください。作ったら、そのブランチへ移ります。",
            "Name the new branch. You will be switched to it."
        ),
        "",
    );
}

fn fetch(window: &AppWindow, live: &Live) {
    let Some(root) = PANE.with(|pane| root_of(&pane.borrow())) else {
        return;
    };
    start(
        window,
        live,
        root,
        pick("Fetchしています…", "Fetching…"),
        true,
        Reload::None,
        Vec::new(),
        After::Nothing,
        "fetch",
        |root, cancel| {
            git::fetch(root, cancel).map(|()| pick("Fetchしました", "Fetched").to_owned())
        },
    );
}

fn push(window: &AppWindow, live: &Live) {
    let (Some(root), Some(status)) = (PANE.with(|pane| root_of(&pane.borrow())), status_now())
    else {
        return;
    };
    let (busy, told) = if status.upstream.is_some() {
        (
            pick("Pushしています…", "Pushing…"),
            pick("Pushしました", "Pushed"),
        )
    } else {
        (
            pick("Publishしています…", "Publishing…"),
            pick("Publishしました", "Published"),
        )
    };
    start(
        window,
        live,
        root,
        busy,
        true,
        Reload::None,
        Vec::new(),
        After::Nothing,
        "push",
        move |root, cancel| {
            // 押したあとに状態が変わっていてもよいように、送る直前に読み直す。
            let status = git::status(root)?;
            git::push(root, &status, cancel).map(|()| told.to_owned())
        },
    );
}

fn commit(window: &AppWindow, live: &Live) {
    let message = window.get_git_message().to_string();
    let amend = window.get_git_amend();
    let Some(status) = status_now() else {
        return;
    };
    if message.trim().is_empty() {
        window.tell(pick("メッセージを入れてください", "Enter a message").into());
        return;
    }
    let all = !amend && status.staged.is_empty();
    act(
        window,
        live,
        Action::Commit {
            message,
            all,
            amend,
        },
    );
}

fn amend_toggled(window: &AppWindow, wanted: bool) {
    let root = PANE.with(|pane| root_of(&pane.borrow()));
    if wanted {
        let Some(root) = root else {
            return;
        };
        match git::last_message(&root) {
            Ok(last) => {
                let kept = window.get_git_message().to_string();
                PANE.with(|pane| pane.borrow_mut().kept_message = Some(kept));
                window.set_git_message(last.into());
                window.set_git_amend(true);
            }
            Err(error) => window.tell(error.to_string().into()),
        }
    } else {
        let kept = PANE.with(|pane| pane.borrow_mut().kept_message.take());
        window.set_git_message(kept.unwrap_or_default().into());
        window.set_git_amend(false);
    }
}

fn row(index: usize) -> Option<(Option<Section>, Option<(bool, Change)>, Option<Stash>)> {
    PANE.with(|pane| match pane.borrow().lines.get(index)? {
        Line::Header(section) => Some((Some(*section), None, None)),
        Line::File { staged, change } => Some((None, Some((*staged, change.clone())), None)),
        Line::Stash(stash) => Some((None, None, Some(stash.clone()))),
        Line::Note => None,
    })
}

fn row_clicked(window: &AppWindow, index: usize) {
    if let Some((Some(section), ..)) = row(index) {
        PANE.with(|pane| {
            let mut pane = pane.borrow_mut();
            match pane.closed.iter().position(|s| *s == section) {
                Some(at) => {
                    pane.closed.remove(at);
                }
                None => pane.closed.push(section),
            }
        });
        render(window);
    }
}

/// ファイルを開く。`compare`なら、変更のあるファイルは前回のCommitと比べる。
fn open_file(window: &AppWindow, live: &Live, staged: bool, change: &Change, compare: bool) {
    let Some(root) = PANE.with(|pane| root_of(&pane.borrow())) else {
        return;
    };
    let path = root.join(&change.path);
    if change.kind == Kind::Deleted || !path.is_file() {
        window.tell(
            say!(
                "{}は削除されているため開けません",
                "{} is deleted, so it cannot be opened",
                change.path
            )
            .into(),
        );
        return;
    }
    crate::open_path_in_focused_pane(window, live, &path, Opening::Kept);
    let opened = live
        .active(window)
        .file
        .borrow()
        .path()
        .is_some_and(|p| key(p) == key(&path));
    let in_head = match change.kind {
        Kind::Modified | Kind::Conflicted => true,
        Kind::Renamed => false,
        Kind::Added | Kind::Untracked | Kind::Deleted => false,
    };
    if compare && opened && in_head && !(staged && change.kind == Kind::Added) {
        window.invoke_compare_head_requested();
    }
}

fn row_activated(window: &AppWindow, live: &Live, index: usize) {
    if let Some((_, Some((staged, change)), _)) = row(index) {
        open_file(window, live, staged, &change, true);
    }
}

fn stage_paths(change: &Change) -> Vec<String> {
    std::iter::once(change.path.clone())
        .chain(change.from.clone())
        .collect()
}

fn row_staged(window: &AppWindow, live: &Live, index: usize) {
    let unborn = status_now().is_some_and(|s| s.unborn);
    match row(index) {
        Some((Some(Section::Staged), ..)) => {
            quick(window, live, "unstage", move |root| {
                git::unstage(root, &[], unborn)
            });
        }
        Some((Some(Section::Changes), ..)) => {
            quick(window, live, "stage", |root| git::stage(root, &[]));
        }
        Some((None, Some((true, change)), _)) => {
            let paths = stage_paths(&change);
            quick(window, live, "unstage", move |root| {
                git::unstage(root, &paths, unborn)
            });
        }
        Some((None, Some((false, change)), _)) => {
            let paths = vec![change.path];
            quick(window, live, "stage", move |root| git::stage(root, &paths));
        }
        _ => {}
    }
}

/// 右クリックの行：0 Open・Apply、1 Stage／Unstage・Pop、2 Undo Changes…・Drop…。
fn row_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    match (row(index), action) {
        (Some((None, Some((staged, change)), _)), 0) => {
            open_file(window, live, staged, &change, false)
        }
        (Some((None, Some(_), _)), 1) => row_staged(window, live, index),
        (Some((None, Some((staged, change)), _)), 2) => act(
            window,
            live,
            Action::Undo {
                changes: vec![change],
                staged,
            },
        ),
        (Some((None, None, Some(stash))), 0 | 1) => act(
            window,
            live,
            Action::StashApply {
                name: stash.name,
                pop: action == 1,
            },
        ),
        (Some((None, None, Some(stash))), 2) => act(window, live, Action::StashDrop(stash.name)),
        _ => {}
    }
}

/// 面が出ているあいだの5秒ごとの読み直し。問いが立っているあいだは読まない。
fn tick(window: &AppWindow, live: &Live) {
    if visible(window) && live.pending.borrow().is_none() {
        refresh(window, live);
    }
}

/// 行の番号を受ける操作は、組み直しを次の回へ回す（`backup_ui`と同じ、6.18）。
fn later(window: &AppWindow, live: &Live, body: impl FnOnce(&AppWindow, &Live) + 'static) {
    let (weak, live) = (window.as_weak(), live.clone());
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(window) = weak.upgrade() {
            body(&window, &live);
        }
    });
}

pub fn wire(window: &AppWindow, live: &Live) {
    TICK.with(|timer| {
        let weak = window.as_weak();
        let live = live.clone();
        timer.start(TimerMode::Repeated, Duration::from_secs(5), move || {
            if let Some(window) = weak.upgrade() {
                tick(&window, &live);
            }
        });
    });

    macro_rules! on {
        ($setter:ident, |$window:ident, $live:ident $(, $arg:ident)*| $body:expr) => {{
            let weak = window.as_weak();
            let held = live.clone();
            window.$setter(move |$($arg),*| {
                if let Some(window) = weak.upgrade() {
                    later(&window, &held, move |$window, $live| $body);
                }
            });
        }};
    }

    on!(on_git_repo_chosen, |w, l, index| repo_chosen(
        w,
        l,
        index.max(0) as usize
    ));
    on!(on_git_create_repository, |w, l| create_repository(w, l));
    on!(on_git_branch_chosen, |w, l, index| branch_chosen(
        w,
        l,
        index.max(0) as usize
    ));
    on!(on_git_new_branch, |w, l| new_branch(w, l));
    on!(on_git_fetch, |w, l| fetch(w, l));
    on!(on_git_pull, |w, l| act(w, l, Action::Pull));
    on!(on_git_push, |w, l| push(w, l));
    on!(on_git_sync, |w, l| act(w, l, Action::Sync));
    on!(on_git_commit, |w, l| commit(w, l));
    on!(on_git_stash_all, |w, l| {
        let message = w.get_git_message().to_string();
        act(w, l, Action::StashAll(message))
    });
    on!(on_git_row_clicked, |w, _l, index| row_clicked(
        w,
        index.max(0) as usize
    ));
    on!(on_git_row_activated, |w, l, index| row_activated(
        w,
        l,
        index.max(0) as usize
    ));
    on!(on_git_row_staged, |w, l, index| row_staged(
        w,
        l,
        index.max(0) as usize
    ));
    on!(on_git_row_menu, |w, l, index, action| row_menu(
        w,
        l,
        index.max(0) as usize,
        action
    ));

    let weak = window.as_weak();
    window.on_git_amend_toggled(move |wanted| {
        if let Some(window) = weak.upgrade() {
            amend_toggled(&window, wanted);
        }
    });
    window.on_git_cancel(|| {
        PANE.with(|pane| {
            if let Some(job) = pane.borrow().job.as_ref() {
                job.cancel.cancel();
            }
        });
    });
}

#[cfg(test)]
#[path = "git_ui_tests.rs"]
mod tests;
