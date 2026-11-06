//! RFN01-70: 左Paneの検索からの一括置換を、画面の値とディスクで確かめる。
use super::*;
use crate::saving::{Harness, open_under, scratch_directory};
use std::fs;

/// a.md は開いている、b.md（CRLF）と c.md は開いていない。検索は root だけを歩く。
fn folder(name: &str) -> (Harness, Rc<OpenDocument>, PathBuf) {
    let root = scratch_directory(name);
    let (harness, document) =
        Harness::new(|weak| open_under(&root, "a.md", "猫と犬\nCat cat\n", weak));
    fs::write(root.join("b.md"), "猫が鳴く\r\ncat\r\n").unwrap();
    fs::write(root.join("c.md"), "犬だけ\n").unwrap();
    harness.live.folder.borrow_mut().searching = Some(root.clone());
    (harness, document, root)
}

fn ask(window: &AppWindow, live: &Live, needle: &str, replacement: &str) -> usize {
    window.set_folder_needle(needle.into());
    window.set_folder_replacement(replacement.into());
    folder_replace::requested(window, live);
    settle(window, live);
    match &*live.pending.borrow() {
        Some(Question::FolderReplace(_)) => 1,
        _ => 0,
    }
}

fn settle(window: &AppWindow, live: &Live) {
    for _ in 0..500 {
        folder_replace::collect(window, live);
        if !window.get_folder_replacing() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("folder replace did not finish");
}

#[test]
fn replaces_open_tabs_unsaved_and_closed_files_after_a_backup() {
    let (harness, document, root) = folder("folder-replace");
    let (window, live) = (&harness.window, &harness.live);

    assert_eq!(ask(window, live, "猫", "ネコ"), 1, "asks before replacing");
    // 訊いただけでは何も変わらない。
    assert_eq!(document.text.borrow().as_str(), "猫と犬\nCat cat\n");
    answer_question(window, live, 0);
    settle(window, live);

    assert_eq!(document.text.borrow().as_str(), "ネコと犬\nCat cat\n");
    assert!(document.text.edited(), "the open tab is left unsaved");
    let a = fs::read_to_string(root.join("a.md")).unwrap();
    assert_eq!(
        a, "猫と犬\nCat cat\n",
        "an open file's disk copy is untouched"
    );
    let b = fs::read_to_string(root.join("b.md")).unwrap();
    assert_eq!(b, "ネコが鳴く\r\ncat\r\n", "line breaks are kept");
    let c = fs::read_to_string(root.join("c.md")).unwrap();
    assert_eq!(c, "犬だけ\n");
    let store = saving::backup_store(window).unwrap();
    let kept = store.list(&root.join("b.md"));
    assert_eq!(kept.len(), 1, "the closed file was backed up first");
    assert_eq!(
        fs::read_to_string(&kept[0].path).unwrap(),
        "猫が鳴く\r\ncat\r\n"
    );

    // 開いているTABは1回のUndoで戻る。
    let id = PaneId::from_index(0);
    undo_in_pane(window, id, &document, &live.states, &live.cache, false);
    assert_eq!(document.text.borrow().as_str(), "猫と犬\nCat cat\n");
}

#[test]
fn the_panel_rules_are_the_body_search_rules() {
    let (harness, document, root) = folder("folder-replace-rules");
    let (window, live) = (&harness.window, &harness.live);

    // 大文字と小文字を区別する：`Cat`は残る。
    window.set_folder_match_case(true);
    assert_eq!(ask(window, live, "cat", "dog"), 1);
    answer_question(window, live, 0);
    settle(window, live);
    assert_eq!(document.text.borrow().as_str(), "猫と犬\nCat dog\n");
    let b = fs::read_to_string(root.join("b.md")).unwrap();
    assert_eq!(b, "猫が鳴く\r\ndog\r\n");

    // 正規表現。
    window.set_folder_match_case(false);
    window.set_folder_regex(true);
    assert_eq!(ask(window, live, "犬|猫", "動物"), 1);
    answer_question(window, live, 0);
    settle(window, live);
    assert_eq!(document.text.borrow().as_str(), "動物と動物\nCat dog\n");
    let c = fs::read_to_string(root.join("c.md")).unwrap();
    assert_eq!(c, "動物だけ\n");

    // 書きかけの正規表現は訊かずに理由を言う。
    assert_eq!(ask(window, live, "(", "x"), 0);
    assert!(!window.get_folder_status().is_empty());

    // 見つからなければ訊かない。tag: の検索では置換しない。
    window.set_folder_regex(false);
    assert_eq!(ask(window, live, "見つからない語", "x"), 0);
    assert_eq!(ask(window, live, "tag:#小説", "x"), 0);
}

#[test]
fn cancel_changes_nothing() {
    let (harness, document, root) = folder("folder-replace-cancel");
    let (window, live) = (&harness.window, &harness.live);

    assert_eq!(ask(window, live, "猫", "ネコ"), 1);
    answer_question(window, live, 1);
    assert_eq!(document.text.borrow().as_str(), "猫と犬\nCat cat\n");
    let b = fs::read_to_string(root.join("b.md")).unwrap();
    assert_eq!(b, "猫が鳴く\r\ncat\r\n");
}

#[test]
fn confirmation_rechecks_contents_only_in_the_files_it_listed() {
    let (harness, _, root) = folder("folder-replace-confirmed-files");
    let (window, live) = (&harness.window, &harness.live);
    let listed = root.join("b.md");
    let unlisted = root.join("c.md");

    assert_eq!(ask(window, live, "cat", "dog"), 1);
    fs::write(&listed, "externally edited\r\ncat cat\r\n").unwrap();
    fs::write(&unlisted, "now also matches: cat\n").unwrap();
    answer_question(window, live, 0);
    settle(window, live);

    assert_eq!(
        fs::read_to_string(&listed).unwrap(),
        "externally edited\r\ndog dog\r\n"
    );
    assert_eq!(
        fs::read_to_string(&unlisted).unwrap(),
        "now also matches: cat\n"
    );
    let store = saving::backup_store(window).unwrap();
    let backups = store.list(&listed);
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read_to_string(&backups[0].path).unwrap(),
        "externally edited\r\ncat cat\r\n"
    );
    assert!(store.list(&unlisted).is_empty());
}

#[test]
fn cancelling_the_background_scan_never_opens_confirmation() {
    let (harness, document, root) = folder("folder-replace-cancel-scan");
    let (window, live) = (&harness.window, &harness.live);
    window.set_folder_needle("猫".into());
    window.set_folder_replacement("ネコ".into());
    folder_replace::requested(window, live);
    assert!(window.get_folder_replacing());
    assert!(live.pending.borrow().is_none());
    folder_replace::requested(window, live);
    settle(window, live);
    assert!(live.pending.borrow().is_none());
    assert_eq!(document.text.borrow().as_str(), "猫と犬\nCat cat\n");
    assert_eq!(
        fs::read_to_string(root.join("b.md")).unwrap(),
        "猫が鳴く\r\ncat\r\n"
    );
}

#[test]
fn edits_made_after_dispatch_are_preserved_and_closed_replacement_continues() {
    let (harness, document, root) = folder("folder-replace-edit-during-worker");
    let (window, live) = (&harness.window, &harness.live);
    assert_eq!(ask(window, live, "猫", "ネコ"), 1);
    answer_question(window, live, 0);
    document.text.borrow_mut().push_str("入力を継続\n");
    settle(window, live);
    assert_eq!(
        document.text.borrow().as_str(),
        "猫と犬\nCat cat\n入力を継続\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.md")).unwrap(),
        "ネコが鳴く\r\ncat\r\n"
    );
}

#[test]
fn a_closed_file_cannot_be_adopted_while_the_worker_is_replacing_it() {
    let (harness, document, root) = folder("folder-replace-reserved-file");
    let (window, live) = (&harness.window, &harness.live);
    let path = root.join("b.md");
    let id = PaneId::from_index(0);
    assert_eq!(ask(window, live, "鳴く", "歩く"), 1);
    answer_question(window, live, 0);
    assert!(folder_replace::closed_write_in_progress());
    open_path_in_pane(window, live, id, &path, Opening::Kept);
    assert!(document_at(live, &path).is_none());
    assert!(!saving::write_document_to(
        window,
        live,
        &document,
        path.clone()
    ));
    settle(window, live);
    open_path_in_pane(window, live, id, &path, Opening::Kept);
    assert_eq!(
        document_at(live, &path).unwrap().text.borrow().as_str(),
        "猫が歩く\ncat\n"
    );
}

#[test]
fn a_closed_file_failure_keeps_the_original_and_other_paths_continue() {
    let (harness, document, root) = folder("folder-replace-failed-file");
    let (window, live) = (&harness.window, &harness.live);
    let read_only = root.join("b.md");
    let other = root.join("c.md");
    fs::write(&other, "猫\n").unwrap();
    let original = fs::metadata(&read_only).unwrap().permissions();
    let mut blocked = original.clone();
    blocked.set_readonly(true);
    fs::set_permissions(&read_only, blocked).unwrap();
    assert_eq!(ask(window, live, "猫", "ネコ"), 1);
    answer_question(window, live, 0);
    settle(window, live);
    fs::set_permissions(&read_only, original).unwrap();
    assert_eq!(
        fs::read_to_string(&read_only).unwrap(),
        "猫が鳴く\r\ncat\r\n"
    );
    assert_eq!(fs::read_to_string(&other).unwrap(), "ネコ\n");
    assert!(document.text.borrow().starts_with("ネコ"));
}
