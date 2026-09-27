//! RFN01-61（書き手と決めた 2026-09-27）: 自動バックアップの画面側。
//!
//! 置き場のファイル操作は[`crate::backup`]が持ち、ここは画面とつなぐだけにする：
//!
//! - Settings → FILES → Auto Backup：残す数、保存先（変えたら全部を移す。失敗したら何も
//!   変えずにダイアログで言う）、Delete Backups…（フォルダを選んで消す）。
//! - Paneメニュー → Backup History…：比較の画面（`diff_view::show_merge`）の左に一覧を足す。
//!   一覧の行で比べる相手を選び、チェックしたものを消す。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::backup::{self, Backup};
use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::saving::backup_root;
use crate::{
    AppWindow, BackupRow, Live, MAX_DOCUMENT_CHARACTERS, PaneId, Question, StatusBar, ask_question,
    diff_view, document, file_dialog, file_io, focused_pane, ime, save_settings, say, timestamp,
};

/// 開いているBackup History。比較の画面を閉じれば消える（`forget`）。
struct History {
    document: Rc<OpenDocument>,
    pane: PaneId,
    path: PathBuf,
    backups: Vec<Backup>,
    /// 一覧の添え字（本文文字数）。**一覧を組むときに1度だけ数える**——チェックのたびに
    /// 全部のバックアップを読み直さない。
    details: Vec<String>,
    /// 比べている相手（`backups`の番号）。
    selected: usize,
    checked: Vec<bool>,
}

thread_local! {
    static HISTORY: RefCell<Option<History>> = const { RefCell::new(None) };
    /// Delete Backups…の一覧と、それぞれのチェック。
    static GROUPS: RefCell<Vec<(backup::Group, bool)>> = const { RefCell::new(Vec::new()) };
}

/// 比較の画面が閉じたとき（`diff_view::dismiss`）。
pub fn forget(window: &AppWindow) {
    HISTORY.with(|held| held.borrow_mut().take());
    window.set_backup_history_active(false);
}

/// `path`のバックアップ（新しい順）。保存先が分からなければ空。
fn backups_of(window: &AppWindow, path: &Path) -> Vec<Backup> {
    backup_root(window).map_or_else(Vec::new, |root| backup::list(&root, path))
}

/// Paneメニューを開いた時点で、そのTABのファイルにバックアップがあるか（無ければ淡く出す）。
pub fn publish_menu(window: &AppWindow, live: &Live, pane: PaneId) {
    let document = live.states.document(pane);
    let path = document.file.borrow().path().map(Path::to_path_buf);
    let has = path.is_some_and(|path| !backups_of(window, &path).is_empty());
    window.set_pane_menu_has_backups(has);
}

/// 主メニューの行を押せるか。
pub fn has_backups(window: &AppWindow, document: &OpenDocument) -> bool {
    let path = document.file.borrow().path().map(Path::to_path_buf);
    path.is_some_and(|path| !backups_of(window, &path).is_empty())
}

fn when(backup: &Backup) -> String {
    timestamp::format("yyyy-MM-dd HH:mm:ss", backup.taken)
}

/// バックアップを、文書の読み方（文字コード）で読む。読めなければ判別に任せる。
fn read_backup(backup: &Backup, document: &OpenDocument) -> Result<String, file_io::LoadError> {
    let encoding = document.file.borrow().form().encoding;
    file_io::read_as(&backup.path, MAX_DOCUMENT_CHARACTERS, encoding)
        .or_else(|_| file_io::read(&backup.path, MAX_DOCUMENT_CHARACTERS))
        .map(|loaded| loaded.text)
}

/// 本文文字数（ステータスバーと同じ数え方：ルビを数えるかは設定に従う）。
fn body_characters(window: &AppWindow, text: &str) -> usize {
    let mut counts = document::DocumentCounts::default();
    counts.refresh(text, crate::reading_of(window));
    let stats = counts.stats();
    if window.get_count_ruby() {
        stats.body_characters
    } else {
        stats.body_characters.saturating_sub(stats.ruby_characters)
    }
}

/// 一覧の添え字：バックアップごとの本文文字数。
fn details_of(window: &AppWindow, backups: &[Backup], document: &OpenDocument) -> Vec<String> {
    backups
        .iter()
        .map(|backup| {
            read_backup(backup, document)
                .map(|text| {
                    let count = crate::thousands(body_characters(window, &text));
                    say!("{count}字", "{count} chars")
                })
                .unwrap_or_default()
        })
        .collect()
}

fn rows_of(history: &History) -> Vec<BackupRow> {
    let each = history.backups.iter().zip(&history.details);
    each.zip(&history.checked)
        .map(|((backup, detail), checked)| BackupRow {
            label: when(backup).into(),
            detail: detail.into(),
            checked: *checked,
        })
        .collect()
}

fn publish_history(window: &AppWindow) {
    HISTORY.with(|held| {
        let held = held.borrow();
        let Some(history) = held.as_ref() else {
            return;
        };
        let rows = rows_of(history);
        window.set_backup_history_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        window.set_backup_history_selected(history.selected as i32);
        let checked = history.checked.iter().filter(|c| **c).count();
        window.set_backup_history_any(checked > 0);
        window.set_backup_history_all(checked > 0 && checked == history.checked.len());
    });
}

/// 比べる相手を`selected`にして、比較の画面を組み直す。
///
/// **まだ本文へ反映していない差分の選択は、ここで捨てる**（`show_merge`が選択を持ち直す）。
fn show_selected(window: &AppWindow, live: &Live) {
    let shown = HISTORY.with(|held| {
        let held = held.borrow();
        let history = held.as_ref()?;
        let backup = history.backups.get(history.selected)?;
        let right = read_backup(backup, &history.document);
        Some((
            history.document.clone(),
            history.pane,
            history.path.clone(),
            when(backup),
            right,
        ))
    });
    let Some((document, pane, path, taken, right)) = shown else {
        return;
    };
    let right = match right {
        Ok(text) => text,
        Err(error) => {
            window.tell_tab(
                say!(
                    "バックアップを読めません: {error}",
                    "Cannot read the backup: {error}"
                )
                .into(),
            );
            return;
        }
    };
    let left = document.text.borrow().clone();
    diff_view::show_merge(
        window,
        live,
        pane,
        document,
        say!(
            "{}（編集中の本文）",
            "{} (text being edited)",
            path.display()
        ),
        left,
        say!("バックアップ {taken}", "Backup {taken}"),
        right,
    );
    window.set_backup_history_active(true);
    publish_history(window);
}

/// Paneメニュー → Backup History…
pub fn open_history(window: &AppWindow, live: &Live) {
    let pane = focused_pane(window);
    let document = live.states.document(pane);
    let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
        window.tell_tab(
            say!(
                "まだ保存先がありません。バックアップは保存したファイルにだけあります",
                "This document has not been saved yet, so it has no backups"
            )
            .into(),
        );
        return;
    };
    let backups = backups_of(window, &path);
    if backups.is_empty() {
        window.tell_tab(say!("バックアップはありません", "There are no backups").into());
        return;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    window.set_backup_history_file(name.into());
    let checked = vec![false; backups.len()];
    let details = details_of(window, &backups, &document);
    HISTORY.with(|held| {
        *held.borrow_mut() = Some(History {
            document,
            pane,
            path,
            backups,
            details,
            selected: 0,
            checked,
        });
    });
    show_selected(window, live);
}

fn history_chosen(window: &AppWindow, live: &Live, index: usize) {
    let changed = HISTORY.with(|held| {
        let mut held = held.borrow_mut();
        let history = held.as_mut()?;
        (index < history.backups.len() && index != history.selected).then(|| {
            history.selected = index;
        })
    });
    if changed.is_some() {
        show_selected(window, live);
    }
}

fn history_toggled(window: &AppWindow, index: usize) {
    HISTORY.with(|held| {
        if let Some(checked) = held
            .borrow_mut()
            .as_mut()
            .and_then(|history| history.checked.get_mut(index))
        {
            *checked = !*checked;
        }
    });
    publish_history(window);
}

fn history_all_toggled(window: &AppWindow) {
    HISTORY.with(|held| {
        if let Some(history) = held.borrow_mut().as_mut() {
            let all = history.checked.iter().all(|c| *c);
            history.checked.iter_mut().for_each(|c| *c = !all);
        }
    });
    publish_history(window);
}

/// 消す前に訊く。
fn ask_delete(window: &AppWindow, live: &Live, paths: Vec<PathBuf>, what: String) {
    if paths.is_empty() {
        return;
    }
    let text = say!(
        "{what}を消しますか？\n\n消したバックアップは元に戻せません。",
        "Delete {what}?\n\nDeleted backups cannot be brought back."
    );
    ask_question(
        window,
        live,
        Question::DeleteBackups(paths),
        text,
        &[pick("消す", "Delete"), pick("キャンセル", "Cancel")],
        0,
    );
}

fn history_delete(window: &AppWindow, live: &Live) {
    let paths: Vec<PathBuf> = HISTORY.with(|held| {
        held.borrow().as_ref().map_or_else(Vec::new, |history| {
            let chosen = history.backups.iter().zip(&history.checked);
            chosen
                .filter(|(_, checked)| **checked)
                .map(|(backup, _)| backup.path.clone())
                .collect()
        })
    });
    let count = paths.len();
    let what = say!("選んだバックアップ{count}件", "{count} selected backup(s)");
    ask_delete(window, live, paths, what);
}

fn publish_groups(window: &AppWindow) {
    GROUPS.with(|held| {
        let held = held.borrow();
        let rows: Vec<BackupRow> = held
            .iter()
            .map(|(group, checked)| BackupRow {
                label: group.folder.display().to_string().into(),
                detail: SharedString::default(),
                checked: *checked,
            })
            .collect();
        let checked = held.iter().filter(|(_, c)| *c).count();
        window.set_backup_groups(ModelRc::from(Rc::new(VecModel::from(rows))));
        window.set_backup_groups_any(checked > 0);
        window.set_backup_groups_all(checked > 0 && checked == held.len());
    });
}

/// Workspaceに登録してあるフォルダ全部（どのWorkspaceのものでも）。
fn registered_folders(live: &Live) -> Vec<PathBuf> {
    let folder = live.folder.borrow();
    folder.workspace.as_ref().map_or_else(Vec::new, |runtime| {
        let runtime = runtime.borrow();
        let folders = runtime.registry().folders();
        folders.iter().map(|f| f.path.clone()).collect()
    })
}

/// Settings → Delete Backups…
pub fn open_groups(window: &AppWindow, live: &Live) {
    let groups = backup_root(window).map_or_else(Vec::new, |root| {
        backup::groups(&root, &registered_folders(live))
    });
    GROUPS.with(|held| *held.borrow_mut() = groups.into_iter().map(|g| (g, false)).collect());
    publish_groups(window);
    window.set_backup_delete_open(true);
}

fn close_groups(window: &AppWindow) {
    window.set_backup_delete_open(false);
    GROUPS.with(|held| held.borrow_mut().clear());
}

fn group_toggled(window: &AppWindow, index: usize) {
    GROUPS.with(|held| {
        if let Some((_, checked)) = held.borrow_mut().get_mut(index) {
            *checked = !*checked;
        }
    });
    publish_groups(window);
}

fn group_all_toggled(window: &AppWindow) {
    GROUPS.with(|held| {
        let mut held = held.borrow_mut();
        let all = held.iter().all(|(_, c)| *c);
        held.iter_mut().for_each(|(_, c)| *c = !all);
    });
    publish_groups(window);
}

fn groups_delete(window: &AppWindow, live: &Live) {
    let (count, paths) = GROUPS.with(|held| {
        let held = held.borrow();
        let chosen: Vec<_> = held.iter().filter(|(_, c)| *c).collect();
        let paths = chosen.iter().flat_map(|(g, _)| g.files.clone()).collect();
        (chosen.len(), paths)
    });
    let what = say!(
        "選んだ{count}個のフォルダのバックアップ",
        "the backups of {count} selected folder(s)"
    );
    ask_delete(window, live, paths, what);
}

/// 「消す」と答えたとき（`Question::DeleteBackups`）。
pub fn delete_confirmed(window: &AppWindow, live: &Live, paths: &[PathBuf]) {
    let Some(root) = backup_root(window) else {
        return;
    };
    let result = backup::delete(&root, paths);
    let count = paths.len();
    live.cache.borrow_mut().log_diag(
        "file",
        &format!("backup delete count={count} ok={}", result.is_ok()),
    );
    if window.get_backup_delete_open() {
        close_groups(window);
    }
    after_history_delete(window, live);
    match result {
        Ok(()) => window.tell(say!("バックアップを消しました", "Backups deleted").into()),
        Err(error) => window.tell(
            say!(
                "消せなかったバックアップがあります: {error}",
                "Some backups could not be deleted: {error}"
            )
            .into(),
        ),
    }
}

/// Backup History で消したあと：**比べていた相手が残っていればそのまま、消えていれば
/// 最新を選び直す。全部消えたら画面を閉じる。**
fn after_history_delete(window: &AppWindow, live: &Live) {
    enum Next {
        Nothing,
        Close,
        List,
        Show,
    }
    let next = HISTORY.with(|held| {
        let mut held = held.borrow_mut();
        let Some(history) = held.as_mut() else {
            return Next::Nothing;
        };
        let compared = history
            .backups
            .get(history.selected)
            .map(|b| b.path.clone());
        history.backups = backups_of(window, &history.path);
        history.details = details_of(window, &history.backups, &history.document);
        history.checked = vec![false; history.backups.len()];
        if history.backups.is_empty() {
            return Next::Close;
        }
        match history
            .backups
            .iter()
            .position(|b| Some(&b.path) == compared.as_ref())
        {
            Some(still) => {
                history.selected = still;
                Next::List
            }
            None => {
                history.selected = 0;
                Next::Show
            }
        }
    });
    match next {
        Next::Nothing => {}
        Next::Close => diff_view::dismiss(window),
        Next::List => publish_history(window),
        Next::Show => show_selected(window, live),
    }
}

/// 保存先を`chosen`（空ならアプリ専用領域）へ変える。**バックアップを全部移し、移せなければ
/// 何も変えない**（書き手の決定 2026-09-27）。
fn change_folder(window: &AppWindow, live: &Live, chosen: String) {
    let from = backup_root(window);
    let to = if chosen.is_empty() {
        backup::default_root()
    } else {
        Some(PathBuf::from(&chosen))
    };
    let (Some(from), Some(to)) = (from, to) else {
        return;
    };
    if from != to
        && let Err(error) = backup::move_all(&from, &to)
    {
        let why = match error {
            backup::MoveError::Nested => pick(
                "移し先が今の保存先の中（またはその逆）にあります。",
                "The new folder is inside the current one, or the other way round.",
            )
            .to_owned(),
            backup::MoveError::Exists(path) => say!(
                "移し先に同じ名前のファイルがあります：{}",
                "A file with the same name is already there: {}",
                path.display()
            ),
            backup::MoveError::Io(error) => error.to_string(),
        };
        live.cache.borrow_mut().log_diag(
            "file",
            &format!("backup move failed to={} {why}", to.display()),
        );
        ask_question(
            window,
            live,
            Question::BackupNotice,
            say!(
                "バックアップを移せなかったため、保存先は変えていません。\n\n{why}",
                "The backups could not be moved, so the backup folder is unchanged.\n\n{why}"
            ),
            &["OK"],
            -1,
        );
        return;
    }
    window.set_backup_folder(chosen.into());
    save_settings(window, &live.cache);
    window.tell(say!("保存先を変えました", "Backup folder changed").into());
}

pub fn wire(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_keep_stepped(move |by| {
        if let Some(window) = weak.upgrade() {
            let most = backup::MAX_KEEP as i32;
            window.set_backup_keep((window.get_backup_keep() + by).clamp(1, most));
            save_settings(&window, &held.cache);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_folder_requested(move || {
        if let Some(window) = weak.upgrade() {
            let owner = ime::window_handle(&window);
            let start = backup_root(&window);
            let Some(chosen) = file_dialog::open_folder_at(owner, start.as_deref()) else {
                return;
            };
            change_folder(&window, &held, chosen.display().to_string());
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_folder_default(move || {
        if let Some(window) = weak.upgrade() {
            change_folder(&window, &held, String::new());
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_delete_requested(move || {
        if let Some(window) = weak.upgrade() {
            open_groups(&window, &held);
        }
    });

    let weak = window.as_weak();
    window.on_backup_group_toggled(move |index| {
        if let Some(window) = weak.upgrade() {
            group_toggled(&window, index.max(0) as usize);
        }
    });

    let weak = window.as_weak();
    window.on_backup_group_all_toggled(move || {
        if let Some(window) = weak.upgrade() {
            group_all_toggled(&window);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_groups_delete(move || {
        if let Some(window) = weak.upgrade() {
            groups_delete(&window, &held);
        }
    });

    let weak = window.as_weak();
    window.on_backup_delete_closed(move || {
        if let Some(window) = weak.upgrade() {
            close_groups(&window);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_history_requested(move || {
        if let Some(window) = weak.upgrade() {
            open_history(&window, &held);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_history_chosen(move |index| {
        if let Some(window) = weak.upgrade() {
            history_chosen(&window, &held, index.max(0) as usize);
        }
    });

    let weak = window.as_weak();
    window.on_backup_history_toggled(move |index| {
        if let Some(window) = weak.upgrade() {
            history_toggled(&window, index.max(0) as usize);
        }
    });

    let weak = window.as_weak();
    window.on_backup_history_all_toggled(move || {
        if let Some(window) = weak.upgrade() {
            history_all_toggled(&window);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_history_delete(move || {
        if let Some(window) = weak.upgrade() {
            history_delete(&window, &held);
        }
    });
}

#[cfg(test)]
#[path = "backup_ui_tests.rs"]
mod tests;
