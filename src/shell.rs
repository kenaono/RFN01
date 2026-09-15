//! What Windows itself does: the recycle bin and Explorer.
//!
//! Both belong to the system rather than to the editor. 要件 5.2 asks that
//! a deleted file stay recoverable and that the writer can reach it in
//! Explorer, and neither is something to imitate — a "delete" that moved the
//! file into a folder of ours would not be in the bin the writer looks in. The colour
//! dialog used to be here too; C5 (2026-09-15) replaced it with the editor's
//! own palette, because the writer found Windows' small and hard to read.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::HWND;
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
