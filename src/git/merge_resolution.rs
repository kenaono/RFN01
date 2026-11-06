use super::{GitError, Output, checked, command, head_sha, run, run_command, stage};
use crate::i18n::pick;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeState {
    pub head: String,
    pub incoming: Vec<String>,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeConflict {
    pub path: String,
    pub versions: [Option<IndexVersion>; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexVersion {
    mode: String,
    oid: String,
}

impl MergeConflict {
    pub fn can_delete(&self) -> bool {
        (self.versions[1].is_none() || self.versions[2].is_none())
            && self.versions.iter().flatten().all(|v| v.mode != "160000")
    }
}

fn incoming(root: &Path) -> Result<Vec<String>, GitError> {
    let path = checked(root, &["rev-parse", "--git-path", "MERGE_HEAD"], None)?;
    let path = root.join(path.trim_end_matches(['\r', '\n']));
    match std::fs::read_to_string(path) {
        Ok(heads) => Ok(heads.lines().map(str::to_owned).collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(GitError::Failed(error.to_string())),
    }
}

pub(crate) fn merge_state(root: &Path) -> Result<Option<MergeState>, GitError> {
    let incoming = incoming(root)?;
    if incoming.is_empty() {
        return Ok(None);
    }
    let head = head_sha(root).ok_or_else(merge_changed)?;
    let index = run(root, &["ls-files", "--unmerged", "-z"], None)?;
    if !index.ok {
        return Err(index.failure());
    }
    let mut paths: BTreeMap<String, [Option<IndexVersion>; 3]> = BTreeMap::new();
    for record in index
        .stdout
        .split(|&byte| byte == 0)
        .filter(|r| !r.is_empty())
    {
        let (entry, path) = std::str::from_utf8(record)
            .ok()
            .and_then(|line| line.split_once('\t'))
            .ok_or_else(merge_changed)?;
        let mut fields = entry.split_ascii_whitespace();
        let (Some(mode), Some(oid), Some(at)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(merge_changed());
        };
        let versions = paths.entry(path.to_owned()).or_default();
        let version = match at {
            "1" => &mut versions[0],
            "2" => &mut versions[1],
            "3" => &mut versions[2],
            _ => return Err(merge_changed()),
        };
        *version = Some(IndexVersion {
            mode: mode.into(),
            oid: oid.into(),
        });
    }
    let conflicts = paths
        .into_iter()
        .map(|(path, versions)| MergeConflict { path, versions })
        .collect();
    Ok(Some(MergeState {
        head,
        incoming,
        conflicts,
    }))
}

pub(crate) fn merge_changed() -> GitError {
    GitError::Failed(
        pick(
            "GitのMerge状態が変わりました。一覧を読み直してからやり直してください。",
            "The Git merge state changed. Review the refreshed list and try again.",
        )
        .into(),
    )
}

pub(crate) fn require_no_merge(root: &Path) -> Result<(), GitError> {
    if incoming(root)?.is_empty() {
        Ok(())
    } else {
        Err(GitError::MergeStopped)
    }
}

pub(super) fn merge_failure(root: &Path, output: Output) -> GitError {
    match require_no_merge(root) {
        Err(GitError::MergeStopped) => GitError::MergeStopped,
        _ => output.failure(),
    }
}

pub(crate) fn conflict_blob(root: &Path, version: &IndexVersion) -> Result<Vec<u8>, GitError> {
    let output = run(root, &["cat-file", "blob", &version.oid], None)?;
    if output.ok {
        Ok(output.stdout)
    } else {
        Err(output.failure())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Resolution {
    Stage,
    Delete(Option<crate::file_io::FileStamp>),
    Continue,
    Abort,
}

pub(crate) fn worktree_stamp(path: &Path) -> Option<crate::file_io::FileStamp> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(crate::file_io::FileStamp {
        modified: metadata.modified().ok(),
        length: metadata.len(),
    })
}

fn check_markers(root: &Path, path: &str) -> Result<(), GitError> {
    let check = run(
        root,
        &["--literal-pathspecs", "diff", "--check", "--", path],
        None,
    )?;
    if check
        .text()
        .lines()
        .any(|line| line.ends_with(": leftover conflict marker"))
    {
        return Err(GitError::Failed(
            pick(
                "衝突マーカーが残っています。作業ファイルを編集して保存してください。",
                "Conflict markers remain. Edit and save the working file before staging.",
            )
            .into(),
        ));
    }
    if !check.ok && !check.stderr.is_empty() {
        return Err(check.failure());
    }
    Ok(())
}

/// Check the Git state again after palette selection or a confirmation dialog.
pub(crate) fn resolve_merge(
    root: &Path,
    expected: &MergeState,
    path: Option<&str>,
    resolution: Resolution,
) -> Result<(), GitError> {
    let state = merge_state(root)?
        .filter(|current| current == expected)
        .ok_or_else(merge_changed)?;
    match resolution {
        Resolution::Stage | Resolution::Delete(_) => {
            let conflict = state
                .conflicts
                .iter()
                .find(|conflict| Some(conflict.path.as_str()) == path)
                .ok_or_else(merge_changed)?;
            if matches!(resolution, Resolution::Stage) {
                check_markers(root, &conflict.path)?;
                stage(root, std::slice::from_ref(&conflict.path))
            } else if let Resolution::Delete(stamp) = resolution
                && conflict.can_delete()
                && worktree_stamp(&root.join(&conflict.path)) == stamp
            {
                checked(
                    root,
                    &["--literal-pathspecs", "rm", "-f", "--", &conflict.path],
                    None,
                )
                .map(drop)
            } else {
                Err(merge_changed())
            }
        }
        Resolution::Continue if state.conflicts.is_empty() => {
            let mut git = command(root);
            git.env("GIT_EDITOR", "true").args(["merge", "--continue"]);
            let output = run_command(git, None, None)?;
            if output.ok {
                Ok(())
            } else {
                Err(output.failure())
            }
        }
        Resolution::Continue => Err(merge_changed()),
        Resolution::Abort => checked(root, &["merge", "--abort"], None).map(drop),
    }
}
