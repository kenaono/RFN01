//! 要件 7.10: 印刷プレビューが開き、紙を繰れて、閉じれば消えること。
//!
//! **プリンタへは出さない。**ここで確かめるのは画面のほうで、本当に刷る試験は
//! `print_tests`にある（`--ignored`）。
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

/// 紙が1枚映り、繰れて、閉じれば消える。
#[test]
fn the_print_preview_shows_a_sheet_and_turns_it() {
    let directory = std::env::temp_dir().join(format!(
        "editor-print-{}-{}",
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
    let (width, height) = (1000usize, 740usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);

    // 紙を何枚も要る長さに。**段落ごとに1行**で、紙の側で折り返させる。
    let source: String = (0..80)
        .map(|at| format!("{at}段落目。紙を何枚も要る長さにするための本文です。\n\n"))
        .collect();
    let path = directory.join("原稿.md");
    std::fs::write(&path, &source).unwrap();
    let (file, text) = DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let document = OpenDocument::new(file, text, window.as_weak());
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&document),
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
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
        screen.shown_width = 950.0;
        screen.shown_height = 620.0;
        screen.preview = true;
    });
    window.show().unwrap();
    set_pane_direction(&window, &live.cache, id, true);
    refresh_pane_from_state(
        &window,
        &live.cache,
        &document,
        id,
        &live.states.of(id),
        &source,
    );

    print_view::open(&window, &live);
    assert!(window.get_print_active(), "the preview must open");
    let pages = window.get_print_pages();
    assert!(pages > 1, "this document must need several sheets: {pages}");
    assert_eq!(window.get_print_at(), 0, "it opens at the first sheet");

    let first = shot(&surface, width, height);
    // **紙が映っている。**白い地に黒い字で、どちらもそれなりの量がある。
    let (paper, ink) = tones(&first);
    assert!(
        paper > 60_000,
        "the sheet must be showing: {paper} white px"
    );
    assert!(ink > 2_000, "and carry text: {ink} dark px");

    print_view::turn(&window, &live, 1);
    assert_eq!(window.get_print_at(), 1, "the sheet must turn");
    let second = shot(&surface, width, height);
    assert!(first != second, "a different sheet must be drawn");

    // 端の外へは繰らない。
    print_view::turn(&window, &live, -1);
    assert_eq!(
        window.get_print_at(),
        1,
        "before the first sheet is nowhere"
    );
    print_view::turn(&window, &live, pages);
    assert_eq!(window.get_print_at(), 1, "and neither is past the last");

    if let Ok(into) = std::env::var("EDITOR_PRINT_OUT") {
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in &second {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        let _ = std::fs::create_dir_all(&into);
        let _ = std::fs::write(std::path::Path::new(&into).join("preview.ppm"), ppm);
    }

    // **1行の字数を決めるのは紙の側**（書き手の指摘 2026-09-16）。1字ずつ動かせて、
    // 紙の姿はその場で言い直される。
    let before = window.get_print_paper_note().to_string();
    assert!(
        before.contains('字') || before.contains('×'),
        "the sheet must say how many characters fit: {before}"
    );
    print_view::step_cells(&window, &live, -1);
    let shorter = window.get_print_paper_note().to_string();
    assert!(
        shorter != before,
        "one character fewer must change the sheet: {before} then {shorter}"
    );
    let short_pages = window.get_print_pages();
    print_view::step_cells(&window, &live, 1);
    assert_eq!(
        window.get_print_paper_note().to_string(),
        before,
        "and stepping back must put it where it was"
    );
    assert!(
        short_pages >= pages,
        "a shorter line needs at least as many sheets: {short_pages} against {pages}"
    );

    print_view::close(&window, &live);
    assert!(!window.get_print_active(), "Close must put the paper away");
    assert!(
        live.preview.borrow().is_none(),
        "and let the paper's layout go"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

fn shot(surface: &Rc<MinimalSoftwareWindow>, width: usize, height: usize) -> Vec<slint::Rgb8Pixel> {
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
    surface.request_redraw();
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    pixels
}

/// ほぼ白い画素と、ほぼ黒い画素の数。
fn tones(pixels: &[slint::Rgb8Pixel]) -> (usize, usize) {
    let white = pixels
        .iter()
        .filter(|pixel| pixel.r > 240 && pixel.g > 240 && pixel.b > 240)
        .count();
    let dark = pixels
        .iter()
        .filter(|pixel| pixel.r < 120 && pixel.g < 120 && pixel.b < 120)
        .count();
    (white, dark)
}
