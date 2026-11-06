//! The same read-only Git snapshots back Git Changes and the Repository WIP preview.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::file_io::{self, LoadError};
use crate::git::{self, Change, Kind};
use crate::i18n::pick;
use crate::{AppWindow, MAX_DOCUMENT_CHARACTERS, StatusBar, say};

pub(crate) struct Target {
    pub root: PathBuf,
    pub staged: bool,
    pub change: Change,
}

pub(crate) struct Preview {
    pub name: String,
    pub caption: String,
    pub left_label: String,
    pub right_label: String,
    pub text: Result<(String, String), String>,
    pub patch: Option<Arc<crate::git_hunks::Patch>>,
}

struct SideText {
    text: String,
    symlink: bool,
}

impl Preview {
    pub fn open(&self, window: &AppWindow) {
        match &self.text {
            Ok((left, right)) => crate::diff_view::show(
                window,
                self.left_label.clone(),
                left.clone(),
                self.right_label.clone(),
                right.clone(),
            ),
            Err(error) => window.tell(error.clone().into()),
        }
    }
}

pub(crate) fn read(target: &Target) -> Preview {
    let Target {
        root,
        staged,
        change,
    } = target;
    let old_path = change.from.as_deref().unwrap_or(&change.path);
    let path = &change.path;
    let index = pick("インデックス", "Index");
    let working = pick("作業ツリー", "Working Tree");
    let (left_source, right_source) = if *staged {
        ("HEAD", index)
    } else {
        (index, working)
    };
    let left = || match change.kind {
        Kind::Added | Kind::Untracked => Ok(None),
        Kind::Conflicted => Err(pick(
            "未解決の衝突があるため、インデックスの版を一つに決められません",
            "The index has no single version because this file has an unresolved conflict",
        )
        .to_owned()),
        _ => blob(root, if *staged { "HEAD" } else { "" }, old_path),
    };
    let right = || {
        if change.kind == Kind::Deleted {
            Ok(None)
        } else if *staged {
            blob(root, "", path)
        } else {
            working_text(&root.join(path)).map(Some)
        }
    };
    preview(change, left_source, right_source, left(), right())
}

pub(crate) fn read_commits(root: &Path, left: &str, right: &str, change: &Change) -> Preview {
    read_commit(root, Some(left), right, change)
}

pub(crate) fn read_commit(
    root: &Path,
    parent: Option<&str>,
    right: &str,
    change: &Change,
) -> Preview {
    let before = match parent {
        Some(left) if change.kind != Kind::Added => {
            tree_blob(root, left, change.from.as_deref().unwrap_or(&change.path))
        }
        _ => Ok(None),
    };
    let after = if change.kind == Kind::Deleted {
        Ok(None)
    } else {
        tree_blob(root, right, &change.path)
    };
    preview(
        change,
        &parent.unwrap_or("—").chars().take(7).collect::<String>(),
        &right.chars().take(7).collect::<String>(),
        before,
        after,
    )
}

pub(crate) fn read_commit_working(root: &Path, sha: &str, change: &Change) -> Preview {
    let before = if change.kind == Kind::Deleted {
        Ok(None)
    } else {
        tree_blob(root, sha, &change.path)
    };
    let path = root.join(&change.path);
    let after = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
        Ok(entry)
            if entry.is_symlink()
                || crate::workspace_boundary::existing_file_allowed(
                    &[root.to_path_buf()],
                    &path,
                ) =>
        {
            working_text(&path).map(Some)
        }
        Ok(_) => Err(pick(
            "作業ツリー内のファイルを読み取れません",
            "Cannot read this file within the working tree",
        )
        .to_owned()),
    };
    // Both sides use the name in the selected commit, including after a rename.
    let same_path = Change {
        from: None,
        ..change.clone()
    };
    preview(
        &same_path,
        &sha.chars().take(7).collect::<String>(),
        pick("作業ツリー", "Working Tree"),
        before,
        after,
    )
}

fn preview(
    change: &Change,
    left_source: &str,
    right_source: &str,
    left: Result<Option<SideText>, String>,
    right: Result<Option<SideText>, String>,
) -> Preview {
    let path = &change.path;
    let old_path = change.from.as_deref().unwrap_or(path);
    let left = left.map_err(|error| format!("{left_source} · {old_path} · {error}"));
    let right = right.map_err(|error| format!("{right_source} · {path} · {error}"));
    let left_name = side_name(left_source, &left);
    let right_name = side_name(right_source, &right);
    Preview {
        name: path.clone(),
        caption: format!("{left_name} → {right_name}"),
        left_label: format!("{left_name} · {old_path}"),
        right_label: format!("{right_name} · {path}"),
        text: left.and_then(|left| {
            right.map(|right| {
                (
                    left.map_or_else(String::new, |side| side.text),
                    right.map_or_else(String::new, |side| side.text),
                )
            })
        }),
        patch: None,
    }
}

fn side_name(source: &str, text: &Result<Option<SideText>, String>) -> String {
    match text {
        Ok(None) => say!("{source}（なし）", "{source} (absent)"),
        Ok(Some(side)) if side.symlink => say!("{source}（リンク）", "{source} (link)"),
        Ok(Some(side)) if side.text.is_empty() => say!("{source}（空）", "{source} (empty)"),
        _ => source.to_owned(),
    }
}

fn tree_blob(root: &Path, revision: &str, path: &str) -> Result<Option<SideText>, String> {
    let symlink =
        git::tree_entry_is_symlink(root, revision, path).map_err(|error| error.to_string())?;
    blob(root, revision, path).map(|side| side.map(|side| SideText { symlink, ..side }))
}

fn blob(root: &Path, revision: &str, path: &str) -> Result<Option<SideText>, String> {
    let bytes = git::change_blob(root, revision, path).map_err(|error| error.to_string())?;
    decode_result(file_io::decode(&bytes, MAX_DOCUMENT_CHARACTERS).map(|(text, _)| text)).map(
        |text| {
            Some(SideText {
                text,
                symlink: false,
            })
        },
    )
}

fn working_text(path: &Path) -> Result<SideText, String> {
    let symlink = fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .is_symlink();
    if symlink {
        let target = fs::read_link(path).map_err(|error| error.to_string())?;
        return Ok(SideText {
            text: target.to_string_lossy().into_owned(),
            symlink,
        });
    }
    decode_result(file_io::read(path, MAX_DOCUMENT_CHARACTERS).map(|file| file.text))
        .map(|text| SideText { text, symlink })
}

fn decode_result(text: Result<String, LoadError>) -> Result<String, String> {
    let binary = || {
        pick(
            "バイナリ、または対応していない文字コードのファイルです",
            "Binary file or unsupported text encoding",
        )
        .to_owned()
    };
    match text {
        Ok(text)
            if text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) =>
        {
            Err(binary())
        }
        Err(LoadError::Unreadable) => Err(binary()),
        other => other.map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{git_ok, repository};
    use crate::saving::scratch_directory;

    #[test]
    fn previews_distinguish_absent_empty_binary_and_read_failures() {
        let root = scratch_directory("git-change-diff");
        let Some(()) = repository(&root) else { return };
        let empty = "空 の原稿.md";
        fs::write(root.join(empty), b"").unwrap();
        git::stage(&root, &[empty.to_owned()]).unwrap();
        let change = git::status(&root).unwrap().staged.pop().unwrap();
        let preview = read(&Target {
            root: root.clone(),
            staged: true,
            change,
        });
        assert!(preview.left_label.contains(pick("なし", "absent")));
        assert!(preview.right_label.contains(pick("空", "empty")));
        assert_eq!(preview.text.unwrap(), (String::new(), String::new()));

        let name = "画像 データ.bin";
        let path = root.join(name);
        fs::write(&path, b"header\0\0payload").unwrap();
        let change = git::status(&root).unwrap().changes.pop().unwrap();
        let target = Target {
            root,
            staged: false,
            change,
        };
        assert!(
            read(&target)
                .text
                .unwrap_err()
                .contains(pick("バイナリ", "Binary"))
        );
        fs::remove_file(path).unwrap();
        let error = read(&target).text.unwrap_err();
        assert!(error.contains(name));
        assert!(!error.contains(pick("バイナリ", "Binary")));
    }

    #[test]
    fn staged_rename_uses_the_old_head_path_and_delete_has_no_index_side() {
        let root = scratch_directory("git-change-rename");
        let Some(()) = repository(&root) else { return };
        git_ok(&root, &["config", "status.renames", "true"]);
        let old = "古い 原稿.md";
        let new = "新しい 原稿.md";
        fs::write(root.join(old), "本文\n").unwrap();
        git::commit(&root, "base", true, false).unwrap();
        git_ok(&root, &["mv", "--", old, new]);
        let change = git::status(&root).unwrap().staged.pop().unwrap();
        assert_eq!(change.kind, Kind::Renamed);
        let preview = read(&Target {
            root: root.clone(),
            staged: true,
            change,
        });
        assert!(preview.left_label.ends_with(old));
        assert!(preview.right_label.ends_with(new));
        assert_eq!(preview.text.unwrap(), ("本文\n".into(), "本文\n".into()));

        git::commit(&root, "rename", false, false).unwrap();
        git_ok(&root, &["rm", "--", new]);
        let change = git::status(&root).unwrap().staged.pop().unwrap();
        let preview = read(&Target {
            root,
            staged: true,
            change,
        });
        assert!(preview.right_label.contains(pick("なし", "absent")));
        assert_eq!(preview.text.unwrap(), ("本文\n".into(), String::new()));
    }

    #[test]
    fn commit_preview_distinguishes_link_blobs_from_file_text() {
        let root = scratch_directory("git-commit-link-diff");
        let Some(()) = repository(&root) else { return };
        fs::write(root.join("link-name"), "original.md").unwrap();
        git::commit(&root, "regular file", true, false).unwrap();
        let first = git::head_sha(&root).unwrap();
        let blob = git::resolve(&root, "HEAD:link-name").unwrap();
        let cache_entry = format!("120000,{blob},link-name");
        git_ok(&root, &["update-index", "--cacheinfo", &cache_entry]);
        git::commit(&root, "link", false, false).unwrap();
        let head = git::head_sha(&root).unwrap();
        let detail = git::detail(&root, &head).unwrap();
        let preview = read_commit(&root, Some(&first), &head, &detail.files[0]);
        assert!(!preview.left_label.contains(pick("（リンク）", "(link)")));
        assert!(preview.right_label.contains(pick("（リンク）", "(link)")));
        assert_eq!(
            preview.text.unwrap(),
            ("original.md".into(), "original.md".into())
        );
    }
}
