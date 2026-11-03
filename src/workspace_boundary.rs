//! Workspace document boundaries. Resolve filesystem aliases before comparing
//! path components; a lexical prefix alone permits junction/symlink escapes.
//!
//! These are admission checks, not a filesystem sandbox: callers must check
//! again immediately before writes because another process can change links.

use std::path::{Path, PathBuf};

/// Whether an existing regular file resolves inside a registered directory.
/// Missing/unreadable paths and an active Workspace with no roots fail closed.
pub fn existing_file_allowed(roots: &[PathBuf], path: &Path) -> bool {
    path.is_file()
        && path
            .canonicalize()
            .is_ok_and(|resolved| within_roots(roots, &resolved))
}

/// Whether a file may be written here. For a new file its immediate parent
/// must already exist, matching Save As (which does not create directories).
/// A dangling link is not a new file and must never be admitted via its parent.
pub fn save_target_allowed(roots: &[PathBuf], path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => existing_file_allowed(roots, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(name) = path.file_name() else {
                return false;
            };
            let Some(parent) = path.parent() else {
                return false;
            };
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            if !parent.is_dir() {
                return false;
            }
            parent
                .canonicalize()
                .is_ok_and(|resolved| within_roots(roots, &resolved.join(name)))
        }
        Err(_) => false,
    }
}

/// RFN01-85: the registered folders resolved once, for marking many tree rows.
/// A root that is missing or cannot be resolved is left out, as
/// [`within_roots`] would refuse it.
pub fn resolved_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| root.is_dir())
        .filter_map(|root| root.canonicalize().ok())
        .collect()
}

/// RFN01-85: whether the files of `folder` lie inside the Workspace, for
/// drawing them. **Display only** — asked once per folder rather than once per
/// file, so a file that is itself a link out is not caught here. Opening still
/// goes through [`existing_file_allowed`], file by file.
pub fn folder_within_resolved(resolved: &[PathBuf], folder: &Path) -> bool {
    folder
        .canonicalize()
        .is_ok_and(|folder| resolved.iter().any(|root| folder.starts_with(root)))
}

fn within_roots(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| {
        root.is_dir()
            && root
                .canonicalize()
                .is_ok_and(|resolved| path.starts_with(resolved))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rfn-boundary-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("root/sub")).unwrap();
            std::fs::create_dir_all(path.join("root-sibling")).unwrap();
            std::fs::write(path.join("root/inside.md"), "inside").unwrap();
            std::fs::write(path.join("root-sibling/outside.md"), "outside").unwrap();
            Self(path)
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.0.join("root")]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn existing_files_require_component_boundary_and_a_real_file() {
        let f = Fixture::new();
        assert!(existing_file_allowed(
            &f.roots(),
            &f.0.join("root/inside.md")
        ));
        assert!(!existing_file_allowed(
            &f.roots(),
            &f.0.join("root-sibling/outside.md")
        ));
        assert!(!existing_file_allowed(
            &f.roots(),
            &f.0.join("root/missing.md")
        ));
        assert!(!existing_file_allowed(&f.roots(), &f.0.join("root/sub")));
        assert!(!existing_file_allowed(&[], &f.0.join("root/inside.md")));
    }

    #[test]
    fn folders_are_marked_against_the_roots_resolved_once() {
        let f = Fixture::new();
        let resolved = resolved_roots(&f.roots());
        assert!(folder_within_resolved(&resolved, &f.0.join("root")));
        assert!(folder_within_resolved(&resolved, &f.0.join("root/sub")));
        assert!(!folder_within_resolved(
            &resolved,
            &f.0.join("root-sibling")
        ));
        // A parent of the root holds files outside it.
        assert!(!folder_within_resolved(&resolved, &f.0));
        assert!(!folder_within_resolved(
            &resolved,
            &f.0.join("root/missing")
        ));
        let gone = resolved_roots(&[f.0.join("not-there")]);
        assert!(gone.is_empty());
        assert!(!folder_within_resolved(&gone, &f.0.join("root")));
    }

    #[test]
    fn save_as_admits_new_files_only_in_existing_registered_directories() {
        let f = Fixture::new();
        assert!(save_target_allowed(
            &f.roots(),
            &f.0.join("root/sub/new.md")
        ));
        assert!(save_target_allowed(&f.roots(), &f.0.join("root/inside.md")));
        assert!(!save_target_allowed(
            &f.roots(),
            &f.0.join("root/missing/new.md")
        ));
        assert!(!save_target_allowed(
            &f.roots(),
            &f.0.join("root/../root-sibling/new.md")
        ));
        assert!(!save_target_allowed(&f.roots(), &f.0.join("root/sub")));
    }

    #[test]
    fn multiple_roots_and_missing_root_fail_closed() {
        let f = Fixture::new();
        let roots = vec![f.0.join("missing"), f.0.join("root-sibling")];
        assert!(existing_file_allowed(
            &roots,
            &f.0.join("root-sibling/outside.md")
        ));
        assert!(!existing_file_allowed(&roots, &f.0.join("root/inside.md")));
        assert!(!save_target_allowed(&[], &f.0.join("root/new.md")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_and_dangling_links_are_rejected() {
        let f = Fixture::new();
        std::os::unix::fs::symlink(f.0.join("root-sibling"), f.0.join("root/escape")).unwrap();
        std::os::unix::fs::symlink(
            f.0.join("root-sibling/missing.md"),
            f.0.join("root/dangling.md"),
        )
        .unwrap();
        assert!(!existing_file_allowed(
            &f.roots(),
            &f.0.join("root/escape/outside.md")
        ));
        assert!(!save_target_allowed(
            &f.roots(),
            &f.0.join("root/escape/new.md")
        ));
        assert!(!save_target_allowed(
            &f.roots(),
            &f.0.join("root/dangling.md")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn junction_escapes_are_rejected_for_open_and_save_as() {
        let f = Fixture::new();
        // A directory junction does not require Developer Mode or symlink
        // privilege. Both paths are newly-created fixture directories.
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(f.0.join("root").join("escape"))
            .arg(f.0.join("root-sibling"))
            .output()
            .unwrap();
        assert!(output.status.success(), "junction fixture: {:?}", output);
        assert!(!existing_file_allowed(
            &f.roots(),
            &f.0.join("root/escape/outside.md")
        ));
        assert!(!save_target_allowed(
            &f.roots(),
            &f.0.join("root/escape/new.md")
        ));
        std::fs::remove_dir(f.0.join("root/escape")).unwrap();
    }
}
