//! RFN01-61: Backup History・Delete Backups…・保存先の変更を、画面の値で確かめる。
use super::*;
use crate::saving::{Harness, attach_workspace, edit, open_under, scratch_directory};
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
    let store = backup_store(&harness.window).unwrap();
    for (second, text) in (1..).zip(old) {
        fs::write(path, text).unwrap();
        store.take(path, 5, at(second)).unwrap();
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

    // 無いうちは、File の「Backup History…」は淡い。
    assert!(!has_backups(window, &document));

    with_backups(&harness, &path, &["古い一\n", "古い二\n"], "現在\n");
    assert!(has_backups(window, &document));

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
    // 2段目はバックアップのファイル名（書き手の求め 2026-09-27）。
    assert_eq!(first.name, "draft.2026-09-27_143002.md");
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
    let oldest = backup_store(window).unwrap().list(&path)[2].path.clone();
    delete_confirmed(window, live, &[oldest]);
    assert_eq!(labels(window).len(), 2);
    assert_eq!(window.get_backup_history_selected(), 0);
    assert!(window.get_diff_right_path().contains("14:30:03"));

    // 比べている最新を消すと、残ったうちの最新を比べ直す。
    let newest = backup_store(window).unwrap().list(&path)[0].path.clone();
    delete_confirmed(window, live, &[newest]);
    assert_eq!(labels(window), vec!["2026-09-27 14:30:02"]);
    assert!(window.get_diff_right_path().contains("14:30:02"));

    // 全部消えたら画面を閉じる。
    let last = backup_store(window).unwrap().list(&path)[0].path.clone();
    delete_confirmed(window, live, &[last]);
    assert!(!window.get_diff_active());
    assert!(!window.get_backup_history_active());
}

fn pane_labels(window: &AppWindow) -> Vec<(i32, String, String, String)> {
    let rows = window.get_backup_pane_rows();
    (0..rows.row_count())
        .map(|at| {
            let row = rows.row_data(at).unwrap();
            let on = if row.can_toggle {
                if row.backup_on { "ON" } else { "off" }
            } else {
                "-"
            };
            (
                row.kind,
                row.label.to_string(),
                row.mode.to_string(),
                format!("{on} {}", row.count),
            )
        })
        .collect()
}

#[test]
fn the_backups_pane_lists_folders_files_and_switches_them() {
    let root = scratch_directory("backup-pane");
    let (harness, document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::Recovery);
    let window = &harness.window;
    let live = &harness.live;
    let folder = root.file_name().unwrap().to_string_lossy().into_owned();
    window.set_tree_open(true);
    window.set_left_tab(7);

    // 登録フォルダは OFF でも並び、バックアップはまだ無い。
    publish_pane(window, live);
    let rows = pane_labels(window);
    assert_eq!(
        rows[0],
        (0, folder.clone(), String::new(), "off ".to_owned())
    );
    assert_eq!(rows[1].0, 2);

    // この面の釦で ON にする（自動保存とは排他）。
    pane_toggled(window, live, 0);
    assert_eq!(pane_labels(window)[0].3, "ON ");
    assert!(crate::saving::backs_up(live, &root.join("draft.md")));

    // 保存すればファイルの行が出る（面が見えていれば組み直す）。
    edit(&document, "二");
    let form = document.file.borrow().form();
    assert!(crate::saving::write_document_in(
        window,
        live,
        &document,
        root.join("draft.md"),
        form
    ));
    let rows = pane_labels(window);
    assert_eq!(
        rows[1],
        (1, "draft.md".to_owned(), String::new(), "- 1".to_owned())
    );

    // 登録を外したフォルダのバックアップも、元のフォルダの名前で並ぶ（切り替えの釦は無い）。
    let loose = scratch_directory("backup-pane-loose");
    with_backups(&harness, &loose.join("外.md"), &["外\n"], "外の今\n");
    publish_pane(window, live);
    let rows = pane_labels(window);
    let loose_name = loose.file_name().unwrap().to_string_lossy().into_owned();
    let at = rows.iter().position(|r| r.1 == loose_name).expect("listed");
    assert_eq!(rows[at].2, pick("未登録", "Not registered"));
    assert_eq!(rows[at].3, "- ");

    // フォルダを押せば閉じ、もう一度で開く。
    pane_clicked(window, live, 0);
    assert_eq!(pane_labels(window)[1].0, 0);
    pane_clicked(window, live, 0);
    assert_eq!(pane_labels(window)[1].0, 1);

    // ファイルの行を押せば Backup History。
    pane_clicked(window, live, 1);
    assert!(window.get_backup_history_active());
    diff_view::dismiss(window);

    // 右クリックの Delete Backups…：訊いてから消す。
    pane_menu(window, live, at, 1);
    assert!(window.get_question_open());
    let store = backup_store(window).unwrap();
    let files: Vec<_> = store
        .list(&loose.join("外.md"))
        .into_iter()
        .map(|b| b.path)
        .collect();
    delete_confirmed(window, live, &files);
    assert!(pane_labels(window).iter().all(|r| r.1 != loose_name));
}

#[test]
fn the_file_name_format_renames_existing_backups() {
    let root = scratch_directory("backup-format");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    let path = root.join("draft.md");
    with_backups(&harness, &path, &["一\n"], "現在\n");
    publish_settings(window);
    assert!(window.get_backup_name_example().contains("第一章."));

    // 使えない書式は、打っているあいだに理由を言い、確定しても何も変えない。
    name_edited(window, "{name}.yyyyMMdd{ext}");
    assert!(!window.get_backup_name_problem().is_empty());
    name_accepted(window, live, "{name}.yyyyMMdd{ext}");
    assert_eq!(window.get_backup_name_format(), backup::DEFAULT_NAME_FORMAT);

    let format = "yyyyMMdd_HHmmss_{name}{ext}.bak";
    name_edited(window, format);
    assert!(window.get_backup_name_problem().is_empty());
    assert!(window.get_backup_name_example().contains("_第一章.md.bak"));
    name_accepted(window, live, format);
    assert_eq!(window.get_backup_name_format(), format);
    let kept = backup_store(window).unwrap().list(&path);
    assert_eq!(kept.len(), 1);
    assert!(
        kept[0]
            .path
            .to_string_lossy()
            .ends_with("20260927_143001_draft.md.bak")
    );
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
    let clash = backup::Store::new(&before, backup::DEFAULT_NAME_FORMAT).list(&path)[0]
        .path
        .strip_prefix(&before)
        .map(|relative| target.join(relative))
        .unwrap();
    fs::create_dir_all(clash.parent().unwrap()).unwrap();
    fs::write(&clash, "先客").unwrap();
    change_folder(window, live, target.display().to_string());
    assert!(window.get_question_open());
    assert_eq!(window.get_backup_folder(), "");
    assert_eq!(
        backup::Store::new(&before, backup::DEFAULT_NAME_FORMAT)
            .list(&path)
            .len(),
        1
    );
    assert_eq!(fs::read_to_string(&clash).unwrap(), "先客");
    fs::remove_file(&clash).unwrap();

    change_folder(window, live, target.display().to_string());
    assert_eq!(window.get_backup_folder(), target.display().to_string());
    assert!(
        backup::Store::new(&before, backup::DEFAULT_NAME_FORMAT)
            .list(&path)
            .is_empty()
    );
    assert_eq!(
        backup::Store::new(&target, backup::DEFAULT_NAME_FORMAT)
            .list(&path)
            .len(),
        1
    );

    // 「Default」で戻せば、また全部が戻る。
    change_folder(window, live, String::new());
    assert_eq!(window.get_backup_folder(), "");
    assert_eq!(
        backup::Store::new(&before, backup::DEFAULT_NAME_FORMAT)
            .list(&path)
            .len(),
        1
    );
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
    let store = backup_store(window).unwrap();
    assert!(store.list(&from).is_empty());
    assert_eq!(store.list(&to).len(), 1);
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
    window.set_tree_open(true);
    window.set_left_tab(7);
    publish_pane(window, live);
    draw("backups-pane");
    crate::workspace_manager_requested(window, live);
    window.set_workspace_manager_open(true);
    draw("workspace-manager");
    window.set_workspace_manager_open(false);
    crate::open_settings(window, live);
    window.set_settings_tab(8);
    publish_settings(window);
    // 設定の面は横に広いので、広い面で描いて全部を写す。
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

#[test]
fn ending_the_comparison_closes_the_history() {
    let root = scratch_directory("backup-end");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    diff_view::wire(window, live);
    with_backups(&harness, &root.join("draft.md"), &["一\n"], "現在\n");
    open_history(window, live);
    assert!(window.get_diff_active());
    window.invoke_diff_dismissed();
    assert!(!window.get_diff_active());
    assert!(!window.get_backup_history_active());
}

#[test]
fn clicking_end_comparison_closes_the_history() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let root = scratch_directory("backup-end-click");
    let (harness, _document) = Harness::new(|weak| open_under(&root, "draft.md", "現在\n", weak));
    attach_workspace(&harness.live, &root, SaveMode::AutoBackup);
    let window = &harness.window;
    let live = &harness.live;
    diff_view::wire(window, live);
    with_backups(&harness, &root.join("draft.md"), &["一\n"], "現在\n");
    window.show().unwrap();
    open_history(window, live);
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
    harness.surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1000);
    });
    let at = slint::LogicalPosition::new(610.0, 20.0);
    let w = window.window();
    w.dispatch_event(WindowEvent::PointerMoved { position: at });
    w.dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Left,
    });
    w.dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Left,
    });
    slint::platform::update_timers_and_animations();
    assert!(!window.get_diff_active(), "End Comparison did not close");
}
