//! Keep the bytes Git generated for each hunk. Only presentation uses decoded text.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::git::{self, GitError};
use crate::{MAX_DOCUMENT_CHARACTERS, file_io, say};

pub(crate) struct Patch {
    path: String,
    pub staged: bool,
    bytes: Vec<u8>,
    header_end: usize,
    pub hunks: Vec<Hunk>,
}

pub(crate) struct Hunk {
    pub heading: String,
    pub lines: Vec<(i32, String)>,
    bytes: Range<usize>,
}

impl Patch {
    pub fn read(root: &Path, path: &str, staged: bool) -> Option<Self> {
        let bytes = git::hunk_patch(root, path, staged).ok()?;
        let mut lines = bytes.split_inclusive(|&byte| byte == b'\n');
        let file = lines.next()?;
        let index = lines.next()?;
        let old = lines.next()?;
        let new = lines.next()?;
        // One regular file, with the same mode on each side. Never stage a file-level change here.
        if !file.starts_with(b"diff --git ")
            || !index.starts_with(b"index ")
            || !(index.ends_with(b" 100644\n") || index.ends_with(b" 100755\n"))
            || !old.starts_with(b"--- ")
            || !new.starts_with(b"+++ ")
        {
            return None;
        }
        let header_end = file.len() + index.len() + old.len() + new.len();
        let mut offset = header_end;
        let mut hunks: Vec<Hunk> = Vec::new();
        for line in lines {
            if line.starts_with(b"@@ ") {
                let heading_end = line[3..].windows(3).position(|p| p == b" @@")? + 6;
                hunks.push(Hunk {
                    heading: std::str::from_utf8(&line[..heading_end]).ok()?.to_owned(),
                    lines: Vec::new(),
                    bytes: offset..offset + line.len(),
                });
            } else {
                let hunk = hunks.last_mut()?;
                let (prefix, text) = line.split_first()?;
                let kind = match prefix {
                    b' ' => 0,
                    b'-' => 1,
                    b'+' => 2,
                    b'\\' if text == b" No newline at end of file\n" => 3,
                    _ => return None,
                };
                let (text, _) = file_io::decode(text, MAX_DOCUMENT_CHARACTERS).ok()?;
                if text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
                {
                    return None;
                }
                let text = text.strip_suffix('\n').unwrap_or(&text);
                hunk.lines.push((kind, text.to_owned()));
                hunk.bytes.end = offset + line.len();
            }
            offset += line.len();
        }
        if hunks.is_empty() {
            return None;
        }
        Some(Self {
            path: path.to_owned(),
            staged,
            bytes,
            header_end,
            hunks,
        })
    }

    pub fn apply(&self, root: &Path, block: usize) -> Result<(), GitError> {
        let hunk = self.hunks.get(block).ok_or_else(stale)?;
        let mut bytes = self.bytes[..self.header_end].to_vec();
        bytes.extend_from_slice(&self.bytes[hunk.bytes.clone()]);

        let index = git::index_path(root)?;
        let lock = IndexLock::acquire(&index)?;
        self.verify(root)?;
        git::apply_hunk(root, lock.path(), &bytes, self.staged)?;
        self.verify(root)?;
        lock.install(&index)
    }

    fn verify(&self, root: &Path) -> Result<(), GitError> {
        if git::hunk_patch(root, &self.path, self.staged)? == self.bytes {
            Ok(())
        } else {
            Err(stale())
        }
    }
}

fn stale() -> GitError {
    GitError::Failed(say!(
        "Gitの差分が変わりました。ファイル行を開き直して変更ブロックを選んでください。",
        "The Git diff changed. Reopen the file row and select a block again."
    ))
}

// Git itself writes the replacement index through GIT_INDEX_FILE. The real index.lock
// protects the comparison and install against other Git writers in this worktree.
struct IndexLock(Option<PathBuf>);

impl IndexLock {
    fn acquire(index: &Path) -> Result<Self, GitError> {
        let mut name = index.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    GitError::Failed(say!(
                        "別のGit操作がインデックスを使用中です。終わってから差分を開き直してください。",
                        "Another Git operation holds the index lock. Reopen the diff once it finishes."
                    ))
                } else {
                    index_error(error)
                }
            })?;
        let lock = Self(Some(path));
        let copied = File::open(index).and_then(|mut original| io::copy(&mut original, &mut file));
        drop(file);
        copied.map_err(index_error)?;
        Ok(lock)
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("held index lock")
    }

    fn install(mut self, index: &Path) -> Result<(), GitError> {
        fs::rename(self.path(), index).map_err(index_error)?;
        self.0 = None;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn index_error(error: io::Error) -> GitError {
    GitError::Failed(say!(
        "Gitインデックスを更新できません。{error}",
        "Could not update the Git index. {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{git_ok, repository};
    use crate::saving::scratch_directory;

    fn document(first: &str, last: &str, eol: &str) -> Vec<u8> {
        format!("{first}{eol}{}{last}", format!("context{eol}").repeat(16)).into_bytes()
    }

    fn index_text(root: &Path, path: &str) -> Vec<u8> {
        git::change_blob(root, "", path).unwrap()
    }

    #[test]
    fn applies_and_reverses_only_the_chosen_git_hunk() {
        let root = scratch_directory("git-hunk-roundtrip");
        let Some(()) = repository(&root) else { return };
        let path = "日本語 の原稿.txt";
        fs::write(root.join(".gitattributes"), "*.txt text eol=crlf ident\n").unwrap();
        fs::write(
            root.join(path),
            document("first base", "$Id$ last base", "\r\n"),
        )
        .unwrap();
        git::commit(&root, "base", true, false).unwrap();
        let working = document("first edit", "$Id: generated $ last edit", "\r\n");
        fs::write(root.join(path), &working).unwrap();

        let patch = Patch::read(&root, path, false).unwrap();
        assert_eq!(patch.hunks.len(), 2);
        assert!(patch.hunks[1].lines.iter().any(|(kind, _)| *kind == 3));
        patch.apply(&root, 1).unwrap();
        assert_eq!(
            index_text(&root, path),
            document("first base", "$Id$ last edit", "\n")
        );
        assert_eq!(fs::read(root.join(path)).unwrap(), working);

        let rest = Patch::read(&root, path, false).unwrap();
        assert_eq!(rest.hunks.len(), 1);
        rest.apply(&root, 0).unwrap();
        let staged = Patch::read(&root, path, true).unwrap();
        assert_eq!(staged.hunks.len(), 2);
        staged.apply(&root, 1).unwrap();
        assert_eq!(
            index_text(&root, path),
            document("first edit", "$Id$ last base", "\n")
        );
        assert_eq!(fs::read(root.join(path)).unwrap(), working);
    }

    #[test]
    fn rejects_a_stale_patch_and_respects_an_external_lock() {
        let root = scratch_directory("git-hunk-stale");
        let Some(()) = repository(&root) else { return };
        let path = "original.txt";
        fs::write(root.join(path), "base\n").unwrap();
        git::commit(&root, "base", true, false).unwrap();
        fs::write(root.join(path), "edit\n").unwrap();
        let patch = Patch::read(&root, path, false).unwrap();

        let lock = git::index_path(&root).unwrap().with_file_name("index.lock");
        fs::write(&lock, b"external lock").unwrap();
        assert!(patch.apply(&root, 0).is_err());
        assert_eq!(fs::read(&lock).unwrap(), b"external lock");
        fs::remove_file(&lock).unwrap();

        fs::write(root.join(path), "another edit\n").unwrap();
        assert_eq!(patch.apply(&root, 0), Err(stale()));
        assert_eq!(index_text(&root, path), b"base\n");
        assert!(!lock.exists());

        fs::write(root.join(path), "edit\n").unwrap();
        fs::write(root.join("other.txt"), "other\n").unwrap();
        git_ok(&root, &["add", "--", "other.txt"]);
        patch.apply(&root, 0).unwrap();
        assert_eq!(index_text(&root, path), b"edit\n");
        assert_eq!(index_text(&root, "other.txt"), b"other\n");
    }
}
