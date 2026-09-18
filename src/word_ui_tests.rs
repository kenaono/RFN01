use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

// Run separately from other font rendering tests.

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
#[ignore = "offscreen word mode verification"]
fn word_mode_selection_repaints_existing_text() {
    let directory = std::env::temp_dir().join(format!(
        "editor-words-{}-{}",
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
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
    });
    window.set_autosave(true);
    let source = include_str!("../testdata/12_単語チェックモード.md");
    let source_path = directory.join("本文.md");
    std::fs::write(&source_path, source).unwrap();
    let (file, text) = DocumentFile::open(&source_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let memo = OpenDocument::new(file, text, window.as_weak());
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

    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
        screen.shown_width = 950.0;
        screen.shown_height = 620.0;
        screen.preview = true;
    });
    window.show().unwrap();

    hold_word_modes(
        &window,
        &live,
        vec![word_marks::WordMode {
            id: 1,
            name: "確認用".into(),
            groups: vec![word_marks::WordGroup {
                id: 2,
                name: "人物".into(),
                colour: Some([0.7, 0.16, 0.16]),
                words: vec![
                    "単語チェックモード".into(),
                    "モード".into(),
                    "語群".into(),
                    "佐藤".into(),
                ],
            }],
        }],
        false,
    );
    // Exercise the E11 body/heading styles that could otherwise cover word colours.
    for sheet in 0..2 {
        for slot in 0..7 {
            for kind in 0..4 {
                Setting::Decoration(slot, kind).write(&numbers, sheet, 1);
            }
            set_colour(&palette, sheet, slot, [0.1, 0.2, 0.1]);
            set_colour(&palette, sheet, 8 + slot, [1.0, 0.9, 0.9]);
        }
    }
    for vertical in [false, true] {
        set_pane_direction(&window, &live.cache, id, vertical);
        for preview in [false, true] {
            id.update_screen(&window, |screen| screen.preview = preview);
            let mut counts = Vec::new();
            for mode in [0, 1, 0] {
                set_word_mode_of(&window, &live, id, mode);
                refresh_pane_from_state(
                    &window,
                    &live.cache,
                    &memo,
                    id,
                    &live.states.of(id),
                    source,
                );
                let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
                window.window().request_redraw();
                surface.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, 1000);
                });
                counts.push(
                    pixels
                        .iter()
                        .filter(|p| p.r > 150 && p.g < 100 && p.b < 100)
                        .count(),
                );
            }
            assert!(
                counts[1] > counts[0] + 20,
                "mode enables colour: {counts:?}"
            );
            assert!(counts[2] < counts[1], "Off removes colour: {counts:?}");
        }
    }
    assert_eq!(&*memo.text.borrow(), source);
    window.hide().unwrap();
}
