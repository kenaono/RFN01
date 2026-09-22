//! The shell, on the other side of a pseudo console (追加要件 2026-09-06).
//!
//! **The thin half.** Everything here is a Windows call; what the shell writes
//! back is bytes, and making sense of them belongs somewhere that knows nothing
//! about Windows (技術検証 9.3 calls the two halves `terminal-win` and
//! `terminal-core`). The line is drawn here so the half that has rules worth
//! testing can be tested without a console.
//!
//! **The shell is another process, and that is the point.** It can hang, it can
//! be killed, it can print anything at all; none of that is in this process. A
//! pseudo console gives it a screen to write to and gives us the bytes it wrote.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, UpdateProcThreadAttribute,
};
use windows::core::PWSTR;

/// A handle that may be carried to another thread.
///
/// **A Windows handle is a number, and the kernel object behind it is shared by
/// every thread in the process** — unlike the COM objects the renderer holds,
/// which are stuck to the thread that made them (技術検証 7.3). The reading
/// thread owns one of these and nothing else, which is the whole of what
/// ペイン分割設計 7.2 asks of a worker.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

// SAFETY: a `HANDLE` is an index into the process-wide handle table. Moving one
// between threads is what `CreateThread` does with every handle it is given.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Read what the shell has written, blocking until there is some.
    ///
    /// `Ok(0)` means the far end has gone: the shell exited and the console
    /// closed its side of the pipe. **A read that fails is the same event** —
    /// the pipe is broken — so the caller has one thing to check, not two.
    pub fn read(&self, into: &mut [u8]) -> io::Result<usize> {
        let mut read = 0_u32;
        let taken = unsafe {
            windows::Win32::Storage::FileSystem::ReadFile(self.0, Some(into), Some(&mut read), None)
        };
        match taken {
            Ok(()) => Ok(read as usize),
            Err(_) => Ok(0),
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// A shell running under a pseudo console.
#[derive(Debug)]
pub struct Pty {
    console: HPCON,
    /// Our end of the shell's standard input.
    writer: OwnedHandle,
    /// Our end of the shell's output. **Taken out to be read on a thread**, so
    /// this is `None` once somebody has it.
    reader: Option<OwnedHandle>,
    process: OwnedHandle,
}

impl Pty {
    /// Start `command` on a console `columns` by `rows` in size.
    ///
    /// The order below is the one the console API asks for and cannot be
    /// rearranged: **the console is given the far ends of both pipes, and we
    /// close our copies of them straight away** — they are duplicated into the
    /// console host, and a copy left open here would keep the pipe alive after
    /// the shell had gone, so nothing would ever read end-of-file.
    #[cfg(test)]
    pub fn open(command: &str, columns: u16, rows: u16) -> windows::core::Result<Self> {
        Self::open_in(command, None, columns, rows)
    }

    pub fn open_in(
        command: &str,
        directory: Option<&std::path::Path>,
        columns: u16,
        rows: u16,
    ) -> windows::core::Result<Self> {
        unsafe {
            let (input_read, input_write) = pipe()?;
            let (output_read, output_write) = pipe()?;
            let size = COORD {
                X: columns.max(1) as i16,
                Y: rows.max(1) as i16,
            };
            let console = CreatePseudoConsole(size, input_read.raw(), output_write.raw(), 0)?;
            drop(input_read);
            drop(output_write);

            // The attribute list has to outlive `CreateProcessW`, so it is a
            // buffer here rather than a temporary: the startup info holds a
            // pointer into it.
            let mut room = 0_usize;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut room);
            let mut attributes = vec![0_u8; room];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(attributes.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut room)?;
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                // **The handle itself, not a pointer to it.** Nearly every
                // other attribute passes an address and a length; this one
                // passes the `HPCON` in the pointer slot (as MS' own EchoCon
                // sample does). Handing over the address instead starts the
                // child successfully and then it hangs forever, attached to
                // nothing — no error anywhere to say so.
                Some(console.0 as *const c_void),
                size_of::<HPCON>(),
                None,
                None,
            )?;

            // **The child's three standard handles are named, and named as
            // nothing.** Left unsaid, a child takes the parent's — and when
            // those are pipes rather than console handles (which is what a
            // process started from a shell has) the shell writes its prompt
            // into our pipes and reads end-of-file from our stdin, both past
            // the pseudo console entirely. Saying `STARTF_USESTDHANDLES` with
            // nothing in the three slots leaves the console the only thing the
            // child has to talk through, which is the point of opening one.
            let startup = STARTUPINFOEXW {
                StartupInfo: STARTUPINFOW {
                    cb: size_of::<STARTUPINFOEXW>() as u32,
                    dwFlags: STARTF_USESTDHANDLES,
                    hStdInput: HANDLE::default(),
                    hStdOutput: HANDLE::default(),
                    hStdError: HANDLE::default(),
                    ..Default::default()
                },
                lpAttributeList: list,
            };
            let directory: Option<Vec<u16>> =
                directory.map(|p| p.as_os_str().encode_wide().chain(Some(0)).collect());
            let mut line = wide(command);
            let mut spawned = PROCESS_INFORMATION::default();
            let started = CreateProcessW(
                None,
                Some(PWSTR(line.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT,
                None,
                directory
                    .as_ref()
                    .map_or(windows::core::PCWSTR::null(), |p| {
                        windows::core::PCWSTR(p.as_ptr())
                    }),
                &startup.StartupInfo,
                &mut spawned,
            );
            DeleteProcThreadAttributeList(list);
            if let Err(error) = started {
                ClosePseudoConsole(console);
                return Err(error);
            }
            // The thread handle is nothing to us: what says the shell is still
            // there is the process. **Our end of the output moves into the
            // struct rather than being copied**: an `OwnedHandle` closes what
            // it holds, so a second one made from the same number would close
            // the reader on the way out of this function and every read would
            // see end-of-file.
            let _ = CloseHandle(spawned.hThread);
            Ok(Self {
                console,
                writer: input_write,
                reader: Some(output_read),
                process: OwnedHandle(spawned.hProcess),
            })
        }
    }

    /// Take the reading end, to be read on a thread of its own.
    ///
    /// **Once.** The output is a stream, and two readers would each get part of
    /// it.
    pub fn take_reader(&mut self) -> Option<OwnedHandle> {
        self.reader.take()
    }

    /// Send keystrokes to the shell.
    pub fn write(&self, bytes: &[u8]) -> windows::core::Result<()> {
        let mut written = 0_u32;
        unsafe {
            windows::Win32::Storage::FileSystem::WriteFile(
                self.writer.raw(),
                Some(bytes),
                Some(&mut written),
                None,
            )
        }
    }

    /// Tell the shell the screen changed size (要件 6.4: a pane can be dragged).
    pub fn resize(&self, columns: u16, rows: u16) -> windows::core::Result<()> {
        let size = COORD {
            X: columns.max(1) as i16,
            Y: rows.max(1) as i16,
        };
        unsafe { ResizePseudoConsole(self.console, size) }
    }

    /// The shell has exited.
    ///
    /// **The pipe cannot answer this.** A pseudo console holds its own end of
    /// the output pipe until it is closed, so end-of-file arrives when *we* let
    /// go of the console — never when the shell does. What says the shell is
    /// gone is the process, and only the process.
    pub fn exited(&self) -> bool {
        let waited = unsafe {
            windows::Win32::System::Threading::WaitForSingleObject(self.process.raw(), 0)
        };
        waited == windows::Win32::Foundation::WAIT_OBJECT_0
    }

    #[cfg(test)]
    pub fn process(&self) -> HANDLE {
        self.process.raw()
    }
}

impl Drop for Pty {
    /// **The console goes first.** Closing it is what tells the shell its
    /// screen has gone, and the shell exits; closing our pipe ends first would
    /// leave it writing into nothing.
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.console) };
    }
}

/// Both ends of one pipe. Neither is inheritable: the console duplicates what
/// it is given, and a handle the whole world inherits is one nothing can close.
fn pipe() -> windows::core::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, None, 0)? };
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// A command line as `CreateProcessW` wants it: writable, and terminated.
fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// ConPTYの経路そのものを、窓を作らずに端から端まで通す。
    ///
    /// **`#[ignore]`なのは本物のシェルが要るから**で、遅いからではない（0.3秒で
    /// 終わる）。`cargo test -- --ignored --nocapture`で走らせると、シェルが
    /// 実際に送ってきたバイト列がそのまま出る——解析器はそれを見て書く。
    /// 相手は`PTY_SHELL`で選べる（既定は要件の既定と同じ`wsl.exe`）。
    #[test]
    #[ignore]
    fn conpty_runs_a_shell() {
        let command = std::env::var("PTY_SHELL").unwrap_or_else(|_| "wsl.exe".to_owned());
        let started = Instant::now();
        let mut pty = match Pty::open(&command, 80, 25) {
            Ok(pty) => pty,
            Err(error) => panic!("open {command}: {error}"),
        };
        println!(
            "opened {command} in {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );

        let reader = pty.take_reader().expect("the reader is there once");
        let (post, collect) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if post.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // 子は本当に生きているか。0バイトのとき、原因は「書いていない」か
        // 「もう居ない」かのどちらかで、それはここでしか分からない。
        {
            use windows::Win32::Foundation::WAIT_OBJECT_0;
            use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
            let waited = unsafe { WaitForSingleObject(pty.process(), 300) };
            let mut code = 0_u32;
            let _ = unsafe { GetExitCodeProcess(pty.process(), &mut code) };
            println!(
                "child: {} (exit code {code}, STILL_ACTIVE=259)",
                if waited == WAIT_OBJECT_0 {
                    "gone"
                } else {
                    "running"
                }
            );
        }

        pty.write(b"echo RFN-PTY-OK\r").expect("write");
        // **答えは行の頭に出る。**打鍵の反響にも同じ字が出るが、反響は`echo `の後ろで、
        // しかもシェルの制御列（bracketed paste の`ESC[?2004l`など）が途中に挟まって
        // 割れることがある（2026-09-22、wsl.exeで実際にそうなった）。回数で数えると、
        // 答えているのに落ちる。
        let marker = b"\nRFN-PTY-OK";
        let mut seen: Vec<u8> = Vec::new();
        let mut first = None;
        let waited = Instant::now();
        let patience = std::env::var("PTY_WAIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(25);
        while waited.elapsed() < Duration::from_secs(patience) {
            match collect.recv_timeout(Duration::from_millis(500)) {
                Ok(chunk) => {
                    if first.is_none() {
                        first = Some(waited.elapsed());
                    }
                    seen.extend_from_slice(&chunk);
                    if seen.windows(marker.len()).any(|window| window == marker) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        println!(
            "first byte after {:?}, answered after {:?}, {} bytes",
            first,
            waited.elapsed(),
            seen.len()
        );
        // What the shell actually sends, so the parser is written against it
        // rather than against a guess.
        let shown = seen
            .iter()
            .map(|byte| match byte {
                0x1b => "<ESC>".to_owned(),
                b'\r' => "<CR>".to_owned(),
                b'\n' => "<LF>\n".to_owned(),
                0x07 => "<BEL>".to_owned(),
                0x20..=0x7e => (*byte as char).to_string(),
                other => format!("<{other:02x}>"),
            })
            .collect::<String>();
        println!(
            "--- raw ---\n{}\n--- end ---",
            &shown[..shown.len().min(4000)]
        );
        pty.write(b"exit\r").expect("write");

        // **行の頭の答え。**無ければ、打鍵は届いても答えが戻っていない。
        assert!(
            seen.windows(marker.len()).any(|window| window == marker),
            "{command} sent {} bytes without answering at the start of a line; \
             the shell is not answering through the console",
            seen.len()
        );
    }
}
