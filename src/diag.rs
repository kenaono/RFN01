//! Bounded runtime diagnostics: ordinary events always, detail by startup flag.
//! Diag and Perf share a run ID and rolling-file implementation; retain the
//! newest 20 runs, excluding runs still in use. Logging is
//! best effort and unbuffered. Failures must not prevent editing or saving.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Where the files go: a directory of their own, because there is one per run,
/// beside the executable rather than under the current one (see
/// [`beside_executable`]).
const DIAG_DIRECTORY: &str = "diag";

/// Rotate at this size so long sessions can still record failures and shutdown.
const DIAG_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const RETAIN_RUNS: usize = 20;
const LINE_BYTE_LIMIT: usize = 4096;

/// Runtime policy, identical in debug and release builds. Ordinary records
/// are never disabled. Unknown detail categories default to the UI group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    groups: Vec<String>,
}

impl Config {
    pub fn from_args(args: impl IntoIterator<Item = OsString>) -> Result<Self, &'static str> {
        let mut config = Self::default();
        for arg in args {
            if arg == "--" {
                break;
            }
            if arg == "--diagnostics" {
                if !config.groups.iter().any(|g| g == "all") {
                    config.groups.push("all".into());
                }
            } else if let Some(value) = arg.to_str().and_then(|s| s.strip_prefix("--diagnostics="))
            {
                for group in value.split(',') {
                    if !matches!(
                        group,
                        "all" | "render" | "input" | "terminal" | "ui" | "search" | "perf"
                    ) {
                        return Err(
                            "--diagnostics: choose all,render,input,terminal,ui,search,perf; using normal mode",
                        );
                    }
                    if !config.groups.iter().any(|g| g == group) {
                        config.groups.push(group.to_owned());
                    }
                }
            }
        }
        Ok(config)
    }

    pub fn summary(&self) -> String {
        if self.groups.is_empty() {
            "normal".into()
        } else {
            self.groups.join(",")
        }
    }

    pub fn performance(&self) -> bool {
        self.group("perf")
    }
    pub fn rendering(&self) -> bool {
        self.group("render")
    }

    fn group(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g == "all" || g == group)
    }

    fn enabled(&self, category: &str) -> bool {
        let base = category.split('.').next().unwrap_or(category);
        if matches!(
            base,
            "session"
                | "error"
                | "panic"
                | "file"
                | "work"
                | "external"
                | "encoding"
                | "spec"
                | "print"
                | "ask"
                | "answer"
        ) {
            return true;
        }
        self.group(match base {
            "geom" | "refresh" | "scroll" | "probe" => "render",
            "edit" | "pointer" | "ime" | "focus" | "lines" | "enter" => "input",
            "terminal" => "terminal",
            "find" | "search" => "search",
            _ => "ui",
        })
    }
}

/// Bound each record and prevent a path/error containing newlines from
/// manufacturing additional log records. Keep UTF-8 boundaries intact.
pub(crate) fn one_line(value: &str) -> String {
    let mut end = value.len().min(LINE_BYTE_LIMIT);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut line = value[..end].replace(['\n', '\r', '\0'], " ");
    if end < value.len() {
        line.push_str(" [truncated]");
    }
    line
}

fn run_id(name: &str) -> Option<&str> {
    let stem = if let Some(stem) = name.strip_prefix("Run_") {
        stem.strip_suffix(".lock")?
    } else {
        let stem = name
            .strip_prefix("Diag_")
            .or_else(|| name.strip_prefix("Perf_"))?
            .strip_suffix(".log")?;
        stem.strip_suffix(".prev").unwrap_or(stem)
    };
    let parts: Vec<_> = stem.split('_').collect();
    (parts.len() == 3
        && parts[0].len() == 8
        && parts[1].len() == 6
        && (3..=4).contains(&parts[2].len())
        && parts.iter().all(|s| s.bytes().all(|c| c.is_ascii_digit())))
    .then_some(stem)
}

/// One exclusive marker stays open across both writers and their rotations.
/// A diagnostic writer being briefly closed for rotation never makes its run
/// look inactive to a second process.
struct RunLease {
    file: Option<File>,
    path: PathBuf,
}

fn lease_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options
}

impl Drop for RunLease {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

/// Preflight every member before removing any. This also protects files from
/// the older build (which has no run lease) and logs held by external tools.
fn deletable(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(0x0001_0000).share_mode(5); // DELETE; SHARE_READ | SHARE_DELETE
    }
    #[cfg(not(windows))]
    options.read(true);
    options.open(path)
}

fn prune(directory: &Path, current: &str, retain: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut runs: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name();
        if let Some(id) = run_id(&name.to_string_lossy()) {
            runs.entry(id.to_owned()).or_default().push(entry.path());
        }
    }
    let mut runs: Vec<_> = runs.into_iter().collect();
    runs.sort_by_key(|(id, _)| {
        let (stamp, serial) = id.rsplit_once('_').unwrap();
        (stamp.to_owned(), serial.parse::<u32>().unwrap_or(0))
    });
    let remove = runs.len().saturating_sub(retain);
    for (id, paths) in runs.into_iter().take(remove) {
        if id == current {
            continue;
        }
        let marker = directory.join(format!("Run_{id}.lock"));
        if fs::symlink_metadata(&marker).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            continue;
        }
        let Ok(file) = lease_options().create(true).truncate(false).open(&marker) else {
            continue;
        };
        let _lease = RunLease {
            file: Some(file),
            path: marker.clone(),
        };
        let members: Vec<_> = paths.into_iter().filter(|p| p != &marker).collect();
        let handles: Result<Vec<_>, _> = members.iter().map(|p| deletable(p)).collect();
        if let Ok(_handles) = handles {
            for path in members {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn log_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE, deliberately no SHARE_DELETE.
        options.share_mode(3);
    }
    options
}

/// How many names to try for one second before giving up. Runs started in the
/// same second are the only thing this guards.
const NAME_ATTEMPTS: u32 = 1_000;

/// A path beside the executable rather than in the current directory.
///
/// The logs are read while the editor is being built, and the current
/// directory is not a place: opening a folder moves it, so one afternoon's
/// runs used to leave their files scattered wherever the person had been
/// looking. Beside the binary they are always in the one place, whatever the
/// editor is pointed at.
///
/// If the executable's own path cannot be had, the bare name is used — the
/// current directory, which is where these files went before. A log that lands
/// in the wrong folder is worth more than no log at all.
pub fn beside_executable(name: &str) -> PathBuf {
    let directory = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    match directory {
        Some(directory) => directory.join(name),
        None => PathBuf::from(name),
    }
}

/// The clock on the wall.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millisecond: u16,
}

impl LocalTime {
    /// Now, in the machine's own time zone.
    ///
    /// Local rather than UTC because the only thing this is compared against
    /// is the person's memory of when they saw the problem.
    pub fn now() -> Self {
        // SAFETY: takes nothing and returns a plain value; there is no handle
        // or pointer of ours involved.
        let now = unsafe { GetLocalTime() };
        Self {
            year: now.wYear,
            month: now.wMonth,
            day: now.wDay,
            hour: now.wHour,
            minute: now.wMinute,
            second: now.wSecond,
            millisecond: now.wMilliseconds,
        }
    }

    /// `20260817_142530`, the part of a file name that orders runs by when they
    /// happened.
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}{:02}{:02}_{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-08-17 14:25:30.123`, for reading.
    pub fn stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millisecond
        )
    }
}

/// The same bounded sink is used for event and performance records.
#[derive(Default)]
pub(crate) struct RollingLog {
    pub(crate) file: Option<File>,
    path: Option<PathBuf>,
    lease: Option<Arc<RunLease>>,
    bytes: usize,
    stopped: bool,
    header: String,
}

impl RollingLog {
    fn opened(file: File, path: PathBuf, lease: Arc<RunLease>, header: String) -> Self {
        Self {
            file: Some(file),
            path: Some(path),
            lease: Some(lease),
            header,
            ..Self::default()
        }
    }

    pub(crate) fn write(&mut self, line: &str) {
        if self.stopped || self.file.is_none() {
            return;
        }
        let line = format!("{}\n", one_line(line));
        if self.bytes + line.len() > DIAG_BYTE_LIMIT && !self.rotate() {
            return;
        }
        if self
            .file
            .as_mut()
            .unwrap()
            .write_all(line.as_bytes())
            .is_err()
        {
            self.stopped = true;
            return;
        }
        self.bytes += line.len();
    }

    fn rotate(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            self.stopped = true;
            return false;
        };
        self.file.take();
        let previous = path.with_extension("prev.log");
        let _ = fs::remove_file(&previous);
        if fs::rename(&path, &previous).is_err() {
            self.stopped = true;
            return false;
        }
        match log_options().create_new(true).open(&path) {
            Ok(mut file) => {
                let header = format!(
                    "{}\nlog continued previous={}\n",
                    one_line(&self.header),
                    previous.file_name().unwrap().to_string_lossy()
                );
                if file.write_all(header.as_bytes()).is_err() {
                    self.stopped = true;
                    return false;
                }
                self.bytes = header.len();
                self.file = Some(file);
                if let (Some(directory), Some(name)) = (path.parent(), path.file_name()) {
                    if let Some(id) = run_id(&name.to_string_lossy()) {
                        prune(directory, id, RETAIN_RUNS);
                    }
                }
                true
            }
            Err(_) => {
                self.stopped = true;
                false
            }
        }
    }
}

pub struct DiagLog {
    config: Config,
    log: RollingLog,
    lines: usize,
    started: Instant,
}

impl Default for DiagLog {
    fn default() -> Self {
        Self {
            config: Config::default(),
            log: RollingLog::default(),
            lines: 0,
            started: Instant::now(),
        }
    }
}

impl DiagLog {
    /// Begin this run's file, and return the moment it began.
    ///
    /// The caller writes the session line itself, because what belongs in it —
    /// the build, the window, the settings — is not this module's to know.
    pub fn start_with(&mut self, config: Config) -> LocalTime {
        let now = self.start_in(&beside_executable(DIAG_DIRECTORY), config.clone());
        if self.path().is_none() {
            if let Some(directory) = crate::app_data::app_directory() {
                return self.start_in(&directory.join(DIAG_DIRECTORY), config);
            }
        }
        now
    }

    pub(crate) fn start_in(&mut self, directory: &Path, config: Config) -> LocalTime {
        *self = Self {
            config,
            ..Self::default()
        };
        let now = LocalTime::now();
        let stamp = now.file_stamp();
        let _ = fs::create_dir_all(&directory);
        for serial in 1..=NAME_ATTEMPTS {
            let id = format!("{stamp}_{serial:03}");
            let name = format!("Diag_{id}.log");
            let path = directory.join(name);
            // Do not reuse a legacy ID or an orphaned performance segment.
            if [
                format!("Diag_{id}.log"),
                format!("Diag_{id}.prev.log"),
                format!("Perf_{id}.log"),
                format!("Perf_{id}.prev.log"),
            ]
            .iter()
            .any(|name| directory.join(name).exists())
            {
                continue;
            }
            let marker = directory.join(format!("Run_{id}.lock"));
            let file = match lease_options().create_new(true).open(&marker) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            };
            let lease = Arc::new(RunLease {
                file: Some(file),
                path: marker,
            });
            // `create_new` rather than asking whether the file exists: two runs
            // starting in the same second must not share a file, and between
            // the question and the answer they could.
            let opened = log_options().create_new(true).open(&path);
            match opened {
                Ok(file) => {
                    self.log = RollingLog::opened(
                        file,
                        path.clone(),
                        lease,
                        format!("diag mode={} run={id}", self.config.summary()),
                    );
                    self.lines = 0;
                    self.started = Instant::now();
                    prune(directory, &id, RETAIN_RUNS);
                    return now;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
        }
        now
    }

    /// The file being written, for the editor to say where it put it.
    pub fn path(&self) -> Option<&Path> {
        self.log.path.as_deref()
    }

    /// The caller cannot accidentally select a different directory or run ID.
    /// Normal runs leave no performance file behind.
    pub(crate) fn performance_log(&mut self, header: &str) -> RollingLog {
        if !self.config.performance() {
            return RollingLog::default();
        }
        let Some(path) = self.path() else {
            return RollingLog::default();
        };
        let Some(lease) = self.log.lease.clone() else {
            return RollingLog::default();
        };
        let name = path.file_name().unwrap().to_string_lossy();
        let id = run_id(&name).unwrap();
        let path = path.with_file_name(format!("Perf_{id}.log"));
        match log_options().create_new(true).open(&path) {
            Ok(file) => {
                let mut log = RollingLog::opened(file, path, lease, one_line(header));
                log.write(header);
                log
            }
            Err(error) => {
                self.write("error", &format!("performance log unavailable: {error}"));
                RollingLog::default()
            }
        }
    }

    /// Send panics to this run's log, wherever they happen (2026-09-06).
    ///
    /// **A panic used to leave nothing behind.** The editor draws its own
    /// window, so there is no console for the default hook to print to: the log
    /// simply stopped, and the last line before the stop was all anyone had —
    /// which meant reading it and guessing what the next line would have been.
    /// Now the last line says what happened.
    ///
    /// **Written straight to the path rather than through this struct.** A hook
    /// has to be `Send + Sync` and outlive everything, and the log lives in an
    /// `Rc<RefCell<…>>` on the UI thread; the panic may also be *inside* a
    /// borrow of it, which is a case a second borrow would turn into a second
    /// panic. Opening the file again is the one way in that cannot make things
    /// worse.
    ///
    /// Called once, after [`start_with`](DiagLog::start_with). Best effort throughout:
    /// the process is ending either way, and a hook that panicked would take
    /// the message with it.
    pub fn catch_panics(&self) {
        let Some(path) = self.log.path.clone() else {
            return;
        };
        let started = self.started;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut file) = OpenOptions::new().append(true).open(&path) {
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let at = match info.location() {
                    Some(place) => format!("{}:{}", place.file(), place.line()),
                    None => "-".to_owned(),
                };
                // The message as the panic wrote it, on one line: a log where
                // one kind of line can be pulled out with a grep stays that way
                // only if every line is one line.
                // Payloads can contain document/input content. Location alone
                // identifies the failing code without persisting that content.
                let _ = writeln!(file, "+{elapsed:11.3} panic at={at} payload=omitted");
            }
            previous(info);
        }));
    }

    /// One line: the milliseconds since the run began, what kind of event it
    /// is, and its fields.
    ///
    /// The shape is `+     12.345 category key=value key=value`. Fixed width at
    /// the front so a column of them lines up, and `key=value` throughout so
    /// that one kind of line can be pulled out of a long file without a parser.
    pub fn write(&mut self, category: &str, fields: &str) {
        if self.log.stopped || self.log.file.is_none() || !self.config.enabled(category) {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64() * 1000.0;
        let line = format!(
            "+{elapsed:11.3} {} {}",
            one_line(category),
            one_line(fields)
        );
        self.log.write(&line);
        self.lines += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = env::temp_dir().join(format!("editor-diag-{}-{serial}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(args: &[&str]) -> Config {
        Config::from_args(args.iter().map(OsString::from)).unwrap()
    }

    #[test]
    fn detail_flags_do_not_consume_file_arguments_and_respect_end_of_options() {
        let chosen = config(&[
            "原稿.md",
            "--diagnostics=render,input",
            "別の原稿.md",
            "--diagnostics=perf",
        ]);
        assert!(chosen.enabled("geom.p0"));
        assert!(chosen.enabled("pointer.p1"));
        assert!(chosen.performance());
        assert!(!chosen.enabled("terminal"));
        assert!(!chosen.enabled("find"));
        assert!(config(&["--diagnostics"]).enabled("new-category"));
        assert_eq!(config(&["--", "--diagnostics"]), Config::default());
        assert!(Config::from_args([OsString::from("--diagnostics=")]).is_err());
        assert!(Config::from_args([OsString::from("--diagnostics=render,typo")]).is_err());
        let paths = crate::paths_from_arguments([
            "--diagnostics=render".into(),
            "日本語 原稿.md".into(),
            "--".into(),
            "--diagnostics".into(),
        ]);
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("日本語 原稿.md"));
        assert!(paths[1].ends_with("--diagnostics"));
    }

    #[test]
    fn normal_mode_keeps_save_and_error_records_but_no_detail() {
        let dir = Directory::new();
        let mut log = DiagLog::default();
        log.start_in(&dir.0, Config::default());
        for category in [
            "session", "file", "work", "external", "encoding", "error", "print", "spec",
        ] {
            log.write(category, "important");
        }
        for category in [
            "refresh.p0",
            "geom.p0",
            "terminal",
            "pointer.p0",
            "edit",
            "find",
            "tab",
        ] {
            log.write(category, "detail-must-not-appear");
        }
        let written = fs::read_to_string(log.path().unwrap()).unwrap();
        assert_eq!(written.lines().count(), 8);
        assert!(!written.contains("detail-must-not-appear"));
        assert!(log.performance_log("must not create a file").file.is_none());
        assert!(
            !fs::read_dir(&dir.0)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("Perf_"))
        );
    }

    #[test]
    fn performance_shares_run_id_directory_and_rotates_without_a_line_limit() {
        let dir = Directory::new();
        let mut diag = DiagLog::default();
        diag.start_in(&dir.0, config(&["--diagnostics=perf"]));
        let diag_path = diag.path().unwrap().to_owned();
        let id = run_id(diag_path.file_name().unwrap().to_str().unwrap()).unwrap();
        let mut perf = diag.performance_log("session build=release");
        let path = dir.0.join(format!("Perf_{id}.log"));
        assert_eq!(perf.path.as_ref(), Some(&path));
        for _ in 0..25_000 {
            perf.write("edit total=1.0");
        }
        perf.write("past the old line limit");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("past the old line limit")
        );
        for _ in 0..2100 {
            perf.write(&"x".repeat(LINE_BYTE_LIMIT));
        }
        perf.write("after capacity rotation");
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("session build=release\n"));
        assert!(written.contains("log continued"));
        assert!(written.contains("after capacity rotation"));
        assert!(fs::metadata(&path).unwrap().len() <= DIAG_BYTE_LIMIT as u64);
        assert!(
            fs::metadata(path.with_extension("prev.log")).unwrap().len() <= DIAG_BYTE_LIMIT as u64
        );
        assert!(!diag_path.with_extension("prev.log").exists());
        // A second editor gets another pair and cannot truncate the first.
        let mut other = DiagLog::default();
        other.start_in(&dir.0, config(&["--diagnostics=perf"]));
        let other_perf = other.performance_log("other session");
        assert_ne!(other.path(), diag.path());
        assert_ne!(other_perf.path, perf.path);
        assert_eq!(fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn retention_counts_runs_and_removes_all_segments_together() {
        let dir = Directory::new();
        for serial in 1..=25 {
            let id = format!("20200101_000000_{serial:03}");
            let names = if serial % 2 == 0 {
                vec![format!("Diag_{id}.log")]
            } else {
                vec![
                    format!("Diag_{id}.log"),
                    format!("Diag_{id}.prev.log"),
                    format!("Perf_{id}.log"),
                    format!("Perf_{id}.prev.log"),
                ]
            };
            for name in names {
                fs::write(dir.0.join(name), "old").unwrap();
            }
        }
        fs::write(dir.0.join("Perf_notes.log"), "not ours").unwrap();
        prune(&dir.0, "", 20);
        for serial in 1..=25 {
            for prefix in ["Diag", "Perf"] {
                for extension in ["log", "prev.log"] {
                    let path = dir
                        .0
                        .join(format!("{prefix}_20200101_000000_{serial:03}.{extension}"));
                    let expected =
                        serial > 5 && (serial % 2 != 0 || (prefix == "Diag" && extension == "log"));
                    assert_eq!(path.exists(), expected, "{}", path.display());
                }
            }
        }
        assert!(dir.0.join("Perf_notes.log").exists());
    }

    #[test]
    fn run_lease_protects_the_whole_pair_during_rotation_and_until_both_writers_close() {
        let dir = Directory::new();
        let mut diag = DiagLog::default();
        diag.start_in(&dir.0, config(&["--diagnostics=perf"]));
        let path = diag.path().unwrap().to_owned();
        let perf = diag.performance_log("session");
        let perf_path = perf.path.clone().unwrap();
        let previous = path.with_extension("prev.log");
        fs::write(&previous, "older segment").unwrap();
        diag.log.file.take(); // the interval when a rotating writer is closed
        prune(&dir.0, "another-run", 0);
        assert!(path.exists() && previous.exists() && perf_path.exists());
        drop(diag);
        prune(&dir.0, "another-run", 0);
        assert!(path.exists() && previous.exists() && perf_path.exists());
        drop(perf);
        prune(&dir.0, "another-run", 0);
        assert!(!path.exists() && !previous.exists() && !perf_path.exists());
    }

    #[test]
    fn rotation_preserves_recent_detail_and_continues_important_events() {
        let dir = Directory::new();
        let mut log = DiagLog::default();
        log.start_in(&dir.0, config(&["--diagnostics=render"]));
        log.write("geom.p0", "before rotation");
        let path = log.path().unwrap().to_owned();
        for _ in 0..2100 {
            log.write("geom.p0", &"x".repeat(LINE_BYTE_LIMIT));
        }
        log.write("file", "save failed after rotation");
        assert!(
            fs::metadata(path.with_extension("prev.log")).unwrap().len() <= DIAG_BYTE_LIMIT as u64
        );
        assert!(fs::metadata(&path).unwrap().len() <= DIAG_BYTE_LIMIT as u64);
        assert!(
            fs::read_to_string(path.with_extension("prev.log"))
                .unwrap()
                .contains("before rotation")
        );
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("diag mode=render"));
        assert!(current.contains("log continued"));
        assert!(current.contains("save failed after rotation"));
        log.log.bytes = DIAG_BYTE_LIMIT;
        log.write("session", "ended");
        assert!(fs::read_to_string(&path).unwrap().contains("ended"));
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3); // two segments and the live marker
    }

    #[test]
    fn retention_only_removes_owned_completed_logs() {
        let dir = Directory::new();
        for serial in 1..=25 {
            fs::write(
                dir.0.join(format!("Diag_20200101_000000_{serial:03}.log")),
                "old",
            )
            .unwrap();
        }
        let unrelated = dir.0.join("Diag_my_notes.log");
        fs::write(&unrelated, "keep").unwrap();
        let active_path = dir.0.join("Diag_20190101_000000_001.log");
        let active_partner = dir.0.join("Perf_20190101_000000_001.prev.log");
        fs::write(&active_partner, "same legacy run").unwrap();
        let active = log_options().create_new(true).open(&active_path).unwrap();
        let mut log = DiagLog::default();
        log.start_in(&dir.0, Config::default());
        assert!(unrelated.exists());
        assert!(active_path.exists());
        assert!(active_partner.exists());
        assert!(!dir.0.join("Diag_20200101_000000_001.log").exists());
        assert!(dir.0.join("Diag_20200101_000000_025.log").exists());
        assert!(log.path().unwrap().exists());
        drop(active);
    }

    #[test]
    fn malformed_fields_are_bounded_and_stay_on_one_line() {
        let dir = Directory::new();
        let mut log = DiagLog::default();
        log.start_in(&dir.0, Config::default());
        log.write("error", &format!("first\nforged\r\n{}", "字".repeat(5000)));
        let written = fs::read_to_string(log.path().unwrap()).unwrap();
        assert_eq!(written.lines().count(), 1);
        assert!(written.len() < LINE_BYTE_LIMIT + 100);
        assert!(written.contains("[truncated]"));
    }

    #[test]
    fn unwritable_destination_does_not_prevent_editor_operations() {
        let dir = Directory::new();
        let file = dir.0.join("file");
        fs::write(&file, "not a directory").unwrap();
        let mut log = DiagLog::default();
        log.start_in(&file, Config::default());
        log.write("error", "ignored safely");
        assert!(log.log.file.is_none());
        assert!(log.path().is_none());
    }

    #[test]
    fn a_file_stamp_orders_runs_by_when_they_happened() {
        let time = LocalTime {
            year: 2026,
            month: 8,
            day: 17,
            hour: 9,
            minute: 5,
            second: 3,
            millisecond: 40,
        };
        assert_eq!(time.file_stamp(), "20260817_090503");
    }

    #[test]
    fn a_readable_stamp_keeps_the_milliseconds() {
        let time = LocalTime {
            year: 2026,
            month: 12,
            day: 1,
            hour: 23,
            minute: 59,
            second: 8,
            millisecond: 7,
        };
        assert_eq!(time.stamp(), "2026-12-01 23:59:08.007");
    }

    /// A log nobody started must cost nothing and lose nothing.
    #[test]
    fn an_unstarted_log_swallows_its_lines() {
        let mut log = DiagLog::default();
        log.write("pane", "height=1");
        assert_eq!(log.lines, 0);
        assert!(log.path().is_none());
    }

    #[test]
    fn starting_twice_in_the_same_second_takes_two_files() {
        let dir = Directory::new();
        let mut first = DiagLog::default();
        let mut second = DiagLog::default();
        first.start_in(&dir.0, Config::default());
        second.start_in(&dir.0, Config::default());
        assert_ne!(first.path().unwrap(), second.path().unwrap());
    }

    /// **A panic reaches the log** (2026-09-06). The editor draws its own
    /// window and has no console, so before this a panic left nothing behind
    /// but a log that stopped — and the last line before a stop says what was
    /// happening, never what went wrong.
    ///
    /// The hook is process-wide, so this puts back whatever was there before.
    #[test]
    fn a_panic_is_written_to_this_run_s_log() {
        let dir = Directory::new();
        let mut log = DiagLog::default();
        log.start_in(&dir.0, Config::default());
        let Some(path) = log.path().map(Path::to_path_buf) else {
            // No place to write beside the executable; nothing to hold.
            return;
        };
        log.write("session", "kind=ordinary");

        let restore = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        log.catch_panics();
        let caught = std::panic::catch_unwind(|| panic!("見えない失敗"));
        std::panic::set_hook(restore);
        assert!(caught.is_err(), "the panic still unwinds");

        let written = fs::read_to_string(&path).expect("the log is readable");
        let last = written.lines().last().expect("a line was written");
        assert!(last.contains("panic"), "{written}");
        assert!(!written.contains("見えない失敗"), "{written}");
        assert!(last.contains("payload=omitted"), "{written}");
        // One line, so a grep still pulls out one kind of event whole.
        assert_eq!(written.lines().count(), 2, "{written}");
        let _ = fs::remove_file(&path);
    }
}
