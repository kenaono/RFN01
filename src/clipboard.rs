//! Putting text on the Windows clipboard (要件 11.2).
//!
//! **Only the way out is written here.** Pasting arrives on its own: `Ctrl+V`
//! lands in the hidden field the IME writes through, and comes back out of it
//! as typed input like anything else the writer puts in (技術検証 7.2). Copying
//! has no such path — the selection lives in the engine and that field is
//! always empty — so this is the direction the editor has to take itself.
//!
//! 要件 11.6 will want a Kill Ring kept apart from this. That is a list of
//! strings inside the editor and shares nothing with the clipboard but the
//! operations that reach both, so it does not belong in this file.

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// `CF_UNICODETEXT`. **The named constant sits in `Win32_System_Ole`**, sixteen
/// thousand lines of the `windows` crate to compile for one number that has not
/// moved since Windows NT.
const UNICODE_TEXT: u32 = 13;

/// Put text on the clipboard. `false` if it did not get there.
///
/// **The answer is what a cut waits on.** Another program can hold the
/// clipboard open, and taking the text out of the document when nothing was
/// handed over would lose it with no way back short of Undo.
///
/// Line breaks go out as CRLF. Documents hold LF alone, and `normalize_typed_
/// input` takes the pair apart again on the way back in — the pairing is what
/// every other Windows program reads and writes.
pub fn put_text(owner: Option<HWND>, text: &str) -> bool {
    let wide = wide_text(text);
    let bytes = size_of_val(wide.as_slice());

    // SAFETY: the block owns `memory` until `SetClipboardData` takes it, and
    // frees it on every path that does not. The copy writes `wide.len()` units
    // into an allocation made for exactly that many.
    unsafe {
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            return false;
        };
        let destination = GlobalLock(memory);
        if destination.is_null() {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), destination.cast::<u16>(), wide.len());
        // **Ignored deliberately.** `GlobalUnlock` reports the lock count that
        // is left, so the last unlock of a block returns false with no error
        // set — success and failure look the same from here.
        let _ = GlobalUnlock(memory);

        if OpenClipboard(owner).is_err() {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        let handed_over = EmptyClipboard().is_ok()
            && SetClipboardData(UNICODE_TEXT, Some(HANDLE(memory.0))).is_ok();
        // The clipboard owns the block once it has taken it, and freeing it
        // here would be freeing somebody else's memory.
        if !handed_over {
            let _ = GlobalFree(Some(HGLOBAL(memory.0)));
        }
        let _ = CloseClipboard();
        handed_over
    }
}

/// The UTF-16 run `CF_UNICODETEXT` is handed: CRLF for every line break, and a
/// NUL after the last unit.
///
/// **A document never holds a carriage return of its own** — everything typed
/// or pasted passes through `normalize_typed_input`, which turns a CRLF back
/// into one break — so every `\r` here is one this put there.
fn wide_text(text: &str) -> Vec<u16> {
    let mut wide: Vec<u16> = Vec::with_capacity(text.len() + 1);
    let mut unit = [0u16; 2];
    for character in text.chars() {
        if character == '\n' {
            wide.push(u16::from(b'\r'));
        }
        wide.extend_from_slice(character.encode_utf16(&mut unit));
    }
    wide.push(0);
    wide
}

#[cfg(test)]
mod tests {
    use super::wide_text;

    fn without_the_nul(text: &str) -> String {
        let wide = wide_text(text);
        let (last, body) = wide.split_last().expect("a run always ends with a NUL");
        assert_eq!(*last, 0, "the run has to end where CF_UNICODETEXT stops");
        String::from_utf16(body).expect("what went in was valid UTF-16")
    }

    #[test]
    fn pairs_every_line_break_with_a_carriage_return() {
        assert_eq!(without_the_nul("一\n二\n"), "一\r\n二\r\n");
    }

    #[test]
    fn leaves_text_without_breaks_as_it_is() {
        assert_eq!(without_the_nul("見出し abc"), "見出し abc");
    }

    #[test]
    fn empty_text_is_just_the_terminator() {
        assert_eq!(wide_text(""), vec![0]);
    }

    /// A character outside the basic plane is two units, and counting it as one
    /// would cut it in half.
    #[test]
    fn carries_a_surrogate_pair_whole() {
        assert_eq!(wide_text("𠮷").len(), 3);
        assert_eq!(without_the_nul("𠮷"), "𠮷");
    }
}
