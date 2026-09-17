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
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(fonts.clone()));
    let (width, height) = (1000usize, 740usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);

    // 紙を何枚も要る長さに。**段落ごとに1行**で、紙の側で折り返させる。
    let source: String = std::iter::once("# 見出し\n\n".to_owned())
        .chain(
            (0..80)
                .map(|at| format!("{at}段落目。**紙を何枚も要る長さ**にするための本文です。\n\n")),
        )
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

    // 端では止まる（行き過ぎた先を断ると、見開きで端の1枚が出せなくなる）。
    print_view::turn(&window, &live, -3);
    assert_eq!(
        window.get_print_at(),
        0,
        "before the first sheet is the first"
    );
    print_view::turn(&window, &live, pages + 5);
    assert_eq!(
        window.get_print_at(),
        pages - 1,
        "and past the last is the last"
    );
    print_view::turn(&window, &live, 1);

    if let Ok(into) = std::env::var("EDITOR_PRINT_OUT") {
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in &second {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        let _ = std::fs::create_dir_all(&into);
        let _ = std::fs::write(std::path::Path::new(&into).join("preview.ppm"), ppm);
    }

    // **余白で行の長さが決まる**（書き手の指摘 2026-09-16：「フォントとサイズを
    // 設定して、組版するのが正しい」）。紙の姿はその場で言い直される。
    let before = window.get_print_paper_note().to_string();
    assert!(
        before.contains("mm"),
        "the sheet must say its measurements: {before}"
    );
    print_view::step_margin(&window, &live, 1);
    let wider = window.get_print_paper_note().to_string();
    assert!(
        wider != before,
        "a wider margin must change the sheet: {before} then {wider}"
    );
    let narrow_pages = window.get_print_pages();
    print_view::step_margin(&window, &live, -1);
    assert_eq!(
        window.get_print_paper_note().to_string(),
        before,
        "and stepping back must put it where it was"
    );
    assert!(
        narrow_pages >= pages,
        "a wider margin needs at least as many sheets: {narrow_pages} against {pages}"
    );

    // **画面で設定したとおりに刷る**（書き手の決定 2026-09-16）。既定では紙の
    // 大きさを持たず、画面の設定がそのまま行く。
    assert_eq!(
        window.get_print_size(),
        0,
        "by default the paper follows the screen"
    );
    let paper_note = window.get_print_paper_note().to_string();
    // **本文の大きさだけは紙のものを持てる**（任意）。1度押せば画面のいまの大きさから。
    print_view::step_size(&window, &live, -4);
    let smaller = window.get_print_paper_note().to_string();
    assert!(
        window.get_print_size() > 0,
        "stepping must take the paper off the screen's size"
    );
    assert!(
        smaller != paper_note,
        "2pt smaller must change the paper: {paper_note} then {smaller}"
    );
    // そして画面へ戻せる。
    print_view::use_screen_size(&window, &live);
    assert_eq!(
        window.get_print_size(),
        0,
        "and it can go back to the screen"
    );
    assert_eq!(
        window.get_print_paper_note().to_string(),
        paper_note,
        "back to what the screen says"
    );

    // **画面で決めた色と字体は紙にも行く**（書き手の指摘 2026-09-16：「画面で見出しに
    // 色指定していても、印刷でフォント変更すると色が無視されます」）。見出しのH1に
    // 赤を置き、コードだけ別の字体にして、紙の体裁がそれを持っていることを見る。
    set_colour(&palette, 1, 1, [1.0, 0.0, 0.0]);
    fonts.set_row_data(font_row(1, CODE_SLOT), "MS Gothic".into());
    fonts.set_row_data(font_row(1, 0), "Yu Mincho".into());
    let spec = print_view::paper_typography(&window, WritingMode::Vertical, true);
    assert_eq!(
        spec.heading_ink[0],
        [1.0, 0.0, 0.0],
        "the heading's colour must reach the paper"
    );
    assert_eq!(spec.body_font, "Yu Mincho", "and the body's own face");
    assert_eq!(
        spec.code_font, "MS Gothic",
        "and code keeps its own face — the paper does not put everything in one"
    );
    assert!(
        !spec.line_numbers,
        "the line numbers are for editing, not paper"
    );

    // **ソースのTABはソースのまま刷る**（書き手の指摘 2026-09-16：「ソースで印刷を
    // 選択したら、ソースのまま印刷されるのが正しい」）。記号が字としてそこにあり、
    // 見出しも本文と同じ大きさ・同じ字体で出る。
    let laid = print_view::paper_typography(&window, WritingMode::Vertical, true);
    assert!(
        laid.heading_scale[0] > 1.0,
        "the preview sets a heading larger than the body"
    );
    let raw = print_view::paper_typography(&window, WritingMode::Vertical, false);
    assert_eq!(
        raw.heading_scale, [1.0; MAX_HEADING_LEVEL],
        "the source sets every line at one size"
    );
    assert_eq!(
        raw.heading_ink[0], raw.ink,
        "and in one ink — the colours belong to the formatted view"
    );
    assert_eq!(raw.code_font, raw.body_font, "and in one face");

    // **縮めれば並ぶ枚数が増える**（書き手の求め 2026-09-17）。6枚まで。
    print_view::turn(&window, &live, 0);
    let close = window.get_print_sheets().row_count();
    for _ in 0..12 {
        print_view::step_zoom(&window, &live, -1);
    }
    let far = window.get_print_sheets().row_count();
    assert!(
        far > close,
        "zooming out must put more sheets on screen: {close} then {far}"
    );
    assert!(
        far <= 8,
        "and the smallest size is the one six sheets fit at: {far}"
    );
    // 拡大すれば1枚だけになる。**場所からはみ出してよい**（ルビを大きく見る）。
    for _ in 0..12 {
        print_view::step_zoom(&window, &live, 1);
    }
    assert_eq!(
        window.get_print_sheets().row_count(),
        1,
        "zooming right in leaves the one sheet"
    );
    assert!(
        window.get_print_zoom() > 100,
        "and it may grow past the room: {}%",
        window.get_print_zoom()
    );
    // 掴んで寄せた量は、繰るたび・拡大のたびに戻る。
    window.set_print_pan_x(40.0);
    print_view::step_zoom(&window, &live, -1);
    assert_eq!(
        window.get_print_pan_x(),
        0.0,
        "a new size starts from the middle again"
    );
    for _ in 0..12 {
        print_view::step_zoom(&window, &live, -1);
    }

    // **天地には字を書く**（書き手の求め 2026-09-17）。決まったものを選ばせるので
    // はなく、「第一稿　3 / 17」のように並べられる。既定は地の真ん中にノンブル。
    assert_eq!(
        window
            .get_print_foot()
            .row_data(1)
            .unwrap_or_default()
            .to_string(),
        "{\u{30da}\u{30fc}\u{30b8}} / {\u{7dcf}\u{6570}}",
        "the page number stands in the middle of the foot"
    );
    assert_eq!(
        window
            .get_print_head()
            .row_data(0)
            .unwrap_or_default()
            .to_string(),
        "",
        "and nothing at the head"
    );
    let bare = shot(&surface, width, height);
    let wanted = "\u{7b2c}\u{4e00}\u{7a3f}\u{3000}{\u{30d5}\u{30a1}\u{30a4}\u{30eb}\u{540d}}";
    print_view::write_trim(&window, &live, true, 0, wanted);
    assert_eq!(
        window
            .get_print_head()
            .row_data(0)
            .unwrap_or_default()
            .to_string(),
        wanted,
        "what was written is what is kept"
    );
    let with_head = shot(&surface, width, height);
    print_view::write_trim(&window, &live, true, 0, "");
    let without = shot(&surface, width, height);
    assert!(with_head != without, "what is written reaches the paper");
    assert!(
        without == bare,
        "and taking it away puts the sheet back as it was"
    );

    // そして**紙に出る本文そのものが変わる**：`**`も`#`も字としてそこにある。
    print_view::close(&window, &live);
    id.update_screen(&window, |screen| screen.preview = false);
    print_view::open(&window, &live);
    let as_source = shot(&surface, width, height);
    assert!(
        as_source != first,
        "the source must not print as the formatted view"
    );
    id.update_screen(&window, |screen| screen.preview = true);

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
