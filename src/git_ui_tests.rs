//! RFN01-67: Git Changes の面を、画面の値で確かめる。Gitは本物を、一時フォルダのリポジトリで。
use super::*;
use crate::git::tests::{git_ok, repository};
use crate::saving::{Harness, attach_workspace, edit, open_under, scratch_directory};
use crate::workspace::SaveMode;
use slint::Model;
use std::fs;

/// 面を開いた状態にして、裏の読み直しが済むまで待つ。
fn show(window: &AppWindow, live: &Live) {
    window.set_tree_open(true);
    window.set_left_tab(TAB);
    publish(window, live);
    settle(window, live);
}

/// 裏の操作と読み直しが済むまで、時計の代わりに受け取りを回す。
fn settle(window: &AppWindow, live: &Live) {
    for _ in 0..500 {
        collect(window, live);
        let idle = PANE.with(|pane| {
            let pane = pane.borrow();
            pane.job.is_none() && pane.reading.is_none()
        });
        if idle {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("git did not finish");
}

fn rows(window: &AppWindow) -> Vec<(i32, String, String)> {
    let rows = window.get_git_rows();
    (0..rows.row_count())
        .map(|at| {
            let row = rows.row_data(at).unwrap();
            (row.kind, row.letter.to_string(), row.label.to_string())
        })
        .collect()
}

fn index_of(window: &AppWindow, label: &str) -> usize {
    rows(window)
        .iter()
        .position(|(_, _, shown)| shown == label)
        .unwrap_or_else(|| panic!("no row {label}"))
}

fn last_message(root: &Path) -> String {
    git::last_message(root).unwrap()
}

/// 答えを待っている問いを取り出す（`answer_question`の代わり）。
fn take_question(live: &Live) -> Question {
    live.pending.borrow_mut().take().expect("a question")
}

/// 書き手の原稿フォルダ：1度Commitした`原稿.md`を開いている。Gitが無ければ`None`。
fn manuscript(name: &str) -> Option<(PathBuf, Harness, Rc<OpenDocument>)> {
    let root = scratch_directory(name);
    repository(&root)?;
    let (harness, document) = Harness::new(|weak| open_under(&root, "原稿.md", "一行目\n", weak));
    git::commit(&root, "最初", true, false).unwrap();
    attach_workspace(&harness.live, &root, SaveMode::Recovery);
    Some((root, harness, document))
}

#[test]
fn lists_changes_and_stages_them() {
    let Some((root, harness, _document)) = manuscript("git-list") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::create_dir_all(root.join("章")).unwrap();
    fs::write(root.join("章").join("二.md"), "二\n").unwrap();
    fs::write(root.join("原稿.md"), "書き換えた\n").unwrap();
    show(window, live);
    assert_eq!(window.get_git_mode(), 2);
    assert_eq!(window.get_git_branch(), "main");
    assert!(window.get_git_has_commit());
    assert!(!window.get_git_has_upstream());
    assert_eq!(window.get_git_change_count(), 2);
    let listed = rows(window);
    assert_eq!(
        listed[0].0, 1,
        "the Changes header first: nothing is staged"
    );
    assert!(listed.contains(&(4, "M".into(), "原稿.md".into())));
    assert!(listed.contains(&(4, "A".into(), "二.md".into())));
    let second = window
        .get_git_rows()
        .row_data(index_of(window, "二.md"))
        .unwrap();
    assert_eq!(second.folder, "章");

    // ＋で1つ Stage すると、Staged Changes が上に出る。
    row_staged(window, live, index_of(window, "原稿.md"));
    settle(window, live);
    assert_eq!(window.get_git_staged_count(), 1);
    assert_eq!(rows(window)[0].0, 0);
    assert!(rows(window).contains(&(3, "M".into(), "原稿.md".into())));
    // 見出しの−で全部外す。
    row_staged(window, live, 0);
    settle(window, live);
    assert_eq!(window.get_git_staged_count(), 0);
    // 見出しを押すと畳む。
    let header = index_of(window, &format!("{} (2)", pick("変更", "Changes")));
    row_clicked(window, header);
    assert_eq!(rows(window).len(), 1);
}

#[test]
fn a_folder_outside_git_offers_to_create_a_repository() {
    let root = scratch_directory("git-none");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "a.md", "a\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    if git::repository_root(&root).ok().flatten().is_some() {
        return;
    }
    attach_workspace(live, &root, SaveMode::Recovery);
    show(window, live);
    if window.get_git_mode() == 0 {
        // Gitの無いPC：断りの理由を出す。
        assert!(!window.get_git_note().is_empty());
        return;
    }
    assert_eq!(window.get_git_mode(), 1);
    create_repository(window, live);
    settle(window, live);
    assert_eq!(window.get_git_mode(), 2);
    assert!(!window.get_git_has_commit());
}

#[test]
fn without_a_workspace_the_pane_says_how_to_start() {
    let root = scratch_directory("git-no-workspace");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "a.md", "a\n", weak));
    show(&harness.window, &harness.live);
    assert_eq!(harness.window.get_git_mode(), 0);
    assert!(!harness.window.get_git_note().is_empty());
}

#[test]
fn commit_asks_to_save_first_and_commits_what_is_shown() {
    let Some((root, harness, document)) = manuscript("git-commit") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    show(window, live);
    edit(&document, "二行目\n");
    window.set_git_message("二行目を足す".into());
    commit(window, live);
    let Question::GitUnsaved(pending) = take_question(live) else {
        panic!("expected the unsaved question");
    };
    unsaved_answered(window, live, pending, 0);
    settle(window, live);
    assert!(!document.text.edited());
    assert_eq!(last_message(&root), "二行目を足す");
    let committed = git::status(&root).unwrap();
    assert!(committed.changes.is_empty() && committed.staged.is_empty());
    // 通ったらメッセージ欄は空になる。
    assert_eq!(window.get_git_message(), "");

    // Amend：直前のメッセージが入り、外すと書きかけへ戻る。
    window.set_git_message("書きかけ".into());
    amend_toggled(window, true);
    assert!(window.get_git_amend());
    assert_eq!(window.get_git_message(), "二行目を足す");
    amend_toggled(window, false);
    assert_eq!(window.get_git_message(), "書きかけ");
    amend_toggled(window, true);
    window.set_git_message("二行目を足した".into());
    commit(window, live);
    settle(window, live);
    assert!(live.pending.borrow().is_none(), "not pushed: no question");
    assert_eq!(last_message(&root), "二行目を足した");
    assert!(!window.get_git_amend());
}

#[test]
fn undo_changes_discards_the_open_tab_and_reads_the_file_again() {
    let Some((root, harness, document)) = manuscript("git-undo") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::write(root.join("原稿.md"), "外で書き換えた\n").unwrap();
    show(window, live);
    edit(&document, "未保存\n");
    row_menu(window, live, index_of(window, "原稿.md"), 2);
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the confirmation");
    };
    confirmed(window, live, pending);
    // 確かめたら、未保存の問いは重ねない。
    assert!(live.pending.borrow().is_none());
    settle(window, live);
    assert!(!document.text.edited());
    let text = document.text.borrow().replace("\r\n", "\n");
    assert_eq!(text, "一行目\n");
    assert_eq!(window.get_git_change_count(), 0);
}

#[test]
fn switching_branches_with_unsaved_work_can_discard_it() {
    let Some((root, harness, document)) = manuscript("git-switch") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    git_ok(&root, &["branch", "draft"]);
    git_ok(&root, &["switch", "-q", "draft"]);
    fs::write(root.join("原稿.md"), "draft\n").unwrap();
    git::commit(&root, "draft", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "main"]);
    show(window, live);
    // 外でブランチを替えたので、TABは古い中身のまま。編集もしている。
    edit(&document, "未保存\n");
    let draft = window
        .get_git_branches()
        .iter()
        .position(|name| name == "draft")
        .unwrap();
    branch_chosen(window, live, draft);
    let Question::GitUnsaved(pending) = take_question(live) else {
        panic!("expected the unsaved question");
    };
    // キャンセルなら何もしない。
    unsaved_answered(window, live, pending.clone(), 2);
    settle(window, live);
    assert_eq!(window.get_git_branch(), "main");
    assert!(document.text.edited());
    // 破棄して続ける。
    unsaved_answered(window, live, pending, 1);
    settle(window, live);
    assert_eq!(window.get_git_branch(), "draft");
    assert!(!document.text.edited());
    let text = document.text.borrow().replace("\r\n", "\n");
    assert_eq!(text, "draft\n");
}

#[test]
fn a_failure_is_shown_as_a_notice_and_changes_nothing() {
    let Some((root, harness, _document)) = manuscript("git-fail") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    show(window, live);
    // 名前とメールが無いとCommitできない。
    git_ok(&root, &["config", "--unset", "user.name"]);
    git_ok(&root, &["config", "--unset", "user.email"]);
    fs::write(root.join("原稿.md"), "x\n").unwrap();
    settle(window, live);
    window.set_git_message("x".into());
    commit(window, live);
    settle(window, live);
    // 手元の設定（global）に名前があれば通ってしまうので、そのときは確かめない。
    if last_message(&root) == "x" {
        return;
    }
    assert!(matches!(take_question(live), Question::GitNotice));
    assert_eq!(
        window.get_git_message(),
        "x",
        "a failed commit keeps the message"
    );
    assert!(!window.get_git_busy());
}

/// 書き手の Accept 2026-10-03：Stash の行の Pop の釦（右クリックの Pop と同じ`row-menu(行, 1)`）。
#[test]
fn the_pop_button_on_a_stash_row_brings_the_changes_back() {
    let Some((root, harness, _document)) = manuscript("git-pop") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::write(root.join("原稿.md"), "書きかけ\n").unwrap();
    show(window, live);
    window.set_git_message("場面".into());
    act(window, live, Action::StashAll("場面".into()));
    settle(window, live);
    assert_eq!(window.get_git_change_count(), 0);
    let stash = rows(window)
        .iter()
        .position(|(kind, ..)| *kind == 5)
        .expect("a stash row");
    row_menu(window, live, stash, 1);
    settle(window, live);
    assert_eq!(window.get_git_change_count(), 1);
    assert!(rows(window).iter().all(|(kind, ..)| *kind != 5));
    let text = fs::read_to_string(root.join("原稿.md")).unwrap();
    assert_eq!(text.replace("\r\n", "\n"), "書きかけ\n");
}

/// 画面の絵を書き出す（`cargo test -- --ignored git_screens`）。一時フォルダの
/// `rfnedit-git-snapshot`に、Git Changes の面をPPMで置く。
#[test]
#[ignore]
fn git_screens() {
    let Some((root, harness, _document)) = manuscript("git-snapshot") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::write(root.join("退避.md"), "退避\n").unwrap();
    git::stash_all(&root, "書きかけの場面").unwrap();
    fs::create_dir_all(root.join("章")).unwrap();
    fs::write(root.join("章").join("第二章.md"), "二\n").unwrap();
    fs::write(root.join("原稿.md"), "書き換えた\n").unwrap();
    fs::write(root.join("メモ.txt"), "メモ\n").unwrap();
    git::stage(&root, &["メモ.txt".to_owned()]).unwrap();
    let out = std::env::temp_dir().join("rfnedit-git-snapshot");
    fs::create_dir_all(&out).unwrap();
    let (width, height) = (1000usize, 740usize);
    let draw = |name: &str| {
        let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
        window.window().request_redraw();
        harness.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in &pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        fs::write(out.join(format!("{name}.ppm")), ppm).unwrap();
    };
    window.show().unwrap();
    show(window, live);
    window.set_git_message("第一章を直す".into());
    draw("git-changes");
    PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        pane.snapshot = None;
        pane.error = None;
    });
    fs::remove_dir_all(root.join(".git")).unwrap();
    publish(window, live);
    settle(window, live);
    draw("git-none");
}
