//! What Windows itself does: the recycle bin and Explorer.
//!
//! Both belong to the system rather than to the editor. 要件 5.2 asks that
//! a deleted file stay recoverable and that the writer can reach it in
//! Explorer, and neither is something to imitate — a "delete" that moved the
//! file into a folder of ours would not be in the bin the writer looks in. The colour
//! dialog used to be here too; C5 (2026-09-15) replaced it with the editor's
//! own palette, because the writer found Windows' small and hard to read.

use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
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
/// the item *cannot* go to the bin (something too large) is not the question
/// that was answered, and Windows has to be allowed to say so. That warning
/// runs its own message loop, so this is one of the few places the editor is
/// inside somebody else's (技術検証 6.18); it is out of reach on the ordinary
/// path, which is why the questions the editor asks itself are drawn in the
/// window instead.
///
/// **Where [`has_recycle_bin`] says there is no bin, the question already said
/// so** (RFN01-64): the writer was told it is deleted for good and cannot be
/// restored, and answered that. Windows asking the same thing again is dropped
/// there. `FOF_ALLOWUNDO` stays, so a place that turns out to have a bin after
/// all still gets it.
///
/// **The shell does not take `\\?\`.** A Workspace holds its roots
/// canonicalized, so every row under one arrives as `\\?\D:\…`, and
/// `SHFileOperationW` refuses that form — the tree's delete, and the replace
/// a move asks about, only ever said the bin had failed (RFN01-64). The prefix
/// is taken off before the path goes in.
pub fn recycle(owner: Option<HWND>, path: &Path) -> bool {
    let path = crate::backup::plain(path);
    // `pFrom` is a *list* of names, one after another, and the last of them is
    // followed by a second NUL. One name here, so two.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);
    let mut flags = FOF_ALLOWUNDO | FOF_NOCONFIRMATION;
    if has_recycle_bin(&path) {
        flags |= FOF_WANTNUKEWARNING;
    }
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

/// Whether what is at `path` can go to the recycle bin (RFN01-64).
///
/// **Asked before the question, so the question can say what will happen.**
/// Windows keeps a bin on fixed drives only: a network share, a mapped network
/// drive, a USB stick or a CD has none, and what is deleted there is gone. A
/// share is known from its name alone, so an unreachable server is never waited
/// on. A fixed drive whose bin is switched off, or something too large for it,
/// is not caught here — Windows still warns about those itself (see
/// [`recycle`]).
pub fn has_recycle_bin(path: &Path) -> bool {
    const DRIVE_FIXED: u32 = 3;
    let path = crate::backup::plain(path);
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let Prefix::Disk(letter) = prefix.kind() else {
        return false;
    };
    let root = HSTRING::from(format!("{}:\\", letter as char));
    // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
    unsafe { GetDriveTypeW(&root) == DRIVE_FIXED }
}

/// Whether `a` and `b` are on one volume, so a rename can carry one to the
/// other (RFN01-64).
///
/// **`false` when it cannot tell.** The answer picks between a rename and a
/// copy, and a copy is right on one volume too — only slower — while a rename
/// across two cannot happen at all. Both paths have to exist.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let volume = |path: &Path| {
        let wide = HSTRING::from(crate::backup::plain(path).as_os_str());
        let mut out = [0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and outlives the call; `out` is
        // written only within its length.
        unsafe { GetVolumePathNameW(&wide, &mut out) }.ok()?;
        let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
        Some(String::from_utf16_lossy(&out[..end]).to_lowercase())
    };
    match (volume(a), volume(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// RFN01-64: a path the way a Workspace row holds it, `\\?\` and all,
    /// still reaches the bin.
    #[test]
    fn a_canonicalized_path_goes_to_the_bin() {
        let dir = std::env::temp_dir().join(format!("rfn-recycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("上書きされる.txt");
        std::fs::write(&file, "a").unwrap();
        let held = file.canonicalize().unwrap();
        assert!(held.to_string_lossy().starts_with(r"\\?\"));

        assert!(recycle(None, &held));
        assert!(!file.exists());
        let _ = std::fs::remove_dir(&dir);
    }

    /// RFN01-64: a fixed drive has a bin, however the path is written; a
    /// network share has none, and is answered without reaching for it.
    #[test]
    fn only_a_fixed_drive_has_a_bin() {
        let temp = std::env::temp_dir();
        assert!(has_recycle_bin(&temp));
        assert!(has_recycle_bin(&temp.canonicalize().unwrap()));
        assert!(!has_recycle_bin(Path::new(r"\\server\share\原稿.md")));
        assert!(!has_recycle_bin(Path::new(r"\\?\UNC\server\share\原稿.md")));
        assert!(!has_recycle_bin(Path::new("原稿.md")));
    }

    /// RFN01-64: one drive is one volume however it is written; a share is
    /// another one.
    #[test]
    fn a_share_is_not_the_drive_it_is_on() {
        let temp = std::env::temp_dir();
        assert!(same_volume(&temp, &temp.canonicalize().unwrap()));
        let text = temp.to_string_lossy().into_owned();
        let letter = &text[..1];
        let shared = PathBuf::from(format!(r"\\localhost\{letter}$\{}", &text[3..]));
        if shared.exists() {
            assert!(!same_volume(&temp, &shared));
        }
    }
}
