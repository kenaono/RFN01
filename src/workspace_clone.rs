//! One explicitly requested repository clone. No Workspace mutation happens here.
//! Results contain bounded generic messages; Git output and repository URLs are
//! never returned to diagnostics because either can contain authentication data.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

pub struct Job {
    result: Receiver<Result<PathBuf, String>>,
    finished: bool,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl Job {
    pub fn start(url: String, destination: PathBuf) -> Result<Self, String> {
        validate_url(&url)?;
        validate_destination(&destination)?;
        let (sender, result) = mpsc::channel();
        let child = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_child = child.clone();
        let worker_cancelled = cancelled.clone();
        std::thread::Builder::new()
            .name("workspace-clone".into())
            .spawn(move || {
                let outcome =
                    clone_repository(&url, &destination, &worker_child, &worker_cancelled);
                let _ = sender.send(outcome);
            })
            .map_err(|_| "Could not start repository download.".to_string())?;
        Ok(Self {
            result,
            finished: false,
            child,
            cancelled,
        })
    }

    fn probe(url: String) -> Result<Self, String> {
        validate_url(&url)?;
        let (sender, result) = mpsc::channel();
        let child = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_child = child.clone();
        let worker_cancelled = cancelled.clone();
        std::thread::Builder::new()
            .name("workspace-url-check".into())
            .spawn(move || {
                let result =
                    run_git(&url, None, &worker_child, &worker_cancelled).map(|()| PathBuf::new());
                let _ = sender.send(result);
            })
            .map_err(|_| "Could not start repository validation.".to_string())?;
        Ok(Self {
            result,
            finished: false,
            child,
            cancelled,
        })
    }

    pub fn try_result(&mut self) -> Option<Result<PathBuf, String>> {
        if self.finished {
            return None;
        }
        match self.result.try_recv() {
            Ok(result) => {
                self.finished = true;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err("Repository download stopped unexpectedly.".into()))
            }
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut process) = child.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

/// One field's validation request. Editing/cancelling drops the old Git job;
/// comparing the URL on delivery prevents a queued stale result from publishing.
#[derive(Default)]
pub struct UrlCheck {
    pending: Option<(String, Job)>,
}
impl UrlCheck {
    pub fn clear(&mut self) {
        self.pending = None;
    }
    pub fn start(&mut self, url: &str) -> Result<(), String> {
        self.clear();
        self.pending = Some((url.to_owned(), Job::probe(url.to_owned())?));
        Ok(())
    }
    pub fn poll(&mut self, current_url: &str) -> Option<Result<(), String>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(url, _)| url != current_url)
        {
            self.clear();
            return None;
        }
        let result = self.pending.as_mut()?.1.try_result()?;
        self.clear();
        Some(result.map(|_| ()))
    }
}

pub fn validate_url(url: &str) -> Result<(), String> {
    let invalid =
        || "Enter an HTTPS or SSH repository URL without passwords or access tokens.".to_string();
    if url.is_empty()
        || url.len() > 4096
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid());
    }
    // Query strings/fragments and backslashes are not required for repository
    // addresses and can disguise credentials or change URL interpretation.
    if url.contains(['?', '#', '\\']) {
        return Err(invalid());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let Some((authority, path)) = rest.split_once('/') else {
            return Err(invalid());
        };
        if authority.is_empty() || authority.contains('@') || path.is_empty() {
            return Err(invalid());
        }
        if !valid_host_port(authority) {
            return Err(invalid());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let Some((authority, path)) = rest.split_once('/') else {
            return Err(invalid());
        };
        if path.is_empty() {
            return Err(invalid());
        }
        let host = if let Some((user, host)) = authority.split_once('@') {
            if user.is_empty()
                || !user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            {
                return Err(invalid());
            }
            host
        } else {
            authority
        };
        if !valid_host_port(host) {
            return Err(invalid());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("git@") {
        let Some((host, path)) = rest.split_once(':') else {
            return Err(invalid());
        };
        if valid_host(host) && !path.is_empty() && !path.starts_with('-') {
            return Ok(());
        }
    }
    Err(invalid())
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
}

fn valid_host_port(authority: &str) -> bool {
    if let Some(ipv6) = authority.strip_prefix('[') {
        let Some((host, suffix)) = ipv6.split_once(']') else {
            return false;
        };
        return host.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }
    if let Some((host, port)) = authority.split_once(':') {
        valid_host(host) && valid_port(port)
    } else {
        valid_host(authority)
    }
}

fn valid_port(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|port| port > 0)
}

pub fn validate_destination(destination: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(destination)
        .map_err(|_| "Choose an existing empty folder.".to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse(&metadata) {
        return Err("Choose a real folder, not a folder link.".into());
    }
    let mut entries = std::fs::read_dir(destination)
        .map_err(|_| "The destination folder cannot be read.".to_string())?;
    match entries.next() {
        None => Ok(()),
        Some(Ok(_)) => Err("The destination folder must be empty.".into()),
        Some(Err(_)) => Err("The destination folder cannot be read.".into()),
    }
}

#[cfg(windows)]
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &std::fs::Metadata) -> bool {
    false
}

fn clone_repository(
    url: &str,
    destination: &Path,
    child: &Arc<Mutex<Option<Child>>>,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    // Recheck immediately before spawning; a failed clone is never cleaned up
    // automatically, because newly created files may already belong to a user.
    validate_destination(destination)?;
    run_git(url, None, child, cancelled)?;
    validate_destination(destination)?;
    run_git(url, Some(destination), child, cancelled)?;
    if !destination.is_dir() || !destination.join(".git").is_dir() {
        return Err("Git finished, but the downloaded repository could not be confirmed.".into());
    }
    Ok(destination.to_path_buf())
}

// Probe with Git itself: .git suffixes and provider-specific URL shapes are
// neither necessary nor sufficient evidence that a URL is a repository.
fn run_git(
    url: &str,
    destination: Option<&Path>,
    child: &Arc<Mutex<Option<Child>>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "credential.interactive=never",
            "-c",
            "core.hooksPath=NUL",
            "-c",
            "init.templateDir=",
            if destination.is_some() {
                "clone"
            } else {
                "ls-remote"
            },
            "--",
        ])
        .arg(url)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(destination) = destination {
        command.arg(destination);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    // Discard subprocess text rather than trying to redact unknown credential
    // formats. Fixed messages are both bounded and safe to expose in the UI.
    {
        // Holding this lock across spawn means Drop cannot miss a child which
        // appears immediately after its cancellation check.
        let mut held = child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cancelled.load(Ordering::Acquire) {
            return Err("Repository download cancelled.".into());
        }
        *held = Some(command.spawn().map_err(|_| {
            "Could not start Git. Check that Git is installed and available.".to_string()
        })?);
    }
    let status = loop {
        {
            let mut held = child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cancelled.load(Ordering::Acquire) {
                if let Some(mut process) = held.take() {
                    let _ = process.kill();
                    let _ = process.wait();
                }
                return Err("Repository download cancelled.".into());
            }
            let Some(process) = held.as_mut() else {
                return Err("Repository download stopped.".into());
            };
            match process.try_wait() {
                Ok(Some(status)) => {
                    *held = None;
                    break status;
                }
                Ok(None) => {}
                Err(_) => {
                    if let Some(mut process) = held.take() {
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    return Err("Could not read the repository download result.".into());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    if !status.success() {
        if destination.is_none() {
            return Err("Could not confirm a Git repository. Check the repository URL and access permissions.".into());
        }
        return Err("Repository download failed. Check the URL, access permission, and connection. Any partial files were left in the destination.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_validation_discards_stale_and_cancelled_results() {
        let queued = || {
            let (sender, result) = mpsc::channel();
            sender.send(Ok(PathBuf::new())).unwrap();
            UrlCheck {
                pending: Some((
                    "https://host/old".into(),
                    Job {
                        result,
                        finished: false,
                        child: Arc::new(Mutex::new(None)),
                        cancelled: Arc::new(AtomicBool::new(false)),
                    },
                )),
            }
        };
        let mut stale = queued();
        assert!(stale.poll("https://host/new").is_none());
        assert!(stale.pending.is_none());
        let mut cancelled = queued();
        cancelled.clear();
        assert!(cancelled.poll("https://host/old").is_none());
        let mut current = queued();
        assert_eq!(current.poll("https://host/old"), Some(Ok(())));
    }

    #[test]
    fn accepts_repository_network_addresses() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://server:8443/team/repo",
            "ssh://git@host/team/repo",
            "ssh://host:2222/team/repo",
            "ssh://git@[::1]:22/repo",
            "git@host:team/repo.git",
        ] {
            assert!(validate_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn rejects_credentials_local_protocols_and_argument_injection() {
        for url in [
            "",
            "--upload-pack=bad",
            "ext::bad",
            "file:///tmp/repo",
            "http://host/repo",
            "https://user:token@host/repo",
            "https://user@host/repo",
            "ssh://git:password@host/repo",
            "https://host/repo?token=secret",
            "https://host/repo#secret",
            "https://host/repo\n--bad",
            "https://host/space here",
            "https://host",
            "https:///repo",
            "git@:repo",
            "git@host:",
            "ssh://-host/repo",
            "https://host:0/repo",
            "https://host:99999/repo",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn destination_must_exist_be_directory_and_be_empty() {
        let root = std::env::temp_dir().join(format!(
            "rfn-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(validate_destination(&root).is_err());
        std::fs::create_dir(&root).unwrap();
        assert!(validate_destination(&root).is_ok());
        let file = root.join("keep.txt");
        std::fs::write(&file, "keep").unwrap();
        assert!(validate_destination(&root).is_err());
        assert!(validate_destination(&file).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn local_repository_worker_success_and_failure_do_not_need_network() {
        let root = std::env::temp_dir().join(format!(
            "rfn-clone-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        let destination = root.join("destination");
        let failure = root.join("failure");
        for dir in [&root, &source, &destination, &failure] {
            std::fs::create_dir(dir).unwrap();
        }
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            command
                .current_dir(&source)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            assert!(command.status().unwrap().success());
        };
        git(&["-c", "init.templateDir=", "init", "--quiet"]);
        std::fs::write(source.join("README.md"), "local clone fixture").unwrap();
        git(&["add", "README.md"]);
        git(&[
            "-c",
            "user.name=Clone Test",
            "-c",
            "user.email=clone-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        let child = Arc::new(Mutex::new(None));
        let cancelled = AtomicBool::new(false);
        // Exercise the worker directly with a local source. Public Job::start
        // still rejects local paths; these tests never contact a remote host.
        assert_eq!(
            clone_repository(source.to_str().unwrap(), &destination, &child, &cancelled),
            Ok(destination.clone())
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "local clone fixture"
        );
        let error = clone_repository(
            root.join("missing-private-url-token").to_str().unwrap(),
            &failure,
            &child,
            &cancelled,
        )
        .unwrap_err();
        assert!(!error.contains("private-url-token"));
        assert!(failure.exists());
        assert!(
            validate_destination(&failure).is_ok(),
            "failed repository probe must not write the destination"
        );
        assert!(
            clone_repository(failure.to_str().unwrap(), &failure, &child, &cancelled).is_err(),
            "an ordinary folder is not a Git repository"
        );
        assert!(validate_destination(&failure).is_ok());
        assert!(child.lock().unwrap().is_none());
        // Resolve the generated test root before recursively removing only it.
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_worker_does_not_spawn_or_write_destination() {
        let root = std::env::temp_dir().join(format!(
            "rfn-clone-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let child = Arc::new(Mutex::new(None));
        assert!(
            clone_repository(
                "https://example.invalid/repo",
                &root,
                &child,
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert!(child.lock().unwrap().is_none());
        assert!(validate_destination(&root).is_ok());
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn worker_disconnection_is_reported_once() {
        let (sender, result) = mpsc::channel();
        drop(sender);
        let mut job = Job {
            result,
            finished: false,
            child: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        assert!(job.try_result().unwrap().is_err());
        assert!(job.try_result().is_none());
    }
}
