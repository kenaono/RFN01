use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn log_lines(from: usize, to: usize) -> String {
    (from..to)
        .map(|number| format!("2026-09-15 12:00:{number:02} ログの行 {number}\n"))
        .collect()
}

/// Exercise management handlers against an isolated live window and registry.
#[test]
fn manager_callbacks_enforce_workspace_transitions_and_boundaries() {
    let directory = std::env::temp_dir().join(format!(
        "editor-workspace-manager-{}-{}",
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
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    window.set_autosave(false);
    let workspace_root = directory.join("registered");
    let explorer_root = directory.join("explorer");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::create_dir_all(&explorer_root).unwrap();
    let explorer_selected = explorer_root.join("selected.md");
    std::fs::write(&explorer_selected, "Explorer selection").unwrap();
    let log_path = workspace_root.join("app.log");
    std::fs::write(&log_path, log_lines(0, 80)).unwrap();
    let (file, text) = DocumentFile::open(&log_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let document = OpenDocument::new(file, text, window.as_weak());
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&document),
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
            panes: vec![{
                let tab = PaneTab::showing(&window, id, document.clone());
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
    let runtime = Rc::new(RefCell::new(workspace_ui::Runtime::open(directory.clone())));
    live.folder.borrow_mut().workspace = Some(runtime.clone());
    live.folder.borrow_mut().root = Some(explorer_root.clone());
    live.folder
        .borrow_mut()
        .expanded
        .insert(explorer_root.clone());
    live.folder.borrow_mut().selected = Some(explorer_selected.clone());
    document.text.borrow_mut().push_str("unsaved edit");
    document.text.set_edited(true);
    workspace_manager_requested(&window, &live);
    assert!(window.get_workspace_manager_open());
    workspace_create(&window, &live, "First".into());
    let first = runtime.borrow().manager_selection().unwrap();
    workspace_duplicate(&window, &live, first, "Second".into());
    let second = runtime.borrow().manager_selection().unwrap();
    workspace_rename(&window, &live, second, "Renamed".into());
    workspace_default_toggled(&window, &live);
    assert_eq!(
        runtime.borrow().registry().default_workspace(),
        Some(second)
    );
    assert_eq!(window.get_workspace_rows().row_count(), 2);
    let folder = runtime
        .borrow_mut()
        .edit(|registry| registry.add_root(second, &workspace_root))
        .unwrap();
    publish_workspace_manager(&window, &live);
    workspace_folder_mode_toggled(&window, &live, 0);
    assert_eq!(
        runtime.borrow().registry().folder(folder).unwrap().mode,
        workspace::SaveMode::AutoSave
    );
    switch_workspace(&window, &live, Some(second));
    let hidden = PaneId::from_index(1);
    let readonly = OpenDocument::snapshot(
        "Outside version".into(),
        document.file.borrow().form(),
        "read-only text".into(),
        window.as_weak(),
    );
    live.states.add(&readonly);
    let hidden_tab = PaneTab::showing(&window, hidden, readonly.clone());
    live.tabs.borrow_mut().add(PaneTabs {
        history: vec![NavigationPlace::from(&hidden_tab)],
        tabs: vec![hidden_tab],
        ..Default::default()
    });

    // Cancel is transactional, including hidden/read-only tabs and history.
    request_workspace_change(&window, &live, Some(first));
    assert!(
        matches!(*live.pending.borrow(), Some(Question::ChangeWorkspace(Some(value))) if value == first)
    );
    answer_question(&window, &live, 2);
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    assert!(Rc::ptr_eq(&live.states.document(id), &document));
    assert!(document.text.edited());
    assert!(document.text.borrow().ends_with("unsaved edit"));
    assert_eq!(live.tabs.borrow().of(id).tabs.len(), 1);
    assert!(Rc::ptr_eq(
        &live.tabs.borrow().of(hidden).tabs[0].document,
        &readonly
    ));
    assert_eq!(live.tabs.borrow().of(hidden).history.len(), 1);

    // Save All cannot complete after an external change. Keep the whole scope.
    std::fs::write(&log_path, "externally replaced content").unwrap();
    request_workspace_change(&window, &live, Some(first));
    answer_question(&window, &live, 0);
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    assert!(document.text.edited());
    assert!(Rc::ptr_eq(
        &live.tabs.borrow().of(id).tabs[0].document,
        &document
    ));
    assert!(Rc::ptr_eq(
        &live.tabs.borrow().of(hidden).tabs[0].document,
        &readonly
    ));

    // An Explorer view change must preserve its own folder location and tabs.
    window.set_left_tab(0);
    publish_left(&window, &live);
    assert_eq!(live.folder.borrow().root.as_ref(), Some(&explorer_root));
    assert!(live.folder.borrow().expanded.contains(&explorer_root));
    assert_eq!(
        live.folder.borrow().selected.as_ref(),
        Some(&explorer_selected)
    );
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    assert!(Rc::ptr_eq(
        &live.tabs.borrow().of(id).tabs[0].document,
        &document
    ));
    window.set_left_tab(4);
    publish_left(&window, &live);
    assert_eq!(
        live.folder.borrow().root,
        Some(workspace_root.canonicalize().unwrap())
    );

    let outside_path = explorer_root.join("outside.md");
    std::fs::write(&outside_path, "outside").unwrap();
    open_path_in_pane(&window, &live, id, &outside_path, Opening::Kept);
    assert!(Rc::ptr_eq(
        &live.tabs.borrow().of(id).tabs[0].document,
        &document
    ));
    assert!(!live.recent.borrow().contains(&outside_path));
    assert!(!saving::write_document_to(
        &window,
        &live,
        &document,
        outside_path.clone()
    ));
    assert_eq!(std::fs::read_to_string(&outside_path).unwrap(), "outside");

    // The second discard confirmation can still cancel without side effects.
    request_workspace_change(&window, &live, Some(first));
    answer_question(&window, &live, 1);
    assert!(
        matches!(*live.pending.borrow(), Some(Question::DiscardForWorkspace(Some(value))) if value == first)
    );
    answer_question(&window, &live, 1);
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    assert!(document.text.edited());
    request_workspace_change(&window, &live, Some(first));
    answer_question(&window, &live, 1);
    answer_question(&window, &live, 0);
    assert_eq!(runtime.borrow().active_workspace(), Some(first));
    for strip in &live.tabs.borrow().panes {
        assert!(strip.tabs.iter().all(|tab| tab.empty));
        assert!(strip.history.iter().all(|place| {
            !Rc::ptr_eq(&place.document, &document) && !Rc::ptr_eq(&place.document, &readonly)
        }));
    }
    assert!(live.closed_tabs.borrow().is_empty());

    request_workspace_change(&window, &live, Some(second));
    open_path_in_pane(&window, &live, id, &log_path, Opening::Kept);
    let fresh = live.states.document(id);
    fresh.text.borrow_mut().push_str(" saved on switch");
    fresh.text.set_edited(true);
    request_workspace_change(&window, &live, Some(first));
    answer_question(&window, &live, 0);
    assert_eq!(runtime.borrow().active_workspace(), Some(first));
    assert!(
        std::fs::read_to_string(&log_path)
            .unwrap()
            .ends_with(" saved on switch")
    );
    assert!(!fresh.text.edited());
    assert!(live.tabs.borrow().of(id).tabs.iter().all(|tab| tab.empty));

    // Reset leaves the Workspace and presents its manager, without moving Explorer.
    request_workspace_change(&window, &live, Some(second));
    live.folder
        .borrow_mut()
        .expanded
        .insert(workspace_root.clone());
    workspace_reset_view(&window, &live, second);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert!(window.get_workspace_manager_open());
    assert!(live.folder.borrow().expanded.is_empty());
    window.set_left_tab(0);
    publish_left(&window, &live);
    assert_eq!(live.folder.borrow().root.as_ref(), Some(&explorer_root));
    assert!(live.folder.borrow().expanded.contains(&explorer_root));
    workspace_manager_requested(&window, &live);

    // Explicit external startup paths opt out without changing the default.
    switch_workspace(&window, &live, Some(second));
    open_startup_paths(&window, &live, id, &[outside_path.clone()]);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert_eq!(
        runtime.borrow().registry().default_workspace(),
        Some(second)
    );
    let recovered = live.states.document(id);
    assert_eq!(recovered.file.borrow().path(), Some(outside_path.as_path()));

    // A recovered dirty external document takes precedence over the default.
    recovered
        .text
        .borrow_mut()
        .push_str(" recovered unsaved work");
    recovered.text.set_edited(true);
    switch_workspace(&window, &live, Some(second));
    open_startup_paths(&window, &live, id, &[]);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert_eq!(
        runtime.borrow().registry().default_workspace(),
        Some(second)
    );
    assert!(Rc::ptr_eq(&live.states.document(id), &recovered));
    assert!(recovered.text.edited());
    assert!(recovered.text.borrow().ends_with(" recovered unsaved work"));

    // Clean external session tabs and their navigation references are pruned.
    recovered.text.mark_saved();
    switch_workspace(&window, &live, Some(second));
    open_startup_paths(&window, &live, id, &[]);
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    for strip in &live.tabs.borrow().panes {
        assert!(
            strip
                .tabs
                .iter()
                .all(|tab| !Rc::ptr_eq(&tab.document, &recovered))
        );
        assert!(
            strip
                .history
                .iter()
                .all(|place| !Rc::ptr_eq(&place.document, &recovered))
        );
    }
    request_workspace_change(&window, &live, None);
    // Restored unnamed text can be clean yet must still be offered Save As.
    let memo = live.states.document(id);
    assert!(memo.file.borrow().path().is_none());
    memo.text.borrow_mut().push_str("restored memo");
    memo.text.mark_saved();
    assert!(workspace_has_unsaved(&live));
    let memo_path = workspace_root.join("restored-memo.md");
    let mut offered = false;
    saving::save_all_including_memos_with_choice(&window, &live, true, |chosen| {
        assert!(Rc::ptr_eq(chosen, &memo));
        offered = true;
        Some((memo_path.clone(), chosen.file.borrow().form()))
    });
    assert!(
        offered,
        "nonempty unnamed clean documents still need a file"
    );
    assert_eq!(
        std::fs::read_to_string(&memo_path).unwrap(),
        "restored memo"
    );
    assert!(!workspace_has_unsaved(&live));
    request_workspace_change(&window, &live, Some(second));
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
    assert!(live.tabs.borrow().of(id).tabs.iter().all(|tab| tab.empty));
    request_workspace_change(&window, &live, None);
    workspace_manager_requested(&window, &live);
    workspace_row_chosen(&window, &live, 1);
    workspace_folder_detached(&window, &live, 0);
    assert_eq!(window.get_workspace_unused_folder_rows().row_count(), 1);
    workspace_remove_unused_folder(&window, &live, folder);
    workspace_remove(&window, &live, second);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert_eq!(window.get_workspace_rows().row_count(), 1);
    assert!(
        log_path.exists(),
        "registry cleanup must retain the document"
    );
    workspace_manager_closed(&window, &live);
    assert!(!window.get_workspace_manager_open());
}
