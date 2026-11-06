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
        let repo_done = REPO.with(|repo| {
            let repo = repo.borrow();
            repo.reading.is_none() && repo.lookup.is_none()
        });
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
fn repository_palette_keeps_the_original_registered_folder() {
    let Some((first, harness)) = manuscript("repo-palette-target") else {
        return;
    };
    let second = scratch_directory("repo-palette-other");
    repository(&second).unwrap();
    let (window, live) = (&harness.window, &harness.live);
    let runtime = live.folder.borrow().workspace.clone().unwrap();
    let workspace = runtime.borrow().active_workspace().unwrap();
    runtime
        .borrow_mut()
        .edit(|registry| registry.add_root(workspace, &second))
        .unwrap();
    git_ui::wire(window, live);
    wire(window, live);
    crate::panel_actions::wire(window, live);
    crate::command_palette::install(window, live);
    git_ui::publish(window, live);
    settle(window, live);
    assert!(git_ui::chosen_folder().is_some_and(|path| git_ui::same_path(&path, &first)));

    crate::command_palette::open(window, live, true);
    let index = window
        .get_palette_rows()
        .iter()
        .position(|row| row.title.contains("Git Repository…"))
        .unwrap();
    window.set_palette_selected(index as i32);
    window.invoke_git_repo_chosen(1);
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert!(git_ui::chosen_folder().is_some_and(|path| git_ui::same_path(&path, &second)));
    window.invoke_palette_accepted();
    slint::platform::update_timers_and_animations();
    assert!(!window.get_git_repo_active());

    window.invoke_surface_action("file.git-repository".into());
    slint::platform::update_timers_and_animations();
    assert!(window.get_git_repo_active());
    assert!(root().is_some_and(|path| git_ui::same_path(&path, &second)));
}

#[test]
fn git_toolbar_and_palette_share_merge_availability() {
    let Some((_root, harness)) = manuscript("repo-palette-availability") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    crate::panel_actions::wire(window, live);
    open(window, live);
    settle(window, live);
    window.set_git_has_upstream(true);
    for merging in [false, true] {
        window.set_git_merge_active(merging);
        crate::command_palette::open(window, live, true);
        for (id, expected) in [
            ("git.fetch", true),
            ("git.pull", !merging),
            ("git.push", !merging),
        ] {
            let at = crate::actions::index_of(id).unwrap();
            let title = &crate::actions::all()[at].title;
            let caption = pick(&title.0, &title.1);
            assert_eq!(
                window.invoke_git_action_allowed(id.into(), false, 0),
                expected,
                "{id}"
            );
            assert_eq!(
                window
                    .get_palette_rows()
                    .iter()
                    .any(|row| row.title.contains(caption)),
                expected,
                "{id}",
            );
        }
        crate::command_palette::close(window);
    }
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
    select_row(window, live, row_of(window, "二"));
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_mode(), 1);
    assert_eq!(window.get_git_repo_detail_message(), "二");
    let files = window.get_git_repo_detail_files();
    assert_eq!(files.row_count(), 1);
    assert_eq!(files.row_data(0).unwrap().label, "原稿.md");
    select_row(window, live, 0);
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_mode(), 2);

    let generation = window.get_git_repo_scroll_generation();
    branch_clicked(window, live, side_index(window, 1, "draft"));
    settle(window, live);
    assert_eq!(
        window.get_git_repo_selected() as usize,
        row_of(window, "場面")
    );
    assert_eq!(window.get_git_repo_detail_message(), "場面");
    assert_eq!(window.get_git_repo_scroll_generation(), generation + 1);

    // ファイルを押すと、右の列に1列の差分（PR 2b）。「Open in Comparison」で比較の画面がこの上に開く。
    select_row(window, live, row_of(window, "二"));
    settle(window, live);
    show_diff(window, live, 0, false);
    settle(window, live);
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
    capture_branch_menu(window, live, side_index(window, 1, "draft"));
    branch_menu(window, live, 4);
    slint::platform::update_timers_and_animations();
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
    select_row(window, live, row_of(window, "二"));
    settle(window, live);
    show_diff(window, live, 0, false);
    settle(window, live);
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
    diff_working_chosen(window, live, true);
    settle(window, live);
    assert!(window.get_git_repo_diff_working());
    let lines = diff_text(window);
    assert_eq!(lines[1].2, ["を直した"]);
    // Reflow and the full comparison use the displayed snapshot even if the
    // working file changes while the split handle is dragged.
    fs::write(root.join("原稿.md"), "later external edit\n").unwrap();
    diff_width_changed(window, 400.0);
    assert_eq!(diff_text(window), lines);
    diff_open_full(window);
    assert!(window.get_diff_active());
    crate::diff_view::dismiss(window);
    // ‹ Files で詳細へ戻る。
    diff_back(window);
    assert_eq!(window.get_git_repo_detail_mode(), 1);
}

#[test]
fn changing_commit_selection_discards_a_delayed_detail_result() {
    let Some((_root, harness)) = manuscript("repo-delayed-detail") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    open(window, live);
    settle(window, live);
    let old = row_of(window, "最初");
    let target = REPO.with(|repo| repo.borrow().rows[old].clone().unwrap());
    select_detail(
        window,
        Selected::Commit(target.clone()),
        None,
        old as i32,
        false,
    );
    let (release, delayed) = mpsc::channel();
    let (sent, done) = mpsc::channel();
    start_lookup(window, live, "waiting", move |root| {
        delayed.recv().unwrap();
        let result = git::detail(root, &target).map(Lookup::SelectedCommit);
        sent.send(()).unwrap();
        result
    });
    select_row(window, live, row_of(window, "二"));
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_message(), "二");
}

#[test]
fn both_git_changes_surfaces_compare_the_selected_section() {
    let Some((root, harness)) = manuscript("repo-wip-diff") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    let path = "空白 を含む原稿.md";
    fs::write(root.join(path), "base\n").unwrap();
    git::commit(&root, "base", true, false).unwrap();
    fs::write(root.join(path), "staged\n").unwrap();
    git::stage(&root, &[path.to_owned()]).unwrap();
    fs::write(root.join(path), "working\n").unwrap();
    git_ui::wire(window, live);
    wire(window, live);
    window.set_tree_open(true);
    window.set_left_tab(8);
    git_ui::publish(window, live);
    settle(window, live);
    let rows = window.get_git_rows();
    let row_for = |kind| {
        (0..rows.row_count())
            .find(|&at| {
                let row = rows.row_data(at).unwrap();
                row.label == path && row.kind == kind
            })
            .unwrap() as i32
    };
    window.invoke_git_row_diff(row_for(3));
    settle(window, live);
    assert!(window.get_diff_active());
    assert!(window.get_diff_left_path().contains("HEAD"));
    assert!(window.get_diff_right_path().contains(path));
    let compared = window.get_diff_rows().row_data(0).unwrap();
    assert_eq!(
        (compared.left.as_str(), compared.right.as_str()),
        ("base ［改行］", "staged ［改行］")
    );
    crate::diff_view::dismiss(window);

    open(window, live);
    settle(window, live);
    select_row(window, live, 0);
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_mode(), 2);
    window.invoke_git_repo_changes_clicked(row_for(3));
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_mode(), 3);
    assert!(!window.get_git_repo_diff_switchable());
    assert!(window.get_git_repo_diff_caption().contains("HEAD"));
    assert_eq!(diff_text(window)[1].1, "staged");
    diff_back(window);

    window.invoke_git_repo_changes_clicked(row_for(4));
    settle(window, live);
    assert!(!window.get_git_repo_diff_caption().contains("HEAD"));
    let lines = diff_text(window);
    assert_eq!((&*lines[0].1, &*lines[1].1), ("staged", "working"));
    fs::write(root.join(path), "later\n").unwrap();
    // The two-pane comparison uses the snapshot already displayed in the inline diff.
    diff_open_full(window);
    let compared = window.get_diff_rows().row_data(0).unwrap();
    assert_eq!(
        (compared.left.as_str(), compared.right.as_str()),
        ("staged ［改行］", "working ［改行］")
    );
    crate::diff_view::dismiss(window);
    diff_back(window);
    assert_eq!(window.get_git_repo_detail_mode(), 2);
}

#[test]
fn the_wip_block_button_stages_and_unstages_the_selected_hunk() {
    let Some((root, harness)) = manuscript("repo-hunk-buttons") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    let path = "原稿.md";
    let base = format!("first base\n{}last base\n", "context\n".repeat(16));
    fs::write(root.join(path), &base).unwrap();
    git::commit(&root, "base for blocks", true, false).unwrap();
    let working = base
        .replace("first base", "first working")
        .replace("last base", "last working");
    fs::write(root.join(path), &working).unwrap();
    git_ui::wire(window, live);
    wire(window, live);
    crate::panel_actions::wire(window, live);
    crate::command_palette::install(window, live);
    open(window, live);
    settle(window, live);
    select_row(window, live, 0);
    settle(window, live);

    let show = |kind| {
        let rows = window.get_git_rows();
        let row = (0..rows.row_count())
            .find(|&at| rows.row_data(at).unwrap().kind == kind)
            .unwrap() as i32;
        window.invoke_git_repo_changes_clicked(row);
        settle(window, live);
        assert!(window.get_git_repo_hunks_available());
        window.invoke_surface_action("git.review-blocks".into());
        slint::platform::update_timers_and_animations();
        assert!(window.get_git_repo_hunks_active());
    };
    show(4);
    assert_eq!(window.get_git_repo_hunks().row_count(), 2);
    assert!(window.invoke_git_action_allowed("git.stage-block".into(), false, 0));
    window.invoke_git_repo_select_hunk(1);
    crate::command_palette::open(window, live, true);
    let title = pick("ブロックをStage", "Stage Block");
    let at = window
        .get_palette_rows()
        .iter()
        .position(|row| row.title.contains(title))
        .unwrap();
    window.set_palette_selected(at as i32);
    window.invoke_git_repo_select_hunk(0);
    window.invoke_palette_accepted();
    slint::platform::update_timers_and_animations();
    assert!(!window.get_git_busy());
    assert_eq!(git::change_blob(&root, "", path).unwrap(), base.as_bytes());
    window.invoke_git_repo_select_hunk(1);
    window.invoke_surface_action("git.stage-block".into());
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_mode(), 2);
    assert_eq!(
        git::change_blob(&root, "", path).unwrap(),
        base.replace("last base", "last working").into_bytes(),
    );

    show(3);
    assert!(window.get_git_repo_hunks_staged());
    assert_eq!(window.get_git_repo_hunks().row_count(), 1);
    crate::command_palette::open(window, live, true);
    let rows = window.get_palette_rows();
    let title = pick("ブロックをUnstage", "Unstage Block");
    let at = rows
        .iter()
        .position(|row| row.title.contains(title))
        .unwrap();
    window.set_palette_selected(at as i32);
    window.invoke_palette_accepted();
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert_eq!(
        git::change_blob(&root, "", path).unwrap(),
        base.into_bytes()
    );
    assert_eq!(fs::read_to_string(root.join(path)).unwrap(), working);
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

#[test]
fn history_search_opens_a_commit_outside_the_graph_page() {
    let root = scratch_directory("repo-history-search");
    let Some(()) = repository(&root) else { return };
    git_ok(&root, &["config", "user.name", "Writer [QA]"]);
    let path = "資料 [前].md";
    let (harness, _) = Harness::new(|weak| open_under(&root, path, "旧版\n", weak));
    git_ok(&root, &["add", "--", path]);
    git_ok(
        &root,
        &["commit", "-qm", "古いコミット", "-m", "detail [needle]"],
    );
    let sha = git::head_sha(&root).unwrap();
    git_ok(&root, &["config", "user.name", "Other"]);
    for _ in 0..=PAGE {
        git_ok(&root, &["commit", "-q", "--allow-empty", "-m", "later"]);
    }
    for (field, query) in [
        (HistoryField::Author, "[qa]"),
        (HistoryField::Path, path),
        (HistoryField::Sha, &sha[..8].to_ascii_uppercase()),
    ] {
        let found = git::search_history(&root, field, query, PAGE).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|item| item.sha.as_str())
                .collect::<Vec<_>>(),
            [sha.as_str()]
        );
    }
    let (window, live) = (&harness.window, &harness.live);
    attach_workspace(live, &root, SaveMode::Recovery);
    wire(window, live);
    crate::panel_actions::wire(window, live);
    open(window, live);
    settle(window, live);
    assert!(
        !messages(window)
            .iter()
            .any(|message| message == "古いコミット")
    );
    window.invoke_surface_action("git.open-history-search".into());
    slint::platform::update_timers_and_animations();
    window.invoke_git_repo_search(0, "[needle]".into());
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert_eq!(window.get_git_repo_search_rows().row_count(), 1);
    window.invoke_git_repo_search_selected(0);
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert_eq!(window.get_git_repo_detail_sha(), sha.as_str());
    assert_eq!(
        window
            .get_git_repo_detail_files()
            .row_data(0)
            .unwrap()
            .label,
        path
    );
    show_diff(window, live, 0, false);
    settle(window, live);
    assert!(diff_text(window).iter().any(|(_, text, _)| text == "旧版"));
}

#[test]
fn comparison_pins_branch_tips_and_reads_the_two_committed_versions() {
    let Some((root, harness)) = manuscript("repo-two-points") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    crate::panel_actions::wire(window, live);
    let old = "原稿 前.md";
    let new = "原稿 後.md";
    let shared = "same line\n".repeat(16);
    fs::write(root.join(old), &shared).unwrap();
    git::commit(&root, "common file", true, false).unwrap();
    git_ok(&root, &["switch", "-q", "-c", "comparison-left"]);
    fs::write(root.join(old), format!("{shared}left only\n")).unwrap();
    git::commit(&root, "left only", true, false).unwrap();
    let left = git::head_sha(&root).unwrap();

    git_ok(&root, &["switch", "-q", "main"]);
    git_ok(&root, &["mv", "--", old, new]);
    fs::write(root.join(new), format!("{shared}right only\n")).unwrap();
    git::commit(&root, "renamed on main", true, false).unwrap();
    let right = git::head_sha(&root).unwrap();
    fs::write(root.join(new), "uncommitted\n").unwrap();

    open(window, live);
    settle(window, live);
    let opened_row = side_index(window, 1, "comparison-left");
    capture_branch_menu(window, live, opened_row);
    git_ok(&root, &["branch", "-f", "comparison-left", "main"]);
    git_ok(&root, &["branch", "aaa-new-row"]);
    refresh(window, live);
    settle(window, live);
    assert_ne!(opened_row, side_index(window, 1, "comparison-left"));
    window.invoke_git_repo_branch_action("git.set-compare-base".into());
    slint::platform::update_timers_and_animations();
    capture_branch_menu(window, live, side_index(window, 1, "main"));
    window.invoke_git_repo_branch_action("git.compare-with-base".into());
    slint::platform::update_timers_and_animations();
    settle(window, live);
    assert!(window.get_git_repo_range_left().ends_with(&left));
    assert!(window.get_git_repo_range_right().ends_with(&right));
    let rows = window.get_git_repo_detail_files();
    assert_eq!(rows.row_count(), 1);
    let row = rows.row_data(0).unwrap();
    assert_eq!(row.letter, "R");
    assert_eq!(row.tip, format!("{old} → {new}").as_str());
    open_file_diff(window, live, 0);
    settle(window, live);
    let lines = diff_text(window);
    assert!(
        lines
            .iter()
            .any(|(kind, text, _)| *kind == 1 && text == "left only")
    );
    assert!(
        lines
            .iter()
            .any(|(kind, text, _)| *kind == 2 && text == "right only")
    );
    diff_open_full(window);
    assert!(window.get_diff_left_path().ends_with(old));
    assert!(window.get_diff_right_path().ends_with(new));
    assert_eq!(fs::read_to_string(root.join(new)).unwrap(), "uncommitted\n");
}

#[test]
fn worktree_creation_and_opening_share_actions_and_isolate_unsaved_documents() {
    let parent = scratch_directory("repo-worktree-actions");
    let root = parent.join("source");
    fs::create_dir_all(&root).unwrap();
    if repository(&root).is_none() {
        return;
    }
    let (harness, document) = Harness::new(|weak| open_under(&root, "note.md", "base\n", weak));
    git::commit(&root, "base", true, false).unwrap();
    let head = git::head_sha(&root).unwrap();
    let (window, live) = (&harness.window, &harness.live);
    attach_workspace(live, &root, SaveMode::Recovery);
    window.set_workspace_active(true);
    git_ui::wire(window, live);
    wire(window, live);
    crate::panel_actions::wire(window, live);
    open(window, live);
    settle(window, live);
    let action = |id: &str| {
        window.invoke_surface_action(id.into());
        slint::platform::update_timers_and_animations();
        settle(window, live);
    };
    action("git.worktrees");
    window.set_git_worktree_parent(parent.to_string_lossy().as_ref().into());
    window.set_git_worktree_folder("Linked notes".into());
    window.set_git_worktree_branch("draft".into());
    action("git.worktree-create");
    assert_eq!(window.get_git_worktree_rows().row_count(), 2);
    assert_eq!(git_ui::workspace_folders(live).len(), 1);

    crate::saving::edit(&document, "unsaved in source\n");
    window.set_git_message("draft message for source".into());
    window.set_git_worktree_selected(1);
    action("git.worktree-open");
    assert_eq!(git_ui::workspace_folders(live).len(), 1);
    let Question::GitOpenWorktree(request) = take_question(live) else {
        panic!("expected the registration confirmation");
    };
    window.set_question_open(false);
    crate::git_worktree_ui::open_worktree(window, live, request);
    settle(window, live);
    let linked = parent.join("Linked notes");
    assert!(git_ui::active_root().is_some_and(|root| git_ui::same_path(&root, &linked)));
    assert_eq!(git_ui::workspace_folders(live).len(), 2);
    assert!(document.text.edited());
    assert!(window.get_git_message().is_empty());

    fs::write(linked.join("note.md"), "linked edit\n").unwrap();
    git_ui::refresh_soon(window, live);
    settle(window, live);
    window.set_git_message("commit in linked worktree".into());
    action("git.commit-changes");
    assert!(live.pending.borrow().is_none());
    assert_eq!(
        git::last_message(&linked).unwrap(),
        "commit in linked worktree"
    );
    assert_eq!(git::head_sha(&root).unwrap(), head);
    assert!(document.text.edited());

    action("git.worktrees");
    window.set_git_worktree_selected(0);
    action("git.worktree-open");
    assert!(git_ui::active_root().is_some_and(|active| git_ui::same_path(&active, &root)));
    assert_eq!(window.get_git_message(), "draft message for source");
    assert!(live.pending.borrow().is_none());
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

/// 書き手の求め 2026-10-03：Git Repository で「戻る」と、画面を閉じる。マウスの戻る釦は、
/// 行の上でも何も無い所でも効く。ショートカットの「戻る」（既定 Alt+←）も同じ。
#[test]
fn back_closes_the_repository_screen() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let Some((_root, harness)) = manuscript("repo-back") else {
        return;
    };
    let (window, live) = (&harness.window, &harness.live);
    wire(window, live);
    crate::shortcuts::wire(window, live);
    let (width, height) = (1400usize, 800usize);
    harness
        .surface
        .set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.show().unwrap();
    let draw = || {
        let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
        window.window().request_redraw();
        harness.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
    };
    let back = |x: f32, y: f32| {
        let position = slint::LogicalPosition::new(x, y);
        for event in [
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Back,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Back,
            },
        ] {
            window.window().dispatch_event(event);
        }
        slint::platform::update_timers_and_animations();
    };
    // 行の上（グラフの1行目）、何も無い所（グラフの下・左の列の下・右の列）。
    for (x, y) in [
        (600.0, 95.0),
        (600.0, 600.0),
        (120.0, 600.0),
        (1200.0, 600.0),
    ] {
        open(window, live);
        settle(window, live);
        draw();
        assert!(window.get_git_repo_active());
        back(x, y);
        assert!(!window.get_git_repo_active(), "back at ({x}, {y})");
    }
    // ショートカットの「戻る」。
    open(window, live);
    settle(window, live);
    draw();
    let left: slint::SharedString = slint::platform::Key::LeftArrow.into();
    assert!(window.invoke_shortcut_key(left, false, true, false));
    slint::platform::update_timers_and_animations();
    assert!(!window.get_git_repo_active());
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
    select_row(window, live, row_of(window, "第一章を直す"));
    settle(window, live);
    draw("git-repository");
    show_diff(window, live, 0, false);
    settle(window, live);
    // 右の列の幅が知らされて、段を折り直す（時計を回す）。
    draw("git-repository-diff");
    slint::platform::update_timers_and_animations();
    draw("git-repository-diff");
    diff_back(window);
    select_row(window, live, 0);
    settle(window, live);
    draw("git-repository-wip");
    // main の先へ TestBranch で Commit した形（紫の幹は main のまま）。
    git_ok(&root, &["switch", "-q", "-c", "TestBranch"]);
    fs::write(root.join("試し.md"), "試し\n").unwrap();
    git::commit(&root, "試し", true, false).unwrap();
    refresh(window, live);
    settle(window, live);
    draw("git-repository-trunk");
}
