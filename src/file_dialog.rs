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

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
    IFileDialog, IFileDialogCustomize, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, Interface, w};

/// The window a dialog is modal to.
pub type Owner = Option<HWND>;

/// What the dialogs offer to filter by.
///
/// Built per call rather than held as a constant so the wide strings live no
/// longer than the dialog that reads them.
///
/// **`すべてのファイル`が最後で、初めから選ばれているのはそれ**（[`ALL_FILES`]）。
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

/// [`filters`]の何番目が初めから選ばれているか（**1から数える**、Windowsの決めごと）。
///
/// **`すべてのファイル`である**（書き手の判断 2026-09-10：「Editorで扱えるのは拡張子に
/// かかわらずテキストファイル全般なので、デフォルトは`*`でいい」）。この編集器は
/// Markdownを読むが、**Markdownしか読まないわけではない**——`.txt`も`.log`も`.csv`も
/// ただのテキストで、絞り込みが`*.md`だと**開けるはずのファイルが一覧に出てこない**。
/// 名前が`…txt`なのに種類が`Markdown`と出ているのは、画面が言っていることと中身が
/// 食い違っている形でもある（要件 7.7）。
///
/// **絞り込み自体は残す**（選べる、というのが書き手の求め）。
const ALL_FILES: u32 = 3;

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
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(w!("開く"));
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FORCEFILESYSTEM);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// 語群へ取り込むファイルを選ぶ（要件 7.9、2026-09-08）。
///
/// **`open_document`と同じ道具で、題と絞り込みだけが違う。**選ぶのは1行1語の
/// テキストで、開く先も違う（設定に覚えるだけで、タブは開かない）——**同じ絵の
/// ダイアログが2つの用事に出るなら、題でそれを言う**。
pub fn open_word_set(owner: Owner) -> Option<PathBuf> {
    let filters = filters();
    // SAFETY: `open_document`と同じ——COMは窓のスレッドで初期化済みで、
    // シェルが返した文字列は`chosen_path`が解放する。
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(w!("語群へ取り込むファイルを選ぶ（1行に1語）"));
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
    open_folder_from(owner, None, w!("作業フォルダを開く"))
}

/// The same dialog, opened inside a folder the caller names (要件 7.7).
///
/// **Where the writer already is, rather than where the shell last was.**
/// Narrowing a search to a chapter and then to a section is walking down a
/// tree; a dialog that starts over at the desktop each time makes the second
/// step as long as the first.
pub fn open_folder_at(owner: Owner, start: Option<&Path>) -> Option<PathBuf> {
    open_folder_from(owner, start, w!("検索するフォルダ"))
}

fn open_folder_from(
    owner: Owner,
    start: Option<&Path>,
    title: windows::core::PCWSTR,
) -> Option<PathBuf> {
    // SAFETY: as in `open_document`.
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetTitle(title);
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_PICKFOLDERS);
        }
        // **A folder that has gone is not an error here.** The dialog opens
        // wherever the shell would have opened it, which is the same answer as
        // asking for no folder at all.
        if let Some(start) = start {
            let path = HSTRING::from(start.as_os_str());
            let item: windows::core::Result<IShellItem> =
                SHCreateItemFromParsingName(&path, None::<&windows::Win32::System::Com::IBindCtx>);
            if let Ok(item) = item {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// 文字コードの欄に付ける番号（要件 E2 の③）。
///
/// **1つのダイアログの中でしか意味を持たない**ので、外へは出さない。
const ENCODING_GROUP: u32 = 1000;
const ENCODING_COMBO: u32 = 1001;

/// 名前を付けて保存で決めたこと：**行き先と、書く文字コード**（要件 E2 の③）。
pub struct SaveChoice {
    pub path: PathBuf,
    /// 選ばれた文字コードの番号（`labels`の並びの何番目か）。
    ///
    /// **欄を出せなかったときは、渡された番号がそのまま返る**——古いWindowsや
    /// 差し替えられたダイアログでも、保存が止まってしまわないようにである。
    pub encoding: u32,
}

/// Ask where to save, starting from the name the document already has.
///
/// **文字コードもここで選ぶ**（要件 E2 の③、書き手の判断 2026-09-10：
/// 「コードを替えて保存したければ、Save Asからコード選択して保存するべき」）。
/// ステータスバーの一覧が受け持つのは**読み方**で、書き方はこの欄である
/// ——保存は行き先を決める操作なので、決めごとは1つのダイアログに集まっている
/// ほうがよい（Windowsのメモ帳もそうしている）。
///
/// `labels`は並びそのもので、`chosen`はその何番目が初めから選ばれているか
/// （＝いまこの文書が持っている形）。
pub fn save_document_as(
    owner: Owner,
    suggested_name: &str,
    labels: &[&str],
    chosen: u32,
) -> Option<SaveChoice> {
    let filters = filters();
    let suggested = HSTRING::from(suggested_name);
    // SAFETY: as above.
    unsafe {
        let created = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(w!("名前を付けて保存"));
        // **付け足す拡張子は、その文書のもの。**`.txt`の原稿を保存するのに
        // `.md`が足されるのは、種類の欄が`Markdown`と出ていたのと同じ食い違い
        // である——名前に拡張子があるならそれ、無いときだけこの編集器の既定
        // （`md`）にする。
        let extension = HSTRING::from(default_extension(suggested_name));
        let _ = dialog.SetDefaultExtension(&extension);
        let _ = dialog.SetFileName(&suggested);
        if let Ok(options) = dialog.GetOptions() {
            // The shell asks about replacing a file; the editor asks only
            // about the outside change it found itself (要件 8.2).
            let wanted = options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT;
            let _ = dialog.SetOptions(wanted);
        }
        // **欄が出せなくても保存は続く。**customizeが取れないダイアログでも
        // 行き先は選べるので、そのときは渡された文字コードのまま書く。
        let customize: Option<IFileDialogCustomize> = dialog.cast().ok();
        if let Some(customize) = &customize
            && !labels.is_empty()
        {
            let _ = customize.StartVisualGroup(ENCODING_GROUP, w!("文字コード"));
            let _ = customize.AddComboBox(ENCODING_COMBO);
            for (index, label) in labels.iter().enumerate() {
                let text = HSTRING::from(*label);
                let _ = customize.AddControlItem(ENCODING_COMBO, index as u32, &text);
            }
            let _ = customize.EndVisualGroup();
            let _ = customize.SetSelectedControlItem(ENCODING_COMBO, chosen);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = chosen_path(&item)?;
        // **選ばれた番号は、閉じたあとに訊く。**開いているあいだの変更を追う
        // 必要はない——決まったことだけが要る。
        let encoding = customize
            .and_then(|customize| customize.GetSelectedControlItem(ENCODING_COMBO).ok())
            .unwrap_or(chosen);
        Some(SaveChoice { path, encoding })
    }
}

/// 名前に拡張子が無いときだけ足す、その文書の拡張子（要件 E2 の③のついで）。
///
/// **名前が持っているものを尊ぶ。**`note.txt`を保存するのに`.md`を足されては、
/// 書き手が付けた名前ではなくなる。持っていなければ、この編集器がふだん書く
/// `md`にする——`無題1`のような新しい文書がそれである。
fn default_extension(suggested_name: &str) -> &str {
    match Path::new(suggested_name)
        .extension()
        .and_then(|it| it.to_str())
    {
        Some(extension) if !extension.is_empty() => extension,
        _ => "md",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **名前が持っている拡張子を尊ぶ**（書き手の報告 2026-09-10）。
    #[test]
    fn the_suggested_name_keeps_its_own_extension() {
        assert_eq!(default_extension("15_文字コード_CP932.txt"), "txt");
        assert_eq!(default_extension("章.md"), "md");
        assert_eq!(default_extension("a.b.log"), "log");
        // 拡張子が無い（新しい文書）ときだけ、この編集器がふだん書く形。
        assert_eq!(default_extension("無題1"), "md");
        assert_eq!(default_extension("note."), "md");
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
