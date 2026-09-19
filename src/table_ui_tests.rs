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

/// 書き手の求め 2026-09-15: **表は表のまま編集する。**
///
/// カーソルのあるセルに打った`|`は原稿に`\|`で入り（表が壊れない）、`Tab`で次のセルへ移る。
/// 太字の記号は編集中も見えたまま太字で組まれる。`EDITOR_SETTINGS_SNAPSHOT`があれば画像も書く。
#[test]
fn a_table_is_edited_as_a_table() {
    let directory = std::env::temp_dir().join(format!(
        "editor-table-{}-{}",
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
    surface.set_size(slint::PhysicalSize::new(1000, 400));
    window.set_tree_open(false);
    window.set_autosave(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let text = "| 名前 | 役割 |\n| --- | --- |\n| 主人公 | **語り手** |\n| 犬 | 相棒 |\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), text.into(), window.as_weak());
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
    window.show().unwrap();
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 300.0;
        screen.shown_width = 950.0;
        screen.shown_height = 300.0;
        screen.preview = true;
    });
    let state = live.states.of(id);
    let caret = || state.borrow().caret_source_byte.unwrap();
    let source = || document.text.borrow().clone();
    let put = |byte: usize| {
        {
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(byte);
            state.selection_anchor_source_byte = Some(byte);
        }
        refresh_pane_from_state(&window, &live.cache, &document, id, &state, &source());
    };

    // 「主人公」の終わりで`|`を打つ → 原稿には`\|`、表の列は増えない。
    put(text.find("主人公").unwrap() + "主人公".len());
    insert_pane_text(
        &window,
        id,
        &document,
        &live.states,
        &live.cache,
        "|",
        false,
    );
    assert!(
        source().contains("| 主人公\\| | **語り手** |"),
        "{}",
        source()
    );
    // `Tab`で次のセル（**語り手**の頭）へ。
    tab_in_pane(&window, &live, id, false);
    assert_eq!(caret(), source().find("**語り手**").unwrap());
    // もう一度で、次の行の最初のセルへ。
    tab_in_pane(&window, &live, id, false);
    assert_eq!(caret(), source().find("犬").unwrap());
    // `Shift+Tab`で前の行の最後のセルへ戻る。
    tab_in_pane(&window, &live, id, true);
    assert_eq!(caret(), source().find("**語り手**").unwrap());

    if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
        put(source().find("語り手").unwrap() + "語り".len());
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 400];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let mut ppm = b"P6\n1000 400\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(PathBuf::from(output).join("table-editing.ppm"), ppm).unwrap();
    }
}
