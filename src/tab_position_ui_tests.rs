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

/// 書き手の報告 2026-09-15: **縦書きで、閉じたTABを開き直すと表示が末尾になる。**
///
/// まだ誰も見ていない表示の位置を`scroll`の0で持っていた——縦書きの0は左端＝末尾で、
/// 同じ面のまま別の文書へ替われば、前の文書との長さの差だけ右端から離れた途中に出た。
/// 開いたばかりのTABは、向きを問わず文書の先頭に立つ。
#[test]
fn a_new_vertical_tab_opens_at_the_start() {
    let directory = std::env::temp_dir().join(format!(
        "editor-tab-position-{}-{}",
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
    let untitled = |number: u32, text: String| {
        let document = OpenDocument::untitled(number, window.as_weak());
        *document.text.borrow_mut() = text;
        document
    };
    let short = untitled(1, "春はあけぼの。\n".repeat(60));
    let long = untitled(2, "夏は夜。月のころはさらなり。\n".repeat(200));
    let live = Live {
        closed_tabs: Rc::default(),
        states: PaneStates::new(&short),
        folder: Rc::default(),
        tree_paths: Rc::default(),
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
            panes: vec![PaneTabs::default()],
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
        screen.preview = false;
    });
    set_pane_direction(&window, &live.cache, id, true);
    let start = || {
        let content = live
            .cache
            .borrow_mut()
            .pane(id)
            .graphics
            .engine
            .total_flow_size() as f32;
        -(content - id.viewport_flow(&window)).max(0.0)
    };
    let at_start = |what: &str| {
        assert!(
            start() < -100.0,
            "{what}: the document is wider than the pane"
        );
        assert!(
            (id.scroll(&window) - start()).abs() < 1.0,
            "{what}: scroll {} should be the start {}",
            id.scroll(&window),
            start()
        );
    };

    add_tab(
        &window,
        &live,
        id,
        PaneTab::showing(&window, id, short.clone()),
    );
    assert!(id.vertical(&window));
    at_start("the first tab");
    // 同じ面のまま、長い文書へ（前の文書との長さの差で途中に出ていた）。
    add_tab(
        &window,
        &live,
        id,
        PaneTab::showing(&window, id, long.clone()),
    );
    at_start("a longer document after a shorter one");
    // 閉じて、開き直す。
    assert!(close_tab(&window, &live, id, 1));
    add_tab(&window, &live, id, PaneTab::showing(&window, id, long));
    at_start("reopened");
}
