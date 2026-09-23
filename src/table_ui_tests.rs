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
            no_tabs: Default::default(),
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

/// 試験の窓と1枚の文書（RFN01-49）。文書は`原稿.md`としてディスクに置く——画像の行の
/// 行き先を、文書のフォルダから読めるように。
struct Rig {
    surface: Rc<MinimalSoftwareWindow>,
    window: AppWindow,
    document: Rc<OpenDocument>,
    live: Live,
    id: PaneId,
    _reset: ResetDirectory,
}

struct ResetDirectory;
impl Drop for ResetDirectory {
    fn drop(&mut self) {
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
    }
}

fn rig(text: &str, size: (u32, u32)) -> Rig {
    let directory = std::env::temp_dir().join(format!(
        "editor-table-rig-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(directory.clone()));
    let reset = ResetDirectory;
    std::fs::write(
        directory.join("a.bmp"),
        crate::image_ui_tests::solid_bmp(120, 40, [255, 0, 0]),
    )
    .unwrap();
    let path = directory.join("原稿.md");
    std::fs::write(&path, text).unwrap();

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
    surface.set_size(slint::PhysicalSize::new(size.0, size.1));
    window.set_tree_open(false);
    window.set_autosave(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let (file, text) = DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS).unwrap();
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
            no_tabs: Default::default(),
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
        screen.width = size.0 as f32 - 50.0;
        screen.height = size.1 as f32 - 100.0;
        screen.shown_width = size.0 as f32 - 50.0;
        screen.shown_height = size.1 as f32 - 140.0;
        screen.preview = true;
    });
    Rig {
        surface,
        window,
        document,
        live,
        id,
        _reset: reset,
    }
}

impl Rig {
    fn source(&self) -> String {
        self.document.text.borrow().clone()
    }

    fn caret(&self) -> usize {
        self.live
            .states
            .of(self.id)
            .borrow()
            .caret_source_byte
            .unwrap()
    }

    /// キャレットを置き、その行を開いて組み直す。
    fn put(&self, byte: usize) {
        let state = self.live.states.of(self.id);
        {
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(byte);
            state.selection_anchor_source_byte = Some(byte);
        }
        let source = self.source();
        refresh_pane_from_state(
            &self.window,
            &self.live.cache,
            &self.document,
            self.id,
            &state,
            &source,
        );
    }

    fn undo(&self) {
        undo_in_pane(
            &self.window,
            self.id,
            &self.document,
            &self.live.states,
            &self.live.cache,
            false,
        );
    }
}

/// RFN01-49 ①: **タイトルバーの挿入・キーからは、キャレットの所にマス目が開く。**選んだ大きさ
/// （行の数は見出し行を含む）の表が置かれ、キャレットは見出し行の最初のセルに立つ。
/// 1回の取り消しで戻る。
#[test]
fn a_table_is_placed_from_the_picker() {
    let text = "書き出しの段落。\n";
    let r = rig(text, (1000, 500));
    r.put("書き出しの段落。".len());

    insert_in_pane(&r.window, &r.live, r.id, menu_commands::InsertShape::Table);
    assert_eq!(r.window.get_table_picker_pane(), r.id.index());
    assert_eq!(r.source(), text, "opening the picker writes nothing");

    insert_table_in_pane(&r.window, &r.live, r.id, 3, 2);
    assert_eq!(
        r.source(),
        "書き出しの段落。\n\n|  |  |\n| --- | --- |\n|  |  |\n|  |  |\n"
    );
    assert_eq!(r.caret(), "書き出しの段落。\n\n| ".len());
    assert_eq!(r.window.get_table_picker_pane(), -1, "the picker closes");

    // 表の中では、表は置けない。代わりに表の編集が出る。
    menu_commands::publish_context_insert(&r.window, &r.live, r.id);
    let rows = r.window.get_insert_rows();
    let table = rows
        .iter()
        .find(|row| row.picker)
        .expect("the insert rows hold the table");
    assert!(!table.enabled, "no table inside a table");
    assert_eq!(r.window.get_table_rows().row_count(), 7);
    // キーはメニューの灰色を通らずに来る——表の中では、マス目は開かない。
    insert_in_pane(&r.window, &r.live, r.id, menu_commands::InsertShape::Table);
    assert_eq!(r.window.get_table_picker_pane(), -1);

    r.undo();
    assert_eq!(r.source(), text);
}

/// RFN01-49 ②: **表の中で右クリックすると「Edit Table」の行が出る。**見出し行では前に足す・
/// 消すが押せず、本文の行ではすべて押せる。押した操作は原稿に表のまま入る。
#[test]
fn the_edit_table_rows_change_the_table() {
    let text = "| 名前 | 役割 |\n| --- | --- |\n| 主人公 | 語り手 |\n| 犬 | 相棒 |\n";
    let r = rig(text, (1000, 500));
    let enabled = |r: &Rig| -> Vec<bool> {
        menu_commands::publish_context_insert(&r.window, &r.live, r.id);
        r.window
            .get_table_rows()
            .iter()
            .map(|row| row.enabled)
            .collect()
    };

    r.put(text.find("名前").unwrap());
    assert_eq!(
        enabled(&r),
        [false, true, true, true, false, true, false],
        "the header row cannot be deleted or preceded"
    );
    r.put(text.find("犬").unwrap());
    assert_eq!(enabled(&r), [true, true, true, true, true, true, false]);

    // 画面の上下左右で言う（書き手の求め 2026-09-23）。縦書きでは行が右から左、列が上から下。
    let titles = |r: &Rig| -> Vec<String> {
        menu_commands::publish_context_insert(&r.window, &r.live, r.id);
        r.window
            .get_table_rows()
            .iter()
            .take(4)
            .map(|row| row.title.to_string())
            .collect()
    };
    assert_eq!(
        titles(&r),
        [
            "上に行を追加",
            "下に行を追加",
            "左に列を追加",
            "右に列を追加"
        ]
    );
    set_pane_direction(&r.window, &r.live.cache, r.id, true);
    assert_eq!(
        titles(&r),
        [
            "右に行を追加",
            "左に行を追加",
            "上に列を追加",
            "下に列を追加"
        ]
    );
    set_pane_direction(&r.window, &r.live.cache, r.id, false);

    table_edit_in_pane(&r.window, &r.live, r.id, table_edit::TableEdit::RowAfter);
    assert_eq!(
        r.source(),
        "| 名前 | 役割 |\n| --- | --- |\n| 主人公 | 語り手 |\n| 犬 | 相棒 |\n|  |  |\n"
    );
    table_edit_in_pane(
        &r.window,
        &r.live,
        r.id,
        table_edit::TableEdit::ColumnBefore,
    );
    assert!(
        r.source()
            .starts_with("|  | 名前 | 役割 |\n| --- | --- | --- |\n"),
        "{}",
        r.source()
    );
    r.undo();
    r.undo();
    assert_eq!(r.source(), text);

    // 本文の外では出ない。
    let r2 = text.len();
    r.put(r2);
    assert!(enabled(&r).is_empty());
}

/// RFN01-49 ③: **列の境目を引くと、区切り行の`-`が書き換わる**（`-`1つが行の長さの1%）。
///
/// 画面のポインタで、横書き・縦書きの両方。引いているあいだはガイド線だけで、離したら
/// いま見えている幅から数えた割合が区切り行に入る。取り消しは1回で戻る。**画面と組版の
/// 座標の差は、同じ紙に置いた赤い絵で測る**（絵のつまみの試験と同じ）。
#[test]
fn a_column_is_widened_by_dragging_its_edge() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let text = "![赤](a.bmp)\n\n| 名前 | 役割 |\n| --- | --- |\n| 主人公 | 語り手 |\n\n本文。\n";
    let (width, height) = (1000usize, 700usize);
    let r = rig(text, (width as u32, height as u32));
    let calls = Rc::new(Cell::new(0));
    let weak = r.window.as_weak();
    let resize_live = r.live.clone();
    let counted = calls.clone();
    r.window
        .on_pane_table_resize(move |pane, index, phase, x, y| {
            counted.set(counted.get() + 1);
            let window = weak.upgrade().unwrap();
            let id = PaneId::from_index(pane);
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            let x = id.flow_x(&window, x);
            resize_table(&window, &resize_live, id, index, phase, (x, y));
        });
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];

    for vertical in [false, true] {
        set_pane_direction(&r.window, &r.live.cache, r.id, vertical);
        let source = r.source();
        {
            let state = r.live.states.of(r.id);
            let mut state = state.borrow_mut();
            let end = source.len() - 1;
            state.caret_source_byte = Some(end);
            state.selection_anchor_source_byte = Some(end);
            state.active_line_start = source.rfind("本文");
        }
        r.put(source.len() - 1);
        r.window.window().request_redraw();
        r.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        let red = pixels
            .iter()
            .enumerate()
            .filter(|(_, pixel)| pixel.r > 200 && pixel.g < 60 && pixel.b < 60)
            .map(|(index, _)| (index % width, index / width))
            .collect::<Vec<_>>();
        let left = red.iter().map(|(x, _)| *x).min().unwrap() as f32;
        let top = red.iter().map(|(_, y)| *y).min().unwrap() as f32;
        let picture = r.live.cache.borrow_mut().pane(r.id).view.pictures[0].1;
        let (dx, dy) = (left - picture.left, top - picture.top);

        let edges = r.live.cache.borrow_mut().pane(r.id).view.tables.clone();
        assert_eq!(
            edges.len(),
            2,
            "vertical={vertical}: a boundary and the far edge"
        );
        let edge = &edges[0];
        let press = (
            (edge.rect.left + edge.rect.right) * 0.5 + dx,
            (edge.rect.top + edge.rect.bottom) * 0.5 + dy,
        );
        let step = 40.0;
        let release = if vertical {
            (press.0, press.1 + step)
        } else {
            (press.0 + step, press.1)
        };
        let at = |(x, y): (f32, f32)| slint::LogicalPosition::new(x, y);
        let before = calls.get();
        r.window.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(press),
        });
        r.window
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: at(press),
                button: PointerEventButton::Left,
            });
        r.window.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(release),
        });
        assert!(
            r.id.screen(&r.window).table_guide_shown,
            "vertical={vertical}: the guide shows while dragging"
        );
        if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
            r.window.window().request_redraw();
            r.surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, width);
            });
            let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
            for pixel in &pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            let name = format!("table-drag-{}.ppm", if vertical { "v" } else { "h" });
            std::fs::write(PathBuf::from(output).join(name), ppm).unwrap();
        }
        r.window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: at(release),
                button: PointerEventButton::Left,
            });
        assert!(
            calls.get() - before >= 3,
            "vertical={vertical}: the edge was not hit"
        );
        assert!(!r.id.screen(&r.window).table_guide_shown);

        let rule = r.source().lines().nth(3).unwrap().to_owned();
        let written = table_edit::table_widths(&rule).expect("the widths are written");
        let expected =
            table_edit::dragged_widths(None, &edge.widths, edge.reach, edge.line_box, 1, step)
                .unwrap();
        assert_eq!(written, expected, "vertical={vertical}: {rule}");
        let still =
            table_edit::dragged_widths(None, &edge.widths, edge.reach, edge.line_box, 1, 0.0)
                .unwrap();
        assert!(
            written[0] > still[0],
            "vertical={vertical}: the first column widened, {still:?} → {written:?}"
        );
        assert_eq!(r.caret(), r.source().len() - 1, "the caret stays");

        r.undo();
        assert_eq!(r.source(), text, "vertical={vertical}");
    }
}

/// RFN01-49 ①: **縦書きのペインでは、マス目も縦書きの見え方で並ぶ**——行は右から左へ、列は
/// 上から下へ。キャレットの所に開いたマス目を画面のポインタで押し、置かれた表の大きさで
/// 確かめる。横書きも同じ形で（行は上から下、列は左から右）。
#[test]
fn the_picker_reads_the_way_the_pane_writes() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let text = "![赤](a.bmp)\n\n書き出しの段落。\n";
    let (width, height) = (1000usize, 700usize);
    let r = rig(text, (width as u32, height as u32));
    let chosen = Rc::new(Cell::new((0, 0)));
    let seen = chosen.clone();
    r.window.on_pane_insert_table(move |_, rows, columns| {
        seen.set((rows, columns));
    });
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
    let cell = 18.0;
    let pad = 8.0;
    let label = 24.0;
    let side = 8.0 * cell + pad * 2.0;

    for vertical in [false, true] {
        set_pane_direction(&r.window, &r.live.cache, r.id, vertical);
        let source = r.source();
        let caret = source.find("書き出し").unwrap();
        r.put(caret);
        r.window.window().request_redraw();
        r.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        // 紙の原点の窓での位置：描いた赤い絵と、組版の座標の絵の差（ずらしの分を引く）。
        let red = pixels
            .iter()
            .enumerate()
            .filter(|(_, pixel)| pixel.r > 200 && pixel.g < 60 && pixel.b < 60)
            .map(|(index, _)| (index % width, index / width))
            .collect::<Vec<_>>();
        let left = red.iter().map(|(x, _)| *x).min().unwrap() as f32;
        let top = red.iter().map(|(_, y)| *y).min().unwrap() as f32;
        let picture = r.live.cache.borrow_mut().pane(r.id).view.pictures[0].1;
        let shift = r.id.page_shift(&r.window);
        let page = (left - picture.left - shift, top - picture.top);

        insert_in_pane(&r.window, &r.live, r.id, menu_commands::InsertShape::Table);
        let screen = r.id.screen(&r.window);
        let frame = if vertical {
            (
                page.0 + screen.caret_x - side - 4.0,
                page.1 + screen.caret_y,
            )
        } else {
            (
                page.0 + screen.caret_x,
                page.1 + screen.caret_y + screen.caret_height.max(2.0) + 4.0,
            )
        };
        let grid = (frame.0 + pad, frame.1 + pad + label);
        // 3行（見出し行を含む）×2列のいちばん奥のマス。
        let (row, column) = (2.0, 1.0);
        let point = if vertical {
            (
                grid.0 + (7.0 - row) * cell + cell * 0.5,
                grid.1 + column * cell + cell * 0.5,
            )
        } else {
            (
                grid.0 + column * cell + cell * 0.5,
                grid.1 + row * cell + cell * 0.5,
            )
        };
        let at = |(x, y): (f32, f32)| slint::LogicalPosition::new(x, y);
        r.window.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(point),
        });
        if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
            r.window.window().request_redraw();
            r.surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, width);
            });
            let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
            for pixel in &pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            let name = format!("table-picker-{}.ppm", if vertical { "v" } else { "h" });
            std::fs::write(PathBuf::from(output).join(name), ppm).unwrap();
        }
        chosen.set((0, 0));
        r.window
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: at(point),
                button: PointerEventButton::Left,
            });
        r.window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: at(point),
                button: PointerEventButton::Left,
            });
        assert_eq!(chosen.get(), (3, 2), "vertical={vertical}");
        r.window.set_table_picker_pane(-1);
    }
}
