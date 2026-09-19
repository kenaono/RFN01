use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

#[test]
fn dropped_files_keep_dirty_text_reuse_tabs_and_report_invalid_paths() {
    let r = Recovery::new();
    append(&r.memo, "未保存の追記");
    let original = r.memo.text.borrow().to_string();
    let first = r.directory.join("日本語 空白.md");
    let second = r.directory.join("second.txt");
    std::fs::write(&first, "最初の文書").unwrap();
    std::fs::write(&second, "次の文書").unwrap();
    open_dropped_file(&r.window, &r.live, &first, r.id);
    open_dropped_file(&r.window, &r.live, &second, r.id);
    assert_eq!(r.live.tabs.borrow().of(r.id).tabs.len(), 3);
    assert_eq!(r.memo.text.borrow().to_string(), original);
    assert!(r.memo.text.edited());
    let document = open_documents(&r.live)
        .into_iter()
        .find(|d| d.file.borrow().path() == Some(first.as_path()))
        .unwrap();
    append(&document, "編集中");
    open_dropped_file(&r.window, &r.live, &first, r.id);
    let tabs = r.live.tabs.borrow();
    let strip = tabs.of(r.id);
    assert_eq!(strip.tabs.len(), 3);
    assert!(Rc::ptr_eq(&strip.tabs[strip.active].document, &document));
    assert!(!strip.tabs[strip.active].provisional.get());
    assert!(document.text.borrow().ends_with("編集中"));
    drop(tabs);
    open_dropped_file(&r.window, &r.live, &r.directory, r.id);
    assert!(!r.window.get_render_status().is_empty());
    open_dropped_file(&r.window, &r.live, &r.directory.join("missing.txt"), r.id);
    assert!(!r.window.get_render_status().is_empty());
    assert_eq!(r.live.tabs.borrow().of(r.id).tabs.len(), 3);
    assert_eq!(r.memo.text.borrow().to_string(), original);
}

#[test]
fn startup_paths_open_the_last_directory_as_the_work_folder_without_moving_cwd() {
    let r = Recovery::new();
    let before_cwd = std::env::current_dir().unwrap();
    append(&r.memo, "未保存の追記");
    let original = r.memo.text.borrow().to_string();
    let first_folder = r.directory.join("最初の フォルダ");
    let second_folder = r.directory.join("second folder");
    std::fs::create_dir_all(&first_folder).unwrap();
    std::fs::create_dir_all(&second_folder).unwrap();
    open_startup_paths(
        &r.window,
        &r.live,
        r.id,
        &[first_folder.clone(), second_folder.clone()],
    );
    assert_eq!(r.live.folder.borrow().root, Some(second_folder.clone()));
    assert_eq!(std::env::current_dir().unwrap(), before_cwd);
    let recent = r.live.recent_folders.borrow();
    assert_eq!(recent.first(), Some(&second_folder));
    assert!(recent.contains(&first_folder));
    drop(recent);

    // Opening a folder must not disturb an already-dirty tab's text (要件 8.1),
    // and that survives a flush/restore round trip too.
    assert_eq!(r.memo.text.borrow().to_string(), original);
    assert!(r.memo.text.edited());
    r.flush();
    let restored = saving::restore_tabs(&r.window);
    let restored_memo = restored
        .iter()
        .find(|(document, _)| document.file.borrow().path().is_none())
        .expect("the dirty untitled tab survives a folder change");
    assert_eq!(*restored_memo.0.text.borrow(), original);

    // The folder that opening settled on is also what the session persists.
    let app_dir = app_data::app_directory().unwrap();
    let session = app_data::read_session(&app_dir).unwrap();
    assert_eq!(session.folder, Some(second_folder));
}

#[test]
fn paths_from_arguments_resolves_relative_paths_and_leaves_the_launch_cwd_alone() {
    let r = Recovery::new();
    let before_cwd = std::env::current_dir().unwrap();
    let args = paths_from_arguments([".".into(), "src".into()]);
    assert_eq!(
        std::env::current_dir().unwrap(),
        before_cwd,
        "resolving relative arguments must not move the process cwd"
    );
    assert_eq!(args.len(), 2);
    assert!(args.iter().all(|path| path.is_absolute()));
    assert!(args[1].ends_with("src"));
    open_startup_paths(&r.window, &r.live, r.id, &args);
    assert_eq!(std::env::current_dir().unwrap(), before_cwd);
    assert_eq!(r.live.folder.borrow().root, Some(args[1].clone()));
}

#[test]
fn startup_paths_keep_file_tab_order_and_open_a_folder_wherever_it_is_named() {
    let r = Recovery::new();
    let folder = r.directory.join("work");
    std::fs::create_dir_all(&folder).unwrap();
    let first = r.directory.join("a.md");
    let second = r.directory.join("b.md");
    std::fs::write(&first, "A").unwrap();
    std::fs::write(&second, "B").unwrap();
    open_startup_paths(
        &r.window,
        &r.live,
        r.id,
        &[second.clone(), folder.clone(), first.clone()],
    );
    assert_eq!(r.live.folder.borrow().root, Some(folder));
    let tabs = r.live.tabs.borrow();
    let paths: Vec<_> = tabs
        .of(r.id)
        .tabs
        .iter()
        .map(|tab| tab.document.file.borrow().path().map(Path::to_path_buf))
        .collect();
    // The pane's own untitled tab, then the two files in the order they were
    // named — the folder in between opened no tab of its own.
    assert_eq!(paths, vec![None, Some(second), Some(first)]);
}

#[test]
fn startup_paths_reuse_a_tab_for_a_repeated_file_without_losing_other_dirty_text() {
    let r = Recovery::new();
    append(&r.memo, "未保存の追記");
    let original = r.memo.text.borrow().to_string();
    let path = r.directory.join("原稿.md");
    std::fs::write(&path, "最初の文書").unwrap();
    open_startup_paths(&r.window, &r.live, r.id, &[path.clone()]);
    assert_eq!(r.live.tabs.borrow().of(r.id).tabs.len(), 2);
    assert_eq!(r.memo.text.borrow().to_string(), original);
    assert!(r.memo.text.edited());
    let document = open_documents(&r.live)
        .into_iter()
        .find(|d| d.file.borrow().path() == Some(path.as_path()))
        .unwrap();
    append(&document, "編集中");
    open_startup_paths(&r.window, &r.live, r.id, &[path.clone()]);
    let tabs = r.live.tabs.borrow();
    let strip = tabs.of(r.id);
    assert_eq!(strip.tabs.len(), 2);
    assert!(Rc::ptr_eq(&strip.tabs[strip.active].document, &document));
    assert!(!strip.tabs[strip.active].provisional.get());
    assert!(document.text.borrow().ends_with("編集中"));
    assert_eq!(r.memo.text.borrow().to_string(), original);
}

#[test]
fn startup_paths_empty_list_is_a_no_op_and_a_missing_path_does_not_block_a_later_valid_one() {
    let r = Recovery::new();
    let before_tabs = r.live.tabs.borrow().of(r.id).tabs.len();
    let before_root = r.live.folder.borrow().root.clone();
    open_startup_paths(&r.window, &r.live, r.id, &[]);
    assert_eq!(r.live.tabs.borrow().of(r.id).tabs.len(), before_tabs);
    assert_eq!(r.live.folder.borrow().root, before_root);
    assert!(r.window.get_render_status().is_empty());

    let missing = r.directory.join("no-such-file.md");
    open_startup_paths(&r.window, &r.live, r.id, &[missing.clone()]);
    assert!(!missing.exists());
    assert!(
        !r.window.get_render_status().is_empty(),
        "a missing path processed alone must report an error immediately"
    );

    // A later valid file still opens: `open_path_in_pane` clears the previous
    // notification as it starts on each path in turn, so the error from the
    // missing one is gone by the time the valid one lands — that is existing,
    // intended behaviour, not something this test re-checks here.
    let valid = r.directory.join("valid.md");
    std::fs::write(&valid, "本文").unwrap();
    open_startup_paths(&r.window, &r.live, r.id, &[missing.clone(), valid.clone()]);
    let tabs = r.live.tabs.borrow();
    let strip = tabs.of(r.id);
    assert_eq!(strip.tabs.len(), before_tabs + 1);
    assert!(
        strip
            .tabs
            .iter()
            .any(|tab| tab.document.file.borrow().path() == Some(valid.as_path()))
    );
}

// The `--diagnostics` flags and the `--` terminator themselves are exercised
// exhaustively in `diag.rs` (`detail_flags_do_not_consume_file_arguments_and_respect_end_of_options`);
// this only checks that what that parser hands back still opens correctly
// once diagnostics options and a folder and files are named together.
#[test]
fn startup_paths_open_correctly_when_parsed_alongside_diagnostics_options() {
    let r = Recovery::new();
    let folder = r.directory.join("フォルダ");
    std::fs::create_dir_all(&folder).unwrap();
    let dashed = r.directory.join("-help.md");
    std::fs::write(&dashed, "本文").unwrap();
    assert!(diag::Config::from_args(["--diagnostics=render".into()]).is_ok());
    let args = paths_from_arguments([
        "--diagnostics=render".into(),
        folder.clone().into_os_string(),
        "--".into(),
        dashed.clone().into_os_string(),
    ]);
    open_startup_paths(&r.window, &r.live, r.id, &args);
    assert_eq!(r.live.folder.borrow().root, Some(folder));
    let tabs = r.live.tabs.borrow();
    assert!(
        tabs.of(r.id)
            .tabs
            .iter()
            .any(|tab| tab.document.file.borrow().path() == Some(dashed.as_path()))
    );
}

struct Offscreen(Rc<MinimalSoftwareWindow>);

#[test]
fn dropped_file_opens_in_captured_pane_after_focus_changes() {
    let r = Recovery::new();
    r.window.set_editor_area_width(1000.0);
    r.window.set_editor_area_height(700.0);
    divide_pane(&r.window, &r.live, r.id, Split::SideBySide);
    let target = PaneId(1);
    let screen = target.screen(&r.window);
    let picked = pane_at_drop_point(
        &r.window,
        r.window.get_editor_area_x() + screen.x + 30.0,
        r.window.get_editor_area_y() + screen.y + 30.0,
    )
    .unwrap();
    assert_eq!(picked, target);
    r.window.set_focused_pane(0);
    let path = r.directory.join("drop-in-second.txt");
    std::fs::write(&path, "別ペインに開く").unwrap();
    open_dropped_file(&r.window, &r.live, &path, picked);
    let tabs = r.live.tabs.borrow();
    assert!(
        tabs.of(target)
            .tabs
            .iter()
            .any(|t| t.document.file.borrow().path() == Some(path.as_path()))
    );
    assert!(
        !tabs
            .of(r.id)
            .tabs
            .iter()
            .any(|t| t.document.file.borrow().path() == Some(path.as_path()))
    );
}

#[test]
fn drop_point_selects_the_hit_pane_not_the_focused_pane() {
    let r = Recovery::new();
    publish_panes(&r.window, 4);
    for (index, (x, y, width, height)) in [
        (0.0, 0.0, 200.0, 400.0),
        (209.0, 0.0, 300.0, 195.0),
        (209.0, 204.0, 300.0, 196.0),
        (0.0, 0.0, 0.0, 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        PaneId(index as u32).update_screen(&r.window, |screen| {
            screen.x = x;
            screen.y = y;
            screen.width = width;
            screen.height = height;
        });
    }
    r.window.set_focused_pane(0);
    let x = r.window.get_editor_area_x();
    let y = r.window.get_editor_area_y();
    assert_eq!(
        pane_at_drop_point(&r.window, x + 250.0, y + 50.0),
        Some(PaneId(1))
    );
    assert_eq!(
        pane_at_drop_point(&r.window, x + 250.0, y + 250.0),
        Some(PaneId(2))
    );
    assert_eq!(
        pane_at_drop_point(&r.window, x + 20.0, y + 50.0),
        Some(PaneId(0))
    );
    assert_eq!(pane_at_drop_point(&r.window, x + 205.0, y + 50.0), None);
    assert_eq!(pane_at_drop_point(&r.window, x + 250.0, y + 200.0), None);
    assert_eq!(pane_at_drop_point(&r.window, x - 1.0, y + 50.0), None);
    assert_eq!(pane_at_drop_point(&r.window, x + 250.0, y + 400.0), None);
}
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

struct Recovery {
    directory: PathBuf,
    window: AppWindow,
    live: Live,
    memo: Rc<OpenDocument>,
    id: PaneId,
}

impl Recovery {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "editor-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(directory.clone()));
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
        let tab = PaneTab::showing(&window, id, memo.clone());
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
                panels: Default::default(),
                panes: vec![PaneTabs {
                    history: vec![NavigationPlace::from(&tab)],
                    tabs: vec![tab],
                    ..Default::default()
                }],
            })),
            writer: Rc::new(FileWriter::start()),
            searcher: Rc::new(Searcher::start(|| {})),
            searched: Rc::default(),
        };
        Self {
            directory,
            window,
            live,
            memo,
            id,
        }
    }

    fn flush(&self) {
        assert_eq!(flush_work_copies(&self.window, &self.live), 0);
    }

    fn save(&self, document: &Rc<OpenDocument>, name: &str) -> PathBuf {
        let path = self.directory.join(name);
        assert!(saving::write_document_in(
            &self.window,
            &self.live,
            document,
            path.clone(),
            file_io::TextForm::default()
        ));
        path
    }

    fn add(&self, document: Rc<OpenDocument>) {
        add_tab(
            &self.window,
            &self.live,
            self.id,
            PaneTab::showing(&self.window, self.id, document),
        );
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        self.live.writer.finish();
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
    }
}

fn append(document: &Rc<OpenDocument>, text: &str) {
    let at = document.text.borrow().len();
    document.history.borrow_mut().separate_next = true;
    document.record(at, String::new(), text.into());
    document.text.borrow_mut().push_str(text);
}

#[test]
fn diagnostics_record_delete_confirmation_without_unsaved_text() {
    let fixture = Recovery::new();
    fixture.live.cache.borrow_mut().diag.start_in(
        &fixture.directory.join("diag"),
        diag::Config::from_args(["--diagnostics".into()]).unwrap(),
    );
    let secret = "原稿にしか残してはいけない本文";
    let question = Question::DeleteEdited(
        fixture.directory.join("document.md"),
        vec![(fixture.directory.join("document.md"), secret.into())],
    );
    ask_question(
        &fixture.window,
        &fixture.live,
        question,
        "削除確認".into(),
        &["退避して削除", "破棄して削除", "キャンセル"],
        0,
    );
    answer_question(&fixture.window, &fixture.live, 2);
    let cache = fixture.live.cache.borrow();
    let written = std::fs::read_to_string(cache.diag.path().unwrap()).unwrap();
    assert!(written.contains("ask DeleteEdited"));
    assert!(written.contains("answer DeleteEdited choice=2"));
    assert!(!written.contains(secret));
}

fn undo(document: &Rc<OpenDocument>) {
    document
        .history
        .borrow_mut()
        .undo_into(&mut document.text.borrow_mut())
        .unwrap();
    document.text.reconcile_saved();
}

#[test]
fn undo_retires_queued_and_completed_backups_and_redo_is_recoverable() {
    let r = Recovery::new();
    let path = r.save(&r.memo, "saved.md");
    let original = r.memo.text.borrow().clone();
    for settled in [false, true] {
        append(&r.memo, "X");
        write_work_copy_of(&r.window, &r.live, &r.memo);
        if settled {
            r.flush();
        }
        undo_in_pane(
            &r.window,
            r.id,
            &r.memo,
            &r.live.states,
            &r.live.cache,
            false,
        );
        assert!(!r.memo.text.edited());
        r.flush();
        assert!(
            saving::restore_tabs(&r.window).is_empty(),
            "stale backup after Undo"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        undo_in_pane(
            &r.window,
            r.id,
            &r.memo,
            &r.live.states,
            &r.live.cache,
            true,
        );
        r.flush();
        let restored = saving::restore_tabs(&r.window);
        assert_eq!(restored.len(), 1);
        assert_eq!(*restored[0].0.text.borrow(), format!("{original}X"));
        undo_in_pane(
            &r.window,
            r.id,
            &r.memo,
            &r.live.states,
            &r.live.cache,
            false,
        );
        r.flush();
    }
}

#[test]
fn undo_keeps_a_backup_when_the_original_is_missing() {
    let r = Recovery::new();
    let path = r.save(&r.memo, "saved.md");
    let original = r.memo.text.borrow().clone();
    std::fs::remove_file(&path).unwrap();
    append(&r.memo, "X");
    undo(&r.memo);
    assert!(!r.memo.text.edited());
    r.flush();
    let restored = saving::restore_tabs(&r.window);
    assert_eq!(restored.len(), 1);
    assert_eq!(*restored[0].0.text.borrow(), original);
    assert_eq!(restored[0].0.file.borrow().path(), Some(path.as_path()));
}

#[test]
fn restored_memo_and_file_keep_their_actual_save_baselines() {
    let r = Recovery::new();
    r.flush();
    let restored = saving::restore_tabs(&r.window);
    let memo = &restored[0].0;
    append(memo, "X");
    undo(memo);
    assert!(memo.text.edited(), "an unsaved memo must stay dirty");

    let path = r.save(&r.memo, "saved.md");
    r.flush();
    append(&r.memo, "未保存");
    r.flush();
    let restored = saving::restore_tabs(&r.window);
    let file = &restored[0].0;
    append(file, "X");
    undo(file);
    assert!(file.text.edited());
    *file.text.borrow_mut() = std::fs::read_to_string(&path).unwrap();
    file.text.reconcile_saved();
    assert!(
        !file.text.edited(),
        "returning to the actual saved text is clean"
    );

    std::fs::write(&path, "外部で書き換えた別の本文").unwrap();
    let restored = saving::restore_tabs(&r.window);
    let file = &restored[0].0;
    assert!(file.outside.get());
    append(file, "X");
    undo(file);
    assert!(file.text.edited());
    *file.text.borrow_mut() = std::fs::read_to_string(&path).unwrap();
    file.text.reconcile_saved();
    assert!(
        file.text.edited(),
        "the old baseline is unknown after an outside change"
    );
    assert!(file.outside.get());
}

#[test]
fn missing_originals_keep_paths_and_independent_backups_across_restarts() {
    let r = Recovery::new();
    r.flush(); // Also recover the existing Untitled-1.
    let work = app_data::work_directory().unwrap();
    let paths = [
        r.directory.join("missing-a.md"),
        r.directory.join("missing-b.md"),
    ];
    for (index, path) in paths.iter().enumerate() {
        app_data::write_into(
            &work,
            &app_data::WorkCopy {
                origin: Some(path.clone()),
                untitled: 0,
                text: format!("原稿{index}"),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let restored = saving::restore_tabs(&r.window);
    assert_eq!(restored.len(), 3);
    let names: std::collections::HashSet<_> = restored
        .iter()
        .map(|(d, _)| app_data::work_file_name(&work_identity(&d.file.borrow())))
        .collect();
    assert_eq!(names.len(), 3);
    let mut files = Vec::new();
    for path in &paths {
        let document = restored
            .iter()
            .find(|(d, _)| d.file.borrow().path() == Some(path.as_path()))
            .unwrap()
            .0
            .clone();
        assert!(document.missing.get() && document.outside.get());
        r.add(document.clone());
        append(&document, "追記");
        files.push(document);
    }
    r.flush();
    let again = saving::restore_tabs(&r.window);
    for path in &paths {
        let document = &again
            .iter()
            .find(|(d, _)| d.file.borrow().path() == Some(path.as_path()))
            .unwrap()
            .0;
        assert!(document.text.borrow().ends_with("追記"));
    }
    save_document(&r.window, &r.live, false);
    assert!(matches!(
        *r.live.pending.borrow(),
        Some(Question::MissingFile(_))
    ));
    assert!(!paths[1].exists());
    answer_question(&r.window, &r.live, 1);
    r.save(&files[0], "rescued.md");
    r.flush();
    let again = saving::restore_tabs(&r.window);
    assert_eq!(again.len(), 2);
    assert!(
        again
            .iter()
            .any(|(d, _)| d.file.borrow().path() == Some(paths[1].as_path()))
    );
    assert!(again.iter().any(|(d, _)| d.file.borrow().path().is_none()));

    std::fs::write(&paths[1], "戻ってきた外部版").unwrap();
    saving::check_external_change(&r.window, &r.live);
    assert!(files[1].outside.get());
    assert!(files[1].text.borrow().ends_with("追記"));
}

#[test]
fn save_all_defers_open_destinations_and_handles_duplicate_choices_and_cancel() {
    let r = Recovery::new();
    let path = r.save(&r.memo, "existing.md");
    let baseline = std::fs::read_to_string(&path).unwrap();
    let a = OpenDocument::untitled(2, r.window.as_weak());
    *a.text.borrow_mut() = "別の本文A".into();
    r.add(a.clone());
    r.flush();
    saving::save_all_with_choice(&r.window, &r.live, |_| {
        Some((path.clone(), file_io::TextForm::default()))
    });
    assert!(a.file.borrow().path().is_none());
    assert!(a.text.edited());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), baseline);
    assert!(r.window.get_render_status().contains("保留"));
    assert_eq!(open_documents(&r.live).len(), 2);

    append(&r.memo, "未保存");
    std::fs::write(&path, "外部版").unwrap();
    r.flush();
    saving::save_all_with_choice(&r.window, &r.live, |_| {
        Some((path.clone(), file_io::TextForm::default()))
    });
    assert!(r.memo.text.edited() && a.text.edited());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "外部版");
    r.flush();
    assert_eq!(saving::restore_tabs(&r.window).len(), 2);

    let b = OpenDocument::untitled(3, r.window.as_weak());
    *b.text.borrow_mut() = "別の本文B".into();
    r.add(b.clone());
    let shared = r.directory.join("chosen.md");
    saving::save_all_with_choice(&r.window, &r.live, |_| {
        Some((shared.clone(), file_io::TextForm::default()))
    });
    assert_eq!(a.file.borrow().path(), Some(shared.as_path()));
    assert!(b.file.borrow().path().is_none() && b.text.edited());
    assert_eq!(std::fs::read_to_string(&shared).unwrap(), "別の本文A");
    assert_eq!(
        open_documents(&r.live)
            .iter()
            .filter(|d| d.file.borrow().path() == Some(shared.as_path()))
            .count(),
        1
    );

    let c = OpenDocument::untitled(4, r.window.as_weak());
    *c.text.borrow_mut() = "別の本文C".into();
    r.add(c.clone());
    let mut calls = 0;
    saving::save_all_with_choice(&r.window, &r.live, |_| {
        calls += 1;
        None
    });
    assert_eq!(
        calls, 1,
        "Cancel stops asking for remaining unnamed documents"
    );
    assert!(b.text.edited() && c.text.edited());
    r.flush();
    assert_eq!(saving::restore_tabs(&r.window).len(), 3);

    let chosen_form = file_io::TextForm {
        encoding: file_io::Encoding::Utf16Le,
        byte_order_mark: true,
        newline: file_io::Newline::Crlf,
        ..Default::default()
    };
    saving::save_all_with_choice(&r.window, &r.live, |d| {
        Some((
            r.directory
                .join(format!("memo-{}.txt", d.file.borrow().untitled_number())),
            chosen_form,
        ))
    });
    assert!(!b.text.edited() && !c.text.edited());
    assert_eq!(b.file.borrow().form(), chosen_form);
}
