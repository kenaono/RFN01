//! A detailed trace of one run.
//!
//! `perf_log.txt` keeps one line per operation and is about **cost**. This is
//! about **what happened**: the events that reached the editor, the geometry
//! each one was decided against, and what it decided. The two are read at
//! different times — the perf log while tuning, this one after something on
//! screen turned out to be wrong — and mixing them would make both harder to
//! read.
//!
//! One file per run, named for the moment the run started, so that runs never
//! overwrite each other and a report can name the file it came from. The perf
//! log keeps only the last two runs on purpose (6.13); this keeps all of them,
//! because the run worth reading is usually not the last one.
//!
//! **Best effort throughout.** The log exists to explain the editor, so it must
//! never be able to stop it: every failure stops the log and nothing else.
//! Writes are unbuffered for the same reason — the tail of the file is the part
//! that matters when a run ends badly.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Where the files go: a directory of their own, because there is one per run,
/// beside the executable rather than under the current one (see
/// [`beside_executable`]).
const DIAG_DIRECTORY: &str = "diag";

/// Beyond this the file is more likely to be in the way than to be read. A
/// session that reaches it says so in the file rather than simply stopping: a
/// log that ends without a word looks like a crash.
const DIAG_LINE_LIMIT: usize = 500_000;

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

pub struct DiagLog {
    file: Option<File>,
    path: Option<PathBuf>,
    /// Set once nothing more will be written: the file could not be opened, or
    /// the line limit was reached. A log that has not been started is stopped,
    /// so an editor built without one costs nothing per event.
    stopped: bool,
    lines: usize,
    started: Instant,
}

impl Default for DiagLog {
    fn default() -> Self {
        Self {
            file: None,
            path: None,
            stopped: true,
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
    pub fn start(&mut self) -> LocalTime {
        let now = LocalTime::now();
        let stamp = now.file_stamp();
        let directory = beside_executable(DIAG_DIRECTORY);
        let _ = fs::create_dir_all(&directory);
        for serial in 1..=NAME_ATTEMPTS {
            let name = format!("Diag_{stamp}_{serial:03}.log");
            let path = directory.join(name);
            // `create_new` rather than asking whether the file exists: two runs
            // starting in the same second must not share a file, and between
            // the question and the answer they could.
            let opened = OpenOptions::new().write(true).create_new(true).open(&path);
            match opened {
                Ok(file) => {
                    self.file = Some(file);
                    self.path = Some(path);
                    self.stopped = false;
                    self.lines = 0;
                    self.started = Instant::now();
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
        self.path.as_deref()
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
    /// Called once, after [`start`](DiagLog::start). Best effort throughout:
    /// the process is ending either way, and a hook that panicked would take
    /// the message with it.
    pub fn catch_panics(&self) {
        let Some(path) = self.path.clone() else {
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
                let said = info.to_string().replace(['\n', '\r'], " ");
                let _ = writeln!(file, "+{elapsed:11.3} panic at={at} said={said}");
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
        if self.stopped {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.lines >= DIAG_LINE_LIMIT {
            let _ = writeln!(file, "diag stopped=line-limit lines={}", self.lines);
            self.stopped = true;
            return;
        }
        self.lines += 1;
        let elapsed = self.started.elapsed().as_secs_f64() * 1000.0;
        let _ = writeln!(file, "+{elapsed:11.3} {category} {fields}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut first = DiagLog::default();
        let mut second = DiagLog::default();
        first.start();
        second.start();
        let first = first.path().expect("first file").to_path_buf();
        let second = second.path().expect("second file").to_path_buf();
        assert_ne!(first, second);
        let _ = fs::remove_file(&first);
        let _ = fs::remove_file(&second);
    }

    /// **A panic reaches the log** (2026-09-06). The editor draws its own
    /// window and has no console, so before this a panic left nothing behind
    /// but a log that stopped — and the last line before a stop says what was
    /// happening, never what went wrong.
    ///
    /// The hook is process-wide, so this puts back whatever was there before.
    #[test]
    fn a_panic_is_written_to_this_run_s_log() {
        let mut log = DiagLog::default();
        log.start();
        let Some(path) = log.path().map(Path::to_path_buf) else {
            // No place to write beside the executable; nothing to hold.
            return;
        };
        log.write("before", "kind=ordinary");

        let restore = std::panic::take_hook();
        std::panic::set_hook(restore);
        log.catch_panics();
        let caught = std::panic::catch_unwind(|| panic!("見えない失敗"));
        let _ = std::panic::take_hook();
        assert!(caught.is_err(), "the panic still unwinds");

        let written = fs::read_to_string(&path).expect("the log is readable");
        let last = written.lines().last().expect("a line was written");
        assert!(last.contains("panic"), "{written}");
        assert!(last.contains("見えない失敗"), "{written}");
        // One line, so a grep still pulls out one kind of event whole.
        assert_eq!(written.lines().count(), 2, "{written}");
        let _ = fs::remove_file(&path);
    }
}
