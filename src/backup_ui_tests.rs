//! RFN01-61: Backup History・Delete Backups…・保存先の変更を、画面の値で確かめる。
use super::*;
use crate::saving::{Harness, attach_workspace, open_under, scratch_directory};
use crate::workspace::SaveMode;
use slint::Model;
use std::fs;

fn at(second: u16) -> timestamp::LocalTime {
    timestamp::LocalTime {
        year: 2026,
        month: 9,
        day: 27,
        hour: 14,
        minute: 30,
        second,
        millis: 0,
    }
}

/// `path`に古い中身を順に書いてバックアップを取り、最後に`now`へ戻す。
fn with_backups(harness: &Harness, path: &Path, old: &[&str], now: &str) {
    let root = backup_root(&harness.window).unwrap();
    for (second, text) in (1..).zip(old) {
        fs::write(path, text).unwrap();
        backup::take(&root, path, 5, at(second)).unwrap();
    }
    fs::write(path, now).unwrap();
}

fn labels(window: &AppWindow) -> Vec<String> {
    let rows = window.get_backup_history_rows();
    (0..rows.row_count())
        .map(|at| rows.row_data(at).unwrap().label.to_string())
        .collect()
}

/// 最初の差分で右（バックアップ）を採り、本文へ反映する。
fn take_first_difference(window: &AppWindow) {
    let rows = window.get_diff_rows();
    let first = (0..rows.row_count())
        .find(|at| rows.row_data(*at).unwrap().hunk_start)
        .expect("a difference");
    window.set_diff_selected_row(first as i32);
    window.invoke_diff_choose_side(true);
    window.invoke_diff_apply_merge();
}

#[test]
fn history_lists_newest_first_and_applies_with_one_undo() {
    let root = scratch_directory("backup-history");
    let (harness, document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let path = root.join("draft.md");
    let window = &harness.window;
    let live = &harness.live;

    // 無いうちは、メニューの行は淡い。
    publish_menu(window, live, PaneId::from_index(0));
    assert!(!window.get_pane_menu_has_backups());

    with_backups(&harness, &path, &["古い一\n", "古い二\n"], "現在\n");
    publish_menu(window, live, PaneId::from_index(0));
    assert!(window.get_pane_menu_has_backups());

    open_history(window, live);
    assert!(window.get_backup_history_active());
    assert!(window.get_diff_active());
    assert!(window.get_diff_merge_enabled());
    assert_eq!(
        labels(window),
        vec!["2026-09-27 14:30:02", "2026-09-27 14:30:01"]
    );
    // 開いた直後は最新を比べ、どれもチェックしていない。
    assert_eq!(window.get_backup_history_selected(), 0);
    assert!(!window.get_backup_history_any());
    assert!(window.get_diff_right_path().contains("14:30:02"));
    let first = window.get_backup_history_rows().row_data(0).unwrap();
    // 添え字はステータスバーと同じ数え方の本文文字数。
    let counted = crate::thousands(body_characters(window, "古い二\n"));
    assert_eq!(first.detail, say!("{counted}字", "{counted} chars"));

    // 行を押すと比べる相手が替わる。
    history_chosen(window, live, 1);
    assert_eq!(window.get_backup_history_selected(), 1);
    assert!(window.get_diff_right_path().contains("14:30:01"));

    take_first_difference(window);
    assert_eq!(document.text.borrow().as_str(), "古い一\n");
    crate::undo_in_pane(
        window,
        PaneId::from_index(0),
        &document,
        &live.states,
        &live.cache,
        false,
    );
    assert_eq!(document.text.borrow().as_str(), "現在\n");
}

#[test]
fn deleting_in_history_reselects_or_closes() {
    let root = scratch_directory("backup-history-delete");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let path = root.join("draft.md");
    let window = &harness.window;
    let live = &harness.live;
    with_backups(&harness, &path, &["一\n", "二\n", "三\n"], "現在\n");
    open_history(window, live);

    // 「All」で全部、もう一度で全部外れる。
    history_all_toggled(window);
    assert!(window.get_backup_history_all());
    history_all_toggled(window);
    assert!(!window.get_backup_history_any());

    // 比べていない古いものを消す：比べている相手はそのまま。
    history_toggled(window, 2);
    assert!(window.get_backup_history_any() && !window.get_backup_history_all());
    history_delete(window, live);
    assert!(window.get_question_open());
    let oldest = backup::list(&backup_root(window).unwrap(), &path)[2]
        .path
        .clone();
    delete_confirmed(window, live, &[oldest]);
    assert_eq!(labels(window).len(), 2);
    assert_eq!(window.get_backup_history_selected(), 0);
    assert!(window.get_diff_right_path().contains("14:30:03"));

    // 比べている最新を消すと、残ったうちの最新を比べ直す。
    let newest = backup::list(&backup_root(window).unwrap(), &path)[0]
        .path
        .clone();
    delete_confirmed(window, live, &[newest]);
    assert_eq!(labels(window), vec!["2026-09-27 14:30:02"]);
    assert!(window.get_diff_right_path().contains("14:30:02"));

    // 全部消えたら画面を閉じる。
    let last = backup::list(&backup_root(window).unwrap(), &path)[0]
        .path
        .clone();
    delete_confirmed(window, live, &[last]);
    assert!(!window.get_diff_active());
    assert!(!window.get_backup_history_active());
}

#[test]
fn delete_backups_lists_folders_and_deletes_the_chosen_ones() {
    let root = scratch_directory("backup-groups");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    let loose = scratch_directory("backup-groups-loose");
    with_backups(&harness, &root.join("draft.md"), &["一\n"], "現在\n");
    with_backups(&harness, &loose.join("外.md"), &["外\n"], "外の今\n");

    open_groups(window, live);
    assert!(window.get_backup_delete_open());
    let rows = window.get_backup_groups();
    assert_eq!(rows.row_count(), 2);
    assert!(!window.get_backup_groups_any());
    let folders: Vec<String> = (0..rows.row_count())
        .map(|at| rows.row_data(at).unwrap().label.to_string())
        .collect();
    let at_loose = folders
        .iter()
        .position(|f| Path::new(f) == loose)
        .expect("the loose folder is listed by its own path");

    group_toggled(window, at_loose);
    assert!(window.get_backup_groups_any());
    groups_delete(window, live);
    assert!(window.get_question_open());
    let files = GROUPS.with(|held| held.borrow()[at_loose].0.files.clone());
    delete_confirmed(window, live, &files);
    assert!(!window.get_backup_delete_open());
    let store = backup_root(window).unwrap();
    assert!(backup::list(&store, &loose.join("外.md")).is_empty());
    assert_eq!(backup::list(&store, &root.join("draft.md")).len(), 1);
}

#[test]
fn changing_the_folder_moves_everything_or_nothing() {
    let root = scratch_directory("backup-move");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    let path = root.join("draft.md");
    with_backups(&harness, &path, &["一\n"], "現在\n");
    let before = backup_root(window).unwrap();

    // 移し先に同じ名前があれば、保存先も中身も変えずに言う。
    let target = scratch_directory("backup-move-target");
    let clash = backup::list(&before, &path)[0]
        .path
        .strip_prefix(&before)
        .map(|relative| target.join(relative))
        .unwrap();
    fs::create_dir_all(clash.parent().unwrap()).unwrap();
    fs::write(&clash, "先客").unwrap();
    change_folder(window, live, target.display().to_string());
    assert!(window.get_question_open());
    assert_eq!(window.get_backup_folder(), "");
    assert_eq!(backup::list(&before, &path).len(), 1);
    assert_eq!(fs::read_to_string(&clash).unwrap(), "先客");
    fs::remove_file(&clash).unwrap();

    change_folder(window, live, target.display().to_string());
    assert_eq!(window.get_backup_folder(), target.display().to_string());
    assert!(backup::list(&before, &path).is_empty());
    assert_eq!(backup::list(&target, &path).len(), 1);

    // 「Default」で戻せば、また全部が戻る。
    change_folder(window, live, String::new());
    assert_eq!(window.get_backup_folder(), "");
    assert_eq!(backup::list(&before, &path).len(), 1);
}

#[test]
fn backups_follow_a_rename_made_in_the_editor() {
    let root = scratch_directory("backup-rename");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let from = root.join("draft.md");
    with_backups(&harness, &from, &["一\n"], "現在\n");
    let to = root.join("序章.md");
    crate::move_entry(window, &harness.live, &from, &to).unwrap();
    let store = backup_root(window).unwrap();
    assert!(backup::list(&store, &from).is_empty());
    assert_eq!(backup::list(&store, &to).len(), 1);
}

#[test]
fn the_manager_and_the_status_bar_say_which_folders_back_up() {
    let root = scratch_directory("backup-status");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;

    crate::observe_folder_autosave(window, live);
    assert_eq!(
        window.get_document_save_mode(),
        pick("自動バックアップ", "Auto Backup")
    );

    // 開いた直後は使用中のWorkspaceだけが開き、ONのものが文字で出る。
    crate::workspace_manager_requested(window, live);
    let row = window.get_workspace_rows().row_data(0).unwrap();
    assert!(row.expanded);
    let line = row.folders.row_data(0).unwrap();
    assert_eq!(line.mode, "AutoBackup");
    assert_eq!(Path::new(line.path.as_str()), root);

    // その行の右クリックで自動保存に替えれば、自動バックアップは外れる。
    crate::workspace_status_mode_toggled(window, live, line.line as usize, false);
    let row = window.get_workspace_rows().row_data(0).unwrap();
    assert_eq!(row.folders.row_data(0).unwrap().mode, "AutoSave");
    crate::workspace_status_mode_toggled(window, live, 0, false);
    let row = window.get_workspace_rows().row_data(0).unwrap();
    assert_eq!(row.folders.row_data(0).unwrap().mode, "");
    crate::observe_folder_autosave(window, live);
    assert_eq!(window.get_document_save_mode(), pick("退避", "Recovery"));

    // 閉じれば行は出ない。
    crate::workspace_row_expand_toggled(window, live, 0);
    let row = window.get_workspace_rows().row_data(0).unwrap();
    assert!(!row.expanded);
    assert_eq!(row.folders.row_count(), 0);
}

#[test]
fn importing_settings_keeps_this_machines_backup_folder() {
    let root = scratch_directory("backup-import");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    let window = &harness.window;
    let here = root.join("ここのバックアップ");
    window.set_backup_folder(here.display().to_string().into());
    let export = crate::settings_transfer::Export {
        settings: vec![
            ("backup.folder".into(), r"Z:\よその機械".into()),
            ("backup.keep".into(), "12".into()),
        ],
        ..Default::default()
    };
    crate::settings_transfer::import(window, &harness.live, export).unwrap();
    assert_eq!(window.get_backup_folder(), here.display().to_string());
    assert_eq!(window.get_backup_keep(), 12);
}

/// 画面の絵を書き出す（`cargo test -- --ignored backup_screens`）。一時フォルダの
/// `rfnedit-backup-snapshot`に、Backup History と Delete Backups… をPPMで置く。
#[test]
#[ignore]
fn backup_screens() {
    let root = scratch_directory("backup-snapshot");
    let (harness, _document) = Harness::new(|weak| {
        open_under(
            &root,
            "第一章.md",
            "# 第一章\n猫が眠っている。\n風が木々を揺らす。\n",
            weak,
        )
    });
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    let path = root.join("第一章.md");
    with_backups(
        &harness,
        &path,
        &[
            "# 第一章\n猫が眠る。\n",
            "# 第一章\n犬が眠っている。\n風が吹く。\n",
            "# 第一章\n猫が眠っている。\n風が吹く。\n夜になった。\n",
        ],
        "# 第一章\n猫が眠っている。\n風が木々を揺らす。\n",
    );
    let out = std::env::temp_dir().join("rfnedit-backup-snapshot");
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
    open_history(window, live);
    history_toggled(window, 2);
    draw("history");
    diff_view::dismiss(window);
    open_groups(window, live);
    group_toggled(window, 0);
    draw("delete-backups");
    close_groups(window);
    crate::workspace_manager_requested(window, live);
    window.set_workspace_manager_open(true);
    draw("workspace-manager");
    window.set_workspace_manager_open(false);
    crate::open_settings(window, live);
    window.set_settings_tab(3);
    // 設定の検索（設定のTABで開いた検索欄の語）で、FILES の2つの欄だけを出す。
    window.on_settings_match(|query, labels| crate::settings_match(&query, labels.iter()));
    window.set_find_open(true);
    window.set_find_pane(0);
    PaneId::from_index(0).update_screen(window, |screen| screen.find_needle = "Backup".into());
    slint::platform::update_timers_and_animations();
    // 検索中も語を持たない群は出たままなので、縦に長い面で描いて全部を写す。
    let (width, tall) = (1600usize, 3000usize);
    harness
        .surface
        .set_size(slint::PhysicalSize::new(width as u32, tall as u32));
    PaneId::from_index(0).update_screen(window, |screen| {
        screen.width = width as f32 - 360.0;
        screen.height = tall as f32 - 120.0;
    });
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * tall];
    window.window().request_redraw();
    harness.surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let mut ppm = format!("P6\n{width} {tall}\n255\n").into_bytes();
    for pixel in &pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    fs::write(out.join("settings-files.ppm"), ppm).unwrap();
}
