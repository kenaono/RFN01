//! RFN01-61（書き手と決めた 2026-09-27）: 自動バックアップの画面側。
//!
//! 置き場のファイル操作は[`crate::backup`]が持ち、ここは画面とつなぐだけにする：
//!
//! - 左ペインの Backups：管理はここへ集める（書き手の求め 2026-09-27）。Auto Recovery の
//!   On/Off、フォルダごとの AutoBackup の ON/OFF、バックアップのあるファイルと件数。
//!   ファイルを押すと Backup History、右クリックで消す。
//! - File → Backup History…：比較の画面（`diff_view::show_merge`）の左に一覧を足す。
//!   一覧の行で比べる相手を選び、チェックしたものを消す。
//! - Settings → Backup：世代数、保存先（変えたら全部を移す）、ファイル名の書式（変えたら全部の
//!   名前を付け替える）。どちらも失敗したら何も変えずにダイアログで言う。

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::backup::{self, Backup};
use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::saving::{backup_root, backup_store};
use crate::{
    AppWindow, BackupPaneRow, BackupRow, Live, MAX_DOCUMENT_CHARACTERS, Opening, PaneId, Question,
    StatusBar, ask_question, diff_view, document, file_dialog, file_io, focused_pane, ime,
    save_settings, say, timestamp, workspace,
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
    /// Backups の面の行が指すもの（行の順）。
    static PANE: RefCell<Vec<PaneLine>> = const { RefCell::new(Vec::new()) };
    /// Backups の面で閉じているフォルダ。**開いた直後は全部開いている**。
    static CLOSED: RefCell<BTreeSet<PathBuf>> = const { RefCell::new(BTreeSet::new()) };
}

/// 比較の画面が閉じたとき（`diff_view::dismiss`）。
pub fn forget(window: &AppWindow) {
    HISTORY.with(|held| held.borrow_mut().take());
    window.set_backup_history_active(false);
}

/// `path`のバックアップ（新しい順）。保存先が分からなければ空。
fn backups_of(window: &AppWindow, path: &Path) -> Vec<Backup> {
    backup_store(window).map_or_else(Vec::new, |store| store.list(path))
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

/// Backups の面の1行が指すもの。
enum PaneLine {
    Folder {
        folder: PathBuf,
        /// 使用中の Workspace の登録フォルダなら、その台帳の番号（この面で切り替えられる）。
        id: Option<workspace::FolderId>,
        files: Vec<PathBuf>,
    },
    File {
        original: PathBuf,
        files: Vec<PathBuf>,
    },
    Note,
}

/// 大文字・小文字を区別せずに同じパスか（Windows）。
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Backups の面が見えていれば組み直す（保存・消去・切り替えのあと）。
pub fn refresh_pane(window: &AppWindow, live: &Live) {
    if window.get_tree_open() && window.get_left_tab() == 7 {
        publish_pane(window, live);
    }
}

/// Backups の面を組む。
///
/// 並べるのは、**使用中の Workspace の登録フォルダ全部**（OFFのものもここでONにできる）と、
/// **バックアップが残っているフォルダ**（別の Workspace のもの、登録を外したものも）。
pub fn publish_pane(window: &AppWindow, live: &Live) {
    // (フォルダ, 台帳の番号, 保存方式, この面で切り替えられるか)
    let mut folders: Vec<(
        PathBuf,
        Option<workspace::FolderId>,
        Option<workspace::SaveMode>,
        bool,
    )> = Vec::new();
    let mut registered = Vec::new();
    let active = {
        let folder = live.folder.borrow();
        folder.workspace.as_ref().map(|runtime| {
            let runtime = runtime.borrow();
            let registry = runtime.registry();
            registered = registry.folders().iter().map(|f| f.path.clone()).collect();
            let ids = runtime
                .active_workspace()
                .and_then(|id| registry.workspace(id))
                .map(|w| w.folders.clone())
                .unwrap_or_default();
            let entries: Vec<_> = ids
                .iter()
                .filter_map(|id| registry.folder(*id))
                .map(|f| (backup::plain(&f.path), Some(f.id), Some(f.mode), true))
                .collect();
            let others: Vec<_> = registry
                .folders()
                .iter()
                .map(|f| (backup::plain(&f.path), f.mode))
                .collect();
            (entries, others)
        })
    };
    let groups = backup_store(window).map_or_else(Vec::new, |store| store.groups(&registered));
    let (entries, others) = active.unwrap_or_default();
    folders.extend(entries);
    for group in &groups {
        if folders.iter().any(|(f, ..)| same_path(f, &group.folder)) {
            continue;
        }
        let mode = others
            .iter()
            .find(|(f, _)| same_path(f, &group.folder))
            .map(|(_, mode)| *mode);
        folders.push((group.folder.clone(), None, mode, false));
    }

    let closed = CLOSED.with(|held| held.borrow().clone());
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    if live.folder.borrow().workspace.is_none() {
        rows.push(note(say!(
            "Workspaceを開くと、フォルダごとにAutoBackupを切り替えられます。",
            "Open a Workspace to turn AutoBackup on for its folders."
        )));
        lines.push(PaneLine::Note);
    }
    for (folder, id, mode, can_toggle) in folders {
        let group = groups.iter().find(|g| same_path(&g.folder, &folder));
        let open = !closed.contains(&folder);
        let label = folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.display().to_string());
        let tag = match (mode, can_toggle) {
            (Some(workspace::SaveMode::AutoSave), _) => "AutoSave".to_owned(),
            (Some(workspace::SaveMode::AutoBackup), false) => "AutoBackup".to_owned(),
            (Some(_), false) => "OFF".to_owned(),
            (None, _) => pick("未登録", "Not registered").to_owned(),
            (Some(_), true) => String::new(),
        };
        rows.push(BackupPaneRow {
            kind: 0,
            label: label.into(),
            tip: folder.display().to_string().into(),
            mode: tag.into(),
            backup_on: mode == Some(workspace::SaveMode::AutoBackup),
            can_toggle,
            count: SharedString::default(),
            open,
        });
        lines.push(PaneLine::Folder {
            folder: folder.clone(),
            id,
            files: group.map(backup::Group::files).unwrap_or_default(),
        });
        if !open {
            continue;
        }
        match group {
            Some(group) => {
                for (original, files) in &group.originals {
                    let name = original
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // 登録フォルダの中のサブフォルダのものは、フォルダからの相対パスで見せる。
                    let shown = original
                        .strip_prefix(&folder)
                        .map(|relative| relative.display().to_string())
                        .unwrap_or(name);
                    rows.push(BackupPaneRow {
                        kind: 1,
                        label: shown.into(),
                        tip: original.display().to_string().into(),
                        mode: SharedString::default(),
                        backup_on: false,
                        can_toggle: false,
                        count: files.len().to_string().into(),
                        open: false,
                    });
                    lines.push(PaneLine::File {
                        original: original.clone(),
                        files: files.clone(),
                    });
                }
            }
            None => {
                rows.push(note(say!(
                    "（まだバックアップはありません）",
                    "(No backups yet)"
                )));
                lines.push(PaneLine::Note);
            }
        }
    }
    window.set_backup_pane_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    PANE.with(|held| *held.borrow_mut() = lines);
}

fn note(text: String) -> BackupPaneRow {
    BackupPaneRow {
        kind: 2,
        label: text.into(),
        ..Default::default()
    }
}

/// 行を押した：フォルダは開閉、ファイルはそのファイルを開いて Backup History。
fn pane_clicked(window: &AppWindow, live: &Live, index: usize) {
    enum Act {
        Fold(PathBuf),
        History(PathBuf),
    }
    let act = PANE.with(|held| match held.borrow().get(index) {
        Some(PaneLine::Folder { folder, .. }) => Some(Act::Fold(folder.clone())),
        Some(PaneLine::File { original, .. }) => Some(Act::History(original.clone())),
        _ => None,
    });
    match act {
        Some(Act::Fold(folder)) => {
            CLOSED.with(|held| {
                let mut held = held.borrow_mut();
                if !held.remove(&folder) {
                    held.insert(folder);
                }
            });
            publish_pane(window, live);
        }
        Some(Act::History(original)) => history_of_file(window, live, &original),
        None => {}
    }
}

/// そのファイルを開いて（開いていなければ今のペインに）Backup History。
fn history_of_file(window: &AppWindow, live: &Live, original: &Path) {
    if !original.is_file() {
        window.tell(
            say!(
                "{}が見つかりません。Backup Historyは開いたファイルに対して出します",
                "{} was not found. Backup History works on a file that can be opened",
                original.display()
            )
            .into(),
        );
        return;
    }
    crate::open_path_in_focused_pane(window, live, original, Opening::Kept);
    open_history(window, live);
}

/// 行の AutoBackup の釦：そのフォルダの ON/OFF（自動保存とは排他）。
fn pane_toggled(window: &AppWindow, live: &Live, index: usize) {
    let id = PANE.with(|held| match held.borrow().get(index) {
        Some(PaneLine::Folder { id, .. }) => *id,
        _ => None,
    });
    if let Some(id) = id {
        crate::toggle_folder_mode(window, live, id, workspace::SaveMode::AutoBackup);
        publish_pane(window, live);
    }
}

/// 右クリックの行：0 Backup History…、1 Delete Backups…。
fn pane_menu(window: &AppWindow, live: &Live, index: usize, action: i32) {
    enum Act {
        History(PathBuf),
        Delete(Vec<PathBuf>, String),
    }
    let act = PANE.with(|held| match (held.borrow().get(index), action) {
        (Some(PaneLine::File { original, .. }), 0) => Some(Act::History(original.clone())),
        (Some(PaneLine::File { original, files }), _) => Some(Act::Delete(
            files.clone(),
            say!(
                "{}のバックアップ{}件",
                "the backups of {} ({})",
                original.display(),
                files.len()
            ),
        )),
        (Some(PaneLine::Folder { folder, files, .. }), _) => Some(Act::Delete(
            files.clone(),
            say!(
                "{}のバックアップ{}件",
                "the backups in {} ({})",
                folder.display(),
                files.len()
            ),
        )),
        _ => None,
    });
    match act {
        Some(Act::History(original)) => history_of_file(window, live, &original),
        Some(Act::Delete(files, _)) if files.is_empty() => {
            window.tell(say!("バックアップはありません", "There are no backups").into());
        }
        Some(Act::Delete(files, what)) => ask_delete(window, live, files, what),
        None => {}
    }
}

/// 「消す」と答えたとき（`Question::DeleteBackups`）。
pub fn delete_confirmed(window: &AppWindow, live: &Live, paths: &[PathBuf]) {
    let Some(store) = backup_store(window) else {
        return;
    };
    let result = store.delete(paths);
    let count = paths.len();
    live.cache.borrow_mut().log_diag(
        "file",
        &format!("backup delete count={count} ok={}", result.is_ok()),
    );
    after_history_delete(window, live);
    refresh_pane(window, live);
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

/// 移す・付け替えるのに失敗したときの理由。
fn why(error: backup::MoveError) -> String {
    match error {
        backup::MoveError::Exists(path) => say!(
            "同じ名前のファイルが先にあります：{}",
            "A file with the same name is already there: {}",
            path.display()
        ),
        backup::MoveError::Io(error) => error.to_string(),
    }
}

/// Settings → Backup の、Rust が入れる値（保存先の実際のパス、ファイル名の例）。
pub fn publish_settings(window: &AppWindow) {
    let shown = backup_root(window)
        .map(|root| backup::plain(&root).display().to_string())
        .unwrap_or_default();
    window.set_backup_folder_shown(shown.into());
    let format = window.get_backup_name_format();
    window.set_backup_name_draft(format.clone());
    name_edited(window, &format);
}

/// 保存先を`chosen`（空ならアプリ専用領域）へ変える。**バックアップを全部移し、移せなければ
/// 何も変えない**（書き手の決定 2026-09-27）。
fn change_folder(window: &AppWindow, live: &Live, chosen: String) {
    let from = backup_store(window);
    let to = if chosen.is_empty() {
        backup::default_root()
    } else {
        Some(PathBuf::from(&chosen))
    };
    let (Some(from), Some(to)) = (from, to) else {
        return;
    };
    if let Err(error) = from.move_to(&to) {
        let why = why(error);
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
    publish_settings(window);
    refresh_pane(window, live);
    window.tell(say!("保存先を変えました", "Backup folder changed").into());
}

/// 書式が使えない理由を、欄の下に出す文にする。
fn problem_text(problem: backup::FormatProblem) -> String {
    match problem {
        backup::FormatProblem::Name => pick(
            "この書式は使えません：{name} がちょうど1つ要ります。",
            "This format cannot be used: it needs exactly one {name}.",
        )
        .to_owned(),
        backup::FormatProblem::Time => pick(
            "この書式は使えません：秒までの日時（yyyy MM dd HH mm ss）が要ります。",
            "This format cannot be used: it needs the time to the second (yyyy MM dd HH mm ss).",
        )
        .to_owned(),
        backup::FormatProblem::Ext => pick(
            "この書式は使えません：{ext} は1つまでです。",
            "This format cannot be used: {ext} may appear only once.",
        )
        .to_owned(),
        backup::FormatProblem::Character(c) => say!(
            "この書式は使えません：ファイル名に「{}」は使えません。",
            "This format cannot be used: a file name cannot contain \"{}\".",
            c
        ),
    }
}

/// 打っているあいだ：例を作り直すか、使えない理由を言う。
fn name_edited(window: &AppWindow, format: &str) {
    match backup::check_format(format) {
        Ok(()) => {
            let example = backup::name_for(format, Path::new(r"C:\第一章.md"), timestamp::now())
                .unwrap_or_default();
            window.set_backup_name_example(say!("例：{example}", "Example: {example}").into());
            window.set_backup_name_problem(SharedString::default());
        }
        Err(problem) => window.set_backup_name_problem(problem_text(problem).into()),
    }
}

/// Enter：書式を替え、**それまでのバックアップの名前も全部付け替える**。付け替えられなければ
/// 何も変えずにダイアログで言い、欄を確定済みの書式へ戻す。
fn name_accepted(window: &AppWindow, live: &Live, format: &str) {
    // Enterのあと欄を離れても2度は付け替えない。
    if format == window.get_backup_name_format().as_str() {
        return;
    }
    if let Err(problem) = backup::check_format(format) {
        window.set_backup_name_problem(problem_text(problem).into());
        return;
    }
    let Some(store) = backup_store(window) else {
        return;
    };
    if let Err(error) = store.rename_to(format) {
        let why = why(error);
        live.cache.borrow_mut().log_diag(
            "file",
            &format!("backup rename failed format={format} {why}"),
        );
        publish_settings(window);
        ask_question(
            window,
            live,
            Question::BackupNotice,
            say!(
                "バックアップの名前を付け替えられなかったため、書式は変えていません。\n\n{why}",
                "The backups could not be renamed, so the file name format is unchanged.\n\n{why}"
            ),
            &["OK"],
            -1,
        );
        return;
    }
    window.set_backup_name_format(format.into());
    save_settings(window, &live.cache);
    live.cache
        .borrow_mut()
        .log_diag("file", &format!("backup rename ok format={format}"));
    publish_settings(window);
    refresh_pane(window, live);
    window.tell(say!("ファイル名の書式を変えました", "File name format changed").into());
}

pub fn wire(window: &AppWindow, live: &Live) {
    // 設定はもう読んである（`apply_settings`はこの手前）。保存先の実際のパスと名前の例を入れる。
    publish_settings(window);

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

    // 左ペインの Backups。行は押した時点の`PANE`で読むので、組み直しは次の回へ回す（6.18）。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_pane_clicked(move |index| {
        let (weak, live) = (weak.clone(), held.clone());
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                pane_clicked(&window, &live, index.max(0) as usize);
            }
        });
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_pane_toggled(move |index| {
        let (weak, live) = (weak.clone(), held.clone());
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                pane_toggled(&window, &live, index.max(0) as usize);
            }
        });
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_pane_menu(move |index, action| {
        let (weak, live) = (weak.clone(), held.clone());
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                pane_menu(&window, &live, index.max(0) as usize, action);
            }
        });
    });

    let weak = window.as_weak();
    window.on_backup_name_edited(move |format| {
        if let Some(window) = weak.upgrade() {
            name_edited(&window, &format);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_backup_name_accepted(move |format| {
        if let Some(window) = weak.upgrade() {
            name_accepted(&window, &held, &format);
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
