use super::{GitError, checked, head_sha, repository_root, run};
use crate::i18n::pick;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub(crate) fn worktrees(root: &Path) -> Result<Vec<Worktree>, GitError> {
    let output = run(root, &["worktree", "list", "--porcelain", "-z"], None)?;
    if !output.ok {
        return Err(output.failure());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|error| GitError::Failed(error.to_string()))?;
    let mut trees = Vec::new();
    for entry in text.split("\0\0").filter(|entry| !entry.is_empty()) {
        let mut fields = entry.split('\0');
        let path = fields.next().and_then(|field| field.strip_prefix("worktree "))
            .filter(|path| !path.is_empty()).ok_or_else(worktree_changed)?;
        let mut tree = Worktree {
            path: path.into(),
            branch: None,
            head: None,
            bare: false,
            locked: false,
            prunable: false,
        };
        for field in fields {
            let (name, value) = field.split_once(' ').unwrap_or((field, ""));
            match name {
                "HEAD" => tree.head = Some(value.to_owned()),
                "branch" => tree.branch = value.strip_prefix("refs/heads/").map(str::to_owned),
                "bare" => tree.bare = true,
                "locked" => tree.locked = true,
                "prunable" => tree.prunable = true,
                _ => {}
            }
        }
        trees.push(tree);
    }
    Ok(trees)
}

pub(crate) fn worktree_changed() -> GitError {
    GitError::Failed(pick(
        "Worktreeの状態が変わりました。一覧を確認して選び直してください。",
        "The worktree state changed. Review the list and select it again.",
    ).into())
}

fn canonical(path: &Path) -> Result<PathBuf, GitError> {
    path.canonicalize().map_err(|error| GitError::Failed(error.to_string()))
}

fn common_dir(root: &Path) -> Result<PathBuf, GitError> {
    let dir = checked(root, &["rev-parse", "--git-common-dir"], None)?;
    canonical(&root.join(dir.trim_end_matches(['\r', '\n'])))
}

/// Reject a stale registration or a different repository now occupying that path.
pub(crate) fn check_worktree(root: &Path, target: &Path) -> Result<PathBuf, GitError> {
    let target = canonical(target)?;
    let listed = worktrees(root)?.into_iter().any(|tree| {
        !tree.bare && !tree.prunable && canonical(&tree.path).is_ok_and(|path| path == target)
    });
    let plain_target = crate::backup::plain(&target);
    if !listed || common_dir(root)? != common_dir(&plain_target)?
        || repository_root(&plain_target)?.and_then(|path| path.canonicalize().ok()).as_ref() != Some(&target)
    {
        return Err(worktree_changed());
    }
    Ok(target)
}

pub(crate) fn worktree_destination(parent: &Path, name: &str) -> Result<PathBuf, GitError> {
    if crate::file_tree::check_name(name).is_err() {
        return Err(GitError::Failed(pick(
            "新しいフォルダの名前を指定してください。",
            "Enter a name for the new folder.",
        ).into()));
    }
    Ok(canonical(parent)?.join(name))
}

/// Create a new branch and linked worktree from the HEAD selected in the UI.
pub(crate) fn add_worktree(
    root: &Path,
    parent: &Path,
    folder: &str,
    branch: &str,
    head: &str,
) -> Result<(), GitError> {
    if head_sha(root).as_deref() != Some(head) {
        return Err(worktree_changed());
    }
    let target = worktree_destination(parent, folder)?;
    match std::fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(GitError::Failed(error.to_string())),
        Ok(_) => return Err(GitError::Failed(pick(
            "作成先は既に存在します。新しいフォルダ名を指定してください。",
            "The destination already exists. Enter a new folder name.",
        ).into())),
    }
    if worktrees(root)?.iter().filter_map(|tree| canonical(&tree.path).ok())
        .any(|path| target.starts_with(&path))
    {
        return Err(GitError::Failed(pick(
            "既存のWorktreeの外に作成先を指定してください。",
            "Choose a destination outside the existing worktrees.",
        ).into()));
    }
    let mut git = super::command(root);
    git.args(["worktree", "add", "-b", branch, "--"]).arg(crate::backup::plain(&target)).arg(head);
    let output = super::run_command(git, None, None)?;
    if output.ok { Ok(()) } else { Err(output.failure()) }
}
