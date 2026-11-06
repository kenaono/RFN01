//! One shell, its screen, and the thread that reads it (要件 2・追加要件 Terminal).
//!
//! **This is the whole of what connects the two halves.** [`crate::pty`] knows
//! Windows and nothing else; [`crate::terminal`] knows VT and nothing else; the
//! pane knows neither. What is left over is the part that has to be true at
//! once: **output arrives when the shell feels like it**, and the window has to
//! stay answerable while it does.
//!
//! The shape is [`crate::searcher`]'s, for the same reasons. The thread is given
//! a handle it owns and a way to wake the window; it hands back bytes it owns.
//! No DirectWrite, no COM, no Slint — so 技術検証 7.3's open question about
//! laying text out on several threads still does not arise here.
//!
//! **The reading thread never touches the screen.** It could: the grid is plain
//! data. But then a repaint would have to lock it, and the cost of being wrong
//! about that lock is a window that stops. Bytes go over a channel and the
//! window applies them where it already owns everything else.

use std::cell::RefCell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
#[cfg(test)]
use std::time::Duration;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

struct FileLog {
    path: PathBuf,
    input: SyncSender<String>,
    completion: Arc<LogCompletion>,
    pending: String,
    stamp: Option<Stamp>,
}

const FILE_LOG_SLOTS: usize = 8;
const FILE_LOG_CHUNK_LIMIT: usize = 1024 * 1024;
const FILE_LOG_WORKER_LIMIT: usize = 16;

#[derive(Default)]
struct LogCompletion {
    tail: Mutex<String>,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
}

struct LogWorker {
    path: PathBuf,
    completion: Arc<LogCompletion>,
    thread: thread::JoinHandle<()>,
}

// The UI owns these workers even after a tab discards its TerminalSession.
// Only application shutdown waits for writes, after the event loop returns.
thread_local! {
    static FILE_LOG_WORKERS: RefCell<Vec<LogWorker>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn file_log_paths() -> Vec<PathBuf> {
    FILE_LOG_WORKERS.with(|workers| {
        workers
            .borrow()
            .iter()
            .filter(|worker| !worker.completion.finished.load(Ordering::Acquire))
            .map(|worker| worker.path.clone())
            .collect()
    })
}

pub(crate) fn collect_file_log_errors() -> Vec<String> {
    FILE_LOG_WORKERS.with(|workers| {
        let mut errors = Vec::new();
        workers.borrow_mut().retain(|worker| {
            if !worker.completion.finished.load(Ordering::Acquire) {
                return true;
            }
            errors.extend(worker.completion.error.lock().unwrap().take());
            false
        });
        errors
    })
}

pub(crate) fn wait_for_file_logs() -> Vec<String> {
    let workers = FILE_LOG_WORKERS.with(|workers| workers.take());
    workers
        .into_iter()
        .filter_map(|worker| {
            if worker.thread.join().is_err() {
                return Some(format!(
                    "{}: Terminal log writer stopped unexpectedly",
                    worker.path.display()
                ));
            }
            worker.completion.error.lock().unwrap().take()
        })
        .collect()
}

use crate::pty::{OwnedHandle, Pty};
use crate::terminal::{
    Key, Modifiers, MouseAction, MouseButton, MouseTracking, Screen, Terminal, encode_focus,
    encode_key, encode_mouse, encode_paste, encode_typing,
};
use crate::timestamp::Stamp;

/// How much is read at once. **A screenful of coloured text is a few KB**, and
/// a program printing a large file will simply come back around the loop.
const CHUNK: usize = 8192;

const OUTPUT_SLOTS: usize = 16;
const CHUNKS_PER_DRAIN: usize = 8;
const INPUT_SLOTS: usize = 16;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

struct Wake {
    posted: AtomicBool,
    post: Mutex<Box<dyn Fn() + Send>>,
}

impl Wake {
    fn notify(&self) {
        if !self.posted.swap(true, Ordering::AcqRel) {
            (self.post.lock().unwrap())();
        }
    }

    fn collected(&self) {
        self.posted.store(false, Ordering::Release);
    }
}

struct Console {
    process: OwnedHandle,
    size: Arc<Mutex<(u16, u16)>>,
    resized: SyncSender<()>,
}

impl Console {
    fn resize(&self, columns: u16, rows: u16) {
        *self.size.lock().unwrap() = (columns, rows);
        let _ = self.resized.try_send(());
    }
}

struct TerminalIo {
    console: Console,
    output: Receiver<Vec<u8>>,
    input: SyncSender<Vec<u8>>,
    wake: Arc<Wake>,
}

struct ShellLaunch {
    profile: crate::TerminalShell,
    directory: String,
    windows_folder: Option<PathBuf>,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A running shell and the screen it is writing on.
pub struct TerminalSession {
    /// What to call this shell on its tab. **The shell's name, not the
    /// command** — `wsl.exe --cd . -- bash -l` is a command; `WSL` is what the
    /// writer chose.
    name: String,
    number: u64,
    file_log: Option<FileLog>,
    log_error: Option<String>,
    /// `None` while a shell put back from the last run is waiting to be shown
    /// (RFN01-97): **it starts when it is first drawn** ([`Self::start_waiting`]).
    console: Option<Console>,
    /// Held while waiting, so the channel stays open and the empty shell is not
    /// taken for one that ended.
    idle: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// RFN01-97: the last run's output, put on the screen just before the shell
    /// starts (so it is laid out at the width it is shown at).
    restored: Option<(Vec<String>, String)>,
    pub(crate) saved_output: Option<String>,
    /// RFN01-99: the name the program had given this shell last run, shown
    /// until it gives one again.
    restored_title: Option<String>,
    terminal: Terminal,
    output: Receiver<Vec<u8>>,
    input: Option<SyncSender<Vec<u8>>>,
    wake: Option<Arc<Wake>>,
    pending_replies: Vec<u8>,
    /// The shell has exited. **The screen stays** — what it last said is often
    /// the reason it exited.
    finished: bool,
    /// The cell the last pointer movement was reported from, so that moving
    /// within one cell is not a report per pixel.
    mouse_at: Option<(usize, usize)>,
    /// RFN01-95: the keyboard is in this shell (the pane says so as its field
    /// takes and loses the focus).
    keyboard: bool,
    /// RFN01-95: the screen's call count when the writer last had the keyboard
    /// here. A higher count is a call nobody has answered yet.
    attention_seen: u64,
    /// RFN01-95/97: the shell this was started as, and the Windows folder it
    /// started in when one is known — where a file name it prints is looked
    /// for, and what a restart starts again.
    launch: Option<ShellLaunch>,
}

impl TerminalSession {
    /// Start `command` on a screen `columns` by `rows`.
    ///
    /// `wake` is called from the reading thread every time bytes arrive, and
    /// means only "there is something to collect". Applying it is
    /// [`Self::drain`], on the thread that owns the window.
    #[cfg(test)]
    pub fn start(
        name: &str,
        command: &str,
        columns: usize,
        rows: usize,
        wake: impl Fn() + Send + 'static,
    ) -> windows::core::Result<Self> {
        Self::start_in(name, command, None, None, columns, rows, wake)
    }

    pub fn start_in(
        name: &str,
        command: &str,
        directory: Option<&std::path::Path>,
        environment: Option<&[u16]>,
        columns: usize,
        rows: usize,
        wake: impl Fn() + Send + 'static,
    ) -> windows::core::Result<Self> {
        let pty = Pty::open_with(
            command,
            directory,
            columns as u16,
            rows as u16,
            false,
            environment,
        )?;
        let TerminalIo {
            console,
            output,
            input,
            wake,
        } = start_io(pty, wake)?;
        Ok(Self {
            name: name.to_owned(),
            number: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            file_log: None,
            log_error: None,
            console: Some(console),
            idle: None,
            restored: None,
            saved_output: None,
            restored_title: None,
            terminal: Terminal::new(columns, rows),
            output,
            input: Some(input),
            wake: Some(wake),
            pending_replies: Vec::new(),
            finished: false,
            mouse_at: None,
            keyboard: false,
            attention_seen: 0,
            launch: None,
        })
    }

    /// RFN01-97: a shell from the last run, **not started yet**. It holds what
    /// that run printed (`lines`, oldest first) and when it was put away
    /// (`when`), and starts the first time it is drawn.
    pub fn waiting(
        shell: crate::TerminalShell,
        launch_directory: Option<String>,
        lines: Vec<String>,
        when: String,
    ) -> Self {
        let (idle, output) = channel::<Vec<u8>>();
        let mut session = Self {
            name: shell.name.clone(),
            number: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            file_log: None,
            log_error: None,
            console: None,
            idle: Some(idle),
            restored: Some((lines, when)),
            saved_output: None,
            restored_title: None,
            terminal: Terminal::new(80, 24),
            output,
            input: None,
            wake: None,
            pending_replies: Vec::new(),
            finished: false,
            mouse_at: None,
            keyboard: false,
            attention_seen: 0,
            launch: None,
        };
        let directory = launch_directory.unwrap_or_else(|| shell.directory.clone());
        session.set_profile(shell, directory);
        session
    }

    /// RFN01-97: still waiting to be shown for the first time.
    pub fn is_waiting(&self) -> bool {
        self.idle.is_some() && !self.finished
    }

    pub fn needs_stop_confirmation(&mut self) -> bool {
        self.drain();
        !self.finished() && !self.is_waiting() && !self.terminal.shell_at_prompt()
    }

    /// RFN01-97: start a shell that was waiting. The last run's output goes on
    /// the screen first, faint, with a line saying where it ends; the console
    /// then starts below it.
    pub fn start_waiting(
        &mut self,
        columns: usize,
        rows: usize,
        wake: impl Fn() + Send + 'static,
    ) -> windows::core::Result<()> {
        let Some(launch) = &self.launch else {
            self.finished = true;
            return Ok(());
        };
        self.terminal.screen.resize(columns, rows);
        if let Some((lines, when)) = self.restored.take()
            && !lines.is_empty()
        {
            self.terminal.feed(&restored_screen(&lines, &when));
        }
        let mut shell = launch.profile.clone();
        shell.directory.clone_from(&launch.directory);
        let (command, directory) = crate::terminal_shells::launch(&shell);
        let environment = crate::terminal_shells::wsl_shell_environment(&shell);
        let started = Pty::open_with(
            &command,
            directory.as_deref(),
            columns as u16,
            rows as u16,
            true,
            environment.as_deref(),
        )
        .and_then(|pty| start_io(pty, wake));
        match started {
            Ok(io) => {
                self.console = Some(io.console);
                self.output = io.output;
                self.input = Some(io.input);
                self.wake = Some(io.wake);
                self.idle = None;
                Ok(())
            }
            Err(error) => {
                self.finished = true;
                self.idle = None;
                Err(error)
            }
        }
    }

    /// RFN01-97: what this shell printed, as lines of plain text, oldest first
    /// and at most `limit` of them — the history and then the screen. A shell
    /// that never started gives back what it was put back with.
    pub fn output_lines(&self, limit: usize) -> Vec<String> {
        let mut lines = match &self.restored {
            Some((lines, _)) => lines.clone(),
            None => self.terminal.screen.plain_lines(),
        };
        if lines.len() > limit {
            lines.drain(..lines.len() - limit);
        }
        lines
    }

    #[cfg(test)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// RFN01-99: what the tab is called, and the whole of it for the tooltip
    /// (empty when the two are the same). **The program's own name for it**
    /// when it gave one — bash's `user@host: /mnt/d/原稿` cut to `原稿`, Claude
    /// Code's thread as it is — and the shell's name and number otherwise.
    pub fn tab_name(&self) -> (String, String) {
        let title = self.given_title();
        match title {
            Some(title) => {
                let shown = short_title(&title);
                let tip = if shown == title { String::new() } else { title };
                (shown, tip)
            }
            // RFN01-101: a shell that names nothing still says where it is,
            // in its prompt (`PS D:\原稿\第一部>`).
            None => match self.terminal.screen.latest_prompt_folder() {
                Some(folder) => {
                    let shown = short_title(&folder);
                    let tip = if shown == folder {
                        String::new()
                    } else {
                        folder
                    };
                    (shown, tip)
                }
                None => (self.title(), String::new()),
            },
        }
    }

    /// RFN01-99: the name the program gave this shell, if any — the one it
    /// gave last run while it has not given one yet.
    pub fn given_title(&self) -> Option<String> {
        let now = self.terminal.screen.title().trim();
        // **The console's own default is not a name**: PowerShell and cmd are
        // titled with the path of their program until something says
        // otherwise (seen in the app, 2026-11-04: `powershell.exe`), which
        // says less than the shell's name does.
        let program = self
            .profile()
            .map(|shell| crate::terminal_shells::command_parts(&shell.command).0)
            .and_then(|program| Path::new(program).file_name())
            .map(|name| name.to_string_lossy().to_ascii_lowercase());
        let defaulted = program.is_some_and(|program| {
            let now = now.to_ascii_lowercase();
            now == program
                || now.ends_with(&format!("\\{program}"))
                || now == program.trim_end_matches(".exe")
        });
        if now.is_empty() || defaulted {
            self.restored_title.clone()
        } else {
            Some(now.to_owned())
        }
    }

    /// RFN01-99: put back the name from the last run.
    pub fn set_restored_title(&mut self, title: Option<String>) {
        self.restored_title = title.filter(|title| !title.trim().is_empty());
    }

    /// RFN01-99: which icon the tab carries — 1 Linux (WSL, bash), 2
    /// PowerShell, 3 cmd, 4 any other shell. From the first word of the
    /// command the shell was started with.
    pub fn shell_icon(&self) -> i32 {
        self.profile()
            .map_or(4, |shell| shell_icon_for(&shell.command))
    }

    /// Unique within this run (the number in its title).
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn title(&self) -> String {
        format!("{}:{}", self.name, self.number)
    }

    pub fn file_log_path(&self) -> Option<&Path> {
        self.file_log
            .as_ref()
            .filter(|log| !log.completion.finished.load(Ordering::Acquire))
            .map(|log| log.path.as_path())
    }

    pub fn take_log_error(&mut self) -> Option<String> {
        self.log_error.take()
    }

    pub fn start_file_log(
        &mut self,
        path: PathBuf,
        stamp: Option<Stamp>,
        wake: impl FnOnce() + Send + 'static,
    ) {
        let file = path.clone();
        self.start_file_log_with(path, stamp, move || std::fs::File::create(file), wake);
    }

    fn start_file_log_with<W: Write>(
        &mut self,
        path: PathBuf,
        stamp: Option<Stamp>,
        open: impl FnOnce() -> io::Result<W> + Send + 'static,
        wake: impl FnOnce() + Send + 'static,
    ) {
        if file_log_paths().len() >= FILE_LOG_WORKER_LIMIT {
            self.log_error = Some(
                crate::pick(
                    "ファイルに保存中の Terminal ログが上限に達しました",
                    "Too many Terminal logs are still writing",
                )
                .to_owned(),
            );
            return;
        }
        self.drain_chunks(OUTPUT_SLOTS + 1);
        self.finish_file_log();
        let (input, output) = sync_channel::<String>(FILE_LOG_SLOTS);
        let completion = Arc::new(LogCompletion::default());
        let worker_completion = completion.clone();
        let worker_path = path.clone();
        let thread = thread::Builder::new()
            .name("rfnedit-terminal-log".to_owned())
            .spawn(move || {
                let write = || -> io::Result<()> {
                    let mut writer = open()?;
                    for batch in output {
                        writer.write_all(batch.as_bytes())?;
                        writer.flush()?;
                    }
                    let tail = std::mem::take(&mut *worker_completion.tail.lock().unwrap());
                    writer.write_all(tail.as_bytes())?;
                    writer.flush()
                };
                if let Err(error) = write() {
                    *worker_completion.error.lock().unwrap() =
                        Some(format!("{}: {error}", worker_path.display()));
                }
                worker_completion.finished.store(true, Ordering::Release);
                wake();
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                self.log_error = Some(format!("{}: {error}", path.display()));
                return;
            }
        };
        FILE_LOG_WORKERS.with(|workers| {
            workers.borrow_mut().push(LogWorker {
                path: path.clone(),
                completion: completion.clone(),
                thread,
            })
        });
        self.terminal.screen.start_file_capture(stamp.clone());
        self.file_log = Some(FileLog {
            path,
            input,
            completion,
            pending: String::new(),
            stamp,
        });
    }

    fn collect_file_log(&mut self) {
        if self.file_log_path().is_none() {
            self.finish_file_log();
            return;
        }
        let Some((completed, pending)) = self.terminal.screen.file_capture_update() else {
            return;
        };
        let log = self.file_log.as_mut().unwrap();
        let too_large =
            completed.len() > FILE_LOG_CHUNK_LIMIT || pending.len() > FILE_LOG_CHUNK_LIMIT;
        let delivery = if too_large {
            Err(TrySendError::Full(completed))
        } else if completed.is_empty() {
            Ok(())
        } else {
            log.input.try_send(completed)
        };
        if let Err(error) = delivery {
            if matches!(error, TrySendError::Full(_)) {
                self.log_error = Some(format!(
                    "{}: {}",
                    log.path.display(),
                    crate::pick(
                        "保存待ちのログが上限に達しました",
                        "Terminal log queue capacity exceeded",
                    )
                ));
            }
            self.file_log = None;
            self.terminal.screen.stop_file_capture();
        } else {
            log.pending = pending;
        }
    }

    pub fn stop_file_log(&mut self) {
        if self.file_log.is_some() {
            self.drain_chunks(OUTPUT_SLOTS + 1);
            self.finish_file_log();
        }
    }

    fn finish_file_log(&mut self) {
        if let Some(mut log) = self.file_log.take() {
            // The unfinished last line is finished by stopping, so it is
            // stamped with the time the log stopped.
            if let Some(stamp) = log.stamp.as_ref().filter(|_| !log.pending.is_empty()) {
                log.pending.insert_str(0, &stamp.text());
            }
            // Dropping the sender finishes the accepted queue, then the worker
            // appends this tail even when the queue was full.
            *log.completion.tail.lock().unwrap() = log.pending;
            self.terminal.screen.stop_file_capture();
        }
    }

    pub fn start_capture(&mut self, stamp: Option<Stamp>) {
        self.terminal.screen.start_capture(stamp);
    }
    pub fn stop_capture(&mut self) {
        self.terminal.screen.stop_capture();
    }
    pub fn capture_update(&mut self) -> Option<(String, String)> {
        self.terminal.screen.capture_update()
    }
    pub fn set_history_limit(&mut self, limit: usize) {
        self.terminal.screen.set_history_limit(limit);
    }
    pub fn clear_history(&mut self) {
        self.terminal.screen.clear_history();
    }
    pub fn clear_screen(&mut self) {
        self.terminal.screen.clear_screen();
    }

    pub fn screen(&self) -> &Screen {
        &self.terminal.screen
    }

    /// The shell has exited and everything it wrote has been applied.
    ///
    /// Once the child exits, the console worker closes the output pipe.
    /// The last buffered bytes are applied before the session is finished.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Apply a bounded batch, then let the UI handle input and paint.
    pub fn drain(&mut self) -> bool {
        self.drain_chunks(CHUNKS_PER_DRAIN)
    }

    fn drain_chunks(&mut self, budget: usize) -> bool {
        if let Some(wake) = &self.wake {
            wake.collected();
        }
        let before = self.terminal.screen.revision();
        let mut applied = 0;
        let mut closed = false;
        self.flush_replies();
        while applied < budget && self.pending_replies.is_empty() {
            match self.output.try_recv() {
                Ok(chunk) => {
                    self.terminal.feed(&chunk);
                    applied += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        self.answer();
        self.collect_file_log();
        if closed {
            self.finished = true;
            self.input = None;
            self.console = None;
            self.finish_file_log();
        } else if self
            .console
            .as_ref()
            .is_some_and(|console| console.process.exited())
        {
            // Let the console thread close the pipe and keep collecting its tail.
            self.input = None;
            self.console = None;
            if let Some(wake) = &self.wake {
                wake.notify();
            }
        }
        if applied == budget
            && let Some(wake) = &self.wake
        {
            wake.notify();
        }
        self.terminal.screen.revision() != before
    }

    /// Wait for the shell to say something, then apply it. **For tests and for
    /// nothing else** — the window is woken, it does not wait.
    #[cfg(test)]
    pub fn wait(&mut self, patience: Duration) -> bool {
        match self.output.recv_timeout(patience) {
            Ok(chunk) => {
                self.terminal.feed(&chunk);
                self.drain();
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.drain();
                false
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.finished = true;
                self.collect_file_log();
                self.finish_file_log();
                false
            }
        }
    }

    /// Send back what the shell asked for (cursor position, what we are).
    ///
    /// **A question left unanswered is a program left waiting**, so this
    /// happens on every drain rather than when somebody thinks to do it.
    fn answer(&mut self) {
        // At most one batch of output has produced replies since the last
        // successful send. Keep those replies ahead of subsequent input.
        self.pending_replies
            .extend(self.terminal.screen.take_replies());
        self.flush_replies();
    }

    fn flush_replies(&mut self) {
        if self.pending_replies.is_empty() {
            return;
        }
        let replies = std::mem::take(&mut self.pending_replies);
        if let Some(input) = &self.input
            && let Err(TrySendError::Full(replies)) = input.try_send(replies)
        {
            self.pending_replies = replies;
        }
    }

    /// Whether the program has asked for the pointer (全画面TUIの互換性、
    /// 2026-09-22). **While it has, a press is its**, not the start of a
    /// selection — Shift gives the pointer back to the pane.
    pub fn wants_mouse(&self) -> bool {
        self.terminal.screen.modes().mouse != MouseTracking::Off
    }

    /// A pointer event over the cell at `row`, `column` of the screen.
    /// `true` when the program took it.
    pub fn send_mouse(
        &mut self,
        action: MouseAction,
        button: MouseButton,
        row: usize,
        column: usize,
        modifiers: Modifiers,
    ) -> bool {
        if !self.wants_mouse() {
            return false;
        }
        if action == MouseAction::Move {
            if self.mouse_at == Some((row, column)) {
                return true;
            }
            self.mouse_at = Some((row, column));
        }
        let modes = self.terminal.screen.modes();
        if let Some(bytes) = encode_mouse(action, button, row, column, modifiers, modes) {
            self.send(&bytes);
        }
        true
    }

    /// A turn of the wheel, `lines` rows' worth. `true` when it went to the
    /// program rather than to the pane's own history.
    ///
    /// **On the alternate screen there is no history to move through**, so a
    /// program that did not ask for the pointer gets arrow keys instead — the
    /// way `less` and `man` scroll in every other terminal.
    pub fn send_wheel(
        &mut self,
        up: bool,
        lines: usize,
        row: usize,
        column: usize,
        modifiers: Modifiers,
    ) -> bool {
        let button = if up {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        };
        if self.wants_mouse() {
            for _ in 0..lines.max(1) {
                self.send_mouse(MouseAction::Press, button, row, column, modifiers);
            }
            return true;
        }
        if self.terminal.screen.modes().alternate_screen {
            let key = if up { Key::Up } else { Key::Down };
            for _ in 0..lines.max(1) {
                self.send_key(key, Modifiers::default());
            }
            return true;
        }
        false
    }

    /// The pane gained or lost the keyboard.
    /// RFN01-95/97: what this shell was started as, and where.
    pub fn set_profile(&mut self, profile: crate::TerminalShell, directory: String) {
        let windows_folder = crate::terminal_shells::windows_folder(&directory);
        self.launch = Some(ShellLaunch {
            profile,
            directory,
            windows_folder,
        });
    }

    pub fn profile(&self) -> Option<&crate::TerminalShell> {
        self.launch.as_ref().map(|launch| &launch.profile)
    }

    pub fn uses_profile(&self, shell: &crate::TerminalShell) -> bool {
        match self.profile() {
            Some(profile) if profile.id != 0 => profile.id == shell.id,
            Some(profile) => profile.name == shell.name && profile.command == shell.command,
            None => self.name == shell.name,
        }
    }

    /// RFN01-95: the Windows folder relative file names are read from.
    pub fn folder(&self) -> Option<&std::path::Path> {
        self.launch
            .as_ref()
            .and_then(|launch| launch.windows_folder.as_deref())
    }

    pub fn launch_directory(&self) -> Option<&str> {
        self.launch.as_ref().map(|launch| launch.directory.as_str())
    }

    /// RFN01-95: a bell or a notification arrived while the keyboard was
    /// somewhere else, and the writer has not been here since.
    pub fn wants_attention(&self) -> bool {
        self.terminal.screen.attention() != self.attention_seen
    }

    /// RFN01-95: the keyboard came here or left. **Coming here answers every
    /// call so far.** True when that took a mark away.
    pub fn set_keyboard(&mut self, here: bool) -> bool {
        self.keyboard = here;
        self.settle_attention()
    }

    /// RFN01-95: a call that arrives while the writer is typing here is
    /// answered as it arrives. True when a mark went away.
    pub fn settle_attention(&mut self) -> bool {
        let wanted = self.wants_attention();
        if self.keyboard {
            self.attention_seen = self.terminal.screen.attention();
        }
        wanted && !self.wants_attention()
    }

    pub fn send_focus(&mut self, focused: bool) {
        let bytes = encode_focus(focused, self.terminal.screen.modes());
        self.send(&bytes);
    }

    pub fn send_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        let bytes = encode_key(key, modifiers, self.terminal.screen.modes());
        self.send(&bytes)
    }

    /// Text the writer produced at the keyboard — a conversion committed, a
    /// character typed (追加要件 Terminal).
    ///
    /// **Not a paste.** A shell shows pasted text highlighted until the next
    /// key (bracketed paste is what tells it to), so a word committed out of an
    /// IME would arrive selected — which is what the writer saw. Typing is
    /// typing: the bytes go as they are.
    pub fn type_text(&mut self, text: &str) -> bool {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        self.send(text.as_bytes())
    }

    /// RFN01-104: Editor PanelのCtrl+Enter——打ったものとして送る（[`encode_typing`]）。
    pub fn send_draft(&mut self, text: &str) -> bool {
        let bytes = encode_typing(text, self.terminal.screen.modes());
        self.send(&bytes)
    }

    pub fn paste(&mut self, text: &str) -> bool {
        let bytes = encode_paste(text, self.terminal.screen.modes());
        self.send(&bytes)
    }

    /// Write bytes as they are. The pane uses this for what it has already
    /// encoded; everything else should go through [`Self::send_key`].
    pub fn send(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        self.flush_replies();
        let submits = bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'));
        if submits {
            self.drain();
        }
        let sent = bytes.len() <= MAX_INPUT_BYTES
            && self.pending_replies.is_empty()
            && self
                .input
                .as_ref()
                .is_some_and(|input| input.try_send(bytes.to_vec()).is_ok());
        if sent && submits {
            self.terminal.command_submitted();
        }
        sent
    }

    /// The pane changed size (要件 6.4).
    ///
    /// **Both halves have to be told, and the order matters.** The screen is
    /// resized first so that what arrives in answer — a shell redrawing its
    /// prompt for the new width — lands on a grid that is already the right
    /// shape.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        if columns == self.terminal.screen.columns() && rows == self.terminal.screen.rows() {
            return;
        }
        self.terminal.screen.resize(columns, rows);
        if let Some(console) = &self.console {
            console.resize(columns as u16, rows as u16);
        }
    }
}

/// ConPTY requires separate read and write threads so either pipe can make
/// progress while the other one is blocked.
fn start_io(mut pty: Pty, wake: impl Fn() + Send + 'static) -> windows::core::Result<TerminalIo> {
    let reader = pty.take_reader().expect("new console has a reader");
    let writer = pty.take_writer().expect("new console has a writer");
    let wake = Arc::new(Wake {
        posted: AtomicBool::new(false),
        post: Mutex::new(Box::new(wake)),
    });
    let (sender, output) = sync_channel(OUTPUT_SLOTS);
    let read_wake = wake.clone();
    thread::Builder::new()
        .name("rfnedit-terminal-read".to_owned())
        .spawn(move || {
            let mut buffer = vec![0_u8; CHUNK];
            let mut delivering = true;
            while let Ok(n @ 1..) = reader.read(&mut buffer) {
                if delivering {
                    delivering = sender.send(buffer[..n].to_vec()).is_ok();
                    read_wake.notify();
                }
            }
            // Make EOF visible before posting the wake.
            drop(sender);
            read_wake.notify();
        })
        .map_err(|error| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
        })?;
    let (input, receive) = sync_channel::<Vec<u8>>(INPUT_SLOTS);
    let write_wake = wake.clone();
    thread::Builder::new()
        .name("rfnedit-terminal-write".to_owned())
        .spawn(move || {
            while let Ok(bytes) = receive.recv() {
                write_wake.notify();
                if writer.write(&bytes).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
        })?;
    let process = pty.take_process().expect("new console has a process");
    let size = Arc::new(Mutex::new((1, 1)));
    let requested = size.clone();
    let (resized, resize) = sync_channel(1);
    let (start_console, receive_console) = sync_channel::<Pty>(1);
    thread::Builder::new()
        .name("rfnedit-terminal-console".to_owned())
        .spawn(move || {
            let Ok(pty) = receive_console.recv() else {
                return;
            };
            while resize.recv().is_ok() {
                let (columns, rows) = *requested.lock().unwrap();
                let _ = pty.resize(columns, rows);
            }
            // ClosePseudoConsole may wait for the output pipe to drain.
            drop(pty);
        })
        .map_err(|error| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
        })?;
    // Keep the console until the thread has started. If spawning fails, the
    // output receiver above is dropped before the console waits for its pipe.
    if let Err(error) = start_console.send(pty) {
        drop(output);
        drop(input);
        drop(error);
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_FAIL,
        ));
    }
    Ok(TerminalIo {
        console: Console {
            process,
            size,
            resized,
        },
        input,
        output,
        wake,
    })
}

/// RFN01-99: the icon for a shell started with `command` (see
/// [`TerminalSession::shell_icon`]).
pub fn shell_icon_for(command: &str) -> i32 {
    let (program, _) = crate::terminal_shells::command_parts(command);
    let stem = Path::new(program)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match stem.as_str() {
        "wsl" | "bash" => 1,
        "powershell" | "pwsh" => 2,
        "cmd" => 3,
        _ => 4,
    }
}

/// RFN01-99（書き手の判断 2026-11-04：縮める）: a title that names a folder,
/// cut to the folder's own name — `ken@KN01: /mnt/d/原稿/第一部` and
/// `D:\原稿\第一部` are both `第一部`. Anything else is kept as it is.
pub fn short_title(title: &str) -> String {
    let title = title.trim();
    // bash's `user@host: path` (Ubuntu's default), and `user@host:path`.
    let path = match title.split_once(':') {
        Some((who, rest))
            if who.contains('@') && !who.contains(char::is_whitespace) && !rest.is_empty() =>
        {
            rest.trim()
        }
        _ => title,
    };
    let windows =
        path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path[1..].starts_with(":\\");
    let unix = path.starts_with('/') || path == "~" || path.starts_with("~/");
    if !(windows || unix) {
        return title.to_owned();
    }
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(last) if !last.is_empty() => last.to_owned(),
        _ => path.to_owned(),
    }
}

/// RFN01-97: the last run's lines as the screen is fed them — faint grey, then
/// `──── Restored · <when> ────`, then the pen put back for the new shell.
pub fn restored_screen(lines: &[String], when: &str) -> Vec<u8> {
    let mut out = String::from("\x1b[0m\x1b[90m");
    for line in lines {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!("──── Restored · {when} ────"));
    out.push_str("\x1b[0m\r\n");
    out.into_bytes()
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stop_file_log();
        // Release the bounded queues before closing the console.
        self.input = None;
        let (_, empty) = channel();
        drop(std::mem::replace(&mut self.output, empty));
        self.console = None;
    }
}

#[cfg(test)]
mod tab_names {
    use super::*;

    /// RFN01-99: a title naming a folder is cut to the folder; anything else
    /// is kept.
    #[test]
    fn folder_titles_are_cut_to_the_folder() {
        assert_eq!(short_title("ken@KN01: /mnt/d/原稿/第一部"), "第一部");
        assert_eq!(short_title("ken@KN01:~/novel"), "novel");
        assert_eq!(short_title("ken@KN01: ~"), "~");
        assert_eq!(short_title("D:\\原稿\\第一部\\"), "第一部");
        assert_eq!(short_title("/mnt/d/My Docs"), "My Docs");
        assert_eq!(
            short_title("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
            "powershell.exe"
        );
        assert_eq!(short_title("✳ 第一章の推敲"), "✳ 第一章の推敲");
        assert_eq!(
            short_title("vim: notes.md"),
            "vim: notes.md",
            "no user@host"
        );
        assert_eq!(short_title("  Windows PowerShell "), "Windows PowerShell");
    }

    /// RFN01-99: the console's default title (the program's path) is not a
    /// name; one a program gives is.
    #[test]
    fn a_default_console_title_leaves_the_shells_name() {
        let mut shell = TerminalSession::waiting(
            crate::TerminalShell {
                id: 0,
                name: "PowerShell".into(),
                command: "powershell.exe -NoLogo".into(),
                directory: String::new(),
            },
            None,
            Vec::new(),
            String::new(),
        );
        let own = format!("PowerShell:{}", shell.number());
        assert_eq!(shell.tab_name().0, own);
        shell
            .terminal
            .feed(b"\x1b]0;C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\x07");
        assert_eq!(shell.tab_name(), (own, String::new()));
        // RFN01-101: no name given, the prompt says where it is.
        shell.terminal.feed("PS D:\\原稿\\第一部> ".as_bytes());
        assert_eq!(
            shell.tab_name(),
            ("第一部".to_owned(), "D:\\原稿\\第一部".to_owned())
        );
        shell.terminal.feed("cd ..\r\nPS D:\\原稿> ".as_bytes());
        assert_eq!(shell.tab_name().0, "原稿", "the newest prompt");
        shell.terminal.feed("\x1b]0;✳ 第一章の推敲\x07".as_bytes());
        assert_eq!(shell.tab_name().0, "✳ 第一章の推敲", "a name given wins");
    }

    #[test]
    fn the_command_says_which_icon() {
        assert_eq!(shell_icon_for("wsl.exe"), 1);
        assert_eq!(shell_icon_for("wsl -d Ubuntu"), 1);
        assert_eq!(
            shell_icon_for("C:\\Program Files\\Git\\bin\\bash.exe --login"),
            4
        );
        assert_eq!(
            shell_icon_for("\"C:\\Program Files\\Git\\bin\\bash.exe\" --login"),
            1
        );
        assert_eq!(shell_icon_for("powershell.exe -NoLogo"), 2);
        assert_eq!(shell_icon_for("pwsh.exe -NoLogo"), 2);
        assert_eq!(shell_icon_for("cmd.exe /K"), 3);
        assert_eq!(shell_icon_for("nu.exe"), 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct GateWriter {
        gate: Option<(std::sync::mpsc::Sender<()>, Receiver<()>)>,
        saved: Arc<Mutex<Vec<u8>>>,
        fail_after: usize,
    }

    impl Write for GateWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some((entered, resume)) = self.gate.take() {
                entered.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let mut saved = self.saved.lock().unwrap();
            if saved.len() >= self.fail_after {
                return Err(io::Error::other("injected disk write failure"));
            }
            saved.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn delayed_file_log_keeps_keys_responsive_and_flushes_in_order() {
        let mut session = TerminalSession::waiting(
            crate::TerminalShell::built_in().remove(1),
            None,
            Vec::new(),
            String::new(),
        );
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_rx) = channel();
        let (resume, resume_rx) = channel();
        let writer = GateWriter {
            gate: Some((entered, resume_rx)),
            saved: saved.clone(),
            fail_after: usize::MAX,
        };
        session.start_file_log_with("delayed.log".into(), None, move || Ok(writer), || {});
        session.terminal.feed(b"first\r\n");
        session.collect_file_log();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (input, keys) = sync_channel(2);
        session.input = Some(input);
        assert!(session.send_key(Key::Char('a'), Modifiers::none()));
        assert!(session.type_text("確定"));
        assert_eq!(keys.try_recv().unwrap(), b"a");
        assert_eq!(keys.try_recv().unwrap(), "確定".as_bytes());
        for at in 0..FILE_LOG_SLOTS {
            session.terminal.feed(format!("batch {at}\r\n").as_bytes());
            session.collect_file_log();
        }
        session.terminal.feed(b"final tail");
        session.stop_file_log();
        assert!(session.file_log_path().is_none());
        assert_eq!(file_log_paths(), [PathBuf::from("delayed.log")]);
        resume.send(()).unwrap();
        assert!(wait_for_file_logs().is_empty());
        let mut expected = String::from("first\n");
        for at in 0..FILE_LOG_SLOTS {
            expected.push_str(&format!("batch {at}\n"));
        }
        expected.push_str("final tail");
        assert_eq!(*saved.lock().unwrap(), expected.as_bytes());
    }

    #[test]
    fn terminal_file_log_reports_failure_after_session_is_dropped() {
        let mut session = TerminalSession::waiting(
            crate::TerminalShell::built_in().remove(1),
            None,
            Vec::new(),
            String::new(),
        );
        let saved = Arc::new(Mutex::new(Vec::new()));
        let writer = GateWriter {
            gate: None,
            saved: saved.clone(),
            fail_after: 1,
        };
        session.start_file_log_with("fail.log".into(), None, move || Ok(writer), || {});
        session.terminal.feed(b"first\r\n");
        session.collect_file_log();
        session.terminal.feed(b"tail");
        session.stop_file_log();
        drop(session);
        let errors = wait_for_file_logs();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("fail.log"));
        assert!(errors[0].contains("injected disk write failure"));
        assert_eq!(*saved.lock().unwrap(), b"first\n");
    }

    #[test]
    fn overflowing_a_file_log_reports_the_gap_and_preserves_accepted_batches() {
        let mut session = TerminalSession::waiting(
            crate::TerminalShell::built_in().remove(1),
            None,
            Vec::new(),
            String::new(),
        );
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_rx) = channel();
        let (resume, resume_rx) = channel();
        let writer = GateWriter {
            gate: Some((entered, resume_rx)),
            saved: saved.clone(),
            fail_after: usize::MAX,
        };
        session.start_file_log_with("full.log".into(), None, move || Ok(writer), || {});
        session.terminal.feed(b"first\r\n");
        session.collect_file_log();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        for _ in 0..FILE_LOG_SLOTS {
            session.terminal.feed(b"accepted\r\n");
            session.collect_file_log();
        }
        session.terminal.feed(b"rejected\r\n");
        session.collect_file_log();
        assert!(
            session
                .take_log_error()
                .is_some_and(|error| error.contains("full.log"))
        );
        assert!(session.file_log_path().is_none());
        resume.send(()).unwrap();
        assert!(wait_for_file_logs().is_empty());
        assert_eq!(
            *saved.lock().unwrap(),
            format!("first\n{}", "accepted\n".repeat(FILE_LOG_SLOTS)).as_bytes()
        );
    }

    #[test]
    #[ignore = "starts a real Windows shell; run explicitly"]
    fn terminal_simultaneous_file_panel_independent_stop_and_failure() {
        let path = std::env::temp_dir().join(format!("rfn-both-log-{}.txt", std::process::id()));
        let mut session = TerminalSession::start("QA", "cmd.exe /Q /D /K", 100, 12, || {}).unwrap();
        session.wait(Duration::from_millis(300));
        session.start_capture(None);
        session.start_file_log(path.clone(), None, || {});
        let collect = |session: &mut TerminalSession, marker: &str| {
            session.type_text(&format!("echo {marker}\r"));
            let mut panel = String::new();
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                session.wait(Duration::from_millis(50));
                if let Some((done, _)) = session.capture_update() {
                    panel.push_str(&done);
                }
                if panel.contains(marker) {
                    break;
                }
            }
            assert!(panel.contains(marker), "missing Panel output: {marker}");
        };
        collect(&mut session, "RFN_BOTH");
        session.stop_file_log();
        assert!(wait_for_file_logs().is_empty());
        let stopped = std::fs::read_to_string(&path).unwrap();
        assert!(stopped.contains("RFN_BOTH"));
        collect(&mut session, "RFN_PANEL_CONTINUES");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), stopped);
        session.start_file_log(path.clone(), None, || {});
        collect(&mut session, "RFN_RESTARTED");
        session.stop_capture();
        session.type_text("echo RFN_FILE_CONTINUES\r");
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            session.wait(Duration::from_millis(50));
            if std::fs::read_to_string(&path).is_ok_and(|text| text.contains("RFN_FILE_CONTINUES"))
            {
                break;
            }
        }
        session.stop_file_log();
        assert!(wait_for_file_logs().is_empty());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("RFN_FILE_CONTINUES")
        );
        session.start_capture(None);
        let read_only = path.clone();
        session.start_file_log_with(
            path.clone(),
            None,
            move || std::fs::File::open(read_only),
            || {},
        );
        collect(&mut session, "RFN_PANEL_AFTER_FILE_FAILURE");
        session.stop_file_log();
        assert!(session.file_log_path().is_none());
        assert_eq!(wait_for_file_logs().len(), 1);
        session.stop_capture();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "starts a real Windows shell; run explicitly"]
    fn stamped_file_log_stamps_every_line_and_the_tail_it_stops_on() {
        let path = std::env::temp_dir().join(format!("rfn-stamp-log-{}.txt", std::process::id()));
        let mut session = TerminalSession::start("QA", "cmd.exe /Q /D /K", 100, 12, || {}).unwrap();
        session.wait(Duration::from_millis(200));
        let stamp = Stamp::new("#yyyy-MM-dd HH:mm:ss.fff# ");
        session.start_file_log(path.clone(), Some(stamp), || {});
        session.type_text("echo RFN_STAMP_A& echo RFN_STAMP_B\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            session.wait(Duration::from_millis(50));
            if std::fs::read_to_string(&path)
                .is_ok_and(|text| text.contains("\nRFN_STAMP_B") || text.contains("# RFN_STAMP_B"))
            {
                break;
            }
        }
        session.stop_file_log();
        assert!(wait_for_file_logs().is_empty());
        let saved = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = saved.lines().collect();
        assert!(
            lines.iter().any(|l| l.ends_with("# RFN_STAMP_A")),
            "{saved:?}"
        );
        assert!(
            lines.iter().any(|l| l.ends_with("# RFN_STAMP_B")),
            "{saved:?}"
        );
        // `#2026-09-23 14:03:12.345# ` — 26 characters in front of every line,
        // the prompt the log stopped on included.
        assert!(
            lines.iter().all(|l| l.len() >= 26
                && l.starts_with('#')
                && &l[24..26] == "# "
                && l[1..24].bytes().filter(u8::is_ascii_digit).count() == 17),
            "{saved:?}"
        );
        assert!(!saved.ends_with('\n'), "the prompt was the unfinished tail");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "starts a real Windows shell; run explicitly"]
    fn direct_file_log_flushes_tail_and_reports_write_failure() {
        let path = std::env::temp_dir().join(format!("rfn-direct-log-{}.txt", std::process::id()));
        let mut session = TerminalSession::start("QA", "cmd.exe /Q /D /K", 100, 12, || {}).unwrap();
        session.wait(Duration::from_millis(200));
        session.set_history_limit(0);
        session.start_file_log(path.clone(), None, || {});
        session.type_text("echo RFN_FILE_LINE\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            session.wait(Duration::from_millis(50));
            if std::fs::read_to_string(&path).is_ok_and(|text| text.contains("RFN_FILE_LINE")) {
                break;
            }
        }
        session.stop_file_log();
        assert!(wait_for_file_logs().is_empty());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("RFN_FILE_LINE"));
        assert!(!saved.contains("[20"), "no timestamps were asked for");
        assert!(session.file_log_path().is_none());
        assert!(session.take_log_error().is_none());
        session.stop_file_log();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        // A read-only handle exercises failure without changing permissions.
        let read_only = path.clone();
        session.start_file_log_with(
            path.clone(),
            None,
            move || std::fs::File::open(read_only),
            || {},
        );
        session.type_text("echo RFN_KEEP_ON_FAILURE\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && session.file_log_path().is_some() {
            session.wait(Duration::from_millis(50));
        }
        assert!(session.file_log_path().is_none());
        assert_eq!(wait_for_file_logs().len(), 1);
        assert!(
            session
                .screen()
                .retained_text()
                .contains("RFN_KEEP_ON_FAILURE")
        );
        std::fs::remove_file(path).unwrap();
    }

    /// The three layers, a real shell, and no window (追加要件 Terminal).
    ///
    /// **`#[ignore]` because it needs a shell**, not because it is slow — it
    /// takes about a second. `PTY_SHELL` chooses one; the default is 要件's
    /// default. Run it with
    /// `cargo test --offline a_shell_answers -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_shell_answers_onto_the_screen() {
        let command = std::env::var("PTY_SHELL").unwrap_or_else(|_| "wsl.exe".to_owned());
        let mut session =
            TerminalSession::start("test", &command, 80, 25, || {}).expect("open the shell");

        // The prompt. **Waiting for the screen to say something is the honest
        // test** — how many reads that takes is the shell's business.
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_secs(10) && session.screen().row_text(0).is_empty()
        {
            session.wait(Duration::from_millis(200));
        }
        let prompt = session.screen().row_text(0);
        assert!(!prompt.is_empty(), "no prompt in 10s from {command}");
        println!("prompt: {prompt}");

        // This test injects keys faster than a user can type. Wait for the
        // bounded input queue instead of counting a refused write as a keypress.
        let keys = "echo RFN-SESSION-OK"
            .chars()
            .map(Key::Char)
            .chain([Key::Enter]);
        for key in keys {
            let bytes = encode_key(key, Modifiers::none(), session.screen().modes());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !session.send(&bytes) {
                assert!(Instant::now() < deadline, "the shell did not accept a key");
                session.wait(Duration::from_millis(10));
            }
        }

        let waited = Instant::now();
        let mut answered = false;
        while waited.elapsed() < Duration::from_secs(10) && !answered {
            session.wait(Duration::from_millis(200));
            // **The line the shell echoed and the line it printed are both on
            // the screen**, so the answer is the second one.
            let hits = (0..session.screen().rows())
                .filter(|row| session.screen().row_text(*row) == "RFN-SESSION-OK")
                .count();
            answered = hits >= 1;
        }
        for row in 0..6 {
            println!("{row}: {}", session.screen().row_text(row));
        }
        assert!(answered, "the shell's answer never reached the screen");

        session.send_key(Key::Char('d'), Modifiers::control());
    }

    /// RFN01-104: Editor PanelのCtrl+Enter（途中の改行は`Ctrl+J`、末尾は`Enter`）を本物の
    /// シェルへ。**どのシェルでも2行とも実行される**ことを見る。
    /// `cargo test --offline a_draft_runs_every_line -- --ignored --nocapture`
    #[test]
    #[ignore = "starts real Windows shells; run explicitly"]
    fn a_draft_runs_every_line_in_each_shell() {
        for command in ["powershell.exe -NoLogo -NoProfile", "cmd.exe", "wsl.exe"] {
            let mut session =
                TerminalSession::start("test", command, 100, 30, || {}).expect("open the shell");
            let waited = Instant::now();
            while waited.elapsed() < Duration::from_secs(15)
                && (0..session.screen().rows()).all(|row| session.screen().row_text(row).is_empty())
            {
                session.wait(Duration::from_millis(200));
            }
            session.wait(Duration::from_millis(1500));
            session.drain();
            println!(
                "{command}: bracketed_paste={} win32_input={}",
                session.screen().modes().bracketed_paste,
                session.screen().modes().win32_input
            );
            session.send_draft("echo RFN-AAA\necho RFN-BBB\n");
            let printed = |session: &TerminalSession, word: &str| {
                (0..session.screen().rows())
                    .filter(|row| session.screen().row_text(*row).trim() == word)
                    .count()
            };
            let waited = Instant::now();
            while waited.elapsed() < Duration::from_secs(10)
                && (printed(&session, "RFN-AAA") == 0 || printed(&session, "RFN-BBB") == 0)
            {
                session.wait(Duration::from_millis(200));
            }
            println!("==== {command}");
            for row in 0..session.screen().rows() {
                let text = session.screen().row_text(row);
                if !text.trim().is_empty() {
                    println!("{row}: {text}");
                }
            }
            let both = printed(&session, "RFN-AAA") >= 1 && printed(&session, "RFN-BBB") >= 1;
            session.send(b"exit\r");
            assert!(both, "{command}: not every line ran");
        }
    }

    /// RFN01-104: Claude Code（WSLの`claude`）は貼り付けの印を受け付け、`Ctrl+J`で区切った
    /// 下書きは入力欄の中の改行になる。**末尾に改行を付けないので送信はしない**——画面を出して
    /// Ctrl+Cで閉じる。`CLAUDE_PROBE=1`のときだけ動く。
    #[test]
    #[ignore = "starts Claude Code in WSL; run explicitly with CLAUDE_PROBE=1"]
    fn claude_code_takes_a_draft_as_one_input() {
        if std::env::var("CLAUDE_PROBE").is_err() {
            return;
        }
        let mut session = TerminalSession::start(
            "test",
            "wsl.exe --cd /mnt/d/Projects/10_Creation/50_Dev -e bash -ic claude",
            110,
            40,
            || {},
        )
        .expect("open claude");
        // 起動役のbashも貼り付けの印を立てるので、**Claude Codeの画面が出るまで**待つ。
        let drawn = |session: &TerminalSession| {
            (0..session.screen().rows())
                .filter(|row| !session.screen().row_text(*row).trim().is_empty())
                .count()
        };
        // `wait`は出力が来れば戻るので、決まった時間はこちらで刻む。
        let settle = |session: &mut TerminalSession, how_long: Duration| {
            let started = Instant::now();
            while started.elapsed() < how_long {
                session.wait(Duration::from_millis(100));
                thread::sleep(Duration::from_millis(50));
            }
            session.drain();
        };
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_secs(40) && drawn(&session) < 3 {
            session.wait(Duration::from_millis(300));
        }
        settle(&mut session, Duration::from_secs(5));
        let modes = session.screen().modes();
        println!(
            "bracketed_paste={} win32_input={}",
            modes.bracketed_paste, modes.win32_input
        );
        session.send_draft("RFN-LINE-ONE\nRFN-LINE-TWO");
        settle(&mut session, Duration::from_secs(3));
        for row in 0..session.screen().rows() {
            let text = session.screen().row_text(row);
            if !text.trim().is_empty() {
                println!("{row}: {text}");
            }
        }
        let rows: Vec<String> = (0..session.screen().rows())
            .map(|row| session.screen().row_text(row))
            .collect();
        let one = rows.iter().position(|row| row.contains("RFN-LINE-ONE"));
        let two = rows.iter().position(|row| row.contains("RFN-LINE-TWO"));
        for _ in 0..3 {
            session.send_key(Key::Char('c'), Modifiers::control());
            settle(&mut session, Duration::from_millis(500));
        }
        assert!(
            modes.bracketed_paste,
            "Claude Code did not turn bracketed paste on"
        );
        assert!(
            matches!((one, two), (Some(a), Some(b)) if b == a + 1),
            "the two lines are not one input on consecutive rows: {one:?} {two:?}"
        );
    }

    /// A command that ends says so, and what it printed is still there.
    #[test]
    #[ignore]
    fn a_shell_that_exits_leaves_its_last_words_on_the_screen() {
        let mut session =
            TerminalSession::start("test", "cmd.exe /c echo BYE-FROM-CONPTY", 80, 25, || {})
                .expect("open the shell");
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_secs(10) && !session.finished() {
            session.wait(Duration::from_millis(200));
            session.drain();
        }
        assert!(session.finished(), "the shell never closed the pipe");
        let seen = (0..session.screen().rows())
            .map(|row| session.screen().row_text(row))
            .any(|text| text.contains("BYE-FROM-CONPTY"));
        assert!(seen, "what it printed before exiting is gone");
    }
}

#[cfg(test)]
mod running_a_program {
    use super::*;
    use std::time::Instant;

    /// **道具**：本物の全画面プログラムを走らせて、升目に何が立ったかと、
    /// 解析器が知らなかった列を出す。
    ///
    /// 「画面が崩れる」に答えるのはこれである——崩れているのが升目なのか
    /// 描画なのかは、升目を字で見れば分かる。`#[ignore]`は本物のシェルが
    /// 要るからで、遅いからではない。
    ///
    /// ```text
    /// PTY_SHELL="wsl.exe -- vim /etc/hostname" PTY_COLUMNS=80 PTY_ROWS=24 \
    ///   cargo test --offline a_full_screen_program -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn a_full_screen_program_lands_on_the_screen() {
        let command =
            std::env::var("PTY_SHELL").unwrap_or_else(|_| "wsl.exe -- top -n 2 -d 1 -b".to_owned());
        let columns: usize = std::env::var("PTY_COLUMNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100);
        let rows: usize = std::env::var("PTY_ROWS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30);
        let patience: u64 = std::env::var("PTY_WAIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8);
        let mut session =
            TerminalSession::start("test", &command, columns, rows, || {}).expect("open the shell");
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_secs(patience) {
            session.wait(Duration::from_millis(200));
        }
        // 打ってみる（`PTY_TYPE`）。**入力側の文字コードはこれでしか分からない。**
        if let Ok(typed) = std::env::var("PTY_TYPE") {
            session.paste(&typed);
            session.send_key(Key::Enter, Modifiers::none());
            let waited = Instant::now();
            while waited.elapsed() < Duration::from_secs(patience) {
                session.wait(Duration::from_millis(200));
            }
        }
        println!("--- {columns}x{rows} {command} ---");
        for row in 0..rows {
            println!("{row:>3}|{}", session.screen().row_text(row));
        }
        println!("--- cursor {:?} ---", session.screen().cursor());
        if let Ok(path) = std::env::var("TERMINAL_PNG") {
            use crate::directwrite_render::cells;
            let look = cells::TerminalLook::default();
            let cell = cells::terminal_cell_size(&look).expect("measure");
            let width = (columns as f32 * cell.advance).ceil() as u32;
            let height = (rows as f32 * cell.line).ceil() as u32;
            let mut pixels = vec![0_u8; (width * height * 4) as usize];
            let at = session.screen().cursor();
            cells::draw_terminal(
                session.screen().lines(),
                &[],
                Some((at.row, at.column)),
                "",
                &look,
                cell,
                &mut pixels,
                width,
                height,
            )
            .expect("draw");
            std::fs::write(&path, &pixels).expect("write");
            println!("wrote {path} {width}x{height}");
        }
        println!("--- underlined blanks per row ---");
        for row in 0..rows.min(session.screen().rows()) {
            let line = session.screen().line(row).expect("row");
            let marked = line
                .cells
                .iter()
                .filter(|cell| cell.attrs.underline && cell.text == ' ')
                .count();
            if marked > 0 {
                println!("  row {row}: {marked} underlined blanks");
            }
        }
        println!("--- unhandled ---");
        for (what, times) in session.screen().unhandled() {
            println!("  {what} ×{times}");
        }
    }

    fn settle(session: &mut TerminalSession, millis: u64) {
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_millis(millis) {
            session.wait(Duration::from_millis(100));
        }
    }

    fn screen_contains(session: &TerminalSession, needle: &str) -> bool {
        (0..session.screen().rows()).any(|row| session.screen().row_text(row).contains(needle))
    }

    /// 全画面TUIの互換性（2026-09-22）: ConPTYを越えて、クリック・ホイール・
    /// フォーカスが本物のvimとlessに届く。WSLにvimとlessが要る。
    ///
    /// `cargo test --offline full_screen_programs_hear -- --ignored --nocapture`
    #[test]
    #[ignore = "starts vim and less in WSL; run explicitly"]
    fn full_screen_programs_hear_the_pointer_and_the_focus() {
        use crate::terminal::{MouseAction, MouseButton};
        let none = Modifiers::none();

        // vim, mouse=a: a click moves the cursor, and the ruler says where.
        // **A file of short lines**, so that screen row 6 is line 6.
        let mut vim = TerminalSession::start(
            "vim",
            "wsl.exe -- vim -N -u NONE -c \"set mouse=a ruler\" \
             -c \"au FocusLost * echo 'RFN-LOST'\" /etc/passwd",
            80,
            24,
            || {},
        )
        .expect("start vim");
        settle(&mut vim, 3000);
        assert!(vim.wants_mouse(), "vim asked for the pointer");
        assert!(vim.send_mouse(MouseAction::Press, MouseButton::Left, 5, 19, none));
        assert!(vim.send_mouse(MouseAction::Release, MouseButton::Left, 5, 19, none));
        settle(&mut vim, 800);
        assert!(screen_contains(&vim, "6,20"), "the click reached vim");
        assert!(vim.send_wheel(false, 3, 5, 19, none));
        settle(&mut vim, 800);
        assert!(!screen_contains(&vim, "Top"), "the wheel scrolled vim");
        vim.send_focus(false);
        settle(&mut vim, 800);
        assert!(screen_contains(&vim, "RFN-LOST"), "vim heard the focus go");
        vim.type_text(":qa!\r");

        // less asks for no pointer, but it is on the alternate screen: the
        // wheel becomes arrow keys.
        let mut less =
            TerminalSession::start("less", "wsl.exe -- less /etc/services", 80, 24, || {})
                .expect("start less");
        settle(&mut less, 3000);
        assert!(!less.wants_mouse());
        assert!(less.screen().modes().alternate_screen);
        let first = less.screen().row_text(0);
        assert!(less.send_wheel(false, 3, 0, 0, none));
        settle(&mut less, 800);
        assert_ne!(less.screen().row_text(0), first, "the wheel scrolled less");
        less.type_text("q");
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn input_reports_backpressure_and_sends_control_replies_before_user_input() {
        let mut session = TerminalSession::waiting(
            crate::TerminalShell::built_in().remove(1),
            None,
            Vec::new(),
            String::new(),
        );
        let (input, queued) = sync_channel(1);
        session.input = Some(input);
        assert!(session.send_key(Key::Char('A'), Modifiers::none()));
        assert!(!session.send_key(Key::Backspace, Modifiers::none()));
        assert!(!session.type_text("確定"));
        assert!(!session.paste("paste"));
        session.pending_replies = b"control reply".to_vec();
        assert_eq!(queued.try_recv().unwrap(), b"A");
        assert!(!session.send_key(Key::Char('B'), Modifiers::none()));
        assert_eq!(queued.try_recv().unwrap(), b"control reply");
        assert!(session.type_text("確定"));
        assert_eq!(queued.try_recv().unwrap(), "確定".as_bytes());
        drop(queued);
        assert!(!session.send_key(Key::Backspace, Modifiers::none()));
        assert!(!session.type_text("確定"));
        assert!(!session.paste("paste"));
    }

    #[test]
    #[ignore = "starts real ConPTY processes; run explicitly"]
    fn burst_output_reaches_its_tail_and_blocked_input_does_not_hold_close() {
        let mut burst = TerminalSession::start(
            "cmd",
            "cmd.exe /d /q /c for /L %i in (1,1,2500) do @echo RFN-BURST-%i",
            80,
            24,
            || {},
        )
        .unwrap();
        burst.set_history_limit(3000);
        thread::sleep(Duration::from_millis(150));
        let until = Instant::now() + Duration::from_secs(10);
        while !burst.finished() && Instant::now() < until {
            burst.wait(Duration::from_millis(15));
        }
        let lines = burst.output_lines(3000);
        assert!(burst.finished());
        let first = lines
            .iter()
            .position(|line| line.contains("RFN-BURST-1"))
            .unwrap();
        let last = lines
            .iter()
            .position(|line| line.contains("RFN-BURST-2500"))
            .unwrap();
        assert!(last > first);

        let mut blocked = TerminalSession::start(
            "PowerShell",
            "powershell.exe -NoProfile -Command Start-Sleep -Seconds 5",
            80,
            24,
            || {},
        )
        .unwrap();
        let payload = vec![b'x'; MAX_INPUT_BYTES];
        let started = Instant::now();
        for _ in 0..INPUT_SLOTS + 3 {
            blocked.send(&payload);
        }
        blocked.resize(90, 26);
        drop(blocked);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
#[cfg(test)]
mod shell_integration_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn shell_close_only_skips_confirmation_at_a_reported_prompt() {
        let mut terminal = Terminal::new(80, 24);
        assert!(!terminal.shell_at_prompt());
        terminal.feed(b"\x1b]13");
        terminal.feed(b"3;A\x07");
        assert!(terminal.shell_at_prompt());
        terminal.command_submitted();
        assert!(!terminal.shell_at_prompt());
        terminal.feed(b"\x1b]133;C\x1b\\");
        assert!(!terminal.shell_at_prompt());
        terminal.feed(b"\x1b]133;A\x1b\\");
        assert!(terminal.shell_at_prompt());
    }

    #[test]
    #[ignore = "starts real ConPTY shells, including WSL Bash"]
    fn shell_integration_reports_idle_and_running_through_conpty() {
        for (command, work) in [
            (
                "powershell.exe -NoLogo -NoProfile",
                "Start-Sleep -Seconds 1\r",
            ),
            ("pwsh.exe -NoLogo -NoProfile", "Start-Sleep -Seconds 1\r"),
            ("wsl.exe", "sleep 1\r"),
        ] {
            let shell = crate::TerminalShell {
                id: 0,
                name: "QA".into(),
                command: command.into(),
                directory: String::new(),
            };
            let (launch, directory) = crate::terminal_shells::launch(&shell);
            let environment = crate::terminal_shells::wsl_shell_environment(&shell);
            let mut session = TerminalSession::start_in(
                "QA",
                &launch,
                directory.as_deref(),
                environment.as_deref(),
                80,
                24,
                || {},
            )
            .unwrap();
            let wait_for_idle = |session: &mut TerminalSession| {
                let until = Instant::now() + Duration::from_secs(8);
                while session.needs_stop_confirmation() && Instant::now() < until {
                    session.wait(Duration::from_millis(30));
                }
                assert!(
                    session.terminal.shell_at_prompt(),
                    "{command}: no prompt marker"
                );
            };
            wait_for_idle(&mut session);
            session.type_text(work);
            assert!(
                session.needs_stop_confirmation(),
                "{command}: command submitted"
            );
            wait_for_idle(&mut session);
        }
    }
}
