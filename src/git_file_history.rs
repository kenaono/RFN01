//! RFN01-67 PR 3（書き手と決めた 2026-10-03）: File の「Git History…」。
//!
//! Backup History（[`crate::backup_ui`]）と同じ形：比較の画面（`diff_view::show_merge`）の左に、
//! 今のTABのファイルが入っている Commit の一覧を置く。行で比べる相手を選び、差分を選んで本文へ
//! 反映する（Undo 1回で戻せる）。消すものではないので、チェックと Delete は出さない。
//!
//! - 一覧は`git log --follow`（[`crate::git::file_log`]）。名前を変える前の Commit まで追う。
//! - 右の版は文書の文字コードで読み、読めなければ判別に任せる（前回のCommitとの比較と同じ決まり）。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::git::{self, FileVersion};
use crate::open_document::OpenDocument;
use crate::{
    AppWindow, BackupRow, Live, MAX_DOCUMENT_CHARACTERS, PaneId, StatusBar, diff_view,
    focused_pane, say,
};

/// 一覧に出す Commit の数の上限。
const LIMIT: usize = 500;

struct History {
    document: Rc<OpenDocument>,
    pane: PaneId,
    path: PathBuf,
    root: PathBuf,
    versions: Vec<FileVersion>,
    selected: usize,
}

thread_local! {
    static HISTORY: RefCell<Option<History>> = const { RefCell::new(None) };
}

/// 比較の画面が閉じたとき（`diff_view::dismiss`）。
pub fn forget(window: &AppWindow) {
    HISTORY.with(|held| held.borrow_mut().take());
    window.set_git_history_active(false);
}

/// File の行を押せるか：保存先があり、Git の管理下にある。
pub fn available(document: &OpenDocument) -> bool {
    let path = document.file.borrow().path().map(Path::to_path_buf);
    path.and_then(|path| Some(path.parent()?.to_path_buf()))
        .is_some_and(|folder| matches!(git::repository_root(&folder), Ok(Some(_))))
}

fn rows_of(history: &History) -> Vec<BackupRow> {
    history
        .versions
        .iter()
        .map(|version| BackupRow {
            label: version.date.clone().into(),
            detail: version.sha.chars().take(7).collect::<String>().into(),
            name: version.subject.clone().into(),
            tip: format!("{} {}", version.sha, version.path).into(),
            checked: false,
        })
        .collect()
}

fn read_version(history: &History, version: &FileVersion) -> Result<String, String> {
    let bytes = git::blob_at(&history.root, &version.sha, &version.path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let encoding = history.document.file.borrow().form().encoding;
    crate::file_io::decode_as(&bytes, MAX_DOCUMENT_CHARACTERS, encoding)
        .or_else(|_| crate::file_io::decode(&bytes, MAX_DOCUMENT_CHARACTERS))
        .map(|(text, _)| text)
        .map_err(|error| error.to_string())
}

/// 比べる相手を`selected`にして、比較の画面を組み直す（まだ反映していない差分の選択は捨てる）。
fn show_selected(window: &AppWindow, live: &Live) {
    let shown = HISTORY.with(|held| {
        let held = held.borrow();
        let history = held.as_ref()?;
        let version = history.versions.get(history.selected)?;
        Some((
            history.document.clone(),
            history.pane,
            history.path.clone(),
            version.clone(),
            read_version(history, version),
        ))
    });
    let Some((document, pane, path, version, right)) = shown else {
        return;
    };
    let right = match right {
        Ok(text) => text,
        Err(error) => {
            window.tell_tab(
                say!(
                    "その版を読めません: {error}",
                    "Cannot read that version: {error}"
                )
                .into(),
            );
            return;
        }
    };
    let left = document.text.borrow().clone();
    let short: String = version.sha.chars().take(7).collect();
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
        format!("Commit {short} {}  {}", version.date, version.subject),
        right,
    );
    window.set_git_history_active(true);
    HISTORY.with(|held| {
        if let Some(history) = held.borrow().as_ref() {
            window.set_git_history_rows(ModelRc::new(VecModel::from(rows_of(history))));
            window.set_git_history_selected(history.selected as i32);
        }
    });
}

/// File → Git History…
pub fn open(window: &AppWindow, live: &Live) {
    let pane = focused_pane(window);
    let document = live.states.document(pane);
    let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
        window.tell_tab(
            say!(
                "まだ保存先がありません。Gitの履歴は保存したファイルにだけあります",
                "This document has not been saved yet, so it has no Git history"
            )
            .into(),
        );
        return;
    };
    let root = path
        .parent()
        .map(git::repository_root)
        .transpose()
        .map(Option::flatten);
    let root = match root {
        Ok(Some(root)) => root,
        Ok(None) => {
            window.tell_tab(
                say!(
                    "このファイルはGitで管理されていません",
                    "This file is not in a Git repository"
                )
                .into(),
            );
            return;
        }
        Err(error) => {
            window.tell_tab(error.to_string().into());
            return;
        }
    };
    let versions = match git::file_log(&path, LIMIT) {
        Ok(versions) => versions,
        Err(error) => {
            window.tell_tab(error.to_string().into());
            return;
        }
    };
    if versions.is_empty() {
        window.tell_tab(
            say!(
                "このファイルはまだCommitされていません",
                "This file has not been committed yet"
            )
            .into(),
        );
        return;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    window.set_git_history_file(name.into());
    HISTORY.with(|held| {
        *held.borrow_mut() = Some(History {
            document,
            pane,
            path,
            root,
            versions,
            selected: 0,
        });
    });
    show_selected(window, live);
}

fn chosen(window: &AppWindow, live: &Live, index: usize) {
    let changed = HISTORY.with(|held| {
        let mut held = held.borrow_mut();
        let history = held.as_mut()?;
        (index < history.versions.len() && index != history.selected).then(|| {
            history.selected = index;
        })
    });
    if changed.is_some() {
        show_selected(window, live);
    }
}

pub fn wire(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let held = live.clone();
    window.on_git_history_requested(move || {
        if let Some(window) = weak.upgrade() {
            open(&window, &held);
        }
    });
    // 行は押した時点の一覧で読むので、組み直しは次の回へ回す（6.18）。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_git_history_chosen(move |index| {
        let (weak, live) = (weak.clone(), held.clone());
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                chosen(&window, &live, index.max(0) as usize);
            }
        });
    });
}

#[cfg(test)]
#[path = "git_file_history_tests.rs"]
mod tests;
