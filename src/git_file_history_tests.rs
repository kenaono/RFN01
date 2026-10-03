//! RFN01-67 PR 3: File の「Git History…」を、画面の値で確かめる。Gitは本物を、一時フォルダで。
use super::*;
use crate::git::tests::{git_ok, repository};
use crate::saving::{Harness, open_under, scratch_directory};
use slint::Model;
use std::fs;

/// 最初の差分で右（その Commit の版）を採り、本文へ反映する。
fn take_first_difference(window: &AppWindow) {
    let rows = window.get_diff_rows();
    let first = (0..rows.row_count())
        .find(|at| rows.row_data(*at).unwrap().hunk_start)
        .expect("a difference");
    window.set_diff_selected_row(first as i32);
    window.invoke_diff_choose_side(true);
    window.invoke_diff_apply_merge();
}

fn subjects(window: &AppWindow) -> Vec<String> {
    let rows = window.get_git_history_rows();
    (0..rows.row_count())
        .map(|at| rows.row_data(at).unwrap().name.to_string())
        .collect()
}

/// 「旧名.md」で2回 Commit してから「原稿.md」へ名前を変え、もう1回 Commit した。
/// 開いているのは「原稿.md」（編集中の本文は「今」）。Gitが無ければ`None`。
fn renamed(name: &str) -> Option<(PathBuf, Harness, Rc<OpenDocument>)> {
    let root = scratch_directory(name);
    repository(&root)?;
    fs::write(root.join("旧名.md"), "一\n").unwrap();
    git::commit(&root, "一", true, false).unwrap();
    fs::write(root.join("旧名.md"), "二\n").unwrap();
    git::commit(&root, "二", true, false).unwrap();
    git_ok(&root, &["mv", "旧名.md", "原稿.md"]);
    git::commit(&root, "名前を変える", false, false).unwrap();
    fs::write(root.join("原稿.md"), "三\n").unwrap();
    git::commit(&root, "三", true, false).unwrap();
    let (harness, document) = Harness::new(|weak| open_under(&root, "原稿.md", "今\n", weak));
    Some((root, harness, document))
}

#[test]
fn the_log_follows_a_rename() {
    let Some((root, _harness, _document)) = renamed("history-log") else {
        return;
    };
    let versions = git::file_log(&root.join("原稿.md"), 500).unwrap();
    let names: Vec<(&str, &str)> = versions
        .iter()
        .map(|v| (v.subject.as_str(), v.path.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("三", "原稿.md"),
            ("名前を変える", "原稿.md"),
            ("二", "旧名.md"),
            ("一", "旧名.md"),
        ]
    );
}

#[test]
fn history_lists_commits_and_applies_a_version_with_one_undo() {
    let Some((_root, harness, document)) = renamed("history-open") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    assert!(available(&document));
    open(window, live);
    assert!(window.get_git_history_active());
    assert!(window.get_diff_active());
    assert!(window.get_diff_merge_enabled());
    assert_eq!(window.get_git_history_file(), "原稿.md");
    assert_eq!(subjects(window), ["三", "名前を変える", "二", "一"]);
    // 開いた直後はいちばん新しい Commit と比べる。
    assert_eq!(window.get_git_history_selected(), 0);
    assert!(window.get_diff_right_path().contains("三"));

    // 名前を変える前の Commit も、その時の名前で読める。
    chosen(window, live, 3);
    assert_eq!(window.get_git_history_selected(), 3);
    assert!(window.get_diff_right_path().contains("一"));
    take_first_difference(window);
    assert_eq!(document.text.borrow().replace("\r\n", "\n"), "一\n");
    crate::undo_in_pane(
        window,
        PaneId::from_index(0),
        &document,
        &live.states,
        &live.cache,
        false,
    );
    assert_eq!(document.text.borrow().as_str(), "今\n");

    // 比較を終えると一覧も閉じる。
    diff_view::dismiss(window);
    assert!(!window.get_git_history_active());
    assert!(!window.get_diff_active());
}

#[test]
fn a_file_outside_git_does_not_open() {
    let outside = scratch_directory("history-outside");
    let (harness, document) = Harness::new(|weak| open_under(&outside, "a.md", "a\n", weak));
    // 一時フォルダの上がリポジトリなら確かめられない。
    if git::repository_root(&outside).ok().flatten().is_some() {
        return;
    }
    assert!(!available(&document));
    open(&harness.window, &harness.live);
    assert!(!harness.window.get_git_history_active());
}

#[test]
fn a_file_not_committed_yet_does_not_open() {
    let root = scratch_directory("history-new");
    if repository(&root).is_none() {
        return;
    }
    let (harness, document) = Harness::new(|weak| open_under(&root, "新.md", "新\n", weak));
    assert!(
        available(&document),
        "inside a repository the row is enabled"
    );
    open(&harness.window, &harness.live);
    assert!(
        !harness.window.get_git_history_active(),
        "nothing committed yet"
    );
}

/// 画面の絵を書き出す（`cargo test -- --ignored git_history_screen`）。一時フォルダの
/// `rfnedit-git-snapshot`に、Git History の画面をPPMで置く。
#[test]
#[ignore]
fn git_history_screen() {
    let Some((_root, harness, _document)) = renamed("history-snapshot") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    let out = std::env::temp_dir().join("rfnedit-git-snapshot");
    fs::create_dir_all(&out).unwrap();
    let (width, height) = (1000usize, 740usize);
    window.show().unwrap();
    open(window, live);
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
    window.window().request_redraw();
    harness.surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for pixel in &pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    fs::write(out.join("git-history.ppm"), ppm).unwrap();
}
