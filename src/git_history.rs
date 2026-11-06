//! RFN01-67 PR 2b（書き手と決めた 2026-10-03）: Git の操作の Undo／Redo。
//!
//! 画面を知らない。操作の前後の HEAD とブランチ（`Point`）と、戻すのに要るもの（ブランチ名、
//! 消したブランチの Commit、Stash など）を`Entry`に残し、Undo／Redo を git の操作に直す。
//! 覚えておく場所と、いつ記録するかは[`crate::git_ui`]が持つ。
//!
//! - **使えるのは、今の HEAD とブランチが記録したときのままのときだけ**（`usable`）。Terminal などで
//!   外から動かしたあとに戻すと、記録と食い違ったところへ戻してしまう。
//! - **Push 済みの Commit を今のブランチから外す Undo／Redo は断る**（`refusal`）。
//! - Push・Fetch・Stage・Undo Changes・Stash の Apply は記録しない（書き手と決めた表）。

use std::path::Path;

use crate::git::{self, GitError};
use crate::say;

/// HEAD がどこにあるか。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Point {
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub fn point(root: &Path) -> Point {
    Point {
        head: git::head_sha(root),
        branch: git::current_branch(root),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Commit・Amend：Undo は`reset --soft`で前へ（変更は Staged に残る）。
    Commit,
    /// ブランチの切り替え。
    Checkout,
    /// Merge・Pull・Sync・Revert・Cherry-pick：動いたブランチを操作の前の Commit へ戻す
    /// （`reset --keep`、Commit していない変更は残す）。`moved`はそのブランチの操作前の Commit。
    Move { moved: Option<String> },
    /// Reset — Keep Changes（--mixed）。
    ResetKeep,
    /// Reset — Delete Changes（--hard）。Undo で位置は戻るが、消えた変更は戻らない。
    ResetDelete,
    /// 新しいブランチを作って移った。
    NewBranch { name: String },
    /// ブランチを消した（`sha`はその先端）。
    DeleteBranch { name: String, sha: String },
    /// Stash All。`stash`は作られた Stash の Commit と reflog のメッセージ。
    StashAll {
        message: String,
        stash: Option<git::CreatedStash>,
    },
    /// Pop。`stash`は Undo で作り直した Stash。
    Pop {
        message: String,
        stash: Option<git::CreatedStash>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 「Commit "第一章を直す"」など。釦の Tip に「Undo …」「Redo …」として出す。
    pub label: String,
    pub kind: Kind,
    pub before: Point,
    pub after: Point,
}

impl Entry {
    /// 記録する値打ちがあるか（何も動かなかった Pull などは記録しない）。
    pub fn changed(&self) -> bool {
        match self.kind {
            Kind::DeleteBranch { .. } | Kind::StashAll { .. } | Kind::Pop { .. } => true,
            _ => self.before != self.after,
        }
    }
}

/// 取り出す方向に、対象の Stash を特定する記録があるか。
pub fn stash_identity_missing(entry: &Entry, redo: bool) -> bool {
    matches!(
        (&entry.kind, redo),
        (Kind::StashAll { stash: None, .. }, false) | (Kind::Pop { stash: None, .. }, true)
    )
}

/// HEAD・ブランチが記録どおりで、対象を特定できるか。
pub fn usable(entry: &Entry, now: &Point, redo: bool) -> bool {
    let expected = if redo { &entry.before } else { &entry.after };
    expected == now && !stash_identity_missing(entry, redo)
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// 断る理由（Push 済みの Commit が今のブランチから外れる）。断らなければ`None`。
pub fn refusal(root: &Path, entry: &Entry, redo: bool) -> Option<String> {
    let target = match (&entry.kind, redo) {
        (Kind::Commit, false) | (Kind::ResetKeep, true) | (Kind::ResetDelete, true) => {
            if redo {
                entry.after.head.clone()
            } else {
                entry.before.head.clone()
            }
        }
        (Kind::Move { moved }, false) => moved.clone(),
        _ => None,
    };
    let target = target?;
    git::reset_drops_pushed(root, &target).then(|| {
        say!(
            "Push済みのCommitが今のブランチから外れるため、{}は戻せません",
            "Cannot undo {}: it would take pushed commits off the current branch",
            entry.label
        )
    })
}

/// 確かめてから戻すもの（消えた変更は戻らない）。確かめなくてよければ`None`。
pub fn warning(entry: &Entry, redo: bool) -> Option<String> {
    match (&entry.kind, redo) {
        (Kind::ResetDelete, false) => Some(say!(
            "{}を戻しますか？\n\nCommitの位置は戻りますが、Resetで消えた変更は戻りません。",
            "Undo {}?\n\nThe commit position comes back, but the changes the reset deleted do not.",
            entry.label
        )),
        (Kind::ResetDelete, true) => Some(say!(
            "{}をやり直しますか？\n\nCommitしていない変更が消えます。",
            "Redo {}?\n\nUncommitted changes are deleted.",
            entry.label
        )),
        _ => None,
    }
}

/// ブランチへ戻る（ブランチの外だったなら、その Commit へ）。
fn go_to(root: &Path, point: &Point) -> Result<(), GitError> {
    match (&point.branch, &point.head) {
        (Some(branch), _) => git::plain(root, &["switch", "-q", branch]),
        (None, Some(head)) => git::plain(root, &["switch", "-q", "--detach", head]),
        (None, None) => Ok(()),
    }
}

fn needs<'a>(value: &'a Option<String>) -> Result<&'a str, GitError> {
    value.as_deref().ok_or_else(|| {
        GitError::Failed(
            crate::i18n::pick("戻す先がありません", "There is nothing to go back to").to_owned(),
        )
    })
}

#[derive(Debug)]
pub struct HistorySuccess {
    pub message: String,
    pub created_stash: Option<git::CreatedStash>,
}

fn pop_expected_top(
    root: &Path,
    stash: Option<&git::CreatedStash>,
    refusal: &str,
) -> Result<(), GitError> {
    let changed = || GitError::Failed(refusal.to_owned());
    let expected = stash.ok_or_else(changed)?;
    let stashes = git::stashes(root)?;
    let selected = stashes
        .first()
        .filter(|current| expected.matches(current))
        .ok_or_else(changed)?;
    git::stash_apply(root, selected, true)
}

/// Undo。通れば知らせる文と、この操作で作った Stash。
pub fn undo(root: &Path, entry: &Entry) -> Result<HistorySuccess, GitError> {
    let before = &entry.before;
    let mut created_stash = None;
    match &entry.kind {
        Kind::Commit => git::plain(root, &["reset", "-q", "--soft", needs(&before.head)?])?,
        Kind::Checkout => go_to(root, before)?,
        Kind::Move { moved } => {
            git::plain(root, &["reset", "-q", "--keep", needs(moved)?])?;
            if before.branch != entry.after.branch {
                go_to(root, before)?;
            }
        }
        Kind::ResetKeep => git::plain(root, &["reset", "-q", "--mixed", needs(&before.head)?])?,
        Kind::ResetDelete => git::plain(root, &["reset", "-q", "--keep", needs(&before.head)?])?,
        Kind::NewBranch { name } => {
            go_to(root, before)?;
            git::plain(root, &["branch", "-D", name])?;
        }
        Kind::DeleteBranch { name, sha } => git::plain(root, &["branch", name, sha])?,
        Kind::StashAll { stash, .. } => pop_expected_top(
            root,
            stash.as_ref(),
            crate::i18n::pick(
                "Stashが替わっているため戻せません",
                "The stash has changed, so it cannot be undone",
            ),
        )?,
        Kind::Pop { message, .. } => {
            created_stash = git::stash_all(root, message)?;
        }
    }
    Ok(HistorySuccess {
        message: say!("{}を戻しました", "Undid {}", entry.label),
        created_stash,
    })
}

/// Redo。通れば知らせる文と、この操作で作った Stash。
pub fn redo(root: &Path, entry: &Entry) -> Result<HistorySuccess, GitError> {
    let after = &entry.after;
    let mut created_stash = None;
    match &entry.kind {
        Kind::Commit => git::plain(root, &["reset", "-q", "--soft", needs(&after.head)?])?,
        Kind::Checkout => go_to(root, after)?,
        Kind::Move { .. } => {
            if entry.before.branch != after.branch {
                go_to(root, after)?;
            }
            git::plain(root, &["reset", "-q", "--keep", needs(&after.head)?])?;
        }
        Kind::ResetKeep => git::plain(root, &["reset", "-q", "--mixed", needs(&after.head)?])?,
        Kind::ResetDelete => git::reset(root, needs(&after.head)?, true)?,
        Kind::NewBranch { name } => {
            git::plain(root, &["branch", name, needs(&after.head)?])?;
            git::plain(root, &["switch", "-q", name])?;
        }
        Kind::DeleteBranch { name, .. } => git::plain(root, &["branch", "-D", name])?,
        Kind::StashAll { message, .. } => {
            created_stash = git::stash_all(root, message)?;
        }
        Kind::Pop { stash, .. } => pop_expected_top(
            root,
            stash.as_ref(),
            crate::i18n::pick(
                "Stashが替わっているためやり直せません",
                "The stash has changed, so it cannot be redone",
            ),
        )?,
    }
    Ok(HistorySuccess {
        message: say!("{}をやり直しました", "Redid {}", entry.label),
        created_stash,
    })
}

/// 記録の名前（Tip に出す）。
pub fn label_commit(message: &str, amend: bool) -> String {
    let first = message.lines().next().unwrap_or("");
    if amend {
        format!("Amend \"{first}\"")
    } else {
        format!("Commit \"{first}\"")
    }
}

pub fn label_sha(verb: &str, sha: &str) -> String {
    format!("{verb} {}", short(sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{git_ok, repository};
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let folder =
                std::env::temp_dir().join(format!("rfnedit-history-{name}-{}", std::process::id()));
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

    fn write(root: &Path, name: &str, text: &str) {
        std::fs::write(root.join(name), text).unwrap();
    }

    fn read(root: &Path, name: &str) -> String {
        std::fs::read_to_string(root.join(name))
            .unwrap()
            .replace("\r\n", "\n")
    }

    /// 1度Commitしたリポジトリ。Gitが無ければ`None`。
    fn started(name: &str) -> Option<Scratch> {
        let scratch = Scratch::new(name);
        repository(&scratch.0)?;
        write(&scratch.0, "a.md", "1\n");
        git::commit(&scratch.0, "1", true, false).unwrap();
        Some(scratch)
    }

    fn entry(label: &str, kind: Kind, before: Point, after: Point) -> Entry {
        Entry {
            label: label.into(),
            kind,
            before,
            after,
        }
    }

    #[test]
    fn a_commit_is_undone_into_staged_changes_and_redone() {
        let Some(scratch) = started("commit") else {
            return;
        };
        let root = &scratch.0;
        let before = point(root);
        write(root, "a.md", "2\n");
        git::commit(root, "2", true, false).unwrap();
        let recorded = entry(
            &label_commit("2", false),
            Kind::Commit,
            before.clone(),
            point(root),
        );
        assert!(recorded.changed());
        assert!(usable(&recorded, &point(root), false));
        undo(root, &recorded).unwrap();
        assert_eq!(point(root), before);
        let status = git::status(root).unwrap();
        assert_eq!(status.staged.len(), 1, "the change stays staged");
        assert_eq!(read(root, "a.md"), "2\n");
        assert!(usable(&recorded, &point(root), true));
        redo(root, &recorded).unwrap();
        assert_eq!(point(root), recorded.after);
    }

    #[test]
    fn a_merge_into_another_branch_goes_back_and_returns() {
        let Some(scratch) = started("merge") else {
            return;
        };
        let root = &scratch.0;
        git_ok(root, &["switch", "-q", "-c", "draft"]);
        write(root, "b.md", "b\n");
        git::commit(root, "b", true, false).unwrap();
        // draft にいながら、main へ移って draft を Merge する（ドラッグの形）。
        let before = point(root);
        let moved = git::resolve(root, "main");
        git::switch(root, "main").unwrap();
        git::merge(root, "draft").unwrap();
        let recorded = entry(
            "Merge draft",
            Kind::Move { moved },
            before.clone(),
            point(root),
        );
        undo(root, &recorded).unwrap();
        assert_eq!(point(root), before);
        assert_eq!(git::resolve(root, "main"), git::resolve(root, "draft~1"));
        redo(root, &recorded).unwrap();
        assert_eq!(point(root), recorded.after);
        assert!(root.join("b.md").exists());
    }

    #[test]
    fn branches_come_back_and_go_away() {
        let Some(scratch) = started("branches") else {
            return;
        };
        let root = &scratch.0;
        let before = point(root);
        git::create_branch(root, "draft").unwrap();
        let made = entry(
            "New Branch draft",
            Kind::NewBranch {
                name: "draft".into(),
            },
            before.clone(),
            point(root),
        );
        undo(root, &made).unwrap();
        assert_eq!(point(root), before);
        assert!(!git::branches(root).unwrap().contains(&"draft".to_owned()));
        redo(root, &made).unwrap();
        assert_eq!(git::current_branch(root).as_deref(), Some("draft"));

        git::switch(root, "main").unwrap();
        let sha = git::resolve(root, "draft").unwrap();
        git::delete_branch(root, "draft", true).unwrap();
        let deleted = entry(
            "Delete Branch draft",
            Kind::DeleteBranch {
                name: "draft".into(),
                sha: sha.clone(),
            },
            point(root),
            point(root),
        );
        assert!(deleted.changed());
        undo(root, &deleted).unwrap();
        assert_eq!(git::resolve(root, "draft"), Some(sha));
        redo(root, &deleted).unwrap();
        assert_eq!(git::resolve(root, "draft"), None);
    }

    #[test]
    fn stash_all_redo_returns_its_identity_for_the_next_undo() {
        let Some(scratch) = started("stash") else {
            return;
        };
        let root = &scratch.0;
        write(root, "a.md", "書きかけ\n");
        let before = point(root);
        let stash = git::stash_all(root, "場面").unwrap();
        let mut stashed = entry(
            "Stash All",
            Kind::StashAll {
                message: "場面".into(),
                stash,
            },
            before,
            point(root),
        );
        assert!(undo(root, &stashed).unwrap().created_stash.is_none());
        assert_eq!(read(root, "a.md"), "書きかけ\n");
        assert!(git::stashes(root).unwrap().is_empty());
        let recreated = redo(root, &stashed).unwrap().created_stash.unwrap();
        assert_eq!(read(root, "a.md"), "1\n");
        let stashes = git::stashes(root).unwrap();
        assert_eq!(stashes.len(), 1);
        assert!(recreated.matches(&stashes[0]));
        let Kind::StashAll { stash, .. } = &mut stashed.kind else {
            unreachable!()
        };
        *stash = Some(recreated);

        write(root, "a.md", "外部\n");
        git_ok(root, &["stash", "push", "-q", "-m", "external"]);
        let with_external = git::stashes(root).unwrap();
        assert!(undo(root, &stashed).is_err());
        assert_eq!(git::stashes(root).unwrap(), with_external);
        assert_eq!(read(root, "a.md"), "1\n");
        git_ok(root, &["stash", "drop", "-q", "stash@{0}"]);
        undo(root, &stashed).unwrap();
        assert_eq!(read(root, "a.md"), "書きかけ\n");
    }

    #[test]
    fn redo_pop_refuses_a_stash_inserted_after_undo() {
        let Some(scratch) = started("pop-external") else {
            return;
        };
        let root = &scratch.0;
        write(root, "a.md", "取り出す内容\n");
        git::stash_all(root, "場面").unwrap().unwrap();
        let popped = git::stashes(root).unwrap().remove(0);
        let before = point(root);
        git::stash_apply(root, &popped, true).unwrap();
        let mut recorded = entry(
            "Pop stash@{0}",
            Kind::Pop {
                message: popped.message,
                stash: None,
            },
            before,
            point(root),
        );
        let recreated = undo(root, &recorded).unwrap().created_stash.unwrap();
        assert!(recreated.matches(&git::stashes(root).unwrap()[0]));
        let Kind::Pop { stash, .. } = &mut recorded.kind else {
            unreachable!()
        };
        *stash = Some(recreated);

        write(root, "a.md", "外部\n");
        git_ok(root, &["stash", "push", "-q", "-m", "external"]);
        let with_external = git::stashes(root).unwrap();
        assert!(redo(root, &recorded).is_err());
        assert_eq!(git::stashes(root).unwrap(), with_external);
        assert_eq!(read(root, "a.md"), "1\n");
        git_ok(root, &["stash", "drop", "-q", "stash@{0}"]);
        assert!(redo(root, &recorded).unwrap().created_stash.is_none());
        assert!(git::stashes(root).unwrap().is_empty());
        assert_eq!(read(root, "a.md"), "取り出す内容\n");
    }

    #[test]
    fn a_different_reflog_entry_for_the_same_object_does_not_match_history() {
        let Some(scratch) = started("stash-relabelled") else {
            return;
        };
        let root = &scratch.0;
        write(root, "a.md", "元の内容\n");
        let stash = git::stash_all(root, "original").unwrap().unwrap();
        let oid = git::stashes(root).unwrap().remove(0).oid;
        git_ok(root, &["stash", "drop", "-q", "stash@{0}"]);
        git_ok(root, &["stash", "store", "-m", "replacement", &oid]);
        let replacement = git::stashes(root).unwrap();
        assert_eq!(replacement[0].oid, oid);
        let recorded = entry(
            "Stash All",
            Kind::StashAll {
                message: "original".into(),
                stash: Some(stash),
            },
            point(root),
            point(root),
        );
        assert!(undo(root, &recorded).is_err());
        assert_eq!(git::stashes(root).unwrap(), replacement);
        assert_eq!(read(root, "a.md"), "1\n");
    }

    #[test]
    fn undo_pop_without_changes_disables_redo_even_with_an_external_stash() {
        let Some(scratch) = started("pop-no-op") else {
            return;
        };
        let root = &scratch.0;
        write(root, "a.md", "外部\n");
        git_ok(root, &["stash", "push", "-q", "-m", "external"]);
        let external = git::stashes(root).unwrap();
        let recorded = entry(
            "Pop stash@{0}",
            Kind::Pop {
                message: "original".into(),
                stash: None,
            },
            point(root),
            point(root),
        );
        assert!(undo(root, &recorded).unwrap().created_stash.is_none());
        assert!(!usable(&recorded, &point(root), true));
        assert!(redo(root, &recorded).is_err());
        assert_eq!(git::stashes(root).unwrap(), external);
        assert_eq!(read(root, "a.md"), "1\n");
    }

    #[test]
    fn hard_reset_comes_back_in_position_only_and_moved_heads_are_not_usable() {
        let Some(scratch) = started("reset") else {
            return;
        };
        let root = &scratch.0;
        let first = git::head_sha(root).unwrap();
        write(root, "a.md", "2\n");
        git::commit(root, "2", true, false).unwrap();
        let before = point(root);
        write(root, "a.md", "書きかけ\n");
        git::reset(root, &first, true).unwrap();
        let recorded = entry(
            &label_sha("Reset to", &first),
            Kind::ResetDelete,
            before.clone(),
            point(root),
        );
        assert!(warning(&recorded, false).is_some());
        undo(root, &recorded).unwrap();
        assert_eq!(point(root), before);
        // 消えた「書きかけ」は戻らない。
        assert_eq!(read(root, "a.md"), "2\n");
        // 外で HEAD を動かすと、その記録は使えない。
        git_ok(root, &["reset", "-q", "--hard", &first]);
        assert!(!usable(&recorded, &point(root), true));
    }

    #[test]
    fn undoing_a_pushed_commit_is_refused() {
        let scratch = Scratch::new("pushed");
        let remote = scratch.0.join("remote.git");
        let root = scratch.0.join("mine");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        if repository(&remote).is_none() {
            return;
        }
        git_ok(&remote, &["config", "receive.denyCurrentBranch", "ignore"]);
        repository(&root).unwrap();
        let url = remote.display().to_string();
        git_ok(&root, &["remote", "add", "origin", &url]);
        write(&root, "a.md", "1\n");
        git::commit(&root, "1", true, false).unwrap();
        let before = point(&root);
        write(&root, "a.md", "2\n");
        git::commit(&root, "2", true, false).unwrap();
        let recorded = entry("Commit \"2\"", Kind::Commit, before, point(&root));
        assert_eq!(refusal(&root, &recorded, false), None);
        git::push(&root, &git::status(&root).unwrap(), &git::Cancel::default()).unwrap();
        assert!(refusal(&root, &recorded, false).is_some());
    }
}
