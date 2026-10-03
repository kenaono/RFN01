//! RFN01-67 PR 2a: Git Repository の画面を、画面の値で確かめる。Gitは本物を、一時フォルダで。
use super::*;
use crate::Question;
use crate::git::tests::{git_ok, repository};
use crate::saving::{Harness, attach_workspace, open_under, scratch_directory};
use crate::workspace::SaveMode;
use slint::Model;
use std::fs;

/// 裏の操作と読み直しが済むまで、時計の代わりに受け取りを回す。
fn settle(window: &AppWindow, live: &Live) {
    for _ in 0..500 {
        let changes_done = git_ui::pump(window, live);
        collect(window, live);
        let repo_done = REPO.with(|repo| repo.borrow().reading.is_none());
        if changes_done && repo_done {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("git did not finish");
}

fn take_question(live: &Live) -> Question {
    live.pending.borrow_mut().take().expect("a question")
}

/// main：最初 ← 二、draft：最初 ← 場面。HEAD は main。原稿.md を開いている。
fn manuscript(name: &str) -> Option<(PathBuf, Harness)> {
    let root = scratch_directory(name);
    repository(&root)?;
    let (harness, _document) = Harness::new(|weak| open_under(&root, "原稿.md", "一行目\n", weak));
    git::commit(&root, "最初", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "-c", "draft"]);
    fs::write(root.join("場面.md"), "場面\n").unwrap();
    git::commit(&root, "場面", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "main"]);
    fs::write(root.join("原稿.md"), "二行目\n").unwrap();
    git::commit(&root, "二", true, false).unwrap();
    attach_workspace(&harness.live, &root, SaveMode::Recovery);
    Some((root, harness))
}

fn messages(window: &AppWindow) -> Vec<String> {
    let rows = window.get_git_repo_commits();
    (0..rows.row_count())
        .map(|at| rows.row_data(at).unwrap().message.to_string())
        .collect()
}

fn side_labels(window: &AppWindow) -> Vec<(i32, String)> {
    let rows = window.get_git_repo_branches();
    (0..rows.row_count())
        .map(|at| {
            let row = rows.row_data(at).unwrap();
            (row.kind, row.label.to_string())
        })
        .collect()
}

fn side_index(window: &AppWindow, kind: i32, label: &str) -> usize {
    side_labels(window)
        .iter()
        .position(|(k, l)| *k == kind && l == label)
        .unwrap_or_else(|| panic!("no side row {label}"))
}

fn row_of(window: &AppWindow, message: &str) -> usize {
    messages(window)
        .iter()
        .position(|m| m == message)
        .unwrap_or_else(|| panic!("no commit {message}"))
}

/// 紫は今のブランチの筋だけ。何本枝が出ても、ほかの筋は紫にならない。
#[test]
fn only_the_current_branch_is_purple() {
    let purple = lane_color(0);
    for index in 1..40 {
        assert_ne!(lane_color(index), purple, "lane colour {index}");
    }
    assert_ne!(lane_color(1), lane_color(2));
}

#[test]
fn without_a_workspace_it_does_not_open() {
    let root = scratch_directory("repo-no-workspace");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "a.md", "a\n", weak));
    open(&harness.window, &harness.live);
    assert!(!harness.window.get_git_repo_active());
}

#[test]
fn opens_with_every_branch_in_one_graph() {
    let Some((_root, harness)) = manuscript("repo-open") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    assert!(window.get_git_repo_active());
    settle(window, live);
    // draft の「場面」も同じグラフに出る。WIP は無い。
    // 同じ秒の Commit の並びは Git が決める。最初の Commit がいちばん下。
    let mut listed = messages(window);
    assert_eq!(listed.pop().as_deref(), Some("最初"));
    listed.sort();
    assert_eq!(listed, ["二", "場面"]);
    assert_eq!(window.get_git_repo_selected(), -1);
    assert_eq!(window.get_git_repo_detail_mode(), 0);
    assert!(!window.get_git_repo_more());
    let rows = window.get_git_repo_commits();
    let head = rows.row_data(row_of(window, "二")).unwrap();
    assert!(head.head && head.in_head);
    let tags: Vec<(String, i32)> = (0..head.refs.row_count())
        .map(|at| {
            let tag = head.refs.row_data(at).unwrap();
            (tag.label.to_string(), tag.kind)
        })
        .collect();
    assert_eq!(tags, [("main".to_owned(), 0)]);
    assert!(head.lines.row_count() > 0);
    // draft にしか無い Commit は今のブランチから辿れない（Cherry-pick できる側）。
    let scene = rows.row_data(row_of(window, "場面")).unwrap();
    assert!(!scene.in_head);
    assert_ne!(scene.node_x, head.node_x, "draft is drawn beside main");
    assert_eq!(
        side_labels(window),
        [
            (0, "LOCAL (2)".to_owned()),
            (1, "draft".to_owned()),
            (1, "main".to_owned()),
        ]
    );
    close(window);
    assert!(!window.get_git_repo_active());
}

#[test]
fn selecting_shows_details_and_a_branch_jumps_to_its_tip() {
    let Some((root, harness)) = manuscript("repo-select") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::write(root.join("メモ.txt"), "メモ\n").unwrap();
    open(window, live);
    settle(window, live);
    assert_eq!(messages(window)[0], "// WIP");
    select_row(window, row_of(window, "二"));
    assert_eq!(window.get_git_repo_detail_mode(), 1);
    assert_eq!(window.get_git_repo_detail_message(), "二");
    let files = window.get_git_repo_detail_files();
    assert_eq!(files.row_count(), 1);
    assert_eq!(files.row_data(0).unwrap().label, "原稿.md");
    select_row(window, 0);
    assert_eq!(window.get_git_repo_detail_mode(), 2);

    let generation = window.get_git_repo_scroll_generation();
    branch_clicked(window, side_index(window, 1, "draft"));
    assert_eq!(
        window.get_git_repo_selected() as usize,
        row_of(window, "場面")
    );
    assert_eq!(window.get_git_repo_detail_message(), "場面");
    assert_eq!(window.get_git_repo_scroll_generation(), generation + 1);

    // ファイルを押すと、右の列に1列の差分（PR 2b）。「Open in Comparison」で比較の画面がこの上に開く。
    select_row(window, row_of(window, "二"));
    show_diff(window, 0, false);
    assert_eq!(window.get_git_repo_detail_mode(), 3);
    diff_open_full(window);
    assert!(window.get_diff_active());
    assert!(window.get_git_repo_active());
    assert!(window.get_diff_right_path().contains("原稿.md"));
    crate::diff_view::dismiss(window);
    assert!(
        window.get_git_repo_active(),
        "closing the comparison returns here"
    );
}

#[test]
fn dropping_a_branch_on_the_current_one_asks_then_merges() {
    let Some((root, harness)) = manuscript("repo-drop") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    branch_dropped(
        window,
        live,
        side_index(window, 1, "draft"),
        side_index(window, 1, "main"),
    );
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the merge confirmation");
    };
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    assert!(root.join("場面.md").exists());
    assert!(messages(window)[0].starts_with("Merge branch 'draft'"));
    // 合流した「場面」は今のブランチから辿れるようになった。
    let scene = window
        .get_git_repo_commits()
        .row_data(row_of(window, "場面"))
        .unwrap();
    assert!(scene.in_head);
}

#[test]
fn dropping_on_another_branch_checks_it_out_first() {
    let Some((root, harness)) = manuscript("repo-drop-other") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    branch_dropped(
        window,
        live,
        side_index(window, 1, "main"),
        side_index(window, 1, "draft"),
    );
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the merge confirmation");
    };
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    assert_eq!(git::status(&root).unwrap().branch.as_deref(), Some("draft"));
    let text = fs::read_to_string(root.join("原稿.md")).unwrap();
    assert_eq!(text.replace("\r\n", "\n"), "二行目\n");
}

#[test]
fn deleting_an_unmerged_branch_asks_twice() {
    let Some((root, harness)) = manuscript("repo-delete") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    branch_menu(window, live, side_index(window, 1, "draft"), 4);
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the delete confirmation");
    };
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    // Merge していないので、もう一度訊かれる。
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the second confirmation");
    };
    assert!(git::branches(&root).unwrap().contains(&"draft".to_owned()));
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    assert_eq!(git::branches(&root).unwrap(), ["main"]);
    let mut listed = messages(window);
    listed.sort();
    assert_eq!(listed, ["二", "最初"]);
}

#[test]
fn cherry_pick_and_reset_from_the_commit_menu() {
    let Some((root, harness)) = manuscript("repo-commit-menu") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    commit_menu(window, live, row_of(window, "場面"), 2);
    settle(window, live);
    assert!(root.join("場面.md").exists());
    assert_eq!(git::last_message(&root).unwrap(), "場面");
    // Keep Changes：確認なしで1つ前へ、中身は残る。
    commit_menu(window, live, row_of(window, "二"), 3);
    assert!(live.pending.borrow().is_none());
    settle(window, live);
    assert_eq!(git::last_message(&root).unwrap(), "二");
    assert!(root.join("場面.md").exists());
    // Delete Changes は確かめる。
    commit_menu(window, live, row_of(window, "最初"), 4);
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the reset confirmation");
    };
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    assert_eq!(git::last_message(&root).unwrap(), "最初");
    let text = fs::read_to_string(root.join("原稿.md")).unwrap();
    assert_eq!(text.replace("\r\n", "\n"), "一行目\n");
    // まだGitが知らないファイル（Keep Changes で外れた「場面.md」）は残る（git reset --hard と同じ）。
    assert!(root.join("場面.md").exists());
}

/// 書き手の確認 2026-10-03：main から作った TestBranch で Commit した（Push も main の Commit も
/// していない）。**今いるのが TestBranch でも、紫の幹は main**で、TestBranch は別の色で分かれる。
#[test]
fn a_branch_ahead_of_main_is_drawn_off_the_purple_trunk() {
    let Some((root, harness)) = manuscript("repo-trunk") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    git_ok(&root, &["switch", "-q", "-c", "TestBranch"]);
    fs::write(root.join("試し.md"), "試し\n").unwrap();
    git::commit(&root, "試し", true, false).unwrap();
    open(window, live);
    settle(window, live);
    let rows = window.get_git_repo_commits();
    let test = rows.row_data(row_of(window, "試し")).unwrap();
    let main = rows.row_data(row_of(window, "二")).unwrap();
    assert!(test.head, "TestBranch is where HEAD is");
    assert_eq!(main.node_color, lane_color(0), "main is the purple trunk");
    assert_ne!(test.node_color, lane_color(0), "TestBranch is not purple");
    assert_ne!(test.node_x, main.node_x, "TestBranch is drawn beside main");
}

#[test]
fn the_trunk_is_main_then_master_then_the_remote_default() {
    let branch = |name: &str, sha: &str, remote: bool| Ref {
        name: name.into(),
        sha: sha.into(),
        remote,
        upstream: None,
        ahead: 0,
        behind: 0,
    };
    let refs = [
        branch("draft", "d", false),
        branch("master", "s", false),
        branch("main", "m", false),
    ];
    assert_eq!(trunk_of(&refs, None).as_deref(), Some("m"));
    assert_eq!(trunk_of(&refs[..2], None).as_deref(), Some("s"));
    let remote = [
        branch("draft", "d", false),
        branch("origin/trunk", "t", true),
    ];
    assert_eq!(
        trunk_of(&remote, Some("origin/trunk")).as_deref(),
        Some("t")
    );
    assert_eq!(trunk_of(&remote, None), None);
}

fn diff_text(window: &AppWindow) -> Vec<(i32, String, Vec<String>)> {
    let rows = window.get_git_repo_diff_rows();
    (0..rows.row_count())
        .map(|at| {
            let row = rows.row_data(at).unwrap();
            let text: String = (0..row.parts.row_count())
                .map(|k| row.parts.row_data(k).unwrap().text.to_string())
                .collect();
            let marked: Vec<String> = (0..row.parts.row_count())
                .map(|k| row.parts.row_data(k).unwrap())
                .filter(|part| part.mark)
                .map(|part| part.text.to_string())
                .collect();
            (row.kind, text, marked)
        })
        .collect()
}

/// PR 2b：ファイルを押すと右の列に1列の差分。変わった字だけを塗る。
#[test]
fn a_file_shows_an_inline_diff_with_the_changed_characters() {
    let Some((root, harness)) = manuscript("repo-inline") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    select_row(window, row_of(window, "二"));
    show_diff(window, 0, false);
    assert_eq!(window.get_git_repo_detail_mode(), 3);
    assert_eq!(window.get_git_repo_diff_name(), "原稿.md");
    assert!(window.get_git_repo_diff_switchable());
    assert_eq!(
        (
            window.get_git_repo_diff_removed(),
            window.get_git_repo_diff_added()
        ),
        (1, 1)
    );
    let lines = diff_text(window);
    assert_eq!(lines[0], (1, "一行目".to_owned(), vec!["一".to_owned()]));
    assert_eq!(lines[1], (2, "二行目".to_owned(), vec!["二".to_owned()]));
    // Working Copy：その Commit と今のファイル。
    fs::write(root.join("原稿.md"), "二行目を直した\n").unwrap();
    diff_working_chosen(window, true);
    assert!(window.get_git_repo_diff_working());
    let lines = diff_text(window);
    assert_eq!(lines[1].2, ["を直した"]);
    // ‹ Files で詳細へ戻る。
    diff_back(window);
    assert_eq!(window.get_git_repo_detail_mode(), 1);
}

#[test]
fn a_wip_file_shows_its_change_from_the_last_commit() {
    let Some((root, harness)) = manuscript("repo-wip-diff") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    fs::write(root.join("原稿.md"), "二行目と三行目\n").unwrap();
    open(window, live);
    settle(window, live);
    select_row(window, 0);
    assert_eq!(window.get_git_repo_detail_mode(), 2);
    let rows = window.get_git_rows();
    let file = (0..rows.row_count())
        .position(|at| rows.row_data(at).unwrap().label == "原稿.md")
        .unwrap();
    changes_clicked(window, file);
    assert_eq!(window.get_git_repo_detail_mode(), 3);
    assert!(!window.get_git_repo_diff_switchable());
    assert_eq!(diff_text(window)[1].2, ["と三行目"]);
    diff_back(window);
    assert_eq!(window.get_git_repo_detail_mode(), 2);
}

#[test]
fn long_lines_are_folded_with_the_number_on_the_first_row() {
    let text = "あ".repeat(100);
    let unified = inline_diff::unified("", &format!("{text}\n"), 3);
    let rows = diff_rows(&unified);
    assert!(rows.len() >= 3, "100 wide characters take several rows");
    assert_eq!(rows[0].new, "1");
    assert!(rows[1..].iter().all(|row| row.new.is_empty()));
    let joined: String = rows
        .iter()
        .flat_map(|row| {
            (0..row.parts.row_count())
                .map(|k| row.parts.row_data(k).unwrap().text.to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(joined, text);
}

/// PR 2b：Undo／Redo。ドラッグで Merge したものを戻し、やり直す。外で動かしたら使えない。
#[test]
fn a_merge_is_undone_and_redone_from_the_toolbar() {
    let Some((root, harness)) = manuscript("repo-undo") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    assert!(!window.get_git_repo_can_undo());
    let before = git::head_sha(&root);
    branch_dropped(
        window,
        live,
        side_index(window, 1, "draft"),
        side_index(window, 1, "main"),
    );
    let Question::GitConfirm(pending) = take_question(live) else {
        panic!("expected the merge confirmation");
    };
    git_ui::confirmed(window, live, pending);
    settle(window, live);
    let merged = git::head_sha(&root);
    assert!(window.get_git_repo_can_undo());
    assert_eq!(window.get_git_repo_undo_tip(), "Undo Merge draft");
    history(window, live, false);
    settle(window, live);
    assert_eq!(git::head_sha(&root), before);
    assert!(!root.join("場面.md").exists());
    assert!(window.get_git_repo_can_redo());
    assert!(!window.get_git_repo_can_undo());
    history(window, live, true);
    settle(window, live);
    assert_eq!(git::head_sha(&root), merged);
    assert!(window.get_git_repo_can_undo());
    // 外で HEAD を動かすと、Undo は使えない（理由を Tip に出す）。
    git_ok(&root, &["reset", "-q", "--hard", "HEAD~1"]);
    refresh(window, live);
    settle(window, live);
    assert!(!window.get_git_repo_can_undo());
    assert!(window.get_git_repo_undo_tip().contains("Undo Merge draft"));
}

/// 画面の絵を書き出す（`cargo test -- --ignored git_repository_screens`）。一時フォルダの
/// `rfnedit-git-snapshot`に、Git Repository の画面をPPMで置く。
#[test]
#[ignore]
fn git_repository_screens() {
    let Some((root, harness)) = manuscript("repo-snapshot") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    git_ok(&root, &["switch", "-q", "draft"]);
    fs::write(root.join("第三章.md"), "三\n").unwrap();
    git::commit(&root, "第三章の下書き", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "main"]);
    git::merge(&root, "draft").unwrap();
    fs::write(
        root.join("原稿.md"),
        "雨は朝から降っていた。\n猫は窓辺で眠っている。風がカーテンを静かに揺らす。夜になると、遠くの家々に灯りがともり、通りを行く人の足音も少しずつ途絶えていった。\n",
    )
    .unwrap();
    git::commit(&root, "第一章を直す", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "draft"]);
    fs::write(root.join("第四章.md"), "四\n").unwrap();
    git::commit(&root, "第四章", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "main"]);
    fs::write(root.join("メモ.txt"), "メモ\n").unwrap();
    let out = std::env::temp_dir().join("rfnedit-git-snapshot");
    fs::create_dir_all(&out).unwrap();
    let (width, height) = (1400usize, 800usize);
    harness
        .surface
        .set_size(slint::PhysicalSize::new(width as u32, height as u32));
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
    wire(window, live);
    window.show().unwrap();
    open(window, live);
    settle(window, live);
    select_row(window, row_of(window, "第一章を直す"));
    draw("git-repository");
    show_diff(window, 0, false);
    // 右の列の幅が知らされて、段を折り直す（時計を回す）。
    draw("git-repository-diff");
    slint::platform::update_timers_and_animations();
    draw("git-repository-diff");
    diff_back(window);
    select_row(window, 0);
    draw("git-repository-wip");
    // main の先へ TestBranch で Commit した形（紫の幹は main のまま）。
    git_ok(&root, &["switch", "-q", "-c", "TestBranch"]);
    fs::write(root.join("試し.md"), "試し\n").unwrap();
    git::commit(&root, "試し", true, false).unwrap();
    refresh(window, live);
    settle(window, live);
    draw("git-repository-trunk");
}
