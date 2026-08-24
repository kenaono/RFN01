//! The Windows open and save dialogs.
//!
//! `IFileOpenDialog` and `IFileSaveDialog` rather than the older
//! `GetOpenFileNameW`: they are what the rest of Windows 11 shows, and the
//! path comes back allocated by the shell rather than written into a buffer of
//! ours that has to be guessed at.
//!
//! **Both are modal and run their own message loop.** Slint goes on delivering
//! events from inside it, so a caller must not be holding a `RefCell` borrow
//! when it opens one. That is the same hazard as 技術検証 6.7, arriving from a
//! different direction.
//!
//! **The questions the editor asks are not here.** A message box comes with the
//! buttons Windows has, so what each one means has to go in the paragraph above
//! them; 要件 8.3 and 8.4 need four choices named on their own buttons. Those
//! are drawn in the window itself (`question-open` in `app-window.slint`), which
//! also keeps them clear of 6.18.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
    IFileDialog, IShellItem, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, w};

/// The window a dialog is modal to.
pub type Owner = Option<HWND>;

/// What the dialogs offer to filter by.
///
/// Built per call rather than held as a constant so the wide strings live no
/// longer than the dialog that reads them.
fn filters() -> [COMDLG_FILTERSPEC; 3] {
    [
        COMDLG_FILTERSPEC {
            pszName: w!("Markdown (*.md)"),
            pszSpec: w!("*.md;*.markdown"),
        },
        COMDLG_FILTERSPEC {
            pszName: w!("テキスト (*.txt)"),
            pszSpec: w!("*.txt"),
        },
        COMDLG_FILTERSPEC {
            pszName: w!("すべてのファイル"),
            pszSpec: w!("*.*"),
        },
    ]
}

/// Ask which file to open.
///
/// `None` for a cancel, and for the rare failure to show the dialog at all.
/// The two mean the same thing here: no file was chosen, so nothing changes.
pub fn open_document(owner: Owner) -> Option<PathBuf> {
    let filters = filters();
    // SAFETY: COM is initialised on this thread — it is the window's, and the
    // apartment is the one 技術検証 7.3 settled on. Every string the shell
    // hands back is freed in `chosen_path`.
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetTitle(w!("開く"));
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FORCEFILESYSTEM);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// Ask which folder to work in (要件 5.1).
///
/// The same dialog as `open_document`, told to pick a folder instead of a file
/// — which is what `FOS_PICKFOLDERS` means, and why there is no separate
/// "browse for folder" here: the old one of those is the tree with no address
/// bar and no typing, and nobody wants it.
pub fn open_folder(owner: Owner) -> Option<PathBuf> {
    // SAFETY: as in `open_document`.
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetTitle(w!("作業フォルダを開く"));
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_PICKFOLDERS);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// Ask where to save, starting from the name the document already has.
pub fn save_document_as(owner: Owner, suggested_name: &str) -> Option<PathBuf> {
    let filters = filters();
    let suggested = HSTRING::from(suggested_name);
    // SAFETY: as above.
    unsafe {
        let created = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetTitle(w!("名前を付けて保存"));
        let _ = dialog.SetDefaultExtension(w!("md"));
        let _ = dialog.SetFileName(&suggested);
        if let Ok(options) = dialog.GetOptions() {
            // The shell asks about replacing a file; the editor asks only
            // about the outside change it found itself (要件 8.2).
            let wanted = options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT;
            let _ = dialog.SetOptions(wanted);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// The chosen item's path in the filesystem.
///
/// # Safety
///
/// `item` must be an item a dialog has just returned. The shell allocated the
/// string, so it is freed here and nowhere else.
unsafe fn chosen_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let wide = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = wide.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(wide.0 as *const core::ffi::c_void));
        path
    }
}
