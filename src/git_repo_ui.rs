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
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use slint::{Color, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::git::{self, GitError, LogEntry, Ref, Stash, Status};
use crate::git_graph;
use crate::git_ui::{self, Action};
use crate::i18n::pick;
use crate::{
    AppWindow, GitBranchRow, GitCommitRow, GitLine, GitRef, GitRow, Live, MAX_DOCUMENT_CHARACTERS,
    Opening, StatusBar, say,
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
    side: Vec<Side>,
    /// グラフの行の Commit（`None`は `// WIP`）。
    rows: Vec<Option<String>>,
}

thread_local! {
    static REPO: RefCell<Repo> = RefCell::new(Repo::default());
    static POLL: Timer = Timer::default();
}

fn root() -> Option<PathBuf> {
    REPO.with(|repo| repo.borrow().root.clone())
}

/// File の「Git Repository…」と、Git Changes のブランチ▾の「Manage Branches」。
pub fn open(window: &AppWindow, live: &Live) {
    let root = match git_ui::chosen_root(window, live) {
        Ok(root) => root,
        Err(text) => {
            window.tell(text.into());
            return;
        }
    };
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
    });
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
    window.set_git_repo_active(false);
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
    POLL.with(Timer::stop);
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
                    Some(Selected::Commit(sha)) => data.entries.iter().any(|e| &e.sha == sha),
                    Some(Selected::Wip) => data.has_changes(),
                    None => true,
                };
                if !keep {
                    repo.selected = None;
                    repo.detail = None;
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
            window.set_git_repo_commits(ModelRc::default());
            window.set_git_repo_branches(ModelRc::default());
            window.set_git_repo_more(false);
            window.set_git_repo_detail_mode(0);
            repo.rows.clear();
            repo.side.clear();
            return;
        };
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

/// 右の列：何も選んでいない（0）、Commit（1）、WIP（2）。
fn publish_detail(window: &AppWindow, repo: &Repo) {
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
            let files: Vec<GitRow> = detail
                .files
                .iter()
                .map(|change| {
                    let (folder, name) = match change.path.rsplit_once('/') {
                        Some((folder, name)) => (folder.to_owned(), name.to_owned()),
                        None => (String::new(), change.path.clone()),
                    };
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
            window.set_git_repo_detail_files(ModelRc::new(VecModel::from(files)));
        }
        _ => window.set_git_repo_detail_mode(0),
    }
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
    REPO.with(|repo| {
        let mut repo = repo.borrow_mut();
        repo.selected = Some(selected);
        repo.detail = detail;
    });
    window.set_git_repo_selected(index as i32);
    REPO.with(|repo| publish_detail(window, &repo.borrow()));
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
fn branch_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    let Some(root) = root() else {
        return;
    };
    let current = current_branch();
    let action = match (side_at(index), action) {
        (Some(Picked::Local(r)), 0) if Some(&r.name) != current.as_ref() => Action::Checkout {
            branch: r.name,
            remote: false,
        },
        (Some(Picked::Remote(r)), 0) => Action::Checkout {
            branch: r.name,
            remote: true,
        },
        (Some(Picked::Local(r) | Picked::Remote(r)), 1) => {
            git_ui::ask_branch_name(window, live, root, Some(r.sha));
            return;
        }
        (Some(Picked::Local(r)), 2) if Some(&r.name) != current.as_ref() => Action::Merge {
            branch: r.name,
            into: None,
            ask: false,
        },
        (Some(Picked::Remote(r)), 2) => Action::Merge {
            branch: r.name,
            into: None,
            ask: false,
        },
        (Some(Picked::Local(r)), 3) => Action::PushBranch(r.name),
        (Some(Picked::Local(r)), 4) if Some(&r.name) != current.as_ref() => Action::DeleteBranch {
            name: r.name,
            force: false,
        },
        (Some(Picked::Stash(s)), 0 | 1) => Action::StashApply {
            name: s.name,
            pop: action == 1,
        },
        (Some(Picked::Stash(s)), 2) => Action::StashDrop(s.name),
        _ => return,
    };
    git_ui::act_on(window, live, root, action);
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

/// 詳細のファイルを比べる。`working`なら今のファイルと、そうでなければ親 Commit と。
fn compare_file(window: &AppWindow, index: usize, working: bool) {
    let target = REPO.with(|repo| {
        let repo = repo.borrow();
        let detail = repo.detail.as_ref()?;
        Some((
            repo.root.clone()?,
            detail.sha.clone(),
            detail.parents.first().cloned(),
            detail.files.get(index)?.clone(),
        ))
    });
    let Some((root, sha, parent, change)) = target else {
        return;
    };
    let short: String = sha.chars().take(7).collect();
    let at_commit = match git::blob_at(&root, &sha, &change.path) {
        Ok(bytes) => read_text(bytes),
        Err(error) => {
            window.tell(error.to_string().into());
            return;
        }
    };
    if working {
        let path = root.join(&change.path);
        let now = read_text(std::fs::read(&path).ok());
        crate::diff_view::show(
            window,
            format!("{} @ {short}", change.path),
            at_commit,
            say!("{}（今のファイル）", "{} (working copy)", change.path),
            now,
        );
        return;
    }
    let before = match &parent {
        Some(parent) => {
            let old = change.from.as_deref().unwrap_or(&change.path);
            match git::blob_at(&root, parent, old) {
                Ok(bytes) => read_text(bytes),
                Err(error) => {
                    window.tell(error.to_string().into());
                    return;
                }
            }
        }
        None => String::new(),
    };
    let parent_short: String = parent
        .as_deref()
        .map(|p| p.chars().take(7).collect())
        .unwrap_or_else(|| "—".to_owned());
    let old_name = change.from.clone().unwrap_or_else(|| change.path.clone());
    crate::diff_view::show(
        window,
        format!("{old_name} @ {parent_short}"),
        before,
        format!("{} @ {short}", change.path),
        at_commit,
    );
}

/// 詳細のファイルの右クリック：0 Compare with Previous、1 Compare with Working Copy、2 Open。
fn file_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    match action {
        0 => compare_file(window, index, false),
        1 => compare_file(window, index, true),
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
    on!(on_git_repository_requested, |w, l| open(w, l));
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
    on!(on_git_repo_branch_menu, |w, l, index, action| branch_menu(
        w,
        l,
        index.max(0) as usize,
        action
    ));
    on!(on_git_repo_branch_dropped, |w, l, from, to| branch_dropped(
        w,
        l,
        from.max(0) as usize,
        to.max(0) as usize
    ));
    on!(on_git_repo_file_clicked, |w, _l, index| compare_file(
        w,
        index.max(0) as usize,
        false
    ));
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
