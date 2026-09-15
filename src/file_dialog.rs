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

use crate::i18n::pick;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
    IFileDialog, IFileDialogCustomize, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, Interface, PCWSTR, w};

/// The window a dialog is modal to.
pub type Owner = Option<HWND>;

/// What the dialogs offer to filter by.
///
/// Built per call rather than held as a constant so the wide strings live no
/// longer than the dialog that reads them.
///
/// **`すべてのファイル`が最後で、初めから選ばれているのはそれ**（[`ALL_FILES`]）。
///
/// **絞り込みの名前は言語で変わる**（国際化②）ので、字列は呼ぶ側が[`filter_names`]で持ち、
/// ダイアログを出し終えるまで生かしておく。
fn filters(names: &[HSTRING; 3]) -> [COMDLG_FILTERSPEC; 3] {
    [
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(names[0].as_ptr()),
            pszSpec: w!("*.md;*.markdown"),
        },
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(names[1].as_ptr()),
            pszSpec: w!("*.txt"),
        },
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(names[2].as_ptr()),
            pszSpec: w!("*.*"),
        },
    ]
}

fn filter_names() -> [HSTRING; 3] {
    [
        HSTRING::from("Markdown (*.md)"),
        HSTRING::from(pick("テキスト (*.txt)", "Text (*.txt)")),
        HSTRING::from(all_files()),
    ]
}

fn all_files() -> &'static str {
    pick("すべてのファイル", "All Files")
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
    open_document_named(owner, pick("開く", "Open"))
}

pub fn open_document_named(owner: Owner, title: &str) -> Option<PathBuf> {
    let names = filter_names();
    let filters = filters(&names);
    // SAFETY: COM is initialised on this thread — it is the window's, and the
    // apartment is the one 技術検証 7.3 settled on. Every string the shell
    // hands back is freed in `chosen_path`.
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(&HSTRING::from(title));
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
    let names = filter_names();
    let filters = filters(&names);
    // SAFETY: `open_document`と同じ——COMは窓のスレッドで初期化済みで、
    // シェルが返した文字列は`chosen_path`が解放する。
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(&HSTRING::from(pick(
            "語群へ取り込むファイルを選ぶ（1行に1語）",
            "Choose a File to Take into the Group (One Word per Line)",
        )));
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FORCEFILESYSTEM);
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        chosen_path(&item)
    }
}

/// 背景の壁紙にする画像を選ぶ（追加要件 2026-09-15）。**Windowsが読める画像なら何でも**
/// （WICで読む）ので、絞り込みは目安で、すべてのファイルも選べる。
pub fn open_image(owner: Owner) -> Option<PathBuf> {
    let names = [
        HSTRING::from(pick(
            "画像 (*.jpg;*.png;*.bmp;*.gif;*.webp;*.tif)",
            "Images (*.jpg;*.png;*.bmp;*.gif;*.webp;*.tif)",
        )),
        HSTRING::from(all_files()),
    ];
    let filters = [
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(names[0].as_ptr()),
            pszSpec: w!("*.jpg;*.jpeg;*.png;*.bmp;*.gif;*.webp;*.tif;*.tiff;*.jxr;*.heic"),
        },
        COMDLG_FILTERSPEC {
            pszName: PCWSTR(names[1].as_ptr()),
            pszSpec: w!("*.*"),
        },
    ];
    // SAFETY: `open_document`と同じ——COMは窓のスレッドで初期化済みで、
    // シェルが返した文字列は`chosen_path`が解放する。
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(1);
        let _ = dialog.SetTitle(&HSTRING::from(pick(
            "背景の画像を選ぶ",
            "Choose a Background Image",
        )));
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
    open_folder_from(owner, None, pick("作業フォルダを開く", "Open Work Folder"))
}

/// The same dialog, opened inside a folder the caller names (要件 7.7).
///
/// **Where the writer already is, rather than where the shell last was.**
/// Narrowing a search to a chapter and then to a section is walking down a
/// tree; a dialog that starts over at the desktop each time makes the second
/// step as long as the first.
pub fn open_folder_at(owner: Owner, start: Option<&Path>) -> Option<PathBuf> {
    open_folder_from(owner, start, pick("検索するフォルダ", "Folder to Search"))
}

fn open_folder_from(owner: Owner, start: Option<&Path>, title: &str) -> Option<PathBuf> {
    // SAFETY: as in `open_document`.
    unsafe {
        let created = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
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

/// 欄に付ける番号（要件 E2 の③⑤）。
///
/// **1つのダイアログの中でしか意味を持たない**ので、外へは出さない。
const ENCODING_GROUP: u32 = 1000;
const ENCODING_COMBO: u32 = 1001;
const NEWLINE_GROUP: u32 = 1002;
const NEWLINE_COMBO: u32 = 1003;

/// 名前を付けて保存の欄に出すもの（要件 E2）。
///
/// **書き方の決めごとは1つのダイアログに集まっている**——名前・場所・文字コード・
/// 改行が一度に決まる。[`SaveChoice`]がその答えで、**並びと番号の対応を決める場所は
/// 呼ぶ側に1つ**（`main.rs`の`save_form_labels`／`newline_labels`）：ここは言葉を
/// 並べて番号を返すだけである。
pub struct SaveFields<'a> {
    pub encodings: &'a [&'a str],
    /// 初めから選ばれている行＝**いまこの文書が持っている形**。
    pub encoding: u32,
    pub newlines: &'a [&'a str],
    pub newline: u32,
}

impl SaveFields<'_> {
    /// 欄を1つも出さない（語群の書き出し）。
    ///
    /// **読むほうがUTF-8しか受けないので、選べると言って選ばせないほうが悪い**
    /// （要件 7.7）。改行も同じで、あれは原稿ではなく道具のための表である。
    pub fn none() -> Self {
        Self {
            encodings: &[],
            encoding: 0,
            newlines: &[],
            newline: 0,
        }
    }
}

/// 名前を付けて保存で決めたこと：**行き先と、書く文字コードと、改行**
/// （要件 E2 の③⑤）。
pub struct SaveChoice {
    pub path: PathBuf,
    /// 選ばれた文字コードの番号（並びの何番目か）。
    ///
    /// **欄を出せなかったときは、渡された番号がそのまま返る**——古いWindowsや
    /// 差し替えられたダイアログでも、保存が止まってしまわないようにである。
    pub encoding: u32,
    /// 選ばれた改行の番号。**同じ決めごと**：欄が無ければ、いまの形のまま。
    pub newline: u32,
}

/// Ask where to save, starting from the name the document already has.
///
/// **文字コードもここで選ぶ**（要件 E2 の③、書き手の判断 2026-09-10：
/// 「コードを替えて保存したければ、Save Asからコード選択して保存するべき」）。
/// ステータスバーの一覧が受け持つのは**読み方**で、書き方はこの欄である
/// ——保存は行き先を決める操作なので、決めごとは1つのダイアログに集まっている
/// ほうがよい（Windowsのメモ帳もそうしている）。
///
/// **改行もここで選ぶ**（要件 E2 の⑤、書き手の選択 2026-09-10）。帯が受け持つのは
/// 読み方だけ、という③の線をそのまま引いてある——文字コードの隣に置くのは、
/// どちらも「書くときに決めること」で、決める時機が同じだからである。
pub fn save_document_as(
    owner: Owner,
    suggested_name: &str,
    fields: SaveFields,
) -> Option<SaveChoice> {
    let names = filter_names();
    let filters = filters(&names);
    let suggested = HSTRING::from(suggested_name);
    // SAFETY: as above.
    unsafe {
        let created = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER);
        let dialog: IFileDialog = created.ok()?;
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetFileTypeIndex(ALL_FILES);
        let _ = dialog.SetTitle(&HSTRING::from(pick("名前を付けて保存", "Save As")));
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
        if let Some(customize) = &customize {
            add_field(
                customize,
                ENCODING_GROUP,
                ENCODING_COMBO,
                pick("文字コード", "Encoding"),
                fields.encodings,
                fields.encoding,
            );
            add_field(
                customize,
                NEWLINE_GROUP,
                NEWLINE_COMBO,
                pick("改行コード", "Line Breaks"),
                fields.newlines,
                fields.newline,
            );
        }
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = chosen_path(&item)?;
        // **選ばれた番号は、閉じたあとに訊く。**開いているあいだの変更を追う
        // 必要はない——決まったことだけが要る。**訊けなければ渡された番号のまま**
        // なので、欄を出せなかったダイアログでも保存は止まらない。
        let taken = |combo: u32, fallback: u32| {
            customize
                .as_ref()
                .and_then(|customize| customize.GetSelectedControlItem(combo).ok())
                .unwrap_or(fallback)
        };
        Some(SaveChoice {
            path,
            encoding: taken(ENCODING_COMBO, fields.encoding),
            newline: taken(NEWLINE_COMBO, fields.newline),
        })
    }
}

/// 欄を1つ、ダイアログへ足す（要件 E2 の③⑤）。
///
/// **並びが空なら、何も足さない**——出せると言って中身の無い欄が出るくらいなら、
/// 欄そのものが無いほうがよい（要件 7.7）。返り値を見ないのは、**欄が出せなくても
/// 保存は続く**からである：行き先は選べるので、そのときは渡された番号のまま書く。
///
/// # Safety
///
/// `customize`は生きているダイアログのもので、`Show`より前に呼ばれること。
unsafe fn add_field(
    customize: &IFileDialogCustomize,
    group: u32,
    combo: u32,
    title: &str,
    labels: &[&str],
    chosen: u32,
) {
    if labels.is_empty() {
        return;
    }
    // SAFETY: 呼ぶ側の決めごとのとおり。
    unsafe {
        let _ = customize.StartVisualGroup(group, &HSTRING::from(title));
        let _ = customize.AddComboBox(combo);
        for (index, label) in labels.iter().enumerate() {
            let text = HSTRING::from(*label);
            let _ = customize.AddControlItem(combo, index as u32, &text);
        }
        let _ = customize.EndVisualGroup();
        let _ = customize.SetSelectedControlItem(combo, chosen);
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
