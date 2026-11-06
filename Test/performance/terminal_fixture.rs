//! Finite output for the RFN Edit Terminal performance scenario.

use std::ffi::c_void;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(kind: u32) -> *mut c_void;
    fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
}

fn enable_ansi_output() -> io::Result<()> {
    // The handle is borrowed from this process and the mode pointer is valid for the call.
    unsafe {
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        // Redirected stdout already transports the escape sequences as bytes.
        if GetConsoleMode(output, &mut mode) != 0
            && SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    enable_ansi_output()?;
    let mut output = io::stdout().lock();
    writeln!(output, "RFN-PERF-TERMINAL BEGIN")?;
    for batch in 0..80 {
        for offset in 0..8 {
            let row = batch * 8 + offset + 1;
            writeln!(
                output,
                "{row:04} \x1b[1;34m20_World\x1b[0m  \x1b[32mrfn-bench.sh\x1b[0m  \
                 \x1b[38;5;202m日本語の描画\x1b[0m  ASCII 0123456789"
            )?;
        }
        output.flush()?;
        thread::sleep(Duration::from_millis(40));
    }
    writeln!(output, "RFN-PERF-TERMINAL END rows=640")?;
    output.flush()
}
