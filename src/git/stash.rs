//! RFN01-150: hold Git's ref lock while identifying and removing a stash.

use super::{
    CreatedStash, GitError, ManagedChild, Stash, check_stash, checked, command, run, run_command,
    stash_changed, stashes,
};
use crate::file_io::stage_atomic_write;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

const STASH_REF: &str = "refs/stash";

pub(super) fn push(root: &Path, message: &str) -> Result<Option<CreatedStash>, GitError> {
    let mut git = command(root);
    // The ref trace comes from this Git process. Reading refs/stash after a
    // successful push alone could attribute a concurrent push to this action.
    git.env("GIT_TRACE_REFS", "1")
        .args(["stash", "push", "--include-untracked"]);
    if !message.trim().is_empty() {
        git.args(["-m", message]);
    }
    let output = run_command(git, None, None)?;
    if !output.ok {
        let error = output
            .stderr
            .lines()
            .filter(|line| ref_trace(line).is_none())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(super::failure_of(&error));
    }
    let Some((oid, subject)) = created_ref(&output.text(), &output.stderr) else {
        return Ok(None);
    };
    let Ok(list) = stashes(root) else {
        return Ok(None);
    };
    let mut matches = list.into_iter().filter(|stash| stash.oid == oid);
    let created = matches.next().filter(|stash| stash.message == subject);
    Ok(created
        .filter(|_| matches.next().is_none())
        .map(CreatedStash))
}

fn ref_trace(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(" refs/debug.c:")?;
    let (_, event) = rest.split_once(' ')?;
    Some(event.trim_start())
}

/// Unknown trace formats and no-op pushes leave no target for Undo/Redo.
fn created_ref(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let subject = stdout
        .strip_prefix("Saved working directory and index state ")?
        .split(|ch: char| ch.is_ascii_whitespace() || ch == '\x0b')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut trace = stderr.lines().filter_map(ref_trace);
    let update = trace.find_map(|event| event.strip_prefix("0: refs/stash "))?;
    let (_, update) = update.split_once(" -> ")?;
    let (oid, detail) = update.split_once(' ')?;
    let (_, message) = detail.split_once(") \"")?;
    if !valid_oid(oid.as_bytes())
        || message.strip_suffix('"')? != subject
        || trace.next()? != "}"
        || trace.next()? != "finish: 0"
        || trace.any(|event| event.starts_with("0: refs/stash "))
    {
        return None;
    }
    Some((oid.to_owned(), subject))
}

pub(super) fn drop_locked(
    root: &Path,
    expected: &Stash,
    after_prepare: impl FnOnce(),
) -> Result<(), GitError> {
    let common = common_directory(root)?;
    if !common.is_absolute() {
        return Err(stash_changed());
    }
    require_files_refs(root)?;

    let list = stashes(root)?;
    let tip = list.first().ok_or_else(stash_changed)?;
    if !valid_oid(tip.oid.as_bytes()) {
        return Err(stash_changed());
    }
    let sole_entry = list.len() == 1;
    let reference = common.join(STASH_REF);
    let log = common.join("logs").join(STASH_REF);
    with_ref_lock(root, &common, &tip.oid, sole_entry, || {
        // Validate the selected name and OID only after Git holds its ref lock.
        // This is shared by every linked worktree.
        after_prepare();
        if common_directory(root)? != common {
            return Err(stash_changed());
        }
        check_stash(root, expected)?;
        check_direct_ref(&reference, &tip.oid)?;
        let _log_lock = ReflogLock::acquire(&log)?;
        let original = read_log(&log)?;
        let entries = parse_log(&original, tip.oid.as_bytes())?;
        let mut found = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| new_oid(entry, tip.oid.len()) == expected.oid.as_bytes());
        let (index, _) = found.next().ok_or_else(stash_changed)?;
        if found.next().is_some() || sole_entry != (entries.len() == 1) {
            return Err(stash_changed());
        }
        if sole_entry {
            // Git owns deletion of both the reflog and the ref, even if packed.
            return Ok(());
        }

        let new_tip =
            (index == entries.len() - 1).then(|| new_oid(entries[index - 1], tip.oid.len()));
        let rewritten = without_entry(&entries, index, tip.oid.len(), original.len());
        replace_entries(&log, &reference, &original, &rewritten, new_tip)
    })
}

fn common_directory(root: &Path) -> Result<PathBuf, GitError> {
    let path = checked(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
    )?;
    Ok(PathBuf::from(path.trim_end_matches(['\r', '\n'])))
}

pub(super) fn require_files_refs(root: &Path) -> Result<(), GitError> {
    if uses_files_refs(root)? {
        return Ok(());
    }
    Err(GitError::Failed(
        crate::i18n::pick(
            "この Git の参照形式では Stash を安全に削除できません。Stash は保持しました。",
            "Safe stash deletion is unavailable for this Git reference format. The stash was kept.",
        )
        .to_owned(),
    ))
}

fn uses_files_refs(root: &Path) -> Result<bool, GitError> {
    let format = checked(root, &["rev-parse", "--show-ref-format"], None)?;
    match format.trim() {
        "files" => Ok(true),
        "--show-ref-format" => {
            // Git versions predating this option echo it. Their default
            // files backend has no ref-storage extension in the config.
            let extension = run(
                root,
                &["config", "--local", "--get", "extensions.refStorage"],
                None,
            )?;
            Ok(!extension.ok && extension.stdout.is_empty() && extension.stderr.is_empty())
        }
        _ => Ok(false),
    }
}

struct ReflogLock(PathBuf);

impl ReflogLock {
    fn acquire(log: &Path) -> Result<Self, GitError> {
        let path = log.with_extension("lock");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(storage_error)?;
        Ok(Self(path))
    }
}

impl Drop for ReflogLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn with_ref_lock(
    root: &Path,
    common: &Path,
    tip: &str,
    delete: bool,
    locked: impl FnOnce() -> Result<(), GitError>,
) -> Result<(), GitError> {
    let mut git = command(root);
    // Lock the very same storage directory used for file updates, even
    // if the worktree's .git pointer changes after resolution.
    git.env("GIT_DIR", common)
        .env("GIT_COMMON_DIR", common)
        .args(["update-ref", "--stdin", "--no-deref"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut git).map_err(storage_error)?;
    let mut input = child.process.stdin.take().expect("piped update-ref input");
    let mut output = BufReader::new(
        child
            .process
            .stdout
            .take()
            .expect("piped update-ref output"),
    );
    let action = if delete { "delete" } else { "verify" };
    let result = (|| {
        writeln!(input, "start\n{action} {STASH_REF} {tip}\nprepare").map_err(storage_error)?;
        expect_response(&mut output, "start: ok")?;
        expect_response(&mut output, "prepare: ok")?;
        locked()?;
        if delete {
            writeln!(input, "commit").map_err(storage_error)?;
            expect_response(&mut output, "commit: ok")?;
        }
        Ok(())
    })();
    // EOF aborts an explicit transaction that hasn't committed. Wait for Git
    // to release its locks even when validation or writing failed.
    drop(input);
    let exit = child.process.wait();
    result?;
    if !exit.map_err(storage_error)?.success() {
        return Err(stash_changed());
    }
    Ok(())
}

fn expect_response(output: &mut impl BufRead, expected: &str) -> Result<(), GitError> {
    let mut line = String::new();
    output.read_line(&mut line).map_err(storage_error)?;
    if line.trim_end() == expected {
        Ok(())
    } else {
        Err(stash_changed())
    }
}

fn check_direct_ref(path: &Path, expected: &str) -> Result<(), GitError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {
            let bytes = fs::read(path).map_err(storage_error)?;
            if bytes.strip_suffix(b"\n") != Some(expected.as_bytes()) {
                return Err(stash_changed());
            }
        }
        // A packed direct ref need not have a loose file. Git verified it.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(stash_changed()),
        Err(error) => return Err(storage_error(error)),
    }
    Ok(())
}

fn read_log(path: &Path) -> Result<Vec<u8>, GitError> {
    if !fs::symlink_metadata(path).map_err(storage_error)?.is_file() {
        return Err(stash_changed());
    }
    fs::read(path).map_err(storage_error)
}

fn parse_log<'a>(bytes: &'a [u8], tip: &[u8]) -> Result<Vec<&'a [u8]>, GitError> {
    let width = tip.len();
    let entries: Vec<_> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
    if entries.is_empty()
        || entries.iter().any(|entry| {
            entry.len() <= 2 * width + 2
                || entry.last() != Some(&b'\n')
                || entry[width] != b' '
                || entry[2 * width + 1] != b' '
                || !valid_oid(&entry[..width])
                || !valid_oid(new_oid(entry, width))
        })
        || new_oid(entries[entries.len() - 1], width) != tip
    {
        return Err(stash_changed());
    }
    Ok(entries)
}

fn valid_oid(bytes: &[u8]) -> bool {
    matches!(bytes.len(), 40 | 64) && bytes.iter().all(u8::is_ascii_hexdigit)
}

fn new_oid(entry: &[u8], width: usize) -> &[u8] {
    &entry[width + 1..2 * width + 1]
}

fn without_entry(entries: &[&[u8]], index: usize, width: usize, capacity: usize) -> Vec<u8> {
    let mut rewritten = Vec::with_capacity(capacity - entries[index].len());
    for (at, entry) in entries.iter().enumerate() {
        if at == index {
            continue;
        }
        if at == index + 1 {
            // Reconnect the next entry to the previous retained OID, as
            // Git does when rewriting its reflog.
            if index == 0 {
                rewritten.resize(width, b'0');
            } else {
                rewritten.extend_from_slice(new_oid(entries[index - 1], width));
            }
            rewritten.extend_from_slice(&entry[width..]);
        } else {
            rewritten.extend_from_slice(entry);
        }
    }
    rewritten
}

fn replace_entries(
    log: &Path,
    reference: &Path,
    original: &[u8],
    rewritten: &[u8],
    new_tip: Option<&[u8]>,
) -> Result<(), GitError> {
    let update_tip = new_tip
        .map(|oid| {
            let mut bytes = oid.to_vec();
            bytes.push(b'\n');
            Ok::<_, io::Error>((
                stage_atomic_write(reference, &bytes)?,
                stage_atomic_write(log, original)?,
            ))
        })
        .transpose()
        .map_err(storage_error)?;
    let update_log = stage_atomic_write(log, rewritten).map_err(storage_error)?;
    if read_log(log)? != original {
        return Err(stash_changed());
    }
    update_log.publish().map_err(storage_error)?;
    if let Some((tip, rollback)) = update_tip
        && let Err(error) = tip.publish()
    {
        // Match Git's publication order. A failed ref update leaves the
        // original tip reachable. Restore its log while both locks are held.
        if read_log(log)? == rewritten {
            rollback.publish().map_err(storage_error)?;
        }
        return Err(storage_error(error));
    }
    Ok(())
}

fn storage_error(error: io::Error) -> GitError {
    GitError::Failed(crate::say!(
        "Stash の更新を完了できませんでした: {error}",
        "Could not complete the stash update: {error}"
    ))
}
