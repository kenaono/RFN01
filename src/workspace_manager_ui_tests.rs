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
fn manager_callbacks_preserve_documents_and_reset_active_tree() {
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
    let log_path = directory.join("app.log");
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
        .edit(|registry| registry.add_root(second, &directory))
        .unwrap();
    publish_workspace_manager(&window, &live);
    workspace_folder_mode_toggled(&window, &live, 0);
    assert_eq!(
        runtime.borrow().registry().folder(folder).unwrap().mode,
        workspace::SaveMode::AutoSave
    );
    switch_workspace(&window, &live, Some(second));
    live.folder.borrow_mut().expanded.insert(directory.clone());
    workspace_reset_view(&window, &live, second);
    assert!(
        live.folder.borrow().expanded.is_empty(),
        "reset must not resave the old active expansion"
    );
    assert!(Rc::ptr_eq(&live.states.document(id), &document));
    assert!(document.text.edited());
    assert!(document.text.borrow().ends_with("unsaved edit"));
    assert_eq!(live.tabs.borrow().of(id).tabs.len(), 1);
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
