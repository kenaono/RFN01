use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

// Run EDITOR_MEMO_SNAPSHOT only with the memo_ui_tests filter: concurrent font
// rendering tests can contend on the shared DirectWrite factory.

struct Offscreen(Rc<MinimalSoftwareWindow>);

#[test]
fn quick_draft_recovers_from_missed_shift_release() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = QuickDraft::new().unwrap();
    crate::draft_editor::install(&window);
    surface.set_size(slint::PhysicalSize::new(690, 340));
    window.show().unwrap();
    let draw = || {
        slint::platform::update_timers_and_animations();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); 690 * 340];
            renderer.render(&mut pixels, 690);
        });
    };
    let press = |text: SharedString| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    };
    for shift in [Key::Shift, Key::ShiftR] {
        for (message, hand, expected) in [
            (false, false, "abcdXef"),
            (true, false, "abcXf"),
            (false, true, "abcXf"),
        ] {
            window.set_text("abcdef".into());
            window.invoke_take_focus();
            window.invoke_set_caret(3);
            draw();
            window
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: shift.into() });
            press(Key::RightArrow.into());
            // No Shift release event: reproduce the state left behind by IME.
            crate::input_platform::repair_shift_state(window.window(), message, hand);
            press(Key::RightArrow.into());
            press("X".into());
            assert_eq!(window.get_text().as_str(), expected);
            crate::input_platform::repair_shift_state(window.window(), false, false);
        }
        window.set_text("abcdef".into());
        window.invoke_set_caret(0);
        draw();
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: shift.into() });
        crate::input_platform::repair_shift_state(window.window(), false, false);
        let position = slint::LogicalPosition::new(55.0, 27.0);
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        press("X".into());
        assert_eq!(
            window.get_text().len(),
            7,
            "ordinary click must insert, not replace a Shift selection"
        );
    }
}
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
#[ignore = "Explorer menu placement and refresh guard"]
fn explorer_menu_blocks_refresh_until_dismissed() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    window.set_tree_open(true);
    window.set_left_tab(0);
    window.set_work_folder("確認用".into());
    window.set_left_rows(ModelRc::new(VecModel::from(vec![LeftRow {
        name: "test.md".into(),
        parent: -1,
        ..Default::default()
    }])));
    window.show().unwrap();
    let draw = || {
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
            renderer.render(&mut pixels, 1000);
            if let Ok(path) = std::env::var("EDITOR_EXPLORER_SNAPSHOT") {
                let mut ppm = b"P6\n1000 740\n255\n".to_vec();
                for pixel in pixels {
                    ppm.extend([pixel.r, pixel.g, pixel.b]);
                }
                std::fs::write(path, ppm).unwrap();
            }
        })
    };
    draw();
    let click = |x, y, button| {
        let position = slint::LogicalPosition::new(x, y);
        window
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
    };
    let actions = Rc::new(RefCell::new(Vec::new()));
    let received = actions.clone();
    window.on_explorer_command(move |command| received.borrow_mut().push(command));
    for x in [205.0, 232.0, 261.0] {
        click(x, 24.0, PointerEventButton::Left);
    }
    assert_eq!(
        &*actions.borrow(),
        &[0, 1, 2],
        "Explorer toolbar dispatches each action"
    );
    click(110.0, 82.0, PointerEventButton::Right);
    draw();
    assert!(window.get_tree_refresh_blocked());
    let command = Rc::new(Cell::new(-1));
    let received = command.clone();
    window.on_tree_command(move |value| received.set(value));
    click(100.0, 114.0, PointerEventButton::Left);
    draw();
    assert_eq!(
        command.get(),
        0,
        "menu is below the clicked row and dispatches New File"
    );
    assert!(!window.get_tree_refresh_blocked());
    click(110.0, 82.0, PointerEventButton::Right);
    draw();
    assert!(window.get_tree_refresh_blocked());
    click(900.0, 700.0, PointerEventButton::Left);
    draw();
    assert!(!window.get_tree_refresh_blocked());
    let finished = Rc::new(RefCell::new(Vec::new()));
    let received = finished.clone();
    window.on_tree_new_finished(move |accept| received.borrow_mut().push(accept));
    window.set_tree_new_name("無題.md".into());
    window.set_tree_new_index(0);
    draw();
    assert!(window.get_tree_refresh_blocked());
    for key in [slint::platform::Key::Return, slint::platform::Key::Escape] {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    }
    assert_eq!(
        &*finished.borrow(),
        &[true, false],
        "inline name accepts Enter and Escape"
    );
}

#[test]
fn memo_close_cancel_discard_empty_and_restart_keep_their_promises() {
    let directory = std::env::temp_dir().join(format!(
        "editor-s1-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(directory.clone()));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    let numbers = Rc::new(VecModel::from(vec![0; 2 * SHEET_NUMBERS]));
    let palette = Rc::new(VecModel::from(vec![Color::default(); 2 * SHEET_COLOURS]));
    let fonts = Rc::new(VecModel::from(vec![
        SharedString::default();
        2 * SHEET_FONTS
    ]));
    reset_settings(&numbers, &palette, &fonts);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers));
    window.set_palette(ModelRc::from(palette));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
    });
    window.set_autosave(true);
    let memo = OpenDocument::untitled(1, window.as_weak());
    *memo.text.borrow_mut() = "残す本文".into();
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&memo),
        folder: Rc::default(),
        tree_paths: Rc::default(),
        workspace_ids: Rc::default(),
        results: Rc::default(),
        recent: Rc::default(),
        recent_folders: Rc::default(),
        find_terms: Rc::new(RefCell::new(find::Terms::restored(Vec::new()))),
        replace_terms: Rc::new(RefCell::new(find::Terms::restored(Vec::new()))),
        layout: Rc::new(RefCell::new(Layout::single(0))),
        pending: Rc::default(),
        close_run: Rc::default(),
        cache: Rc::new(RefCell::new(RenderCache::default())),
        tabs: Rc::new(RefCell::new(Tabs {
            no_tabs: Default::default(),
            panes: vec![{
                let tab = PaneTab::showing(&window, id, memo.clone());
                PaneTabs {
                    history: vec![NavigationPlace::from(&tab)],
                    tabs: vec![tab],
                    ..Default::default()
                }
            }],
        })),
        writer: Rc::new(FileWriter::start()),
        searcher: Rc::new(Searcher::start(|| {})),
        searched: Rc::default(),
    };
    // R4: removing the focused right pane must request focus after renumbering.
    duplicate_tab(&window, &live, id, 0);
    assert_eq!(live.tabs.borrow().of(id).history.len(), 2);
    live.states.of(id).borrow_mut().viewer = true;
    navigate(&window, &live, id, false);
    assert_eq!(live.tabs.borrow().of(id).active, 0);
    assert!(!live.states.of(id).borrow().viewer);
    navigate(&window, &live, id, true);
    assert_eq!(live.tabs.borrow().of(id).active, 1);
    assert!(live.states.of(id).borrow().viewer);
    navigate(&window, &live, id, false);
    assert!(!close_tab(&window, &live, id, 1));
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id).unwrap();
        strip.history.truncate(1);
        strip.at = 0;
    }
    window.set_editor_area_width(950.0);
    window.set_editor_area_height(620.0);
    divide_pane(&window, &live, id, Split::SideBySide);
    let right = PaneId::from_index(1);
    window.set_focused_pane(right.index());
    // Explorer should use the selected pane, including when another pane
    // already holds the same file.
    let path = directory.join("explorer-target.md");
    std::fs::write(&path, "Explorer destination").unwrap();
    *live.tree_paths.borrow_mut() = vec![path.clone()];
    activate_tree_row(&window, &live, 0);
    assert_eq!(
        live.states.document(right).file.borrow().path(),
        Some(path.as_path())
    );
    assert!(Rc::ptr_eq(&live.states.document(id), &memo));
    let opened = live.tabs.borrow().of(right).active;
    assert!(!close_tab(&window, &live, right, opened));
    window.set_focused_pane(right.index());
    let generation = window.get_focus_generation();
    assert!(!close_tab(&window, &live, right, 0));
    assert_eq!(PaneId::count(&window), 1);
    assert_eq!(window.get_focused_pane(), id.index());
    assert!(window.get_focus_generation() > generation);
    assert!(Rc::ptr_eq(&live.active(&window), &memo));
    // A close asks the same three choices with automatic backup both on and off.
    for backup in [true, false] {
        window.set_autosave(backup);
        assert!(close_tab(&window, &live, id, 0));
        assert!(matches!(
            *live.pending.borrow(),
            Some(Question::CloseMemo { .. })
        ));
        let choices = window.get_question_choices();
        assert_eq!(
            (0..choices.row_count())
                .map(|i| choices.row_data(i).unwrap().to_string())
                .collect::<Vec<_>>(),
            ["名前を付けて保存", "破棄", "キャンセル"]
        );
        if backup && let Ok(output) = std::env::var("EDITOR_MEMO_SNAPSHOT") {
            window.show().unwrap();
            let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 1000);
            });
            let mut ppm = b"P6\n1000 740\n255\n".to_vec();
            for pixel in pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(output, ppm).unwrap();
        }
        window.set_autosave(!backup);
        answer_question(&window, &live, 2);
        assert_eq!(live.tabs.borrow().of(id).tabs.len(), 1);
        assert_eq!(memo.text.borrow().as_str(), "残す本文");
    }
    // The shutdown backup updates the same copy; restart places it back in its tab.
    window.set_autosave(true);
    *memo.text.borrow_mut() = "終了直前の本文".into();
    assert_eq!(flush_work_copies(&window, &live), 0);
    let stored = session::capture_session(&window, &live);
    let restored = saving::restore_tabs(&window);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0.text.borrow().as_str(), "終了直前の本文");
    let (tabs, _) = session::open_session(&window, Some(stored), restored);
    assert_eq!(tabs.of(id).tabs.len(), 1);
    assert_eq!(
        tabs.of(id).tabs[0].document.text.borrow().as_str(),
        "終了直前の本文"
    );
    // Removing one of two views does not ask to discard the shared document.
    live.tabs
        .borrow_mut()
        .of_mut(id)
        .unwrap()
        .tabs
        .push(PaneTab::showing(&window, id, memo.clone()));
    assert!(!close_tab(&window, &live, id, 1));
    assert_eq!(memo.text.borrow().as_str(), "終了直前の本文");
    // Explicit discard also removes the backup and Back/Forward references.
    assert!(close_tab(&window, &live, id, 0));
    answer_question(&window, &live, 1);
    assert!(matches!(
        *live.pending.borrow(),
        Some(Question::DiscardOnClose { .. })
    ));
    answer_question(&window, &live, 0);
    assert_eq!(flush_work_copies(&window, &live), 0);
    assert!(saving::restore_tabs(&window).is_empty());
    assert!(live.tabs.borrow().panes.iter().all(|strip| {
        strip
            .history
            .iter()
            .all(|held| !Rc::ptr_eq(&held.document, &memo))
    }));
    // Typed then erased is empty even though the edited flag is still raised.
    let empty = live.active(&window);
    *empty.text.borrow_mut() = "".into();
    assert!(!close_tab(&window, &live, id, 0));
    assert!(!window.get_question_open());
    assert_eq!(flush_work_copies(&window, &live), 0);
    assert!(saving::restore_tabs(&window).is_empty());
    // A failed backup deletion keeps the memo visible instead of leaving a ghost.
    let kept = live.active(&window);
    *kept.text.borrow_mut() = "消せなければ残す".into();
    window.set_autosave(false);
    let blocked = app_data::work_directory()
        .unwrap()
        .join(app_data::work_file_name(&work_identity(
            &kept.file.borrow(),
        )));
    std::fs::create_dir_all(&blocked).unwrap();
    assert!(close_tab(&window, &live, id, 0));
    answer_question(&window, &live, 1);
    answer_question(&window, &live, 0);
    assert!(Rc::ptr_eq(&live.active(&window), &kept));
    assert_eq!(kept.text.borrow().as_str(), "消せなければ残す");
    assert!(window.get_render_status().contains("閉じずに"));
    std::fs::remove_dir(&blocked).unwrap();
    // Save failure leaves the text and tab; a successful named save retires its backup.
    window.set_autosave(true);
    assert_eq!(flush_work_copies(&window, &live), 0);
    assert!(!saving::write_document_in(
        &window,
        &live,
        &kept,
        directory.clone(),
        file_io::TextForm::default()
    ));
    assert!(kept.file.borrow().path().is_none());
    assert!(kept.text.edited());
    assert!(Rc::ptr_eq(&live.active(&window), &kept));
    let saved = directory.join("正式.md");
    assert!(saving::write_document_in(
        &window,
        &live,
        &kept,
        saved.clone(),
        file_io::TextForm::default()
    ));
    assert_eq!(flush_work_copies(&window, &live), 0);
    assert!(saving::restore_tabs(&window).is_empty());
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "消せなければ残す");
    // External comparison preserves the draft, save baseline, tabs and warning.
    crate::diff_view::wire(&window, &live);
    let stamp = kept.file.borrow().agreed_stamp();
    let tab_count = live.tabs.borrow().of(id).tabs.len();
    let active_tab = live.tabs.borrow().of(id).active;
    *kept.text.borrow_mut() = "編集中の未保存本文".into();
    std::fs::write(&saved, "外で変更した本文です\n").unwrap();
    let outside = kept.file.borrow().external_change();
    kept.outside.set(true);
    window.invoke_compare_saved_requested();
    assert!(window.get_diff_active());
    assert!(!window.get_diff_merge_enabled());
    assert!(window.get_diff_right_path().contains("保存版"));
    assert!(
        window
            .get_diff_rows()
            .row_data(0)
            .unwrap()
            .right
            .contains("外で変更")
    );
    assert_eq!(kept.text.borrow().as_str(), "編集中の未保存本文");
    assert_eq!(kept.file.borrow().agreed_stamp(), stamp);
    assert!(kept.outside.get());
    window.invoke_diff_dismissed();
    ask_outside_change(&window, &live);
    assert!(window.get_question_open());
    answer_question(&window, &live, 3);
    assert!(window.get_diff_active());
    assert!(window.get_diff_right_path().contains("外部版"));
    assert!(
        window
            .get_diff_rows()
            .row_data(0)
            .unwrap()
            .left
            .contains("未保存本文")
    );
    assert!(
        window
            .get_diff_rows()
            .row_data(0)
            .unwrap()
            .right
            .contains("外で変更")
    );
    assert_eq!(live.tabs.borrow().of(id).tabs.len(), tab_count);
    assert_eq!(live.tabs.borrow().of(id).active, active_tab);
    // Choosing is only a preview; apply is one undoable edit and never a disk write.
    assert!(window.get_diff_merge_enabled());
    window.invoke_diff_next_difference(true);
    if let Ok(output) = std::env::var("EDITOR_MERGE_SNAPSHOT") {
        use slint::platform::{PointerEventButton, WindowEvent};
        window.show().unwrap();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let click = |x, y| {
            let position = slint::LogicalPosition::new(x, y);
            for event in [
                WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                },
                WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                },
            ] {
                window.window().dispatch_event(event);
            }
        };
        click(517.0, 157.0);
        assert!(
            window.get_diff_can_apply(),
            "right checkbox stages the hunk"
        );
        assert_eq!(window.get_diff_rows().row_data(0).unwrap().decision, 2);
        click(17.0, 157.0);
        assert!(
            !window.get_diff_can_apply(),
            "left checkbox cancels right adoption"
        );
        assert_eq!(window.get_diff_rows().row_data(0).unwrap().decision, 1);
        click(145.0, 64.0);
        assert_eq!(
            window.get_diff_rows().row_data(0).unwrap().decision,
            2,
            "header updates checkbox state"
        );
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let mut ppm = b"P6\n1000 740\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output, ppm).unwrap();
    } else {
        window.invoke_diff_choose_side(true);
    }
    assert!(window.get_diff_can_apply());
    assert_eq!(kept.text.borrow().as_str(), "編集中の未保存本文");
    *kept.text.borrow_mut() = "比較開始後に変わった本文".into();
    window.invoke_diff_apply_merge();
    assert!(window.get_diff_active());
    assert_eq!(kept.text.borrow().as_str(), "比較開始後に変わった本文");
    *kept.text.borrow_mut() = "編集中の未保存本文".into();
    window.invoke_diff_choose_side(false);
    assert!(!window.get_diff_can_apply());
    window.invoke_diff_dismissed();
    assert_eq!(kept.text.borrow().as_str(), "編集中の未保存本文");
    live.states.of(id).borrow_mut().viewer = true;
    open_external_snapshot(&window, &live, &saved);
    assert!(!window.get_diff_merge_enabled());
    live.states.of(id).borrow_mut().viewer = false;
    open_external_snapshot(&window, &live, &saved);
    window.invoke_diff_next_difference(true);
    window.invoke_diff_choose_side(true);
    window.invoke_diff_apply_merge();
    assert!(!window.get_diff_active());
    assert_eq!(kept.text.borrow().as_str(), "外で変更した本文です\n");
    assert_eq!(kept.file.borrow().agreed_stamp(), stamp);
    assert!(kept.outside.get());
    undo_in_pane(&window, id, &kept, &live.states, &live.cache, false);
    assert_eq!(kept.text.borrow().as_str(), "編集中の未保存本文");
    open_external_snapshot(&window, &live, &saved);
    // Simulate a different process moving the file: bypass Editor's move_entry.
    let moved = directory.join("外部で移動後.md");
    std::fs::rename(&saved, &moved).unwrap();
    std::fs::write(&moved, "移動先で追加した新しい本文\n").unwrap();
    saving::check_external_change(&window, &live);
    assert_eq!(kept.file.borrow().path(), Some(saved.as_path()));
    assert_eq!(
        kept.file.borrow().external_change(),
        buffer::ExternalChange::Missing
    );
    assert!(window.get_diff_active());
    assert!(
        window
            .get_diff_rows()
            .row_data(0)
            .unwrap()
            .right
            .contains("外で変更")
    );
    assert!(
        !window
            .get_diff_rows()
            .row_data(0)
            .unwrap()
            .right
            .contains("移動先")
    );
    window.invoke_diff_dismissed();
    assert!(!window.get_diff_active());
    open_external_snapshot(&window, &live, &saved);
    assert!(!window.get_diff_active());
    assert!(
        window
            .get_render_status()
            .contains("外部版を比較できません")
    );
    std::fs::rename(&moved, &saved).unwrap();
    std::fs::write(&saved, "外で変更した本文です\n").unwrap();
    assert!(Rc::ptr_eq(&live.active(&window), &kept));
    assert_eq!(kept.text.borrow().as_str(), "編集中の未保存本文");
    assert_eq!(kept.file.borrow().agreed_stamp(), stamp);
    assert_eq!(kept.file.borrow().external_change(), outside);
    assert!(kept.outside.get());
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "外で変更した本文です\n"
    );
    std::fs::remove_file(&saved).unwrap();
    open_external_snapshot(&window, &live, &saved);
    assert!(!window.get_diff_active());
    assert!(
        window
            .get_render_status()
            .contains("外部版を比較できません")
    );
    // Restore this test's saved state before its existing close/cleanup check.
    std::fs::write(&saved, "消せなければ残す").unwrap();
    *kept.text.borrow_mut() = "消せなければ残す".into();
    kept.text.mark_saved();
    kept.outside.set(false);
    let before_reconnect = kept.text.borrow().clone();
    insert_pane_text(
        &window,
        id,
        &kept,
        &live.states,
        &live.cache,
        "移動前の追記",
        false,
    );
    let draft = kept.text.borrow().clone();
    let relocated = directory.join("指定した移動先.md");
    std::fs::rename(&saved, &relocated).unwrap();
    saving::check_external_change(&window, &live);
    assert!(kept.missing.get());
    save_document(&window, &live, false);
    assert!(matches!(
        *live.pending.borrow(),
        Some(Question::MissingFile(_))
    ));
    assert!(
        !saved.exists(),
        "Save must not recreate the old file without asking"
    );
    assert_eq!(window.get_question_choices().row_count(), 2);
    assert!(window.get_question_text().contains("比較できません"));
    answer_question(&window, &live, 1);
    assert_eq!(kept.text.borrow().as_str(), draft);
    assert_eq!(kept.file.borrow().path(), Some(saved.as_path()));
    assert!(kept.missing.get());
    assert_eq!(kept.file.borrow().agreed_stamp(), stamp);
    undo_in_pane(&window, id, &kept, &live.states, &live.cache, false);
    assert_eq!(kept.text.borrow().as_str(), before_reconnect);
    assert!(!saved.exists());
    let rescued = directory.join("別名で保管.md");
    assert!(saving::write_document_in(
        &window,
        &live,
        &kept,
        rescued.clone(),
        file_io::TextForm::default()
    ));
    assert_eq!(std::fs::read_to_string(&rescued).unwrap(), before_reconnect);
    assert!(!kept.missing.get());
    assert!(!saved.exists());
    assert!(!close_tab(&window, &live, id, 0));
    live.writer.finish();
    // Explorer refresh sees external additions/moves/deletions without opening a file.
    let tree_root = directory.join("tree-refresh");
    let child = tree_root.join("sub");
    std::fs::create_dir_all(&child).unwrap();
    let selected = child.join("selected.md");
    std::fs::write(&selected, "選択中").unwrap();
    {
        let mut folder = live.folder.borrow_mut();
        folder.root = Some(tree_root.clone());
        folder.expanded.insert(child.clone());
        folder.selected = Some(selected.clone());
    }
    window.set_left_tab(0);
    publish_tree(&window, &live);
    let focused = window.get_focused_pane();
    let added = tree_root.join("added.md");
    std::fs::write(&added, "追加").unwrap();
    let request = tree_watch::Request {
        roots: vec![tree_root.clone()],
        multi: false,
        expanded: live.folder.borrow().expanded.clone(),
        displayed_paths: live.tree_paths.borrow().clone(),
    };
    let mut watcher = tree_watch::Watcher::new().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let rows = loop {
        if let Some(rows) = watcher.poll(&request) {
            break rows;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    publish_tree_rows(&window, &live, rows);
    assert!(live.tree_paths.borrow().contains(&added));
    assert_eq!(live.folder.borrow().selected.as_ref(), Some(&selected));
    assert!(live.folder.borrow().expanded.contains(&child));
    assert_eq!(window.get_focused_pane(), focused);
    let moved = child.join("moved.md");
    std::fs::rename(&added, &moved).unwrap();
    std::fs::remove_file(&selected).unwrap();
    publish_tree_rows(
        &window,
        &live,
        file_tree::rows(&tree_root, &request.expanded, &file_tree::read_folder),
    );
    assert!(!live.tree_paths.borrow().contains(&added));
    assert!(live.tree_paths.borrow().contains(&moved));
    assert!(live.folder.borrow().selected.is_none());
    explorer_command(&window, &live, 1);
    assert!(live.folder.borrow().expanded.is_empty());
    assert!(!live.tree_paths.borrow().contains(&moved));
    reveal_in_tree(&window, &live, &moved);
    assert!(live.folder.borrow().expanded.contains(&child));
    assert_eq!(live.folder.borrow().selected.as_ref(), Some(&moved));
    assert!(window.get_tree_selected() >= 0);
    let selected_before = live.folder.borrow().selected.clone();
    reveal_in_tree(&window, &live, &directory.join("outside.md"));
    assert_eq!(live.folder.borrow().selected, selected_before);
    explorer_command(&window, &live, 2);
    assert_eq!(live.folder.borrow().selected, selected_before);
    assert_eq!(window.get_focused_pane(), focused);
    begin_tree_entry(&window, &live, child.clone(), true);
    assert!(window.get_tree_new_index() >= 0);
    assert!(!child.join("新しいフォルダー").exists());
    window.set_tree_new_name("moved.md".into());
    finish_tree_entry(&window, &live, true);
    assert!(
        live.folder.borrow().creating.is_some(),
        "collision keeps the input row"
    );
    assert!(!window.get_tree_new_error().is_empty());
    window.set_tree_new_name("作成したフォルダー".into());
    finish_tree_entry(&window, &live, true);
    assert!(child.join("作成したフォルダー").is_dir());
    assert_eq!(window.get_tree_new_index(), -1);
    begin_tree_entry(&window, &live, child.clone(), false);
    window.set_tree_new_name("取り消し.md".into());
    finish_tree_entry(&window, &live, false);
    assert!(!child.join("取り消し.md").exists());
    assert_eq!(window.get_tree_new_index(), -1);
    begin_tree_entry(&window, &live, child.clone(), false);
    window.set_tree_new_name("../不可.md".into());
    finish_tree_entry(&window, &live, true);
    assert!(live.folder.borrow().creating.is_some());
    window.set_tree_new_name("作成したファイル.md".into());
    finish_tree_entry(&window, &live, true);
    assert!(child.join("作成したファイル.md").is_file());
    assert_eq!(
        live.active(&window).file.borrow().path(),
        Some(child.join("作成したファイル.md").as_path())
    );
    let deleting = live.active(&window);
    let deleting_path = deleting.file.borrow().path().unwrap().to_owned();
    let duplicate = live.tabs.borrow().of(id).current().unwrap().clone();
    live.tabs
        .borrow_mut()
        .of_mut(id)
        .unwrap()
        .tabs
        .push(duplicate);
    live.closed_tabs.borrow_mut().clear();
    let closed_at = live.tabs.borrow().of(id).tabs.len() - 1;
    finish_close(&window, &live, id, closed_at);
    assert_eq!(live.closed_tabs.borrow().len(), 1);
    reopen_closed_tab(&window, &live);
    assert!(live.closed_tabs.borrow().is_empty());
    assert!(views_of(&live, &deleting) >= 2);
    *deleting.text.borrow_mut() = "未保存の編集".into();
    ask_delete_entry(&window, &live, &deleting_path);
    assert_eq!(window.get_question_choices().row_count(), 3);
    answer_question(&window, &live, 2);
    assert!(deleting_path.exists());
    assert!(views_of(&live, &deleting) >= 2);
    deleting.outside.set(true);
    assert!(!save_before_delete(
        &window,
        &live,
        &deleting_documents(&live, &child)
    ));
    assert_eq!(std::fs::read_to_string(&deleting_path).unwrap(), "");
    deleting.outside.set(false);
    assert!(save_before_delete(
        &window,
        &live,
        &deleting_documents(&live, &child)
    ));
    assert_eq!(
        std::fs::read_to_string(&deleting_path).unwrap(),
        "未保存の編集"
    );
    ask_delete_entry(&window, &live, &deleting_path);
    assert_eq!(window.get_question_choices().row_count(), 2);
    answer_question(&window, &live, 1);
    *deleting.text.borrow_mut() = "保存せずに削除する編集".into();
    delete_entry_with(&window, &live, &deleting_path, |_| false);
    assert!(deleting.text.edited());
    assert!(views_of(&live, &deleting) >= 2);
    let recycled = directory.join("recycled-for-test.md");
    delete_entry_with(&window, &live, &deleting_path, |path| {
        std::fs::rename(path, &recycled).is_ok()
    });
    assert!(recycled.exists());
    assert_eq!(views_of(&live, &deleting), 0);
    assert!(live.tabs.borrow().panes.iter().all(|strip| {
        strip
            .history
            .iter()
            .all(|held| !Rc::ptr_eq(&held.document, &deleting))
    }));
    assert!(
        live.closed_tabs
            .borrow()
            .iter()
            .all(|tab| !Rc::ptr_eq(&tab.document, &deleting))
    );
    crate::shortcuts::wire(&window, &live);
    window.set_shortcut_selected(1);
    window.set_shortcut_edit("Ctrl+O".into());
    window.invoke_shortcut_save(false);
    assert!(window.get_shortcut_status().contains("割り当て済み"));
    window.invoke_shortcut_capture("s".into(), true, true, false);
    assert_eq!(window.get_shortcut_edit(), "Ctrl+Alt+S");
    assert_eq!(window.get_shortcut_keys().row_data(1).unwrap(), "Ctrl+S");
    window.invoke_shortcut_save(false);
    assert_eq!(
        window.get_shortcut_keys().row_data(1).unwrap(),
        "Ctrl+Alt+S"
    );
    let saves = Rc::new(Cell::new(0));
    let received = saves.clone();
    window.on_save_requested(move || received.set(received.get() + 1));
    assert!(window.invoke_shortcut_key("s".into(), true, false, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(saves.get(), 0, "old binding is disabled");
    assert!(window.invoke_shortcut_key("s".into(), true, true, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(saves.get(), 1, "new binding dispatches save");
    window.invoke_shortcut_save(true);
    assert_eq!(window.get_shortcut_keys().row_data(1).unwrap(), "Ctrl+S");
    window.set_shortcut_query("".into());
    window.invoke_shortcut_filter();
    let keys_in = |category: i32| {
        window
            .get_shortcut_rows()
            .iter()
            .filter(|row| !row.header && row.category == category)
            .count()
    };
    assert_eq!(keys_in(5), 12);
    // A folded group keeps its heading and hides its keys.
    window.invoke_shortcut_fold(5);
    assert_eq!(keys_in(5), 0);
    window.invoke_shortcut_fold(5);
    assert_eq!(keys_in(5), 12);
    window.set_shortcut_selected(24);
    window.invoke_shortcut_capture("k".into(), true, true, false);
    window.invoke_shortcut_save(false);
    let kills = Rc::new(Cell::new(0));
    let received = kills.clone();
    window.on_pane_kill(move |_, which| {
        assert_eq!(which, 0);
        received.set(received.get() + 1);
    });
    assert!(window.invoke_shortcut_key("k".into(), true, false, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(kills.get(), 0);
    assert!(window.invoke_shortcut_key("k".into(), true, true, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(kills.get(), 1);
    window.invoke_shortcut_save(true);
    window.set_shortcut_selected(10);
    window.invoke_shortcut_capture("q".into(), true, true, false);
    window.invoke_shortcut_save(false);
    let drafts = Rc::new(Cell::new(0));
    let received = drafts.clone();
    window.on_quick_draft_requested(move || received.set(received.get() + 1));
    assert!(window.invoke_shortcut_key("q".into(), true, true, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(drafts.get(), 1);
    window.invoke_shortcut_save(true);
    window.set_shortcut_selected(1);
    let modes = Rc::new(RefCell::new(Vec::new()));
    let received = modes.clone();
    window.on_pane_direction_toggled(move |_| received.borrow_mut().push("direction"));
    let received = modes.clone();
    window.on_pane_preview_toggled(move |_| received.borrow_mut().push("preview"));
    let received = modes.clone();
    window.on_pane_viewer_toggled(move |_| received.borrow_mut().push("viewer"));
    for (command, key) in [(42, "r"), (43, "p"), (44, "v")] {
        window.set_shortcut_selected(command);
        window.invoke_shortcut_capture(key.into(), true, true, false);
        window.invoke_shortcut_save(false);
        let before = modes.borrow().len();
        for modifier in [
            slint::platform::Key::Control,
            slint::platform::Key::Alt,
            slint::platform::Key::Shift,
            slint::platform::Key::AltGr,
        ] {
            window.invoke_shortcut_capture(modifier.into(), true, true, false);
            assert_eq!(
                window.get_shortcut_edit(),
                format!("Ctrl+Alt+{}", key.to_uppercase())
            );
            assert!(window.invoke_shortcut_key(modifier.into(), true, true, false));
            slint::platform::update_timers_and_animations();
            assert_eq!(modes.borrow().len(), before);
        }
        assert!(window.invoke_shortcut_key(key.into(), true, true, false));
        slint::platform::update_timers_and_animations();
        window.invoke_shortcut_save(true);
    }
    assert_eq!(&*modes.borrow(), &["direction", "preview", "viewer"]);
    let pane = focused_pane(&window);
    live.states.of(pane).borrow_mut().viewer = true;
    window.set_shortcut_selected(43);
    window.invoke_shortcut_capture("p".into(), true, true, false);
    window.invoke_shortcut_save(false);
    assert!(window.invoke_shortcut_key("p".into(), true, true, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(&modes.borrow()[3..], &["viewer", "preview"]);
    live.states.of(pane).borrow_mut().viewer = false;
    window.invoke_shortcut_save(true);
    let draft_window = crate::QuickDraft::new().unwrap();
    crate::quick_draft::wire_shortcuts(&window, &draft_window);
    let copied_then_closed = Rc::new(RefCell::new(Vec::new()));
    let received = copied_then_closed.clone();
    draft_window.on_copied(move || received.borrow_mut().push("copied"));
    let received = copied_then_closed.clone();
    draft_window.on_copy_and_close(move || received.borrow_mut().push("closed"));
    window.set_shortcut_selected(39);
    window.invoke_shortcut_capture("q".into(), true, true, false);
    window.invoke_shortcut_save(false);
    assert!(draft_window.invoke_shortcut_key("q".into(), true, true, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(&*copied_then_closed.borrow(), &["copied", "closed"]);
    window.invoke_shortcut_save(true);
    assert!(!draft_window.invoke_shortcut_key("q".into(), true, true, false));
    window.set_shortcut_selected(1);
    window.set_shortcut_query("保存".into());
    window.invoke_shortcut_filter();
    assert_eq!(
        window
            .get_shortcut_rows()
            .iter()
            .filter(|row| !row.header)
            .count(),
        2
    );
    // Pressing a key finds what it is bound to, fixed keys included.
    window.set_shortcut_query("".into());
    window.invoke_shortcut_key_search("o".into(), true, false, false);
    let found: Vec<_> = window
        .get_shortcut_rows()
        .iter()
        .filter(|row| !row.header)
        .map(|row| row.name.to_string())
        .collect();
    assert_eq!(found, ["ファイルを開く"]);
    window.invoke_shortcut_key_search("v".into(), true, false, false);
    assert_eq!(
        window
            .get_shortcut_rows()
            .iter()
            .filter(|row| !row.header && row.fixed)
            .count(),
        3
    );
    window.set_shortcut_key_query("".into());
    window.invoke_shortcut_filter();
    if let Ok(output) = std::env::var("EDITOR_SHORTCUT_SNAPSHOT") {
        window.set_shortcut_query("".into());
        window.invoke_shortcut_filter();
        window.set_settings_tab(6);
        focused_pane(&window).update_screen(&window, |screen| screen.settings = true);
        window.show().unwrap();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let mut ppm = b"P6\n1000 740\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output, ppm).unwrap();
    }
    drop(watcher);
    // Only this test's unique temporary directory is removed.
    assert!(
        directory
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    assert!(
        directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("editor-s1-")
    );
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn search_shortcuts_open_the_bar_in_both_directions() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    let numbers = Rc::new(VecModel::from(vec![0; 2 * SHEET_NUMBERS]));
    let palette = Rc::new(VecModel::from(vec![Color::default(); 2 * SHEET_COLOURS]));
    let fonts = Rc::new(VecModel::from(vec![
        SharedString::default();
        2 * SHEET_FONTS
    ]));
    reset_settings(&numbers, &palette, &fonts);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers));
    window.set_palette(ModelRc::from(palette));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
    });
    window.set_autosave(false);
    let memo = OpenDocument::untitled(1, window.as_weak());
    *memo.text.borrow_mut() = "残す本文".into();
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&memo),
        folder: Rc::default(),
        tree_paths: Rc::default(),
        workspace_ids: Rc::default(),
        results: Rc::default(),
        recent: Rc::default(),
        recent_folders: Rc::default(),
        find_terms: Rc::new(RefCell::new(find::Terms::restored(Vec::new()))),
        replace_terms: Rc::new(RefCell::new(find::Terms::restored(Vec::new()))),
        layout: Rc::new(RefCell::new(Layout::single(0))),
        pending: Rc::default(),
        close_run: Rc::default(),
        cache: Rc::new(RefCell::new(RenderCache::default())),
        tabs: Rc::new(RefCell::new(Tabs {
            no_tabs: Default::default(),
            panes: vec![{
                let tab = PaneTab::showing(&window, id, memo.clone());
                PaneTabs {
                    history: vec![NavigationPlace::from(&tab)],
                    tabs: vec![tab],
                    ..Default::default()
                }
            }],
        })),
        writer: Rc::new(FileWriter::start()),
        searcher: Rc::new(Searcher::start(|| {})),
        searched: Rc::default(),
    };
    shortcuts::wire(&window, &live);
    let steps = Rc::new(Cell::new(0));
    let seen = steps.clone();
    window.on_find_requested(move |_| seen.set(seen.get() + 1));
    let press = |text: &str, alt: bool| {
        assert!(window.invoke_shortcut_key(text.into(), true, alt, false));
        slint::platform::update_timers_and_animations();
    };
    for vertical in [false, true] {
        id.update_screen(&window, |screen| screen.vertical = vertical);
        window.set_find_open(false);
        press("f", false);
        assert!(window.get_find_open());
        assert!(!window.get_find_replacing());
        assert!(window.get_find_focus());
        assert_eq!(window.get_find_pane(), id.index());
        press("h", false);
        assert!(window.get_find_open() && window.get_find_replacing());
        press("h", false);
        assert!(!window.get_find_open());
        press("F", false);
        assert!(window.get_find_open() && !window.get_find_replacing());
        window.set_find_focus(false);
        press("f", false);
        assert!(window.get_find_open() && window.get_find_focus());
        press("f", false);
        assert!(!window.get_find_open());
    }
    window.set_shortcut_bindings("4=Ctrl+Alt+F".into());
    press("f", false);
    assert!(
        !window.get_find_open(),
        "old binding must be consumed without opening"
    );
    press("f", true);
    assert!(window.get_find_open());
    assert_eq!(
        steps.get(),
        0,
        "opening search must not invoke next/previous match"
    );

    // Exercise real key delivery: Slint discards focus on a fully clipped
    // TextInput before its key callback runs. A direct callback test misses it.
    use slint::platform::{Key, WindowEvent};
    window.set_shortcut_bindings("".into());
    window.set_find_open(false);
    id.update_screen(&window, |screen| {
        screen.empty = false;
        screen.content_width = 20000;
        screen.content_height = 20000;
    });
    window.show().unwrap();
    let settle = || {
        slint::platform::update_timers_and_animations();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
            renderer.render(&mut pixels, 1000);
        });
    };
    settle();
    for vertical in [false, true] {
        id.update_screen(&window, |screen| {
            screen.vertical = vertical;
            screen.ime_anchor_x = 30.0;
            screen.ime_anchor_y = 30.0;
        });
        window.set_focus_generation(window.get_focus_generation() + 1);
        settle();
        let wrap_before = id.line_fit(&window, &Typography::default());
        // A later layout result moves the input after focus was acquired.
        id.update_screen(&window, |screen| {
            screen.ime_anchor_x = 15000.0;
            screen.ime_anchor_y = 15000.0;
        });
        settle();
        for (key, opened, replacing) in [
            ("f", true, false),
            ("h", true, true),
            ("h", false, true),
            ("f", true, false),
            ("f", false, false),
        ] {
            window.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Control.into(),
            });
            window
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
            window
                .window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
            window.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Control.into(),
            });
            settle();
            assert_eq!(
                window.get_find_open(),
                opened,
                "clipped anchor: vertical={vertical}, Ctrl+{key}"
            );
            assert_eq!(window.get_find_replacing(), replacing);
            settle();
            assert_eq!(
                id.line_fit(&window, &Typography::default()),
                wrap_before,
                "search/replace must preserve wrapping: vertical={vertical}, Ctrl+{key}"
            );
        }
        id.update_screen(&window, |screen| {
            screen.width -= 80.0;
            screen.height -= 80.0;
        });
        settle();
        settle();
        assert_ne!(
            id.line_fit(&window, &Typography::default()),
            wrap_before,
            "resizing the pane must still update wrapping: vertical={vertical}"
        );
    }

    // Compare rendered document pixels, not just wrapping numbers: the bar
    // must occlude the top without translating the document below it.
    let mut marker = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(40, 40);
    marker.make_mut_slice().fill(slint::Rgb8Pixel {
        r: 17,
        g: 213,
        b: 83,
    });
    let marker = slint::Image::from_rgb8(marker);
    let marker_position = || {
        settle();
        settle();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        pixels
            .iter()
            .position(|pixel| pixel.r == 17 && pixel.g == 213 && pixel.b == 83)
            .map(|index| (index % 1000, index / 1000))
            .expect("document marker must be visible")
    };
    for vertical in [false, true] {
        id.update_screen(&window, |screen| {
            screen.vertical = vertical;
            screen.scroll_x = 0.0;
            screen.scroll_y = 0.0;
            screen.ime_anchor_x = 300.0;
            screen.ime_anchor_y = 300.0;
            screen.tiles = ModelRc::new(VecModel::from(vec![PreviewTile {
                x: 300,
                y: 300,
                width: 40,
                height: 40,
                source: marker.clone(),
            }]));
        });
        let before = marker_position();
        for replacing in [false, true, true] {
            window.invoke_toggle_find(id.index(), replacing);
            assert_eq!(
                marker_position(),
                before,
                "bar must not move document pixels: vertical={vertical}, replacing={replacing}"
            );
        }
    }
}

/// The real standalone window uses the shared document/history/layout, without
/// constructing an AppWindow, pane registry or a TAB.
#[test]
fn standalone_draft_edits_japanese_and_keeps_composition_out_of_saved_text() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = QuickDraft::new().unwrap();
    crate::draft_editor::install(&window);
    surface.set_size(slint::PhysicalSize::new(690, 340));
    window.show().unwrap();
    let edits = Rc::new(Cell::new(0));
    let count = edits.clone();
    window.on_edited(move || count.set(count.get() + 1));
    window.set_text("猫と犬\n次の行".into());
    window.invoke_set_caret(3);
    window.invoke_editor_move(1, true);
    window.invoke_editor_preedit("にほん".into());
    assert_eq!(window.get_text().as_str(), "猫と犬\n次の行");
    assert_eq!(edits.get(), 0);
    window.invoke_editor_text("や".into());
    assert_eq!(window.get_text().as_str(), "猫や犬\n次の行");
    assert_eq!(edits.get(), 1);
    window.invoke_editor_undo(false);
    assert_eq!(window.get_text().as_str(), "猫と犬\n次の行");
    window.invoke_editor_undo(true);
    assert_eq!(window.get_text().as_str(), "猫や犬\n次の行");
    window.invoke_editor_select_all();
    assert!(window.get_editor_selected());
    assert!(window.invoke_editor_escape());
    assert!(!window.get_editor_selected());
    window.invoke_editor_preedit("取り消す".into());
    assert!(window.invoke_editor_escape());
    assert_eq!(window.get_text().as_str(), "猫や犬\n次の行");
    window.set_text("復元した下書き".into());
    window.invoke_set_caret(3);
    window.invoke_editor_undo(false);
    assert_eq!(window.get_text().as_str(), "復元した下書き");
    assert_eq!(window.get_caret(), 3);
    assert!(!window.get_editor_screen().can_undo);
    drop(window);
    slint::platform::update_timers_and_animations();
}

#[test]
fn standalone_draft_keeps_end_visible_after_long_text_and_resize() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = QuickDraft::new().unwrap();
    crate::draft_editor::install(&window);
    surface.set_size(slint::PhysicalSize::new(690, 340));
    window.show().unwrap();
    let draw = |width: usize, height: usize| {
        slint::platform::update_timers_and_animations();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
            renderer.render(&mut pixels, width);
        });
    };
    draw(690, 340);
    let initial = window.get_editor_screen();
    assert!(
        initial.caret_x < 20.0 && initial.caret_y < 20.0,
        "draft text starts at the host padding, not the main editor page margin"
    );
    let text = "日本語の本文と English text\n".repeat(80);
    window.set_text(text.clone().into());
    window.invoke_set_caret(text.len() as i32);
    window.invoke_editor_text("末尾".into());
    draw(690, 340);
    let screen = window.get_editor_screen();
    assert!(screen.scroll_y < 0.0);
    assert!(screen.caret_y + screen.scroll_y >= -1.0);
    assert!(screen.caret_y + screen.scroll_y < screen.shown_height);
    assert!(screen.tiles.row_count() > 0);
    surface.set_size(slint::PhysicalSize::new(400, 240));
    draw(400, 240);
    window.invoke_set_caret(window.get_text().len() as i32);
    draw(400, 240);
    assert_eq!(window.get_text().as_str(), format!("{text}末尾"));
    assert!(window.get_editor_screen().shown_width < screen.shown_width);
    window.invoke_editor_delete(true);
    assert!(window.get_text().ends_with("末"));
    window.invoke_editor_undo(false);
    assert!(window.get_text().ends_with("末尾"));
}

#[test]
fn standalone_draft_reports_real_ime_cell_at_bottom_after_scroll_and_resize() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = QuickDraft::new().unwrap();
    crate::draft_editor::install(&window);
    let areas = Rc::new(RefCell::new(Vec::new()));
    let reported = areas.clone();
    window.on_editor_ime_area(move |x, y, w, h, vertical| {
        reported.borrow_mut().push((x, y, w, h, vertical))
    });
    surface.set_size(slint::PhysicalSize::new(690, 340));
    window.show().unwrap();
    window.invoke_take_focus();
    for (width, height) in [(690, 340), (400, 240)] {
        surface.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
            renderer.render(&mut pixels, width as usize);
        });
        window.set_text("日本語の行\n".repeat(80).into());
        window.invoke_set_caret(window.get_text().len() as i32);
        window.invoke_editor_text("末尾".into());
        areas.borrow_mut().clear();
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(5));
            slint::platform::update_timers_and_animations();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                renderer.render(&mut pixels, width as usize);
            });
        }
        let screen = window.get_editor_screen();
        let &(x, y, w, h, vertical) = areas
            .borrow()
            .last()
            .expect("focused shared editor reports its native IME area");
        assert!(!vertical);
        assert_eq!((w, h), (screen.caret_width, screen.caret_height));
        assert!(h > 10.0, "IME exclusion must not use the hidden 1px font");
        assert!(
            x >= 0.0 && y >= 0.0 && y + h <= height as f32,
            "native area=({x},{y},{w},{h}), window={width}x{height}, caret_y={}, scroll={}, viewport={}",
            screen.caret_y,
            screen.scroll_y,
            screen.shown_height
        );
        assert!(screen.scroll_y < 0.0);
    }
}

#[test]
fn editor_ime_exclusion_tracks_vertical_heading_size_without_moving_input_focus() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let areas = Rc::new(RefCell::new(Vec::new()));
    let reported = areas.clone();
    window.on_editor_ime_area(move |x, y, w, h, vertical| {
        reported.borrow_mut().push((x, y, w, h, vertical))
    });
    id.update_screen(&window, |s| {
        s.empty = false;
        s.width = 950.0;
        s.height = 620.0;
        s.content_width = 1000;
        s.content_height = 2000;
        s.caret_visible = true;
        s.caret_x = 300.0;
        s.caret_y = 300.0;
        s.ime_anchor_x = 15000.0;
        s.ime_anchor_y = 15000.0;
    });
    window.show().unwrap();
    for (vertical, w, h) in [(false, 2.0, 22.0), (true, 22.0, 2.0), (true, 52.0, 2.0)] {
        areas.borrow_mut().clear();
        id.update_screen(&window, |s| {
            s.vertical = vertical;
            s.caret_width = w;
            s.caret_height = h;
        });
        window.set_focus_generation(window.get_focus_generation() + 1);
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(5));
            slint::platform::update_timers_and_animations();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
                renderer.render(&mut pixels, 1000);
            });
        }
        let &(x, y, actual_w, actual_h, actual_vertical) = areas
            .borrow()
            .last()
            .expect("main editor reports its native IME area");
        assert_eq!((actual_w, actual_h, actual_vertical), (w, h, vertical));
        assert!(
            x > 0.0 && x < 1000.0 && y > 0.0 && y < 740.0,
            "candidate position follows the real cell, not the clamped/offscreen input anchor"
        );
    }
}
