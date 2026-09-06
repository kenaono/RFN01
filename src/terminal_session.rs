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

// Unwired until the pane exists, exactly as in [`crate::pty`].
#![allow(dead_code)]

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use crate::pty::Pty;
use crate::terminal::{Key, Modifiers, Screen, Terminal, encode_key, encode_paste};

/// How much is read at once. **A screenful of coloured text is a few KB**, and
/// a program printing a large file will simply come back around the loop.
const CHUNK: usize = 8192;

/// A running shell and the screen it is writing on.
pub struct TerminalSession {
    /// What to call this shell on its tab. **The shell's name, not the
    /// command** — `wsl.exe --cd . -- bash -l` is a command; `WSL` is what the
    /// writer chose.
    name: String,
    pty: Pty,
    terminal: Terminal,
    output: Receiver<Vec<u8>>,
    /// The shell has exited. **The screen stays** — what it last said is often
    /// the reason it exited.
    finished: bool,
}

impl TerminalSession {
    /// Start `command` on a screen `columns` by `rows`.
    ///
    /// `wake` is called from the reading thread every time bytes arrive, and
    /// means only "there is something to collect". Applying it is
    /// [`Self::drain`], on the thread that owns the window.
    pub fn start(
        name: &str,
        command: &str,
        columns: usize,
        rows: usize,
        wake: impl Fn() + Send + 'static,
    ) -> windows::core::Result<Self> {
        let mut pty = Pty::open(command, columns as u16, rows as u16)?;
        let reader = pty
            .take_reader()
            .expect("a freshly opened pty still has its reader");
        let (sender, output) = channel::<Vec<u8>>();
        let spawned = thread::Builder::new()
            .name("rfnedit-terminal".to_owned())
            .spawn(move || {
                let mut buffer = vec![0_u8; CHUNK];
                loop {
                    // **Blocking, and that is the point**: nothing here spins,
                    // and a shell that says nothing for an hour costs nothing.
                    let read = match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    if sender.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                    wake();
                }
            });
        if spawned.is_err() {
            // Without the thread there is no terminal — reading on the window's
            // thread is exactly what must not happen.
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "the terminal's reading thread could not be started",
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            pty,
            terminal: Terminal::new(columns, rows),
            output,
            finished: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn screen(&self) -> &Screen {
        &self.terminal.screen
    }

    /// The shell has exited and everything it wrote has been applied.
    ///
    /// **The pipe does not say this** (see [`Pty::exited`]): a pseudo console
    /// holds the far end open until we close it, so waiting for end-of-file
    /// waits forever. The process is asked instead, and only once a drain has
    /// found nothing left — otherwise the pane would be told the shell is gone
    /// while its last line was still on its way.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Apply everything that has arrived. `true` if the screen changed.
    ///
    /// **Everything waiting is applied before one repaint**, which is what
    /// makes a program printing thousands of lines cost the window a frame
    /// rather than a frame per line.
    pub fn drain(&mut self) -> bool {
        let before = self.terminal.screen.revision();
        let mut applied = 0_usize;
        let mut closed = false;
        loop {
            match self.output.try_recv() {
                Ok(chunk) => {
                    self.terminal.feed(&chunk);
                    applied += 1;
                }
                Err(TryRecvError::Empty) => break,
                // The reading thread stopped, which with a pseudo console means
                // we closed the console — not that the shell exited.
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        self.answer();
        if applied == 0 && (closed || self.pty.exited()) {
            self.finished = true;
        }
        self.terminal.screen.revision() != before
    }

    /// Wait for the shell to say something, then apply it. **For tests and for
    /// nothing else** — the window is woken, it does not wait.
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
                false
            }
        }
    }

    /// Send back what the shell asked for (cursor position, what we are).
    ///
    /// **A question left unanswered is a program left waiting**, so this
    /// happens on every drain rather than when somebody thinks to do it.
    fn answer(&mut self) {
        let replies = self.terminal.screen.take_replies();
        if !replies.is_empty() {
            let _ = self.pty.write(&replies);
        }
    }

    pub fn send_key(&mut self, key: Key, modifiers: Modifiers) {
        let bytes = encode_key(key, modifiers, self.terminal.screen.modes());
        self.send(&bytes);
    }

    pub fn paste(&mut self, text: &str) {
        let bytes = encode_paste(text, self.terminal.screen.modes());
        self.send(&bytes);
    }

    /// Write bytes as they are. The pane uses this for what it has already
    /// encoded; everything else should go through [`Self::send_key`].
    pub fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.pty.write(bytes);
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
        let _ = self.pty.resize(columns as u16, rows as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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

        for byte in b"echo RFN-SESSION-OK" {
            session.send_key(Key::Char(*byte as char), Modifiers::none());
        }
        session.send_key(Key::Enter, Modifiers::none());

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
                Some((at.row, at.column)),
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
}
