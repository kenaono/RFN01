//! 前回のCommitの版を取り出す（追加要件「Gitの版管理」2026-09-15）。
//!
//! **Gitは書き手のPCに入っているものを呼ぶ。**ライブラリを持たないのは、
//! 書き手の`git`が読む設定（`safe.directory`、LFSなど）と食い違わないため。
//! 取り出すのは生のバイトだけで、読み方は文書の文字コード設定に従う（要件 7.11）。
use std::io;
use std::path::Path;
use std::process::{Command, Output};

#[derive(Debug, PartialEq, Eq)]
pub enum GitError {
    /// `git`が無い。
    Missing,
    /// 管理下にない、またはまだ1度もCommitしていない。
    NoCommit,
    /// フォルダの持ち主が違うので、Gitが読むのを断った（`safe.directory`）。
    Untrusted,
    /// 前回のCommitにこのファイルが無い。
    NotCommitted,
    Failed(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Missing => f.write_str(crate::i18n::pick(
                "Gitが見つかりません（インストールされていないか、PATHにありません）",
                "Git was not found (it is not installed, or not on PATH)",
            )),
            GitError::NoCommit => f.write_str(crate::i18n::pick(
                "Gitの管理下にないか、まだCommitがありません",
                "Not under Git, or there are no commits yet",
            )),
            GitError::Untrusted => f.write_str(crate::i18n::pick(
                "フォルダの持ち主が違うため、Gitが読むのを断りました（safe.directoryへの追加が必要です）",
                "Git refused to read a folder owned by someone else (add it to safe.directory)",
            )),
            GitError::NotCommitted => f.write_str(crate::i18n::pick(
                "前回のCommitにこのファイルがありません",
                "This file is not in the last commit",
            )),
            GitError::Failed(error) => f.write_str(&crate::say!(
                "Gitを実行できません: {error}",
                "Cannot run Git: {error}"
            )),
        }
    }
}

fn git(folder: &Path, arguments: &[&str]) -> Result<Output, GitError> {
    run("git", folder, arguments)
}

/// 呼ぶ名前を差し替えられるのは、**Gitの無いPC**を試験で作るため。
fn run(program: &str, folder: &Path, arguments: &[&str]) -> Result<Output, GitError> {
    use std::os::windows::process::CommandExt;
    // コンソールの窓を一瞬でも出さない。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(program)
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => GitError::Missing,
            _ => GitError::Failed(error.to_string()),
        })
}

/// `path`の、前回のCommit（HEAD）での中身と、そのCommitの短い名前。
///
/// **名前を先に決めてから中身を取る。**2回の呼び出しのあいだにCommitされても、
/// 名前と中身が別の版を指さない。
pub fn head_version(path: &Path) -> Result<(String, Vec<u8>), GitError> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(GitError::NotCommitted);
    };
    let head = git(folder, &["rev-parse", "--short", "HEAD"])?;
    if !head.status.success() {
        // 断りの文は訳されうるが、設定の名前は訳されない。
        let said = String::from_utf8_lossy(&head.stderr);
        return Err(if said.contains("safe.directory") {
            GitError::Untrusted
        } else {
            GitError::NoCommit
        });
    }
    let commit = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    // `<commit>:./name`は`-C`で移った先から見た名前（Gitの決まり）。
    let object = format!("{commit}:./{}", name.to_string_lossy());
    let blob = git(folder, &["cat-file", "blob", &object])?;
    if !blob.status.success() {
        return Err(GitError::NotCommitted);
    }
    Ok((commit, blob.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 書き手の確認 2026-09-15: Gitが入っていなくても、断りの理由が出る。
    #[test]
    fn a_machine_without_git_says_so() {
        let error = run("rfnedit-no-such-git", &std::env::temp_dir(), &["--version"]);
        assert_eq!(error.err(), Some(GitError::Missing));
        assert!(
            GitError::Missing
                .to_string()
                .contains("Gitが見つかりません")
        );
    }

    #[test]
    fn reads_the_committed_version_not_the_working_file() {
        let folder = std::env::temp_dir().join(format!("rfnedit-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("原稿.md");
        std::fs::write(&file, "一行目\n").unwrap();
        let run = |arguments: &[&str]| git(&folder, arguments).unwrap().status.success();
        if !run(&["init", "-q"]) {
            return;
        }
        assert_eq!(head_version(&file), Err(GitError::NoCommit));
        assert!(run(&["add", "原稿.md"]));
        let commit = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "1",
        ];
        assert!(run(&commit));
        std::fs::write(&file, "書き換えた\n").unwrap();
        let (commit, bytes) = head_version(&file).unwrap();
        assert!(!commit.is_empty());
        assert_eq!(bytes, "一行目\n".as_bytes());
        let other = folder.join("未登録.md");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(head_version(&other), Err(GitError::NotCommitted));
        let _ = std::fs::remove_dir_all(&folder);
    }
}
