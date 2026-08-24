//! What Windows itself does: the recycle bin, Explorer, and the colour dialog.
//!
//! All three belong to the system rather than to the editor. 要件 5.2 asks that
//! a deleted file stay recoverable and that the writer can reach it in
//! Explorer, and neither is something to imitate — a "delete" that moved the
//! file into a folder of ours would not be in the bin the writer looks in. The
//! colour dialog is here for the same reason: 要件 9 asks for a colour to be
//! chosen, and **the way a colour is chosen on Windows is this window**, with
//! its wheel, its hex box and its sixteen custom slots.

use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, ILCreateFromPathW, ILFree,
    SHFILEOPSTRUCTW, SHFileOperationW, SHOpenFolderAndSelectItems,
};
use windows::core::{HSTRING, PCWSTR};

/// Move a file or folder to the recycle bin (要件 5.2).
///
/// `false` when nothing was moved, which covers both a failure and a writer who
/// stopped at a warning Windows put up.
///
/// **`FOF_NOCONFIRMATION` without `FOF_WANTNUKEWARNING` would delete
/// outright.** The editor has already asked its own question by the time this
/// is called, so the shell's confirmation is turned off — but the case where
/// the item *cannot* go to the bin (a network drive, something too large) is
/// not the question that was answered, and Windows has to be allowed to say so.
/// That warning runs its own message loop, so this is one of the few places the
/// editor is inside somebody else's (技術検証 6.18); it is out of reach on the
/// ordinary path, which is why the questions the editor asks itself are drawn
/// in the window instead.
pub fn recycle(owner: Option<HWND>, path: &Path) -> bool {
    // `pFrom` is a *list* of names, one after another, and the last of them is
    // followed by a second NUL. One name here, so two.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);
    let flags = FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING;
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: owner.unwrap_or_default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(wide.as_ptr()),
        fFlags: flags.0 as u16,
        ..Default::default()
    };
    // SAFETY: `wide` outlives the call and is the only pointer the structure
    // holds. The shell copies what it needs before it returns.
    let outcome = unsafe { SHFileOperationW(&mut operation) };
    outcome == 0 && !operation.fAnyOperationsAborted.as_bool()
}

/// Show something in Explorer, with it selected (要件 5.2).
///
/// **The item's own list, not its folder's.** Given one item and no selection
/// to make inside it, the shell opens the folder holding it and selects it,
/// which is what 「エクスプローラーで表示」 means; handing it the folder would
/// open the folder with nothing picked out.
pub fn reveal(path: &Path) {
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: the list is freed on both ways out, and COM is initialised on
    // this thread — it is the window's (技術検証 7.3).
    unsafe {
        let item = ILCreateFromPathW(&wide);
        if item.is_null() {
            return;
        }
        let _ = SHOpenFolderAndSelectItems(item as *const _, None, 0);
        ILFree(Some(item as *const _));
    }
}

/// A colour chosen in the dialog Windows draws (要件 9).
///
/// `initial` and the answer are both plain sRGB channels; `COLORREF` puts blue
/// first, which is the one thing to get wrong here.
///
/// **The sixteen custom slots are kept for the run.** A writer who mixes a
/// colour for the body ink and then opens the dialog again for a heading
/// expects to find it still there — the dialog does not keep them, the caller
/// does.
///
/// This runs somebody else's message loop while it is open (技術検証 6.18), so
/// it is asked for from the event loop rather than from inside the click that
/// wanted it.
pub fn choose_colour(owner: Option<HWND>, initial: [u8; 3]) -> Option<[u8; 3]> {
    CUSTOM_COLOURS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let mut chosen = CHOOSECOLORW {
            lStructSize: size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: owner.unwrap_or_default(),
            rgbResult: COLORREF(colorref(initial)),
            lpCustColors: slots.as_mut_ptr(),
            // Open on the full picker rather than on the twenty basic colours:
            // the colours this sets are a page and its ink, and neither is
            // likely to be one of the twenty.
            Flags: CC_RGBINIT | CC_FULLOPEN,
            ..Default::default()
        };
        // SAFETY: the structure and the slots both outlive the call, and the
        // dialog copies what it needs before it returns.
        let picked = unsafe { ChooseColorW(&mut chosen) };
        picked.as_bool().then(|| channels(chosen.rgbResult.0))
    })
}

thread_local! {
    /// The dialog's custom colours, kept where the dialog can be handed them
    /// again. White, because that is what an empty slot looks like.
    static CUSTOM_COLOURS: RefCell<[COLORREF; 16]> = RefCell::new([COLORREF(0x00ff_ffff); 16]);
}

/// sRGB as Windows wants it: `0x00bbggrr`.
fn colorref(rgb: [u8; 3]) -> u32 {
    u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16)
}

/// And back.
fn channels(colorref: u32) -> [u8; 3] {
    [
        (colorref & 0xff) as u8,
        ((colorref >> 8) & 0xff) as u8,
        ((colorref >> 16) & 0xff) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_survives_the_trip_through_windows_byte_order() {
        // Red, which is the one that moves: 0xrrggbb becomes 0x00bbggrr.
        assert_eq!(colorref([0xff, 0x00, 0x00]), 0x0000_00ff);
        assert_eq!(colorref([0x24, 0x21, 0x1e]), 0x001e_2124);
        assert_eq!(channels(colorref([0x24, 0x21, 0x1e])), [0x24, 0x21, 0x1e]);
    }
}
