//! RFN01-67（書き手と決めた 2026-10-03）: Gitを呼ぶ1か所。
//!
//! **書き手のPCに入っている`git`を呼ぶ**（`git_version`と同じ方針）。ライブラリを持たない
//! のは、書き手がコマンドで使っているGitと設定（`safe.directory`、資格情報、フック）が
//! 食い違わないため。ここは画面を知らない——Git Changesの面（`git_ui`）が裏のスレッドから呼ぶ。
//!
//! - **コンソールの窓を出さない**（`CREATE_NO_WINDOW`）。端末で訊く問い（`GIT_TERMINAL_PROMPT`）
//!   も止める。資格情報はGit Credential Managerが自分の窓で訊く（書き手の判断 2026-10-03）。
//! - **出力は英語で読む**（`LC_ALL=C`）。衝突や`safe.directory`の断りを文で見分けるため。
//!   ファイル名は`-z`と`core.quotepath=false`で生のUTF-8のまま受け取る。
//! - **衝突したら中止して元に戻す**（書き手の判断 2026-10-03）。衝突を解く画面は持たない。
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `git`が無い。
    Missing,
    /// フォルダの持ち主が違うので、Gitが読むのを断った（`safe.directory`）。
    Untrusted,
    /// 書き手が止めた。
    Cancelled,
    /// 衝突したので中止し、元に戻した。
    Conflict,
    /// Gitが断った。中身はGitの文（資格情報を伏せたもの）か、こちらの文。
    Failed(String),
    /// ブランチがどこにも Merge されていないので、`-d`では消せない（`-D`なら消せる）。
    NotMerged(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Missing => f.write_str(crate::i18n::pick(
                "Gitが見つかりません（インストールされていないか、PATHにありません）",
                "Git was not found (it is not installed, or not on PATH)",
            )),
            GitError::Untrusted => f.write_str(crate::i18n::pick(
                "フォルダの持ち主が違うため、Gitが読むのを断りました（safe.directoryへの追加が必要です）",
                "Git refused to read a folder owned by someone else (add it to safe.directory)",
            )),
            GitError::Cancelled => f.write_str(crate::i18n::pick("止めました", "Cancelled")),
            GitError::Conflict => f.write_str(crate::i18n::pick(
                "衝突したため中止し、元に戻しました。衝突はTerminalで解いてください",
                "There were conflicts, so it was stopped and undone. Resolve them in a Terminal",
            )),
            GitError::Failed(said) => f.write_str(said),
            GitError::NotMerged(name) => f.write_str(&crate::say!(
                "{name}はまだMergeされていません",
                "{name} has not been merged"
            )),
        }
    }
}

/// 裏で走っている操作を止める合図。
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct Output {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// 断りを、書き手へ見せる文にする。
    fn failure(&self) -> GitError {
        failure_of(&self.stderr)
    }
}

/// `git -C folder …`を、この編集器の決まりで組む。`git_version`もこれを使う。
pub fn command(folder: &Path) -> Command {
    let mut command = Command::new(program());
    command
        .arg("-C")
        .arg(folder)
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        // 5秒ごとの読み直しが、書き手がTerminalで打つgitと`index.lock`を取り合わない。
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // コンソールの窓を一瞬でも出さない。
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(not(test))]
fn program() -> &'static str {
    "git"
}

/// 試験で**Gitの無いPC**を作るため、呼ぶ名前を差し替えられる。
#[cfg(test)]
fn program() -> String {
    tests::PROGRAM.with(|name| name.borrow().clone())
}

fn run(folder: &Path, arguments: &[&str], cancel: Option<&Cancel>) -> Result<Output, GitError> {
    let mut child = command(folder)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => GitError::Missing,
            _ => GitError::Failed(error.to_string()),
        })?;
    // 出力は別のスレッドで読み切る。待つ側が読まないと、管が詰まってGitが止まる。
    let reader = |source: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut source) = source {
                let _ = source.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let status = loop {
        if cancel.is_some_and(Cancel::cancelled) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(GitError::Failed(error.to_string())),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();
    Ok(Output {
        ok: status.success(),
        stdout,
        stderr,
    })
}

/// 通れば標準出力、断られたら断りの文。
fn checked(folder: &Path, arguments: &[&str], cancel: Option<&Cancel>) -> Result<String, GitError> {
    let output = run(folder, arguments, cancel)?;
    if output.ok {
        Ok(output.text())
    } else {
        Err(output.failure())
    }
}

/// Gitの断りの文から、見せる行を選ぶ。`hint:`は落とし、URLに入った資格情報は伏せる。
fn failure_of(stderr: &str) -> GitError {
    if stderr.contains("safe.directory") {
        return GitError::Untrusted;
    }
    let lines: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .map(|line| {
            let line = line
                .strip_prefix("fatal: ")
                .or_else(|| line.strip_prefix("error: "))
                .unwrap_or(line);
            hide_credentials(line)
        })
        .take(4)
        .collect();
    if lines.is_empty() {
        GitError::Failed(crate::i18n::pick("Gitが失敗しました", "Git failed").to_owned())
    } else {
        GitError::Failed(lines.join("\n"))
    }
}

/// `https://name:token@host/…`の`name:token`を伏せる。
fn hide_credentials(line: &str) -> String {
    let mut shown = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        shown.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '/' || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(sign) => {
                shown.push_str("***");
                shown.push_str(&tail[sign..end]);
            }
            None => shown.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    shown.push_str(rest);
    shown
}

/// `folder`を含むリポジトリの根。管理下になければ`None`。
pub fn repository_root(folder: &Path) -> Result<Option<PathBuf>, GitError> {
    let output = run(folder, &["rev-parse", "--show-toplevel"], None)?;
    if output.ok {
        let root = output.text().trim().to_owned();
        return Ok(Some(PathBuf::from(root.replace('/', "\\"))));
    }
    if output.stderr.contains("safe.directory") {
        return Err(GitError::Untrusted);
    }
    Ok(None)
}

/// 管理下にないフォルダで`git init`。
pub fn init(folder: &Path) -> Result<(), GitError> {
    checked(folder, &["init", "-q"], None).map(drop)
}

/// 変更の種類。字はVisual Studioの一覧と同じ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Modified,
    Added,
    Deleted,
    Renamed,
    /// まだGitが知らないファイル。一覧では`A`と見せる（Visual Studioと同じ）。
    Untracked,
    /// 衝突が解かれずに残っている（Terminalで始めたMergeなど）。
    Conflicted,
}

impl Kind {
    pub fn letter(self) -> &'static str {
        match self {
            Kind::Modified => "M",
            Kind::Added | Kind::Untracked => "A",
            Kind::Deleted => "D",
            Kind::Renamed => "R",
            Kind::Conflicted => "U",
        }
    }

    fn of(code: u8) -> Option<Kind> {
        match code {
            b'.' => None,
            b'A' => Some(Kind::Added),
            b'D' => Some(Kind::Deleted),
            b'R' | b'C' => Some(Kind::Renamed),
            // M（中身）とT（種類）は、書き手から見ればどちらも「変わった」。
            _ => Some(Kind::Modified),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// リポジトリの根からの道。区切りは`/`（Gitのまま）。
    pub path: String,
    pub kind: Kind,
    /// 名前を変えたときの元の道。
    pub from: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// 今のブランチ。`None`はブランチの外（detached）。
    pub branch: Option<String>,
    /// まだ1度もCommitしていない。
    pub unborn: bool,
    pub upstream: Option<String>,
    /// 送っていない数（↑）と、取り込んでいない数（↓）。
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<Change>,
    pub changes: Vec<Change>,
}

impl Status {
    /// 追跡しているファイルに、Commitしていない変更があるか（新しいファイルは数えない）。
    pub fn has_tracked_changes(&self) -> bool {
        !self.staged.is_empty() || self.changes.iter().any(|c| c.kind != Kind::Untracked)
    }
}

/// `git status --porcelain=v2 -z --branch`を読む。
fn parse_status(bytes: &[u8]) -> Status {
    let mut status = Status::default();
    let mut fields = bytes
        .split(|b| *b == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    while let Some(field) = fields.next() {
        if let Some(header) = field.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => status.unborn = value == "(initial)",
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_owned()),
                "branch.upstream" => status.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let mut parts = field.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match tag {
            // 1 XY sub mH mI mW hH hI path
            "1" => {
                let items: Vec<&str> = rest.splitn(8, ' ').collect();
                if let (Some(xy), Some(path)) = (items.first(), items.get(7)) {
                    push_pair(&mut status, xy.as_bytes(), path, None);
                }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            "2" => {
                let items: Vec<&str> = rest.splitn(9, ' ').collect();
                let from = fields.next();
                if let (Some(xy), Some(path)) = (items.first(), items.get(8)) {
                    push_pair(&mut status, xy.as_bytes(), path, from);
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => {
                if let Some(path) = rest.splitn(10, ' ').nth(9) {
                    status.changes.push(Change {
                        path: path.to_owned(),
                        kind: Kind::Conflicted,
                        from: None,
                    });
                }
            }
            "?" => status.changes.push(Change {
                path: rest.to_owned(),
                kind: Kind::Untracked,
                from: None,
            }),
            _ => {}
        }
    }
    status
}

fn push_pair(status: &mut Status, xy: &[u8], path: &str, from: Option<String>) {
    let (Some(&x), Some(&y)) = (xy.first(), xy.get(1)) else {
        return;
    };
    if let Some(kind) = Kind::of(x) {
        status.staged.push(Change {
            path: path.to_owned(),
            kind,
            from: from.clone(),
        });
    }
    if let Some(kind) = Kind::of(y) {
        // 作業ツリー側では名前の変更は起きない（Gitは索引でだけ対にする）。
        status.changes.push(Change {
            path: path.to_owned(),
            kind,
            from: None,
        });
    }
}

pub fn status(root: &Path) -> Result<Status, GitError> {
    let output = run(
        root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
        None,
    )?;
    if !output.ok {
        return Err(output.failure());
    }
    Ok(parse_status(&output.stdout))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// `stash@{0}`。
    pub name: String,
    pub message: String,
}

pub fn stashes(root: &Path) -> Result<Vec<Stash>, GitError> {
    let text = checked(root, &["stash", "list", "--format=%gd%x1f%s%x1e"], None)?;
    Ok(text
        .split('\x1e')
        .filter_map(|entry| {
            let (name, message) = entry.trim_start_matches('\n').split_once('\x1f')?;
            Some(Stash {
                name: name.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect())
}

/// ローカルのブランチ（名前の順）。
pub fn branches(root: &Path) -> Result<Vec<String>, GitError> {
    let text = checked(
        root,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        None,
    )?;
    Ok(text.lines().map(str::to_owned).collect())
}

/// 道を、Gitの模様（`*`や`?`）として読ませない。
fn literal(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| format!(":(literal){path}"))
        .collect()
}

fn with_paths<'a>(head: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut all: Vec<&str> = head.to_vec();
    all.push("--");
    all.extend(paths.iter().map(String::as_str));
    all
}

/// Stage（`git add -A`）。`paths`が空ならすべて。
pub fn stage(root: &Path, paths: &[String]) -> Result<(), GitError> {
    if paths.is_empty() {
        return checked(root, &["add", "-A"], None).map(drop);
    }
    let paths = literal(paths);
    checked(root, &with_paths(&["add", "-A"], &paths), None).map(drop)
}

/// Unstage。`paths`が空ならすべて。まだCommitが無ければ索引から外すだけ。
pub fn unstage(root: &Path, paths: &[String], unborn: bool) -> Result<(), GitError> {
    let paths = if paths.is_empty() {
        vec![":/".to_owned()]
    } else {
        literal(paths)
    };
    let head: &[&str] = if unborn {
        &["rm", "-r", "-q", "--cached"]
    } else {
        &["reset", "-q"]
    };
    checked(root, &with_paths(head, &paths), None).map(drop)
}

/// Undo Changes。**ファイルの中身を捨てる**ので、呼ぶ前に訊いておく。
///
/// - Changes の行：作業ツリーを索引（Stageした内容）へ戻す。新しいファイルは消す。
/// - Staged Changes の行：索引と作業ツリーを前回のCommitへ戻す。新しく足したファイルは消す。
pub fn undo(root: &Path, changes: &[Change], staged: bool) -> Result<(), GitError> {
    let mut restore = Vec::new();
    let mut remove = Vec::new();
    let mut clean = Vec::new();
    for change in changes {
        match (staged, change.kind) {
            (false, Kind::Untracked) => clean.push(change.path.clone()),
            (true, Kind::Added) => remove.push(change.path.clone()),
            (true, Kind::Renamed) => {
                remove.push(change.path.clone());
                restore.extend(change.from.clone());
            }
            _ => restore.push(change.path.clone()),
        }
    }
    if !clean.is_empty() {
        let paths = literal(&clean);
        checked(root, &with_paths(&["clean", "-f", "-q"], &paths), None)?;
    }
    if !remove.is_empty() {
        let paths = literal(&remove);
        checked(root, &with_paths(&["rm", "-f", "-q"], &paths), None)?;
    }
    if !restore.is_empty() {
        let paths = literal(&restore);
        let head: &[&str] = if staged {
            &["restore", "--source=HEAD", "--staged", "--worktree"]
        } else {
            &["restore", "--worktree"]
        };
        checked(root, &with_paths(head, &paths), None)?;
    }
    Ok(())
}

/// Commit。`all`は「Commit All」（Stageが空のとき、すべての変更をStageしてから）。
pub fn commit(root: &Path, message: &str, all: bool, amend: bool) -> Result<(), GitError> {
    if all {
        stage(root, &[])?;
    }
    let mut arguments = vec!["commit", "-q", "-m", message];
    if amend {
        arguments.push("--amend");
    }
    checked(root, &arguments, None).map(drop)
}

/// 直前のCommitのメッセージ（Amendの欄に入れる）。
pub fn last_message(root: &Path) -> Result<String, GitError> {
    checked(root, &["log", "-1", "--format=%B"], None).map(|text| text.trim_end().to_owned())
}

/// 直前のCommitが、どこかのリモートのブランチに入っているか（Push済みか）。
pub fn head_is_pushed(root: &Path) -> bool {
    checked(root, &["branch", "-r", "--contains", "HEAD"], None)
        .is_ok_and(|text| !text.trim().is_empty())
}

/// Stash All（新しいファイルも）。`message`が空ならGitの既定の名前。
pub fn stash_all(root: &Path, message: &str) -> Result<(), GitError> {
    let mut arguments = vec!["stash", "push", "-q", "--include-untracked"];
    if !message.trim().is_empty() {
        arguments.extend(["-m", message]);
    }
    checked(root, &arguments, None).map(drop)
}

/// Apply・Pop。**追跡しているファイルに変更があれば断る**——衝突したときに、書き手の変更を
/// 巻き込まずに元へ戻せるのは、手元が前回のCommitと同じときだけだから。
pub fn stash_apply(root: &Path, name: &str, pop: bool) -> Result<(), GitError> {
    if status(root)?.has_tracked_changes() {
        return Err(GitError::Failed(
            crate::i18n::pick(
                "変更があるため取り出せません。先にCommitかStashしてください",
                "There are changes. Commit or stash them before applying a stash",
            )
            .to_owned(),
        ));
    }
    let before = untracked(root)?;
    let output = run(root, &["stash", "apply", "-q", name], None)?;
    if !output.ok {
        if !conflicted(root)? && !output.stderr.contains("CONFLICT") {
            return Err(output.failure());
        }
        // 元に戻す：追跡しているファイルは前回のCommitへ、取り出されて増えたファイルは消す。
        checked(root, &["reset", "-q", "--hard", "HEAD"], None)?;
        let added: Vec<String> = untracked(root)?
            .into_iter()
            .filter(|path| !before.contains(path))
            .collect();
        if !added.is_empty() {
            let paths = literal(&added);
            checked(root, &with_paths(&["clean", "-f", "-q"], &paths), None)?;
        }
        return Err(GitError::Conflict);
    }
    if pop {
        checked(root, &["stash", "drop", "-q", name], None)?;
    }
    Ok(())
}

pub fn stash_drop(root: &Path, name: &str) -> Result<(), GitError> {
    checked(root, &["stash", "drop", "-q", name], None).map(drop)
}

fn untracked(root: &Path) -> Result<Vec<String>, GitError> {
    Ok(status(root)?
        .changes
        .into_iter()
        .filter(|change| change.kind == Kind::Untracked)
        .map(|change| change.path)
        .collect())
}

fn conflicted(root: &Path) -> Result<bool, GitError> {
    let text = checked(root, &["diff", "--name-only", "--diff-filter=U"], None)?;
    Ok(!text.trim().is_empty())
}

pub fn switch(root: &Path, branch: &str) -> Result<(), GitError> {
    checked(root, &["switch", "-q", branch], None).map(drop)
}

/// 新しいブランチを作って移る。名前はGitの決まりで確かめる。
pub fn create_branch(root: &Path, name: &str) -> Result<(), GitError> {
    let name = valid_branch_name(root, name)?;
    checked(root, &["switch", "-q", "-c", &name], None).map(drop)
}

pub fn fetch(root: &Path, cancel: &Cancel) -> Result<(), GitError> {
    checked(root, &["fetch", "-q"], Some(cancel)).map(drop)
}

/// Pull（取り込みはMerge）。**衝突したら`merge --abort`で元に戻す**。
pub fn pull(root: &Path, cancel: &Cancel) -> Result<(), GitError> {
    let output = run(
        root,
        &["pull", "-q", "--no-rebase", "--no-edit"],
        Some(cancel),
    )?;
    if output.ok {
        return Ok(());
    }
    let merging = run(root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], None)?;
    if merging.ok {
        checked(root, &["merge", "--abort"], None)?;
        return Err(GitError::Conflict);
    }
    Err(output.failure())
}

/// Push。上流が無ければPublish（`-u`で`origin`へ。無ければ最初のリモートへ）。
pub fn push(root: &Path, status: &Status, cancel: &Cancel) -> Result<(), GitError> {
    if status.upstream.is_some() {
        return checked(root, &["push", "-q"], Some(cancel)).map(drop);
    }
    let Some(branch) = status.branch.as_deref() else {
        return Err(GitError::Failed(
            crate::i18n::pick(
                "ブランチの外にいるため、送れません",
                "Not on a branch, so there is nothing to push",
            )
            .to_owned(),
        ));
    };
    let remote = publish_remote(root)?;
    checked(root, &["push", "-q", "-u", &remote, branch], Some(cancel)).map(drop)
}

/// Publish の送り先：`origin`、無ければ最初のリモート。
fn publish_remote(root: &Path) -> Result<String, GitError> {
    let remotes = checked(root, &["remote"], None)?;
    let remotes: Vec<&str> = remotes.lines().collect();
    remotes
        .iter()
        .find(|name| **name == "origin")
        .or(remotes.first())
        .map(|name| (*name).to_owned())
        .ok_or_else(|| {
            GitError::Failed(
                crate::i18n::pick(
                    "リモートがありません（git remote add で足してください）",
                    "There is no remote (add one with git remote add)",
                )
                .to_owned(),
            )
        })
}

// RFN01-67 PR 2a: Git Repository の画面が読むもの・行う操作。

/// グラフの1行（Commit）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    /// `2026-10-03 14:20`。
    pub date: String,
    pub subject: String,
}

/// ローカルとリモートのブランチの Commit を、新しい順（親より子が先）に`count`件。
/// Stash の Commit は入れない。まだCommitが無ければ空。
pub fn log(root: &Path, count: usize) -> Result<Vec<LogEntry>, GitError> {
    let limit = format!("-n{count}");
    let output = run(
        root,
        &[
            "log",
            "--topo-order",
            "--branches",
            "--remotes",
            "HEAD",
            "--date=format:%Y-%m-%d %H:%M",
            "--format=%H%x1f%P%x1f%an%x1f%ad%x1f%s%x1e",
            &limit,
            "--",
        ],
        None,
    )?;
    if !output.ok {
        // まだ1度もCommitしていない（HEADが無い）。
        if run(root, &["rev-parse", "-q", "--verify", "HEAD"], None)?.ok {
            return Err(output.failure());
        }
        return Ok(Vec::new());
    }
    Ok(output
        .text()
        .split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            let sha = fields.next()?.to_owned();
            if sha.is_empty() {
                return None;
            }
            let parents = fields
                .next()?
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            Some(LogEntry {
                sha,
                parents,
                author: fields.next()?.to_owned(),
                date: fields.next()?.to_owned(),
                subject: fields.next()?.to_owned(),
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// `main`、`origin/main`。
    pub name: String,
    pub sha: String,
    pub remote: bool,
    /// ローカルのブランチの上流（`origin/main`）。
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// ローカルとリモートのブランチ。リモートの`HEAD`（`origin/HEAD`）は入れない。
pub fn refs(root: &Path) -> Result<Vec<Ref>, GitError> {
    let text = checked(
        root,
        &[
            "for-each-ref",
            "--format=%(objectname)%1f%(refname)%1f%(upstream:short)%1f%(upstream:track,nobracket)",
            "refs/heads",
            "refs/remotes",
        ],
        None,
    )?;
    let mut refs = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\x1f').collect();
        let [sha, full, upstream, track] = fields[..] else {
            continue;
        };
        let (name, remote) = if let Some(name) = full.strip_prefix("refs/heads/") {
            (name, false)
        } else if let Some(name) = full.strip_prefix("refs/remotes/") {
            (name, true)
        } else {
            continue;
        };
        if remote && (name.ends_with("/HEAD") || !name.contains('/')) {
            continue;
        }
        let count = |word: &str| {
            track
                .split(", ")
                .find_map(|part| part.strip_prefix(word))
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0)
        };
        refs.push(Ref {
            name: name.to_owned(),
            sha: sha.to_owned(),
            remote,
            upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
            ahead: count("ahead "),
            behind: count("behind "),
        });
    }
    Ok(refs)
}

/// HEAD の Commit。まだ無ければ`None`。
pub fn head_sha(root: &Path) -> Option<String> {
    checked(root, &["rev-parse", "-q", "--verify", "HEAD"], None)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// 上流にあって HEAD に無い Commit（取り込んでいない）と、HEAD にあって上流に無い Commit
/// （送っていない）。上流が無ければ両方空。
pub fn incoming_outgoing(root: &Path) -> (Vec<String>, Vec<String>) {
    let list = |range: &str| {
        checked(root, &["rev-list", range, "--"], None)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    if run(root, &["rev-parse", "-q", "--verify", "@{u}"], None).is_ok_and(|o| o.ok) {
        (list("HEAD..@{u}"), list("@{u}..HEAD"))
    } else {
        (Vec::new(), Vec::new())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub date: String,
    /// メッセージの全文。
    pub message: String,
    /// 1つ目の親との差（最初の Commit は全ファイル）。
    pub files: Vec<Change>,
}

pub fn detail(root: &Path, sha: &str) -> Result<Detail, GitError> {
    let text = checked(
        root,
        &[
            "log",
            "-1",
            "--date=format:%Y-%m-%d %H:%M",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%B",
            sha,
            "--",
        ],
        None,
    )?;
    let mut fields = text.splitn(6, '\x1f');
    let mut next = || fields.next().unwrap_or("").to_owned();
    let mut detail = Detail {
        sha: next(),
        parents: next().split_whitespace().map(str::to_owned).collect(),
        author: next(),
        email: next(),
        date: next(),
        message: next().trim_end().to_owned(),
        files: Vec::new(),
    };
    let output = match detail.parents.first() {
        Some(parent) => run(
            root,
            &[
                "diff-tree",
                "-r",
                "-M",
                "--no-commit-id",
                "--name-status",
                "-z",
                parent,
                &detail.sha,
            ],
            None,
        )?,
        None => run(
            root,
            &[
                "diff-tree",
                "-r",
                "--root",
                "-M",
                "--no-commit-id",
                "--name-status",
                "-z",
                &detail.sha,
            ],
            None,
        )?,
    };
    if !output.ok {
        return Err(output.failure());
    }
    detail.files = parse_name_status(&output.stdout);
    Ok(detail)
}

/// `--name-status -z`：`M\0道\0`、名前の変更は`R100\0元\0先\0`。
fn parse_name_status(bytes: &[u8]) -> Vec<Change> {
    let mut fields = bytes
        .split(|b| *b == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    let mut files = Vec::new();
    while let Some(code) = fields.next() {
        let Some(&first) = code.as_bytes().first() else {
            continue;
        };
        let Some(kind) = Kind::of(first) else {
            continue;
        };
        if kind == Kind::Renamed {
            let (Some(from), Some(path)) = (fields.next(), fields.next()) else {
                break;
            };
            files.push(Change {
                path,
                kind,
                from: Some(from),
            });
        } else if let Some(path) = fields.next() {
            files.push(Change {
                path,
                kind,
                from: None,
            });
        }
    }
    files
}

/// `sha`の版の`path`の中身。その版に無ければ`None`。
pub fn blob_at(root: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>, GitError> {
    let object = format!("{sha}:{path}");
    let output = run(root, &["cat-file", "blob", &object], None)?;
    Ok(output.ok.then_some(output.stdout))
}

/// 途中で止まった操作（Merge・Revert・Cherry-pick）が残っていれば`--abort`で戻し、衝突と答える。
fn abort_if_stopped(root: &Path, failed: Output, head: &str, command: &str) -> GitError {
    if run(root, &["rev-parse", "-q", "--verify", head], None).is_ok_and(|o| o.ok) {
        return match checked(root, &[command, "--abort"], None) {
            Ok(_) => GitError::Conflict,
            Err(error) => error,
        };
    }
    failed.failure()
}

/// `branch`を今のブランチへ Merge する。衝突したら`merge --abort`。
pub fn merge(root: &Path, branch: &str) -> Result<(), GitError> {
    let output = run(root, &["merge", "-q", "--no-edit", branch, "--"], None)?;
    if output.ok {
        return Ok(());
    }
    Err(abort_if_stopped(root, output, "MERGE_HEAD", "merge"))
}

/// Revert。Merge の Commit は1つ目の親に対して打ち消す。衝突したら`revert --abort`。
pub fn revert(root: &Path, sha: &str, merge_commit: bool) -> Result<(), GitError> {
    let mut arguments = vec!["revert", "--no-edit"];
    if merge_commit {
        arguments.extend(["-m", "1"]);
    }
    arguments.push(sha);
    let output = run(root, &arguments, None)?;
    if output.ok {
        return Ok(());
    }
    Err(abort_if_stopped(root, output, "REVERT_HEAD", "revert"))
}

/// Cherry-pick。Merge の Commit は1つ目の親との差を取る。衝突したら`cherry-pick --abort`。
pub fn cherry_pick(root: &Path, sha: &str, merge_commit: bool) -> Result<(), GitError> {
    let mut arguments = vec!["cherry-pick"];
    if merge_commit {
        arguments.extend(["-m", "1"]);
    }
    arguments.push(sha);
    let output = run(root, &arguments, None)?;
    if output.ok {
        return Ok(());
    }
    Err(abort_if_stopped(
        root,
        output,
        "CHERRY_PICK_HEAD",
        "cherry-pick",
    ))
}

/// Reset。`hard`は Delete Changes（作業ツリーの変更も消す）、そうでなければ Keep Changes（--mixed）。
pub fn reset(root: &Path, sha: &str, hard: bool) -> Result<(), GitError> {
    let mode = if hard { "--hard" } else { "--mixed" };
    checked(root, &["reset", "-q", mode, sha, "--"], None).map(drop)
}

/// `target`へ Reset すると、どこかのリモートにある Commit が今のブランチから外れるか。
pub fn reset_drops_pushed(root: &Path, target: &str) -> bool {
    let count = |arguments: &[&str]| {
        checked(root, arguments, None)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let range = format!("{target}..HEAD");
    let dropped = count(&["rev-list", &range, "--"]);
    let unpushed = count(&["rev-list", &range, "--not", "--remotes", "--"]);
    dropped > unpushed
}

/// ローカルのブランチを消す。`force`でなければ、Merge されていないブランチは`NotMerged`で断る。
pub fn delete_branch(root: &Path, name: &str, force: bool) -> Result<(), GitError> {
    let flag = if force { "-D" } else { "-d" };
    let output = run(root, &["branch", flag, name], None)?;
    if output.ok {
        return Ok(());
    }
    if output.stderr.contains("not fully merged") {
        return Err(GitError::NotMerged(name.to_owned()));
    }
    Err(output.failure())
}

/// `at`から新しいブランチを作って移る。
pub fn create_branch_at(root: &Path, name: &str, at: &str) -> Result<(), GitError> {
    let name = valid_branch_name(root, name)?;
    checked(root, &["switch", "-q", "-c", &name, at], None).map(drop)
}

/// リモートのブランチへ移る：同じ名前のローカルブランチがあればそこへ、無ければ作って移る。
pub fn checkout_remote(root: &Path, remote_branch: &str) -> Result<(), GitError> {
    let Some((_, local)) = remote_branch.split_once('/') else {
        return switch(root, remote_branch);
    };
    let exists = run(
        root,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{local}"),
        ],
        None,
    )?;
    if exists.ok {
        return switch(root, local);
    }
    checked(root, &["switch", "-q", "--track", remote_branch], None).map(drop)
}

/// ローカルのブランチを送る。上流が無ければ Publish（`-u`）。
pub fn push_branch(root: &Path, name: &str, cancel: &Cancel) -> Result<(), GitError> {
    let upstream = format!("{name}@{{upstream}}");
    let tracked = run(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &upstream,
        ],
        None,
    )?;
    if tracked.ok {
        let text = tracked.text();
        let remote = text.trim().split('/').next().unwrap_or("origin").to_owned();
        return checked(root, &["push", "-q", &remote, name], Some(cancel)).map(drop);
    }
    let remote = publish_remote(root)?;
    checked(root, &["push", "-q", "-u", &remote, name], Some(cancel)).map(drop)
}

fn valid_branch_name(root: &Path, name: &str) -> Result<String, GitError> {
    let name = name.trim();
    let valid = run(root, &["check-ref-format", "--branch", name], None)?;
    if name.is_empty() || name.starts_with('-') || !valid.ok {
        return Err(GitError::Failed(crate::say!(
            "ブランチの名前に使えません: {name}",
            "Not a valid branch name: {name}"
        )));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
#[path = "git_tests.rs"]
pub(crate) mod tests;
