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
            panels: Default::default(),
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
    assert!(
        matches!(*live.pending.borrow(), Some(Question::ChangeWorkspace(Some(value))) if value == first)
    );
    answer_question(&window, &live, 2);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert!(Rc::ptr_eq(&live.states.document(id), &document));
    workspace_duplicate(&window, &live, first, "Second".into());
    let second = runtime.borrow().manager_selection().unwrap();
    workspace_rename(&window, &live, second, "Renamed".into());
    workspace_default_toggled(&window, &live);
    assert_eq!(
        runtime.borrow().registry().default_workspace(),
        Some(second)
    );
    assert_eq!(window.get_workspace_rows().row_count(), 2);
    workspace_row_context_chosen(&window, &live, 0);
    assert_eq!(runtime.borrow().active_workspace(), None);
    assert_eq!(runtime.borrow().manager_selection(), Some(first));
    assert!(live.pending.borrow().is_none());
    workspace_row_context_chosen(&window, &live, 1);
    assert_eq!(runtime.borrow().manager_selection(), Some(second));
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
    workspace_manager_requested(&window, &live);
    assert!(!window.get_workspace_manager_open());
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
    workspace_row_chosen(&window, &live, 0);
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
    assert_eq!(runtime.borrow().active_workspace(), Some(second));
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

    // Clone completion registers contents in the chosen scope. Use local
    // completed-clone fixtures: network/authentication belongs to the worker.
    request_workspace_change(&window, &live, Some(first));
    let clone_a = directory.join("clone-a");
    let clone_b = directory.join("clone-b");
    let clone_new = directory.join("clone-new");
    for path in [&clone_a, &clone_b, &clone_new] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("same.md"), "clone contents").unwrap();
    }
    workspace_register_clone(&window, &live, clone_a.clone(), Some(first));
    workspace_register_clone(&window, &live, clone_b.clone(), Some(first));
    assert_eq!(runtime.borrow().active_workspace(), Some(first));
    assert_eq!(runtime.borrow().registry().workspaces().len(), 1);
    assert_eq!(runtime.borrow().active_roots().len(), 2);
    let paths = live.tree_paths.borrow().clone();
    let drawn = window.get_left_rows();
    let a_file = clone_a.canonicalize().unwrap().join("same.md");
    let b_file = clone_b.canonicalize().unwrap().join("same.md");
    let a_row = paths.iter().position(|path| path == &a_file).unwrap();
    let b_row = paths.iter().position(|path| path == &b_file).unwrap();
    for row in [a_row, b_row] {
        let entry = drawn.row_data(row).unwrap();
        assert_eq!(entry.name, "same.md");
        assert_eq!(entry.depth, 0);
        assert!(!entry.is_root);
    }
    assert_ne!(
        a_row, b_row,
        "same basenames retain distinct root ownership"
    );
    let a_root = paths
        .iter()
        .position(|path| path == a_file.parent().unwrap())
        .unwrap();
    let b_root = paths
        .iter()
        .position(|path| path == b_file.parent().unwrap())
        .unwrap();
    for (root_row, label, full_path) in [
        (a_root, "clone-a", a_file.parent().unwrap()),
        (b_root, "clone-b", b_file.parent().unwrap()),
    ] {
        assert_eq!(
            window.get_tree_root_labels().row_data(root_row).unwrap(),
            label
        );
        let row = drawn.row_data(root_row).unwrap();
        assert!(row.is_root);
        assert_eq!(
            row.name.as_str(),
            full_path.to_string_lossy().as_ref(),
            "tooltip retains full root path"
        );
    }
    window.set_tree_filter("no-matching-file".into());
    tree_filter_changed(&window, &live);
    assert!(window.get_left_rows().iter().all(|row| row.folder));
    window.set_tree_filter("SAME".into());
    tree_filter_changed(&window, &live);
    assert_eq!(
        window
            .get_left_rows()
            .iter()
            .filter(|row| !row.folder)
            .count(),
        2
    );
    window.set_tree_filter("".into());
    tree_filter_changed(&window, &live);
    let folders = runtime
        .borrow()
        .registry()
        .workspace(first)
        .unwrap()
        .folders
        .clone();
    workspace_context_requested(&window, &live, a_row as i32);
    workspace_tree_command(&window, &live, 2);
    assert_eq!(
        runtime.borrow().registry().folder(folders[0]).unwrap().mode,
        workspace::SaveMode::AutoSave
    );
    assert_eq!(
        runtime.borrow().registry().folder(folders[1]).unwrap().mode,
        workspace::SaveMode::Recovery
    );
    workspace_context_requested(&window, &live, -1);
    assert!(live.folder.borrow().selected.is_none());
    workspace_tree_command(&window, &live, 2);
    assert_eq!(
        runtime.borrow().registry().folder(folders[0]).unwrap().mode,
        workspace::SaveMode::AutoSave
    );

    // The section highlight follows the edited document, not a selected file
    // or a right-clicked root, even when both files have the same basename.
    open_path_in_pane(&window, &live, id, &a_file, Opening::Kept);
    assert_eq!(
        window
            .get_tree_pane_root_indices()
            .row_data(id.index() as usize),
        Some(a_root as i32)
    );
    workspace_context_requested(&window, &live, b_row as i32);
    publish_tabs(&window, &live);
    assert_eq!(
        window
            .get_tree_pane_root_indices()
            .row_data(id.index() as usize),
        Some(a_root as i32)
    );
    workspace_context_requested(&window, &live, b_root as i32);
    publish_tabs(&window, &live);
    assert_eq!(
        window
            .get_tree_pane_root_indices()
            .row_data(id.index() as usize),
        Some(a_root as i32)
    );
    open_path_in_pane(&window, &live, id, &b_file, Opening::Kept);
    assert_eq!(
        window
            .get_tree_pane_root_indices()
            .row_data(id.index() as usize),
        Some(b_root as i32)
    );
    // Returning to an already open tab must update without a tree refresh.
    let a_tab = live
        .tabs
        .borrow()
        .of(id)
        .tabs
        .iter()
        .position(|tab| tab.document.file.borrow().path() == Some(a_file.as_path()))
        .unwrap();
    switch_to_tab(&window, &live, id, a_tab);
    assert_eq!(
        window
            .get_tree_pane_root_indices()
            .row_data(id.index() as usize),
        Some(a_root as i32)
    );

    // Renaming a target updates only Auto Save reference sources, including
    // the current unsaved buffer without losing its independent draft text.
    let auto_reference = clone_a.join("auto-reference.md");
    let manual_reference = clone_b.join("manual-reference.md");
    std::fs::write(&auto_reference, "[label](../clone-b/same.md)\r\n").unwrap();
    std::fs::write(&manual_reference, "[label](./same.md)\n").unwrap();
    let reading_reference = clone_a.join("reading-reference.md");
    std::fs::write(&reading_reference, "[read](../clone-b/same.md)").unwrap();
    open_path_in_pane(&window, &live, id, &reading_reference, Opening::Kept);
    let reading_document = live.states.document(id);
    live.states.of(id).borrow_mut().viewer = true;
    switch_to_tab(&window, &live, id, a_tab);
    live.states
        .document(id)
        .text
        .borrow_mut()
        .push_str("\n[ref](../clone-b/same.md)\nUnfinished draft");
    let renamed = b_file.parent().unwrap().join("renamed.md");
    move_entry(&window, &live, &b_file, &renamed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while live.folder.borrow().link_move_job.is_some() && Instant::now() < deadline {
        link_move::tick(&window, &live);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(live.folder.borrow().link_move_job.is_none());
    assert_eq!(
        std::fs::read_to_string(&auto_reference).unwrap(),
        "[label](../clone-b/renamed.md)\r\n"
    );
    assert_eq!(
        std::fs::read_to_string(&manual_reference).unwrap(),
        "[label](./same.md)\n"
    );
    let updated = live.states.document(id).text.borrow().clone();
    assert!(updated.contains("[ref](../clone-b/renamed.md)"));
    assert!(updated.ends_with("Unfinished draft"));
    assert_eq!(
        std::fs::read_to_string(&reading_reference).unwrap(),
        "[read](../clone-b/same.md)"
    );
    assert_eq!(
        reading_document.text.borrow().as_str(),
        "[read](../clone-b/same.md)"
    );

    // Detaching a root from a file context is transactional with dirty tabs.
    let dirty_clone = live.states.document(id);
    dirty_clone
        .text
        .borrow_mut()
        .push_str(" unsaved clone edit");
    dirty_clone.text.set_edited(true);
    workspace_context_requested(&window, &live, a_row as i32);
    workspace_tree_command(&window, &live, 3);
    assert!(live.pending.borrow().is_some());
    answer_question(&window, &live, 2);
    assert_eq!(runtime.borrow().active_workspace(), Some(first));
    assert_eq!(runtime.borrow().active_roots().len(), 2);
    assert!(Rc::ptr_eq(&live.states.document(id), &dirty_clone));
    assert!(dirty_clone.text.edited());

    // A clone from the initial view creates its own Workspace, and opening
    // it must still honor Save All / cancel rather than discard current text.
    window.set_tree_filter("SAME".into());
    tree_filter_changed(&window, &live);
    workspace_register_clone(&window, &live, clone_new.clone(), None);
    assert_eq!(runtime.borrow().registry().workspaces().len(), 2);
    assert_eq!(runtime.borrow().active_workspace(), Some(first));
    assert!(live.pending.borrow().is_some());
    answer_question(&window, &live, 0);
    let created = runtime.borrow().active_workspace().unwrap();
    assert!(window.get_tree_filter().is_empty());
    assert_ne!(created, first);
    assert!(
        window
            .get_tree_pane_root_indices()
            .iter()
            .all(|index| index == -1),
        "empty tabs in the newly opened Workspace must not retain a previous root highlight"
    );
    assert_eq!(
        runtime.borrow().active_roots(),
        vec![clone_new.canonicalize().unwrap()]
    );
    assert!(
        std::fs::read_to_string(&a_file)
            .unwrap()
            .ends_with(" unsaved clone edit")
    );
    assert!(!window.get_workspace_manager_open());
    // Invalid Clone input stays in the same form, preserving both fields.
    workspace_clone_requested(&window, &live);
    assert!(window.get_question_is_clone());
    window.set_question_name("not-a-repository-url".into());
    window.set_workspace_clone_destination(clone_new.display().to_string().into());
    workspace_clone_url_check(&window, &live);
    workspace_clone_destination_check(&window, &live);
    assert!(!window.get_workspace_clone_url_error().is_empty());
    assert!(!window.get_workspace_clone_destination_error().is_empty());
    assert!(live.folder.borrow().clone_job.is_none());
    answer_question(&window, &live, 0);
    assert!(window.get_question_open());
    assert!(!window.get_workspace_clone_url_error().is_empty());
    assert_eq!(window.get_question_name(), "not-a-repository-url");
    assert!(live.folder.borrow().clone_job.is_none());
    window.set_question_name("https://example.invalid/repo".into());
    answer_question(&window, &live, 0);
    assert!(window.get_question_open());
    assert!(
        live.folder.borrow().clone_job.is_none(),
        "nonempty folder is rejected before network access"
    );
    answer_question(&window, &live, 1);
    assert!(!window.get_question_open());
    assert!(live.pending.borrow().is_none());
}
