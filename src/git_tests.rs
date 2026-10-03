//! RFN01-67: `git.rs`の試験。**本物のGitで、一時フォルダに本物のリポジトリを作って**確かめる。
//! Gitの無いPCでは、リポジトリを作れないので何もせずに通す（`git_version`の試験と同じ）。
use super::*;
use std::cell::RefCell;

thread_local! {
    pub(crate) static PROGRAM: RefCell<String> = RefCell::new("git".to_owned());
}

/// このスレッドだけ、Gitの無いPCにする。
pub(crate) fn without_git<T>(body: impl FnOnce() -> T) -> T {
    PROGRAM.with(|name| *name.borrow_mut() = "rfnedit-no-such-git".to_owned());
    let result = body();
    PROGRAM.with(|name| *name.borrow_mut() = "git".to_owned());
    result
}

/// 試験ごとの一時フォルダ。落ちるときに消す。
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let folder =
            std::env::temp_dir().join(format!("rfnedit-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        Scratch(folder)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn git_ok(folder: &Path, arguments: &[&str]) {
    let output = run(folder, arguments, None).unwrap();
    assert!(output.ok, "git {arguments:?}: {}", output.stderr);
}

/// 名前とメールを入れたリポジトリ。Gitが無ければ`None`。
pub(crate) fn repository(folder: &Path) -> Option<()> {
    if !run(folder, &["init", "-q", "-b", "main"], None).ok()?.ok {
        return None;
    }
    git_ok(folder, &["config", "user.name", "t"]);
    git_ok(folder, &["config", "user.email", "t@t"]);
    git_ok(folder, &["config", "commit.gpgsign", "false"]);
    Some(())
}

fn write(folder: &Path, name: &str, text: &str) {
    std::fs::write(folder.join(name), text).unwrap();
}

/// 改行は`\n`へ揃えて読む。書き手のPCの`core.autocrlf`で、取り出したファイルは`\r\n`になる。
fn read(folder: &Path, name: &str) -> String {
    let text = std::fs::read_to_string(folder.join(name)).unwrap();
    text.replace("\r\n", "\n")
}

#[test]
fn a_machine_without_git_says_so() {
    let error = without_git(|| status(&std::env::temp_dir()).err());
    assert_eq!(error, Some(GitError::Missing));
}

#[test]
fn reads_branch_counts_renames_and_untracked_files() {
    let bytes = b"# branch.oid 0123\0# branch.head main\0# branch.upstream origin/main\0\
# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa aaa \xe5\x8e\x9f\xe7\xa8\xbf.md\0\
1 A. N... 000000 100644 100644 000 bbb new file.md\0\
2 R. N... 100644 100644 100644 ccc ccc R100 to.md\0from.md\0\
? memo.txt\0";
    let status = parse_status(bytes);
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.ahead, status.behind), (2, 1));
    assert!(!status.unborn);
    assert_eq!(status.staged.len(), 2);
    assert_eq!(status.staged[0].path, "new file.md");
    assert_eq!(status.staged[0].kind, Kind::Added);
    assert_eq!(status.staged[1].kind, Kind::Renamed);
    assert_eq!(status.staged[1].from.as_deref(), Some("from.md"));
    assert_eq!(status.changes[0].path, "原稿.md");
    assert_eq!(status.changes[0].kind, Kind::Modified);
    assert_eq!(status.changes[1].kind, Kind::Untracked);
    assert_eq!(status.changes[1].kind.letter(), "A");
}

#[test]
fn hides_credentials_written_into_a_url() {
    assert_eq!(
        hide_credentials("unable to access 'https://me:secret@example.com/r.git/': 403"),
        "unable to access 'https://***@example.com/r.git/': 403"
    );
    assert_eq!(
        hide_credentials("see https://example.com/r"),
        "see https://example.com/r"
    );
}

#[test]
fn commits_all_then_amends_with_japanese_names() {
    let scratch = Scratch::new("commit");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    assert_eq!(repository_root(root).unwrap().is_some(), true);
    write(root, "原稿.md", "一行目\n");
    let first = status(root).unwrap();
    assert!(first.unborn);
    assert_eq!(first.changes[0].path, "原稿.md");
    commit(root, "最初", true, false).unwrap();
    let after = status(root).unwrap();
    assert!(!after.unborn);
    assert!(after.changes.is_empty() && after.staged.is_empty());
    assert_eq!(last_message(root).unwrap(), "最初");
    commit(root, "直した", false, true).unwrap();
    assert_eq!(last_message(root).unwrap(), "直した");
    let count = checked(root, &["rev-list", "--count", "HEAD"], None).unwrap();
    assert_eq!(count.trim(), "1");
    assert!(!head_is_pushed(root));
}

#[test]
fn stages_and_unstages_single_files() {
    let scratch = Scratch::new("stage");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    write(root, "a[1].md", "a\n");
    write(root, "b.md", "b\n");
    // 道は模様として読まない：`a[1].md`は`a1.md`を指さない。
    stage(root, &["a[1].md".to_owned()]).unwrap();
    let staged = status(root).unwrap();
    assert_eq!(staged.staged.len(), 1);
    assert_eq!(staged.staged[0].path, "a[1].md");
    // まだCommitが無いリポジトリでも外せる。
    unstage(root, &[], true).unwrap();
    assert!(status(root).unwrap().staged.is_empty());
    commit(root, "1", true, false).unwrap();
    write(root, "b.md", "B\n");
    stage(root, &[]).unwrap();
    assert_eq!(status(root).unwrap().staged.len(), 1);
    unstage(root, &["b.md".to_owned()], false).unwrap();
    let back = status(root).unwrap();
    assert!(back.staged.is_empty());
    assert_eq!(back.changes[0].kind, Kind::Modified);
}

#[test]
fn undo_restores_tracked_files_and_removes_new_ones() {
    let scratch = Scratch::new("undo");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    write(root, "a.md", "a\n");
    commit(root, "1", true, false).unwrap();
    write(root, "a.md", "changed\n");
    write(root, "new.md", "new\n");
    let changes = status(root).unwrap().changes;
    undo(root, &changes, false).unwrap();
    assert_eq!(read(root, "a.md"), "a\n");
    assert!(!root.join("new.md").exists());

    // Staged Changes の行は前回のCommitへ。足したファイルは消える。
    write(root, "a.md", "staged\n");
    write(root, "added.md", "x\n");
    stage(root, &[]).unwrap();
    write(root, "a.md", "staged and more\n");
    let staged = status(root).unwrap().staged;
    undo(root, &staged, true).unwrap();
    assert_eq!(read(root, "a.md"), "a\n");
    assert!(!root.join("added.md").exists());
    let clean = status(root).unwrap();
    assert!(clean.staged.is_empty() && clean.changes.is_empty());
}

#[test]
fn a_conflicting_stash_is_undone_and_kept() {
    let scratch = Scratch::new("stash");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    write(root, "a.md", "1\n");
    commit(root, "1", true, false).unwrap();
    write(root, "a.md", "2\n");
    write(root, "extra.md", "extra\n");
    stash_all(root, "書きかけ").unwrap();
    let list = stashes(root).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].message.contains("書きかけ"));
    assert_eq!(read(root, "a.md"), "1\n");
    assert!(!root.join("extra.md").exists());

    write(root, "a.md", "3\n");
    // 変更があるうちは取り出さない。
    assert!(matches!(
        stash_apply(root, "stash@{0}", true),
        Err(GitError::Failed(_))
    ));
    commit(root, "3", true, false).unwrap();
    assert_eq!(
        stash_apply(root, "stash@{0}", true),
        Err(GitError::Conflict)
    );
    assert_eq!(read(root, "a.md"), "3\n");
    assert!(!root.join("extra.md").exists());
    let clean = status(root).unwrap();
    assert!(clean.staged.is_empty() && clean.changes.is_empty());
    assert_eq!(stashes(root).unwrap().len(), 1);
    stash_drop(root, "stash@{0}").unwrap();
    assert!(stashes(root).unwrap().is_empty());
}

#[test]
fn a_stash_pops_back_into_a_clean_tree() {
    let scratch = Scratch::new("pop");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    write(root, "a.md", "1\n");
    commit(root, "1", true, false).unwrap();
    write(root, "a.md", "2\n");
    stash_all(root, "").unwrap();
    stash_apply(root, "stash@{0}", true).unwrap();
    assert_eq!(read(root, "a.md"), "2\n");
    assert!(stashes(root).unwrap().is_empty());
}

#[test]
fn branches_are_listed_created_and_switched() {
    let scratch = Scratch::new("branch");
    let root = &scratch.0;
    if repository(root).is_none() {
        return;
    }
    write(root, "a.md", "1\n");
    commit(root, "1", true, false).unwrap();
    create_branch(root, "draft").unwrap();
    assert_eq!(status(root).unwrap().branch.as_deref(), Some("draft"));
    assert!(matches!(
        create_branch(root, "bad name"),
        Err(GitError::Failed(_))
    ));
    assert!(matches!(
        create_branch(root, "-x"),
        Err(GitError::Failed(_))
    ));
    switch(root, "main").unwrap();
    assert_eq!(status(root).unwrap().branch.as_deref(), Some("main"));
    assert_eq!(branches(root).unwrap(), ["draft", "main"]);
}

#[test]
fn publishes_pulls_and_undoes_a_conflicting_pull() {
    let scratch = Scratch::new("remote");
    let remote = scratch.0.join("remote.git");
    let mine = scratch.0.join("mine");
    let theirs = scratch.0.join("theirs");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&mine).unwrap();
    if !run(&remote, &["init", "-q", "--bare", "-b", "main"], None).is_ok_and(|o| o.ok) {
        return;
    }
    repository(&mine).unwrap();
    let url = remote.display().to_string();
    git_ok(&mine, &["remote", "add", "origin", &url]);
    write(&mine, "a.md", "1\n");
    commit(&mine, "1", true, false).unwrap();
    let cancel = Cancel::default();
    // 上流が無いPushはPublish。
    push(&mine, &status(&mine).unwrap(), &cancel).unwrap();
    let published = status(&mine).unwrap();
    assert_eq!(published.upstream.as_deref(), Some("origin/main"));
    assert!(head_is_pushed(&mine));

    let parent = scratch.0.as_path();
    let target = theirs.display().to_string();
    git_ok(parent, &["clone", "-q", &url, &target]);
    git_ok(&theirs, &["config", "user.name", "u"]);
    git_ok(&theirs, &["config", "user.email", "u@u"]);
    git_ok(&theirs, &["config", "commit.gpgsign", "false"]);
    write(&theirs, "a.md", "theirs\n");
    commit(&theirs, "theirs", true, false).unwrap();
    git_ok(&theirs, &["push", "-q"]);

    write(&mine, "a.md", "mine\n");
    commit(&mine, "mine", true, false).unwrap();
    let head = checked(&mine, &["rev-parse", "HEAD"], None).unwrap();
    assert_eq!(pull(&mine, &cancel), Err(GitError::Conflict));
    assert_eq!(checked(&mine, &["rev-parse", "HEAD"], None).unwrap(), head);
    assert_eq!(read(&mine, "a.md"), "mine\n");
    let merging = run(&mine, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], None);
    assert!(!merging.unwrap().ok);

    // 衝突しない取り込みは通る。
    fetch(&mine, &cancel).unwrap();
    git_ok(&mine, &["reset", "-q", "--hard", "origin/main"]);
    write(&theirs, "b.md", "b\n");
    commit(&theirs, "b", true, false).unwrap();
    git_ok(&theirs, &["push", "-q"]);
    fetch(&mine, &cancel).unwrap();
    assert_eq!(status(&mine).unwrap().behind, 1);
    pull(&mine, &cancel).unwrap();
    assert_eq!(read(&mine, "b.md"), "b\n");
}

#[test]
fn a_folder_outside_git_has_no_root_and_can_be_made_one() {
    let scratch = Scratch::new("init");
    let root = &scratch.0;
    if run(root, &["--version"], None).is_err() {
        return;
    }
    // 一時フォルダの上がリポジトリでないときだけ確かめられる。
    if repository_root(root).unwrap().is_some() {
        return;
    }
    init(root).unwrap();
    assert!(repository_root(root).unwrap().is_some());
}
