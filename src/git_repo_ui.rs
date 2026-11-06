//! RFN01-67 PR 2a（書き手と決めた 2026-10-03）: Git Repository の画面。
//!
//! GitKraken を手本に、全ブランチのグラフを真ん中に置く。Gitを呼ぶのは[`crate::git`]、筋を
//! 決めるのは[`crate::git_graph`]、確認・未保存の確かめ・裏の実行は Git Changes と同じ
//! [`crate::git_ui`]の道（`Action`）を使う。ここは画面の状態と、行・列を組むことだけを持つ。
//!
//! - **対象は Git Changes で選んでいるリポジトリ。**比較の画面と同じく窓に重ねて開き、
//!   Close／Escで閉じる。ペインとTABはその下にそのまま残る。
//! - 読むのは裏のスレッドで（status・log・refs・上流との差・stash）。操作のあと、保存のあと、
//!   開いているあいだの5秒ごとに読み直す。
//! - ファイルを押すと、今までの左右の比較の画面がこの上に開き、閉じるとここへ戻る。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use slint::{Color, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::git::{self, Change, GitError, HistoryField, LogEntry, Ref, Stash, Status};
use crate::git_change_diff::Preview;
use crate::git_graph;
use crate::git_history::Point;
use crate::git_ui::{self, Action};
use crate::i18n::pick;
use crate::inline_diff;
use crate::{
    AppWindow, GitBranchRow, GitCommitRow, GitDiffPart, GitDiffRow, GitHunkLine, GitHunkRow,
    GitLine, GitRef, GitRow, GitSearchRow, Live, MAX_DOCUMENT_CHARACTERS, Opening, StatusBar, say,
};

/// 1度に読む Commit の数（「Show more」で同じだけ足す）。
const PAGE: usize = 200;
/// 行の高さ（`git-repository.slint`の`row-height`と同じ）。
const ROW_HEIGHT: f32 = 30.0;
/// 筋の色。0 は幹（main、アクセントの紫）。グラフは筋を見分けるために色が要るので、
/// ここだけ紫の外の色を使う（書き手と見本で確かめた 2026-10-03）。
const LANE_COLORS: [u32; 8] = [
    0x6b4cae, 0x2f7d79, 0xa8792a, 0x3f6fb0, 0x9a4f8a, 0x4f8a3a, 0xb0603f, 0x5f6b7a,
];

fn lane_x(lane: usize) -> f32 {
    10.0 + lane as f32 * 14.0
}

/// 筋の色。**紫（0）は幹（main）だけ**——ほかの筋は残りの7色を順に回す。8色を単に
/// 回すと、9本目の枝が幹と同じ紫になり、main から分かれたように見えなかった
/// （書き手の確認 2026-10-03）。
fn lane_color(index: usize) -> Color {
    let others = LANE_COLORS.len() - 1;
    let rgb = if index == 0 {
        LANE_COLORS[0]
    } else {
        LANE_COLORS[1 + (index - 1) % others]
    };
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// 読み直した結果。同じなら画面を組み直さない（5秒ごとの読み直しで、行が描き直されない）。
#[derive(PartialEq)]
struct Data {
    entries: Vec<LogEntry>,
    refs: Vec<Ref>,
    head: Option<String>,
    /// 幹（紫の筋）の先端（`trunk_of`）。
    trunk: Option<String>,
    status: Status,
    incoming: HashSet<String>,
    outgoing: HashSet<String>,
    worktrees: Result<Vec<git::Worktree>, GitError>,
    stashes: Vec<Stash>,
}

impl Data {
    fn has_changes(&self) -> bool {
        !self.status.staged.is_empty() || !self.status.changes.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Selected {
    Wip,
    Commit(String),
}

/// 左の列の行が指すもの。
enum Side {
    Header,
    Local(Ref),
    Remote(Ref),
    Group,
    Stash(Stash),
}

/// 右の列の差分（PR 2b）が何を比べているか。
enum DiffShown {
    Commit(CommitDiff),
    Wip(Preview),
    Range(Preview),
}

#[derive(Clone)]
struct CommitDiff {
    sha: String,
    parent: Option<String>,
    change: Change,
    /// Working Copy（今のファイル）と比べるか。
    working: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct ComparePoint {
    sha: String,
    branch: Option<String>,
}

impl ComparePoint {
    fn label(&self, full: bool) -> String {
        let sha = if full {
            self.sha.clone()
        } else {
            self.sha.chars().take(7).collect()
        };
        match &self.branch {
            Some(name) => format!("{name} · {sha}"),
            None => sha,
        }
    }
}

struct RangeComparison {
    left: ComparePoint,
    right: ComparePoint,
    files: Vec<Change>,
}

#[derive(Clone)]
struct SearchQuery {
    field: HistoryField,
    text: String,
    limit: usize,
}

struct SearchResults {
    query: SearchQuery,
    entries: Vec<LogEntry>,
    more: bool,
}

enum Lookup {
    Search(SearchResults),
    Commit(git::Detail),
    Range(RangeComparison),
    File(Preview),
}

#[derive(Default)]
struct Repo {
    root: Option<PathBuf>,
    count: usize,
    data: Option<Data>,
    error: Option<String>,
    reading: Option<Receiver<Result<Data, GitError>>>,
    /// 読んでいるあいだに頼まれた読み直し。終わったらもう一度読む。
    again: bool,
    selected: Option<Selected>,
    detail: Option<git::Detail>,
    diff: Option<Rc<DiffShown>>,
    block_mode: bool,
    block: usize,
    search: Option<SearchResults>,
    lookup: Option<Receiver<Result<Lookup, GitError>>>,
    off_graph: bool,
    base: Option<ComparePoint>,
    range: Option<RangeComparison>,
    side: Vec<Side>,
    /// グラフの行の Commit（`None`は `// WIP`）。
    rows: Vec<Option<String>>,
}

thread_local! {
    static REPO: RefCell<Repo> = RefCell::new(Repo::default());
    static POLL: Timer = Timer::default();
    static BRANCH_MENU: RefCell<Option<BranchMenu>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepoAction {
    OpenComparison,
    ReviewBlocks,
    StageBlock,
    UnstageBlock,
    OpenSearch,
    CloseSearch,
    SetBase,
    CompareWithBase,
}

#[derive(Clone)]
pub(crate) struct ActionTarget {
    root: PathBuf,
    diff: Option<Rc<DiffShown>>,
    block_mode: bool,
    block: usize,
    search_open: bool,
    point: Option<ComparePoint>,
    base: Option<ComparePoint>,
    from_menu: bool,
}

impl ActionTarget {
    pub(crate) fn capture(window: &AppWindow) -> Option<Self> {
        if !window.get_git_repo_active() {
            return None;
        }
        REPO.with(|repo| {
            let repo = repo.borrow();
            let point = repo.detail.as_ref().map(|detail| ComparePoint {
                sha: detail.sha.clone(),
                branch: None,
            });
            Some(Self {
                root: repo.root.clone()?,
                diff: repo.diff.clone(),
                block_mode: repo.block_mode,
                block: repo.block,
                search_open: window.get_git_repo_search_open(),
                point,
                base: repo.base.clone(),
                from_menu: false,
            })
        })
    }

    pub(crate) fn for_branch(mut self, branch: &Ref) -> Self {
        self.point = Some(ComparePoint {
            sha: branch.sha.clone(),
            branch: Some(branch.name.clone()),
        });
        self.from_menu = true;
        self
    }

    pub(crate) fn valid(&self, current: &Self, action: RepoAction) -> bool {
        use RepoAction::*;
        if self.root != current.root {
            return false;
        }
        let same_diff = || self.diff.as_ref().zip(current.diff.as_ref())
            .is_some_and(|(left, right)| Rc::ptr_eq(left, right))
            && self.block_mode == current.block_mode;
        let same_point = || self.from_menu || self.point == current.point;
        match action {
            OpenComparison | ReviewBlocks => same_diff(),
            StageBlock | UnstageBlock => same_diff() && self.block == current.block,
            OpenSearch | CloseSearch => self.search_open == current.search_open,
            SetBase => same_point(),
            CompareWithBase => same_point() && self.base == current.base,
        }
    }

    fn patch(&self) -> Option<&Arc<crate::git_hunks::Patch>> {
        match self.diff.as_deref() {
            Some(DiffShown::Wip(preview)) => preview.patch.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn enabled(&self, window: &AppWindow, action: RepoAction) -> bool {
        use RepoAction::*;
        match action {
            OpenComparison => window.get_git_repo_detail_mode() == 3
                && window.get_git_repo_diff_note().is_empty()
                && self.diff.is_some() && !self.block_mode,
            ReviewBlocks => window.get_git_repo_hunks_available()
                && !self.block_mode && !window.get_git_busy(),
            StageBlock | UnstageBlock => self.block_mode && !window.get_git_busy()
                && !window.get_git_merge_active()
                && git_ui::active_root().as_ref() == Some(&self.root)
                && self.patch().is_some_and(|patch| {
                    patch.staged == (action == UnstageBlock) && patch.hunks.get(self.block).is_some()
                }),
            OpenSearch => !self.search_open,
            CloseSearch => self.search_open,
            SetBase => self.point.is_some(),
            CompareWithBase => self.point.is_some() && self.base.is_some(),
        }
    }

    pub(crate) fn execute(&self, window: &AppWindow, live: &Live, action: RepoAction) {
        use RepoAction::*;
        match action {
            OpenComparison => diff_open_full(window),
            ReviewBlocks => open_hunks(window),
            StageBlock | UnstageBlock => {
                if let Some(patch) = self.patch() {
                    apply_block(window, live, &self.root, patch.clone(), self.block);
                }
            }
            OpenSearch | CloseSearch => toggle_search(window),
            SetBase | CompareWithBase => {
                if let Some(point) = &self.point {
                    choose_point(window, live, point.clone(), action == SetBase);
                }
            }
        }
    }
}

pub(crate) fn root() -> Option<PathBuf> {
    REPO.with(|repo| repo.borrow().root.clone())
}

/// File の「Git Repository…」と、Git Changes の見出しの釦。
pub fn open(window: &AppWindow, live: &Live) {
    open_folder(window, live, git_ui::repository_folder(window, live).as_deref());
}

pub(crate) fn open_folder(window: &AppWindow, live: &Live, folder: Option<&Path>) {
    let root = match git_ui::prepare_repository(window, live, folder) {
        Ok(root) => root,
        Err(text) => {
            window.tell(text.into());
            return;
        }
    };
    git_ui::cancel_change_diff();
    BRANCH_MENU.with(|menu| menu.borrow_mut().take());
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if repo.root.as_ref() != Some(&root) {
            *repo = Repo {
                root: Some(root.clone()),
                count: PAGE,
                ..Repo::default()
            };
        }
        // 開いた直後は何も選ばない（書き手と決めた見本のとおり）。
        repo.selected = None;
        repo.detail = None;
        repo.diff = None;
        repo.block_mode = false;
        repo.lookup = None;
        repo.off_graph = false;
        repo.base = None;
        repo.range = None;
        repo.search = None;
    });
    window.set_git_repo_search_open(false);
    window.set_git_worktrees_open(false);
    window.set_git_repo_inspection_note("".into());
    window.set_git_repo_compare_base("".into());
    publish_search(window, None);
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    window.set_git_repo_name(name.into());
    window.set_git_repo_active(true);
    live.cache
        .borrow_mut()
        .log_diag("spec.git", "repository open");
    render(window);
    refresh(window, live);
    // 右の列の WIP（Git Changes）も同じリポジトリを読む。
    git_ui::refresh_soon(window, live);
}

pub fn close(window: &AppWindow) {
    if !window.get_git_repo_active() {
        return;
    }
    git_ui::cancel_change_diff();
    BRANCH_MENU.with(|menu| menu.borrow_mut().take());
    REPO.with(|repo| repo.borrow_mut().lookup = None);
    window.set_git_repo_active(false);
    window.set_git_merge_open(false);
    window.set_git_worktrees_open(false);
    crate::restore_editor_focus(window);
}

/// 比較の画面を閉じたとき（`diff_view::dismiss`）：この画面が開いていれば鍵盤を取り直す。
pub fn regain_focus(window: &AppWindow) {
    if window.get_git_repo_active() {
        window.set_git_repo_focus_generation(window.get_git_repo_focus_generation() + 1);
    }
}

/// 開いていれば、裏で読み直す。
pub fn refresh(window: &AppWindow, live: &Live) {
    if !window.get_git_repo_active() {
        return;
    }
    let start = REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if repo.reading.is_some() {
            repo.again = true;
            return None;
        }
        let root = repo.root.clone()?;
        let (sender, receiver) = mpsc::channel();
        repo.reading = Some(receiver);
        Some((root, repo.count, sender))
    });
    let Some((root, count, sender)) = start else {
        return;
    };
    std::thread::spawn(move || {
        let _ = sender.send(read(&root, count));
    });
    poll(window, live);
}

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

fn read(root: &Path, count: usize) -> Result<Data, GitError> {
    let status = git::status(root)?;
    let (incoming, outgoing) = git::incoming_outgoing(root);
    let refs = git::refs(root)?;
    let head = git::head_sha(root);
    let trunk = trunk_of(&refs, git::remote_default(root).as_deref()).or_else(|| head.clone());
    Ok(Data {
        entries: git::log(root, count)?,
        refs,
        head,
        trunk,
        stashes: git::stashes(root)?,
        incoming: incoming.into_iter().collect(),
        outgoing: outgoing.into_iter().collect(),
        worktrees: git::worktrees(root),
        status,
    })
}

/// 幹（紫の筋）にするブランチの先端（書き手の判断 2026-10-03：「紫の幹は main」）。
/// ローカルの main、master、origin の既定のブランチ（同じ名前のローカルがあればそちら）の順。
/// どれも無ければ`None`（呼ぶ側が HEAD を幹にする）。
fn trunk_of(refs: &[Ref], remote_default: Option<&str>) -> Option<String> {
    let local = |name: &str| refs.iter().find(|r| !r.remote && r.name == name);
    if let Some(found) = local("main").or_else(|| local("master")) {
        return Some(found.sha.clone());
    }
    let remote = remote_default?;
    let short = remote.split_once('/').map_or(remote, |(_, name)| name);
    local(short)
        .or_else(|| refs.iter().find(|r| r.remote && r.name == remote))
        .map(|found| found.sha.clone())
}

fn collect(window: &AppWindow, live: &Live) {
    collect_graph(window, live);
    collect_lookup(window);
    if REPO.with(|repo| {
        let repo = repo.borrow();
        repo.reading.is_none() && repo.lookup.is_none()
    }) {
        POLL.with(Timer::stop);
    }
}

fn collect_graph(window: &AppWindow, live: &Live) {
    let result = REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        let receiver = repo.reading.as_ref()?;
        match receiver.try_recv() {
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                repo.reading = None;
                Some(Err(GitError::Failed("Git stopped".into())))
            }
            Ok(result) => {
                repo.reading = None;
                Some(result)
            }
        }
    });
    let Some(result) = result else {
        return;
    };
    let mut changed = true;
    let again = REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        match result {
            Ok(data) if repo.error.is_none() && repo.data.as_ref() == Some(&data) => {
                changed = false;
            }
            Ok(data) => {
                // 選んでいた Commit が無くなっていれば（Reset など）、選びを外す。
                let keep = match &repo.selected {
                    Some(Selected::Commit(sha)) => {
                        repo.off_graph || data.entries.iter().any(|e| &e.sha == sha)
                    }
                    Some(Selected::Wip) => data.has_changes(),
                    None => true,
                };
                if !keep {
                    repo.selected = None;
                    repo.detail = None;
                    repo.diff = None;
                    repo.block_mode = false;
                }
                repo.data = Some(data);
                repo.error = None;
            }
            Err(error) => {
                repo.data = None;
                repo.error = Some(error.to_string());
            }
        }
        std::mem::take(&mut repo.again)
    });
    if changed {
        render(window);
    }
    if again {
        refresh(window, live);
    }
}

fn start_lookup(
    window: &AppWindow,
    live: &Live,
    note: &str,
    read: impl FnOnce(&Path) -> Result<Lookup, GitError> + Send + 'static,
) {
    let Some(root) = root().filter(|_| window.get_git_repo_active()) else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    REPO.with(|repo| repo.borrow_mut().lookup = Some(receiver));
    window.set_git_repo_inspection_note(note.into());
    std::thread::spawn(move || {
        let _ = sender.send(read(&root));
    });
    poll(window, live);
}

fn collect_lookup(window: &AppWindow) {
    let result = REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        let result = match repo.lookup.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(GitError::Failed("Git stopped".into())),
        };
        repo.lookup = None;
        Some(result)
    });
    let Some(result) = result.filter(|_| window.get_git_repo_active()) else {
        return;
    };
    window.set_git_repo_inspection_note("".into());
    match result {
        Ok(Lookup::Search(search)) => REPO.with(|repo| {
            publish_search(window, Some(&search));
            repo.borrow_mut().search = Some(search);
        }),
        Ok(Lookup::Commit(detail)) => {
            select_detail(
                window,
                Selected::Commit(detail.sha.clone()),
                Some(detail),
                -1,
                true,
            );
        }
        Ok(Lookup::Range(range)) => {
            git_ui::cancel_change_diff();
            REPO.with(|repo| {
                let mut repo = repo.borrow_mut();
                repo.selected = None;
                repo.detail = None;
                repo.diff = None;
                repo.block_mode = false;
                repo.range = Some(range);
                window.set_git_repo_selected(-1);
                publish_detail(window, &repo);
            });
        }
        Ok(Lookup::File(preview)) => set_diff(window, DiffShown::Range(preview)),
        Err(error) => window.set_git_repo_inspection_note(error.to_string().into()),
    }
}

fn publish_search(window: &AppWindow, search: Option<&SearchResults>) {
    let rows = search.map_or_else(Vec::new, |search| {
        search
            .entries
            .iter()
            .map(|entry| GitSearchRow {
                sha: entry.sha.chars().take(7).collect::<String>().into(),
                subject: entry.subject.clone().into(),
                author: entry.author.clone().into(),
                date: entry.date.clone().into(),
            })
            .collect()
    });
    window.set_git_repo_search_more(search.is_some_and(|search| search.more));
    window.set_git_repo_search_done(search.is_some());
    window.set_git_repo_search_rows(ModelRc::new(VecModel::from(rows)));
}

fn toggle_search(window: &AppWindow) {
    let open = !window.get_git_repo_search_open();
    window.set_git_repo_search_open(open);
    if !open {
        REPO.with(|repo| repo.borrow_mut().lookup = None);
        window.set_git_repo_inspection_note("".into());
        regain_focus(window);
    }
}

fn search(window: &AppWindow, live: &Live, field: i32, text: &str) {
    let text = text.trim();
    if !window.get_git_repo_search_open() || text.is_empty() {
        return;
    }
    let field = match field {
        0 => HistoryField::Message,
        1 => HistoryField::Author,
        2 => HistoryField::Sha,
        3 => HistoryField::Path,
        _ => return,
    };
    REPO.with(|repo| repo.borrow_mut().search = None);
    publish_search(window, None);
    search_page(
        window,
        live,
        SearchQuery {
            field,
            text: text.to_owned(),
            limit: PAGE,
        },
    );
}

fn search_page(window: &AppWindow, live: &Live, query: SearchQuery) {
    start_lookup(
        window,
        live,
        pick("履歴を検索しています…", "Searching history…"),
        move |root| {
            let mut entries = git::search_history(root, query.field, &query.text, query.limit + 1)?;
            let more = entries.len() > query.limit;
            entries.truncate(query.limit);
            Ok(Lookup::Search(SearchResults {
                query,
                entries,
                more,
            }))
        },
    );
}

fn search_more(window: &AppWindow, live: &Live) {
    let query = REPO.with(|repo| {
        let repo = repo.borrow();
        let search = repo.search.as_ref()?;
        (search.more && repo.lookup.is_none()).then(|| search.query.clone())
    });
    if let Some(mut query) = query.filter(|_| window.get_git_repo_search_open()) {
        query.limit += PAGE;
        search_page(window, live, query);
    }
}

fn select_result(window: &AppWindow, live: &Live, index: i32) {
    let sha = REPO.with(|repo| {
        let repo = repo.borrow();
        Some(
            repo.search
                .as_ref()?
                .entries
                .get(usize::try_from(index).ok()?)?
                .sha
                .clone(),
        )
    });
    if let Some(sha) = sha.filter(|_| window.get_git_repo_search_open()) {
        start_lookup(
            window,
            live,
            pick("Commitを読み込んでいます…", "Reading commit…"),
            move |root| git::detail(root, &sha).map(Lookup::Commit),
        );
    }
}

/// 3つの列を、いまの状態から組む。
fn render(window: &AppWindow) {
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        let repo = &mut *repo;
        let note = match (&repo.error, &repo.data) {
            (Some(error), _) => error.clone(),
            (None, None) => say!("読み込んでいます…", "Reading the repository…"),
            (None, Some(data)) if data.entries.is_empty() => {
                say!("まだCommitがありません", "No commits yet")
            }
            _ => String::new(),
        };
        window.set_git_repo_note(note.into());
        let Some(data) = repo.data.as_ref() else {
            crate::git_worktree_ui::publish(window, repo.root.as_deref(), None, None);
            window.set_git_repo_commits(ModelRc::default());
            window.set_git_repo_branches(ModelRc::default());
            window.set_git_repo_more(false);
            window.set_git_repo_detail_mode(0);
            repo.rows.clear();
            repo.side.clear();
            return;
        };
        crate::git_worktree_ui::publish(window, repo.root.as_deref(), data.head.as_deref(), Some(&data.worktrees));
        let (rows, shas) = commit_rows(data);
        let selected = repo
            .selected
            .as_ref()
            .and_then(|selected| match selected {
                Selected::Wip => shas.iter().position(Option::is_none),
                Selected::Commit(sha) => shas.iter().position(|s| s.as_deref() == Some(sha)),
            })
            .map_or(-1, |at| at as i32);
        window.set_git_repo_commits(ModelRc::new(VecModel::from(rows)));
        window.set_git_repo_selected(selected);
        window.set_git_repo_more(data.entries.len() >= repo.count);
        window.set_git_repo_has_stash(!data.stashes.is_empty());
        window.set_git_repo_has_changes(data.has_changes());
        let (side_rows, side) = side_rows(data);
        window.set_git_repo_branches(ModelRc::new(VecModel::from(side_rows)));
        repo.side = side;
        repo.rows = shas;
        publish_detail(window, repo);
        if let Some(root) = &repo.root {
            let now = Point {
                head: data.head.clone(),
                branch: data.status.branch.clone(),
            };
            let (can_undo, undo_tip) = git_ui::history_state(root, &now, false);
            let (can_redo, redo_tip) = git_ui::history_state(root, &now, true);
            window.set_git_repo_can_undo(can_undo);
            window.set_git_repo_undo_tip(undo_tip.into());
            window.set_git_repo_can_redo(can_redo);
            window.set_git_repo_redo_tip(redo_tip.into());
        }
    });
}

/// グラフの行。`(画面の行, 行ごとの Commit)`。
fn commit_rows(data: &Data) -> (Vec<GitCommitRow>, Vec<Option<String>>) {
    let wip = data.has_changes();
    let pairs: Vec<(&str, &[String])> = data
        .entries
        .iter()
        .map(|e| (e.sha.as_str(), e.parents.as_slice()))
        .collect();
    let layout = git_graph::layout(&pairs, data.trunk.as_deref(), data.head.as_deref(), wip);
    // 今のブランチから辿れる Commit（読んだ範囲の中で）。
    let parents: HashMap<&str, &[String]> = pairs.iter().copied().collect();
    let mut in_head: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = data.head.as_deref().into_iter().collect();
    while let Some(sha) = stack.pop() {
        if !in_head.insert(sha) {
            continue;
        }
        if let Some(list) = parents.get(sha) {
            stack.extend(list.iter().map(String::as_str));
        }
    }
    let current = data.status.branch.as_deref();
    let mut tags: HashMap<&str, Vec<GitRef>> = HashMap::new();
    for reference in &data.refs {
        let kind = if reference.remote {
            2
        } else if Some(reference.name.as_str()) == current {
            0
        } else {
            1
        };
        tags.entry(reference.sha.as_str())
            .or_default()
            .push(GitRef {
                label: reference.name.clone().into(),
                kind,
            });
    }
    for list in tags.values_mut() {
        list.sort_by_key(|tag| tag.kind);
    }
    let mut rows = Vec::with_capacity(layout.len());
    let mut shas = Vec::with_capacity(layout.len());
    let mut entries = data.entries.iter();
    for (at, row) in layout.iter().enumerate() {
        let lines: Vec<GitLine> = git_graph::commands(row, lane_x, ROW_HEIGHT)
            .into_iter()
            .map(|(color, commands)| GitLine {
                commands: commands.into(),
                color: lane_color(color),
            })
            .collect();
        let node_x = lane_x(git_graph::drawn_lane(row.lane));
        let node_color = lane_color(row.color);
        if wip && at == 0 {
            let count = data.status.staged.len() + data.status.changes.len();
            rows.push(GitCommitRow {
                message: "// WIP".into(),
                extra: say!("{count}ファイル", "{count} files").into(),
                lines: ModelRc::new(VecModel::from(lines)),
                node_x,
                node_color,
                wip: true,
                in_head: true,
                ..Default::default()
            });
            shas.push(None);
            continue;
        }
        let Some(entry) = entries.next() else {
            break;
        };
        let mark = if data.incoming.contains(&entry.sha) {
            "↓"
        } else if data.outgoing.contains(&entry.sha) {
            "↑"
        } else {
            ""
        };
        let refs = tags.get(entry.sha.as_str()).cloned().unwrap_or_default();
        rows.push(GitCommitRow {
            sha: entry.sha.chars().take(7).collect::<String>().into(),
            message: entry.subject.clone().into(),
            extra: SharedString::new(),
            author: entry.author.clone().into(),
            date: entry.date.clone().into(),
            mark: mark.into(),
            refs: ModelRc::new(VecModel::from(refs)),
            lines: ModelRc::new(VecModel::from(lines)),
            node_x,
            node_color,
            head: data.head.as_deref() == Some(entry.sha.as_str()),
            wip: false,
            in_head: in_head.contains(entry.sha.as_str()),
        });
        shas.push(Some(entry.sha.clone()));
    }
    (rows, shas)
}

/// 左の列：LOCAL・REMOTE・STASHES。行の高さはどれも同じ（ドラッグの落とし先を高さで割り出す）。
fn side_rows(data: &Data) -> (Vec<GitBranchRow>, Vec<Side>) {
    let mut rows = Vec::new();
    let mut side = Vec::new();
    let current = data.status.branch.as_deref();
    let locals: Vec<&Ref> = data.refs.iter().filter(|r| !r.remote).collect();
    rows.push(GitBranchRow {
        kind: 0,
        label: format!("LOCAL ({})", locals.len()).into(),
        ..Default::default()
    });
    side.push(Side::Header);
    for reference in locals {
        let mut counts = Vec::new();
        if reference.ahead > 0 {
            counts.push(format!("↑{}", reference.ahead));
        }
        if reference.behind > 0 {
            counts.push(format!("↓{}", reference.behind));
        }
        rows.push(GitBranchRow {
            kind: 1,
            label: reference.name.clone().into(),
            detail: counts.join(" ").into(),
            current: Some(reference.name.as_str()) == current,
        });
        side.push(Side::Local(reference.clone()));
    }
    let remotes: Vec<&Ref> = data.refs.iter().filter(|r| r.remote).collect();
    if !remotes.is_empty() {
        rows.push(GitBranchRow {
            kind: 0,
            label: "REMOTE".into(),
            ..Default::default()
        });
        side.push(Side::Header);
        let mut group = "";
        for reference in remotes {
            let (remote, branch) = reference
                .name
                .split_once('/')
                .unwrap_or(("", &reference.name));
            if remote != group {
                group = remote;
                rows.push(GitBranchRow {
                    kind: 3,
                    label: remote.into(),
                    ..Default::default()
                });
                side.push(Side::Group);
            }
            rows.push(GitBranchRow {
                kind: 2,
                label: branch.into(),
                ..Default::default()
            });
            side.push(Side::Remote(reference.clone()));
        }
    }
    if !data.stashes.is_empty() {
        rows.push(GitBranchRow {
            kind: 0,
            label: format!("STASHES ({})", data.stashes.len()).into(),
            ..Default::default()
        });
        side.push(Side::Header);
        for stash in &data.stashes {
            rows.push(GitBranchRow {
                kind: 4,
                label: stash.message.clone().into(),
                detail: stash.name.clone().into(),
                current: false,
            });
            side.push(Side::Stash(stash.clone()));
        }
    }
    (rows, side)
}

/// 右の列：何も選んでいない（0）、Commit（1）、WIP（2）、差分（3）。
fn publish_detail(window: &AppWindow, repo: &Repo) {
    publish_detail_contents(window, repo);
    refresh_action_bindings(window);
}

fn refresh_action_bindings(window: &AppWindow) {
    window.set_git_repo_action_generation(window.get_git_repo_action_generation().wrapping_add(1));
}

fn publish_detail_contents(window: &AppWindow, repo: &Repo) {
    let patch = match repo.diff.as_deref() {
        Some(DiffShown::Wip(preview)) => preview.patch.as_deref(),
        _ => None,
    };
    window.set_git_repo_hunks_available(patch.is_some());
    window.set_git_repo_hunks_active(repo.block_mode && patch.is_some());
    window.set_git_repo_hunk_selected(if repo.block_mode { repo.block as i32 } else { -1 });
    if let Some(patch) = patch.filter(|_| repo.block_mode) {
        let rows: Vec<_> = patch
            .hunks
            .iter()
            .map(|hunk| GitHunkRow {
                title: hunk.heading.clone().into(),
                lines: ModelRc::new(VecModel::from(
                    hunk.lines
                        .iter()
                        .map(|(kind, text)| GitHunkLine {
                            kind: *kind,
                            text: text.as_str().into(),
                        })
                        .collect::<Vec<_>>(),
                )),
            })
            .collect();
        window.set_git_repo_hunks_staged(patch.staged);
        window.set_git_repo_hunks(ModelRc::new(VecModel::from(rows)));
    } else {
        window.set_git_repo_hunks(ModelRc::default());
    }
    if let (Some(diff), Some(root)) = (&repo.diff, &repo.root) {
        if !window.get_git_repo_hunks_active() {
            publish_diff(window, root, diff.as_ref());
        }
        return;
    }
    if let Some(range) = &repo.range {
        window.set_git_repo_range_left(range.left.label(true).into());
        window.set_git_repo_range_right(range.right.label(true).into());
        window.set_git_repo_detail_files(file_rows(&range.files));
        window.set_git_repo_detail_mode(1);
        return;
    }
    window.set_git_repo_range_left("".into());
    window.set_git_repo_range_right("".into());
    match (&repo.selected, &repo.detail) {
        (Some(Selected::Wip), _) => window.set_git_repo_detail_mode(2),
        (Some(Selected::Commit(_)), Some(detail)) => {
            window.set_git_repo_detail_mode(1);
            window.set_git_repo_detail_sha(detail.sha.clone().into());
            window.set_git_repo_detail_message(detail.message.clone().into());
            let author = if detail.email.is_empty() {
                detail.author.clone()
            } else {
                format!("{} <{}>", detail.author, detail.email)
            };
            window.set_git_repo_detail_author(author.into());
            window.set_git_repo_detail_date(detail.date.clone().into());
            let parents: Vec<String> = detail
                .parents
                .iter()
                .map(|p| p.chars().take(7).collect())
                .collect();
            let parents = if parents.is_empty() {
                pick("親：なし（最初のCommit）", "Parent: none (first commit)").to_owned()
            } else {
                format!("{}: {}", pick("親", "Parent"), parents.join(", "))
            };
            window.set_git_repo_detail_parents(parents.into());
            window.set_git_repo_detail_files(file_rows(&detail.files));
        }
        _ => window.set_git_repo_detail_mode(0),
    }
}

fn file_rows(files: &[Change]) -> ModelRc<GitRow> {
    let rows: Vec<_> = files
        .iter()
        .map(|change| {
            let (folder, name) = change.path.rsplit_once('/').unwrap_or(("", &change.path));
            let tip = match &change.from {
                Some(from) => format!("{from} → {}", change.path),
                None => change.path.clone(),
            };
            GitRow {
                kind: 4,
                label: name.into(),
                folder: folder.into(),
                letter: change.kind.letter().into(),
                tip: tip.into(),
                open: false,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// グラフの行を選ぶ。Commit なら詳細を読む。
fn select_row(window: &AppWindow, index: usize) {
    let target = REPO.with(|repo| {
        let repo = repo.borrow();
        Some((repo.root.clone()?, repo.rows.get(index)?.clone()))
    });
    let Some((root, sha)) = target else {
        return;
    };
    git_ui::cancel_change_diff();
    let (selected, detail) = match sha {
        None => (Selected::Wip, None),
        Some(sha) => match git::detail(&root, &sha) {
            Ok(detail) => (Selected::Commit(sha), Some(detail)),
            Err(error) => {
                window.tell(error.to_string().into());
                return;
            }
        },
    };
    select_detail(window, selected, detail, index as i32, false);
}

fn select_detail(
    window: &AppWindow,
    selected: Selected,
    detail: Option<git::Detail>,
    index: i32,
    off_graph: bool,
) {
    git_ui::cancel_change_diff();
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        repo.selected = Some(selected);
        repo.detail = detail;
        repo.diff = None;
        repo.block_mode = false;
        repo.lookup = None;
        repo.range = None;
        repo.off_graph = off_graph;
    });
    window.set_git_repo_inspection_note("".into());
    window.set_git_repo_selected(index);
    REPO.with(|repo| publish_detail(window, &repo.borrow()));
}

fn choose_point(window: &AppWindow, live: &Live, point: ComparePoint, as_base: bool) {
    if as_base {
        window.set_git_repo_compare_base(point.label(false).into());
        window.set_git_repo_inspection_note("".into());
        REPO.with(|repo| {
            let mut repo = repo.borrow_mut();
            repo.base = Some(point);
            repo.lookup = None;
        });
        refresh_action_bindings(window);
        return;
    }
    let left = REPO.with(|repo| repo.borrow().base.clone());
    if let Some(left) = left {
        start_lookup(
            window,
            live,
            pick("二点の差分を読み込んでいます…", "Reading the comparison…"),
            move |root| {
                let files = git::compare_commits(root, &left.sha, &point.sha)?;
                Ok(Lookup::Range(RangeComparison {
                    left,
                    right: point,
                    files,
                }))
            },
        );
    }
}

fn entry_of(sha: &str) -> Option<LogEntry> {
    REPO.with(|repo| {
        let repo = repo.borrow();
        repo.data
            .as_ref()?
            .entries
            .iter()
            .find(|e| e.sha == sha)
            .cloned()
    })
}

fn commit_at(index: usize) -> Option<String> {
    REPO.with(|repo| repo.borrow().rows.get(index).cloned().flatten())
}

/// Commit の右クリック：0 New Branch…、1 Revert、2 Cherry-pick、3 Reset Keep、4 Reset Delete。
fn commit_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    let (Some(root), Some(sha)) = (root(), commit_at(index)) else {
        return;
    };
    let merge = entry_of(&sha).is_some_and(|e| e.parents.len() > 1);
    let action = match action {
        0 => {
            git_ui::ask_branch_name(window, live, root, Some(sha));
            return;
        }
        1 => Action::Revert { sha, merge },
        2 => Action::CherryPick { sha, merge },
        3 => Action::Reset { sha, hard: false },
        4 => Action::Reset { sha, hard: true },
        _ => return,
    };
    git_ui::act_on(window, live, root, action);
}

fn show_more(window: &AppWindow, live: &Live) {
    REPO.with(|repo| repo.borrow_mut().count += PAGE);
    refresh(window, live);
}

enum Picked {
    Local(Ref),
    Remote(Ref),
    Stash(Stash),
}

struct BranchMenu {
    root: PathBuf,
    picked: Picked,
    target: crate::menu_commands::Target,
}

fn capture_branch_menu(window: &AppWindow, live: &Live, index: usize) {
    let menu = root().zip(side_at(index)).map(|(root, picked)| {
        let mut target = crate::menu_commands::Target::for_surface(window, live);
        if let Picked::Local(branch) | Picked::Remote(branch) = &picked {
            target.set_git_branch_target(branch);
        }
        BranchMenu { root, picked, target }
    });
    BRANCH_MENU.with(|stored| *stored.borrow_mut() = menu);
    refresh_action_bindings(window);
}

pub(crate) fn branch_target() -> Option<crate::menu_commands::Target> {
    BRANCH_MENU.with(|menu| menu.borrow().as_ref().map(|menu| menu.target.clone()))
}

fn side_at(index: usize) -> Option<Picked> {
    REPO.with(|repo| match repo.borrow().side.get(index)? {
        Side::Local(r) => Some(Picked::Local(r.clone())),
        Side::Remote(r) => Some(Picked::Remote(r.clone())),
        Side::Stash(s) => Some(Picked::Stash(s.clone())),
        Side::Header | Side::Group => None,
    })
}

fn current_branch() -> Option<String> {
    REPO.with(|repo| repo.borrow().data.as_ref()?.status.branch.clone())
}

/// ブランチを押した：グラフでその先端を選び、見える所へ移す。
fn branch_clicked(window: &AppWindow, index: usize) {
    let reference = match side_at(index) {
        Some(Picked::Local(r) | Picked::Remote(r)) => r,
        _ => return,
    };
    let row = REPO.with(|repo| {
        repo.borrow()
            .rows
            .iter()
            .position(|sha| sha.as_deref() == Some(reference.sha.as_str()))
    });
    match row {
        Some(row) => {
            select_row(window, row);
            window.set_git_repo_scroll_row(row as i32);
            window.set_git_repo_scroll_generation(window.get_git_repo_scroll_generation() + 1);
        }
        None => window.tell(
            say!(
                "{}の先端はもっと下にあります。「Show more」で読み足してください",
                "The tip of {} is further down. Press Show more to load it",
                reference.name
            )
            .into(),
        ),
    }
}

/// ブランチをダブルクリック：Checkout。
fn branch_activated(window: &AppWindow, live: &Live, index: usize) {
    let Some(root) = root() else {
        return;
    };
    match side_at(index) {
        Some(Picked::Local(r)) if Some(&r.name) != current_branch().as_ref() => git_ui::act_on(
            window,
            live,
            root,
            Action::Checkout {
                branch: r.name,
                remote: false,
            },
        ),
        Some(Picked::Remote(r)) => git_ui::act_on(
            window,
            live,
            root,
            Action::Checkout {
                branch: r.name,
                remote: true,
            },
        ),
        _ => {}
    }
}

/// 左の列の右クリック。ローカル：0 Checkout、1 New Branch…、2 Merge into、3 Push、4 Delete…。
/// リモート：0 Checkout、1 New Branch…、2 Merge into。Stash：0 Apply、1 Pop、2 Drop…。
fn branch_menu(window: &AppWindow, live: &Live, action: i32) {
    let Some(menu) = BRANCH_MENU.with(|stored| stored.borrow_mut().take()) else {
        return;
    };
    if !window.get_git_repo_active() || root().as_ref() != Some(&menu.root) {
        return;
    }
    let BranchMenu { root, picked, .. } = menu;
    let current = current_branch();
    let action = match (picked, action) {
        (Picked::Local(r), 0) if Some(&r.name) != current.as_ref() => Action::Checkout {
            branch: r.name,
            remote: false,
        },
        (Picked::Remote(r), 0) => Action::Checkout {
            branch: r.name,
            remote: true,
        },
        (Picked::Local(r) | Picked::Remote(r), 1) => {
            later(window, live, move |window, live| {
                git_ui::ask_branch_name(window, live, root, Some(r.sha));
            });
            return;
        }
        (Picked::Local(r), 2) if Some(&r.name) != current.as_ref() => Action::Merge {
            branch: r.name,
            into: None,
            ask: false,
        },
        (Picked::Remote(r), 2) => Action::Merge {
            branch: r.name,
            into: None,
            ask: false,
        },
        (Picked::Local(r), 3) => Action::PushBranch(r.name),
        (Picked::Local(r), 4) if Some(&r.name) != current.as_ref() => Action::DeleteBranch {
            name: r.name,
            force: false,
        },
        (Picked::Stash(s), 0 | 1) => Action::StashApply {
            name: s.name,
            pop: action == 1,
        },
        (Picked::Stash(s), 2) => Action::StashDrop(s.name),
        _ => return,
    };
    later(window, live, move |window, live| git_ui::act_on(window, live, root, action));
}

pub(crate) fn take_branch_target() -> Option<crate::menu_commands::Target> {
    BRANCH_MENU.with(|menu| menu.borrow_mut().take().map(|menu| menu.target))
}

/// ブランチを別のローカルブランチへ落とした：確認してから Merge（書き手の求め 2026-10-03）。
fn branch_dropped(window: &AppWindow, live: &Live, from: usize, to: usize) {
    let Some(root) = root() else {
        return;
    };
    let branch = match side_at(from) {
        Some(Picked::Local(r) | Picked::Remote(r)) => r.name,
        _ => return,
    };
    let Some(Picked::Local(target)) = side_at(to) else {
        return;
    };
    if target.name == branch {
        return;
    }
    let into = (Some(&target.name) != current_branch().as_ref()).then_some(target.name);
    git_ui::act_on(
        window,
        live,
        root,
        Action::Merge {
            branch,
            into,
            ask: true,
        },
    );
}

fn read_text(bytes: Option<Vec<u8>>) -> String {
    bytes
        .and_then(|bytes| {
            crate::file_io::decode(&bytes, MAX_DOCUMENT_CHARACTERS)
                .ok()
                .map(|(text, _)| text)
        })
        .unwrap_or_default()
}

/// 差分の段の幅（px）＝右の列の幅から、行番号・印・余白を引いたもの。右の列は窓の幅に
/// 合わせて変わるので、画面が幅を知らせてくる（`diff_width_changed`）。字の幅は目安で、
/// 少し狭めに数えて段からはみ出さないようにする。
const DIFF_CHROME: f32 = 126.0;

thread_local! {
    static DIFF_WIDTH: std::cell::Cell<f32> = const { std::cell::Cell::new(530.0) };
}

/// 右の列の幅が変わった（差分を出しているとき）。段を折り直す。
fn diff_width_changed(window: &AppWindow, panel: f32) {
    let width = (panel - DIFF_CHROME).max(120.0);
    let old = DIFF_WIDTH.with(|held| held.replace(width));
    if (old - width).abs() >= 1.0 {
        REPO.with(|repo| publish_detail(window, &repo.borrow()));
    }
}

fn char_width(c: char) -> f32 {
    if c.is_ascii() || ('\u{ff61}'..='\u{ff9f}').contains(&c) {
        7.6
    } else {
        13.0
    }
}

/// 1列の差分を、右の列の段に折る。行番号は1段目だけに出す。
fn diff_rows(unified: &inline_diff::Unified) -> Vec<GitDiffRow> {
    let limit = DIFF_WIDTH.with(std::cell::Cell::get);
    let mut rows = Vec::new();
    for line in &unified.lines {
        let kind = match line.kind {
            inline_diff::Kind::Context => 0,
            inline_diff::Kind::Removed => 1,
            inline_diff::Kind::Added => 2,
            inline_diff::Kind::Gap => 3,
        };
        let number = |n: Option<usize>| n.map(|n| n.to_string()).unwrap_or_default();
        let mut parts: Vec<GitDiffPart> = Vec::new();
        let mut width = 0.0;
        let mut first = true;
        let mut flush = |parts: &mut Vec<GitDiffPart>, first: &mut bool| {
            if parts.is_empty() {
                parts.push(GitDiffPart::default());
            }
            rows.push(GitDiffRow {
                kind,
                old: if *first {
                    number(line.old)
                } else {
                    String::new()
                }
                .into(),
                new: if *first {
                    number(line.new)
                } else {
                    String::new()
                }
                .into(),
                parts: ModelRc::new(VecModel::from(std::mem::take(parts))),
            });
            *first = false;
        };
        for (text, mark) in &line.parts {
            for c in text.chars() {
                let c = if c == '\t' { ' ' } else { c };
                let w = char_width(c);
                if width + w > limit && width > 0.0 {
                    flush(&mut parts, &mut first);
                    width = 0.0;
                }
                width += w;
                match parts.last_mut() {
                    Some(part) if part.mark == *mark => {
                        let mut held = part.text.to_string();
                        held.push(c);
                        part.text = held.into();
                    }
                    _ => parts.push(GitDiffPart {
                        text: c.to_string().into(),
                        mark: *mark,
                    }),
                }
            }
        }
        flush(&mut parts, &mut first);
    }
    rows
}

/// 差分の左右の文と、見出しに出す文。
fn commit_diff_texts(root: &Path, diff: &CommitDiff) -> Result<(String, String, String), GitError> {
    let disk = |path: &str| read_text(std::fs::read(root.join(path)).ok());
    let blob = |sha: &str, path: &str| git::blob_at(root, sha, path).map(read_text);
    let CommitDiff {
        sha,
        parent,
        change,
        working,
    } = diff;
    let short: String = sha.chars().take(7).collect();
    if *working {
        let caption = say!(
            "Commit {short}（左）と今のファイル（右）",
            "Commit {short} (left) and the working copy (right)"
        );
        return Ok((blob(sha, &change.path)?, disk(&change.path), caption));
    }
    let old_path = change.from.as_deref().unwrap_or(&change.path);
    let before = match parent {
        Some(parent) => blob(parent, old_path)?,
        None => String::new(),
    };
    let parent_short: String = parent
        .as_deref()
        .map(|p| p.chars().take(7).collect())
        .unwrap_or_else(|| "—".to_owned());
    let caption = say!(
        "親Commit {parent_short}（左）と {short}（右）",
        "Parent {parent_short} (left) and {short} (right)"
    );
    Ok((before, blob(sha, &change.path)?, caption))
}

fn publish_diff(window: &AppWindow, root: &Path, diff: &DiffShown) {
    window.set_git_repo_diff_switchable(matches!(diff, DiffShown::Commit(_)));
    match diff {
        DiffShown::Commit(commit) => {
            window.set_git_repo_diff_working(commit.working);
            let name = &commit.change.path;
            match commit_diff_texts(root, commit) {
                Ok((left, right, caption)) => {
                    publish_diff_rows(window, name, &caption, Ok((&left, &right)));
                }
                Err(error) => publish_diff_rows(window, name, "", Err(&error.to_string())),
            }
        }
        DiffShown::Wip(preview) | DiffShown::Range(preview) => {
            window.set_git_repo_diff_working(false);
            let text = preview
                .text
                .as_ref()
                .map(|(left, right)| (left.as_str(), right.as_str()));
            publish_diff_rows(
                window,
                &preview.name,
                &preview.caption,
                text.map_err(String::as_str),
            );
        }
    }
}

fn publish_diff_rows(
    window: &AppWindow,
    name: &str,
    caption: &str,
    text: Result<(&str, &str), &str>,
) {
    let (rows, removed, added, note) = match text {
        Ok((left, right)) => {
            let unified = inline_diff::unified(left, right, 3);
            (diff_rows(&unified), unified.removed, unified.added, "")
        }
        Err(error) => (Vec::new(), 0, 0, error),
    };
    window.set_git_repo_diff_name(name.into());
    window.set_git_repo_diff_caption(caption.into());
    window.set_git_repo_diff_note(note.into());
    window.set_git_repo_diff_removed(removed as i32);
    window.set_git_repo_diff_added(added as i32);
    window.set_git_repo_diff_rows(ModelRc::new(VecModel::from(rows)));
    window.set_git_repo_detail_mode(3);
}

/// 右の列に差分を出す（Commit の詳細のファイル）。
fn show_diff(window: &AppWindow, index: usize, working: bool) {
    let source = REPO.with(|repo| {
        let repo = repo.borrow();
        let detail = repo.detail.as_ref()?;
        Some(CommitDiff {
            sha: detail.sha.clone(),
            parent: detail.parents.first().cloned(),
            change: detail.files.get(index)?.clone(),
            working,
        })
    });
    if let Some(source) = source {
        set_diff(window, DiffShown::Commit(source));
    }
}

fn open_file_diff(window: &AppWindow, live: &Live, index: usize) {
    let range_file = REPO.with(|repo| {
        let repo = repo.borrow();
        let range = repo.range.as_ref()?;
        Some((
            range.left.sha.clone(),
            range.right.sha.clone(),
            range.files.get(index)?.clone(),
        ))
    });
    if let Some((left, right, change)) = range_file {
        start_lookup(
            window,
            live,
            pick(
                "ファイルの差分を読み込んでいます…",
                "Reading the file diff…",
            ),
            move |root| {
                Ok(Lookup::File(crate::git_change_diff::read_commits(
                    root, &left, &right, &change,
                )))
            },
        );
    } else {
        show_diff(window, index, false);
    }
}

fn set_diff(window: &AppWindow, diff: DiffShown) {
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        repo.diff = Some(Rc::new(diff));
        repo.block_mode = false;
        publish_detail(window, &repo);
    });
}

/// 「‹ Files」：差分を閉じて、詳細（または WIP）へ戻る。
fn diff_back(window: &AppWindow) {
    git_ui::cancel_change_diff();
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if repo.block_mode {
            repo.block_mode = false;
        } else {
            repo.diff = None;
        }
        publish_detail(window, &repo);
    });
}

fn open_hunks(window: &AppWindow) {
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if matches!(repo.diff.as_deref(), Some(DiffShown::Wip(preview)) if preview.patch.is_some()) {
            repo.block_mode = true;
            repo.block = 0;
            publish_detail(window, &repo);
        }
    });
}

fn select_block(window: &AppWindow, index: i32) {
    let Ok(block) = usize::try_from(index) else {
        return;
    };
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if repo.block_mode && let Some(DiffShown::Wip(preview)) = repo.diff.as_deref()
            && preview.patch.as_ref().is_some_and(|patch| patch.hunks.get(block).is_some())
        {
            repo.block = block;
            window.set_git_repo_hunk_selected(index);
        }
    });
}

fn apply_block(window: &AppWindow, live: &Live, root: &Path, patch: Arc<crate::git_hunks::Patch>, block: usize) {
    if !git_ui::apply_hunk(window, live, root, patch, block) {
        return;
    }
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        repo.diff = None;
        repo.block_mode = false;
        publish_detail(window, &repo);
    });
}

/// Previous／Working Copy。
fn diff_working_chosen(window: &AppWindow, working: bool) {
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        if let Some(DiffShown::Commit(commit)) = repo.diff.as_deref() {
            let mut commit = commit.clone();
            commit.working = working;
            repo.diff = Some(Rc::new(DiffShown::Commit(commit)));
            publish_detail(window, &repo);
        }
    });
}

/// 「Open in Comparison」：今までの左右の比較の画面で開く（閉じるとここへ戻る）。
fn diff_open_full(window: &AppWindow) {
    REPO.with(|repo| {
        let repo = repo.borrow();
        let (Some(root), Some(diff)) = (&repo.root, &repo.diff) else {
            return;
        };
        match diff.as_ref() {
            DiffShown::Wip(preview) | DiffShown::Range(preview) => preview.open(window),
            DiffShown::Commit(commit) => match commit_diff_texts(root, commit) {
                Ok((left, right, caption)) => {
                    let name = &commit.change.path;
                    crate::diff_view::show(
                        window,
                        format!("{name}（{caption}）"),
                        left,
                        name.clone(),
                        right,
                    );
                }
                Err(error) => window.tell(error.to_string().into()),
            },
        }
    });
}

pub(crate) fn show_wip_diff(window: &AppWindow, root: &Path, preview: Preview) {
    let selected = REPO.with(|repo| {
        let repo = repo.borrow();
        repo.root.as_deref() == Some(root) && repo.selected == Some(Selected::Wip)
    });
    if window.get_git_repo_active() && selected {
        set_diff(window, DiffShown::Wip(preview));
    }
}

fn history(window: &AppWindow, live: &Live, redo: bool) {
    if let Some(root) = root() {
        git_ui::history_step(window, live, root, redo);
    }
}

/// 詳細のファイルの右クリック：0 Compare with Previous、1 Compare with Working Copy、2 Open。
fn file_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    match action {
        0 => show_diff(window, index, false),
        1 => show_diff(window, index, true),
        2 => {
            let target = REPO.with(|repo| {
                let repo = repo.borrow();
                let change = repo.detail.as_ref()?.files.get(index)?;
                Some(repo.root.clone()?.join(&change.path))
            });
            let Some(path) = target else {
                return;
            };
            if !path.is_file() {
                window.tell(
                    say!(
                        "{}は今は無いため開けません",
                        "{} does not exist now, so it cannot be opened",
                        path.display()
                    )
                    .into(),
                );
                return;
            }
            close(window);
            crate::open_path_in_focused_pane(window, live, &path, Opening::Kept);
        }
        _ => {}
    }
}

/// 上の帯の「Branch」：選んでいる Commit（無ければ HEAD）から新しいブランチ。
fn branch_here(window: &AppWindow, live: &Live) {
    let Some(root) = root() else {
        return;
    };
    let at = REPO.with(|repo| {
        let repo = repo.borrow();
        match &repo.selected {
            Some(Selected::Commit(sha)) => Some(sha.clone()),
            _ => repo.data.as_ref()?.head.clone(),
        }
    });
    git_ui::ask_branch_name(window, live, root, at);
}

/// 上の帯の「Pop」：いちばん新しい Stash。
fn pop(window: &AppWindow, live: &Live) {
    let (Some(root), Some(name)) = (
        root(),
        REPO.with(|repo| Some(repo.borrow().data.as_ref()?.stashes.first()?.name.clone())),
    ) else {
        return;
    };
    git_ui::act_on(window, live, root, Action::StashApply { name, pop: true });
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
    crate::git_merge_ui::wire(window);
    crate::git_worktree_ui::wire(window);
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
    on!(on_git_repo_close, |w, _l| close(w));
    on!(on_git_repo_branch_here, |w, l| branch_here(w, l));
    on!(on_git_repo_pop, |w, l| pop(w, l));
    on!(on_git_repo_commit_selected, |w, _l, index| select_row(
        w,
        index.max(0) as usize
    ));
    on!(on_git_repo_commit_menu, |w, l, index, action| commit_menu(
        w,
        l,
        index.max(0) as usize,
        action
    ));
    on!(on_git_repo_show_more, |w, l| show_more(w, l));
    on!(on_git_repo_branch_clicked, |w, _l, index| branch_clicked(
        w,
        index.max(0) as usize
    ));
    on!(
        on_git_repo_branch_activated,
        |w, l, index| branch_activated(w, l, index.max(0) as usize)
    );
    let weak = window.as_weak();
    let held = live.clone();
    window.on_git_repo_capture_branch_menu(move |index| {
        if let Some(window) = weak.upgrade() && let Ok(index) = usize::try_from(index) {
            capture_branch_menu(&window, &held, index);
        }
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_git_repo_branch_menu(move |action| {
        if let Some(window) = weak.upgrade() {
            branch_menu(&window, &held, action);
        }
    });
    on!(on_git_repo_branch_dropped, |w, l, from, to| branch_dropped(
        w,
        l,
        from.max(0) as usize,
        to.max(0) as usize
    ));
    on!(on_git_repo_file_clicked, |w, l, index| open_file_diff(
        w,
        l,
        index.max(0) as usize,
    ));
    on!(on_git_repo_search, |w, l, field, query| search(
        w,
        l,
        field,
        query.as_str()
    ));
    on!(on_git_repo_next_search_page, |w, l| search_more(w, l));
    on!(on_git_repo_search_selected, |w, l, index| select_result(
        w, l, index
    ));
    on!(on_git_repo_undo, |w, l| history(w, l, false));
    on!(on_git_repo_redo, |w, l| history(w, l, true));
    on!(on_git_repo_diff_back, |w, _l| diff_back(w));
    on!(on_git_repo_diff_working_chosen, |w, _l, working| {
        diff_working_chosen(w, working)
    });
    let weak = window.as_weak();
    window.on_git_repo_select_hunk(move |index| {
        if let Some(window) = weak.upgrade() {
            select_block(&window, index);
        }
    });
    on!(on_git_repo_diff_width_changed, |w, _l, width| {
        diff_width_changed(w, width)
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_git_repo_changes_clicked(move |index| {
        if let Some(window) = weak.upgrade() {
            git_ui::diff_clicked(&window, &held, index, true);
        }
    });
    on!(on_git_repo_file_menu, |w, l, index, action| file_menu(
        w,
        l,
        index.max(0) as usize,
        action
    ));
}

#[cfg(test)]
#[path = "git_repo_ui_tests.rs"]
mod tests;
