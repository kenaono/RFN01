//! 組版の書き出し：代表の文書を縦横・プレビュー／原文で描き、画素と座標を書き出す。
//!
//! **座標の作り替え（縦書きの原点を読み始めの右上へ、2026-09-16）の前後を比べるための道具。**
//! 同じ文書・同じ操作で、画面の画素、カーソル・選択・絵の矩形、面の格子点のクリック先が変わって
//! いないことを、書き出した2つのフォルダの突き合わせで確かめる。
//!
//! `EDITOR_LAYOUT_SNAPSHOT`に書き出し先のフォルダを渡したときだけ動く：
//! `cargo test -- --ignored layout_snapshot`。
use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use std::fmt::Write as _;

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

const WIDTH: usize = 1100;
const HEIGHT: usize = 760;
/// Slintのソフトウェア描画は座標を16ビットで持つ。これより広い文書は画素を書かない（座標だけ）。
const PIXEL_LIMIT: u32 = 30_000;

/// 1色塗りの24ビットBMP。
fn solid_bmp(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let row = (width * 3).div_ceil(4) * 4;
    let size = 54 + row * height;
    let mut bytes = Vec::with_capacity(size as usize);
    bytes.extend(b"BM");
    bytes.extend(size.to_le_bytes());
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(54u32.to_le_bytes());
    bytes.extend(40u32.to_le_bytes());
    bytes.extend((width as i32).to_le_bytes());
    bytes.extend((height as i32).to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(24u16.to_le_bytes());
    bytes.extend([0u8; 24]);
    for _ in 0..height {
        for _ in 0..width {
            bytes.extend([rgb[2], rgb[1], rgb[0]]);
        }
        bytes.extend(std::iter::repeat_n(0u8, (row - width * 3) as usize));
    }
    bytes
}

fn wait_for_layout(window: &AppWindow, states: &PaneStates, cache: &Rc<RefCell<RenderCache>>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while cache
        .borrow_mut()
        .pane(PaneId::FIRST)
        .graphics
        .engine
        .layout_pending()
    {
        assert!(Instant::now() < deadline, "layout did not complete");
        collect_layout_results(window, states, cache);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// 今の面を書き出す：画素（入るなら）と、座標の行。
#[allow(clippy::too_many_arguments)]
fn snapshot(
    surface: &MinimalSoftwareWindow,
    window: &AppWindow,
    live: &Live,
    document: &OpenDocument,
    output: &Path,
    name: &str,
    geometry: &mut String,
) {
    let id = PaneId::FIRST;
    let screen = id.screen(window);
    let source = document.text.borrow().clone();
    let content = screen.content_width.max(screen.content_height) as u32;
    if content < PIXEL_LIMIT {
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); WIDTH * HEIGHT];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, WIDTH);
        });
        let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join(format!("{name}.ppm")), ppm).unwrap();
    }
    let rects = |model: &ModelRc<PreviewSelectionRect>| {
        model
            .iter()
            .map(|rect| {
                format!(
                    "({:.1},{:.1},{:.1},{:.1})",
                    rect.x, rect.y, rect.width, rect.height
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let _ = writeln!(geometry, "== {name}");
    let _ = writeln!(
        geometry,
        "content {}x{} scroll {:.1},{:.1}",
        screen.content_width, screen.content_height, screen.scroll_x, screen.scroll_y
    );
    let _ = writeln!(
        geometry,
        "caret {} {:.1},{:.1} {:.1}x{:.1} ime {:.1},{:.1}",
        screen.caret_visible,
        screen.caret_x,
        screen.caret_y,
        screen.caret_width,
        screen.caret_height,
        screen.ime_anchor_x,
        screen.ime_anchor_y
    );
    let _ = writeln!(geometry, "selection {}", rects(&screen.selection_rects));
    let _ = writeln!(geometry, "pictures {}", rects(&screen.picture_rects));
    // 見えている範囲の格子点を押したら、どの字に当たるか。
    let state = live.states.of(id);
    let revealed = PaneId::revealed_line(id.vertical(window), &state, &source);
    let (left, top) = (-screen.scroll_x, -screen.scroll_y);
    let mut hits = String::new();
    for row in 0..12 {
        for column in 0..16 {
            let x = left + 20.0 + column as f32 * 62.0;
            let y = top + 20.0 + row as f32 * 45.0;
            // 紙の座標で選んだ点を、クリックと同じく組版の座標へ直して訊く。
            let x = id.flow_x(window, x);
            let hit = {
                let mut borrowed = live.cache.borrow_mut();
                hit_test_pane(window, &mut borrowed, document, id, &source, revealed, x, y)
            };
            let _ = write!(
                hits,
                "{} ",
                hit.map_or("-".to_owned(), |hit| hit.byte.to_string())
            );
        }
        hits.push('\n');
    }
    geometry.push_str(&hits);
}

#[test]
#[ignore = "layout snapshots for comparing two builds; set EDITOR_LAYOUT_SNAPSHOT"]
fn layout_snapshot() {
    let Ok(output) = std::env::var("EDITOR_LAYOUT_SNAPSHOT") else {
        return;
    };
    let output = PathBuf::from(output);
    std::fs::create_dir_all(&output).unwrap();
    let scratch =
        std::env::temp_dir().join(format!("editor-layout-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(scratch.clone()));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    std::fs::write(scratch.join("red.bmp"), solid_bmp(200, 100, [255, 0, 0])).unwrap();
    std::fs::write(scratch.join("wide.bmp"), solid_bmp(1600, 60, [0, 0, 255])).unwrap();
    let pictures = scratch.join("画像.md");
    std::fs::write(
        &pictures,
        "# 画像の行\n本文の行。\n![赤|150](red.bmp)\n\n![[wide.bmp]]\n末尾の本文。**強調**も。\n",
    )
    .unwrap();

    let mut documents = [
        "01_行属性.md",
        "02_文字装飾.md",
        "03_見出しとアウトライン.md",
        "04_縦書き.md",
        "09_段落長の計測.md",
        "11_表.md",
        "13_ルビ・傍点・縦中横.md",
        "22_リンク表示.md",
        "24_編集中の見出し.md",
        "40_長い段落の入力応答.md",
    ]
    .iter()
    .map(|name| PathBuf::from("testdata").join(name))
    .collect::<Vec<_>>();
    documents.push(pictures);

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
    surface.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    window.show().unwrap();

    let mut geometry = String::new();
    for path in &documents {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        for vertical in [false, true] {
            for preview in [true, false] {
                let (file, text) = DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS).unwrap();
                let document = OpenDocument::new(file, text.clone(), window.as_weak());
                let live = Live {
                    closed_tabs: Rc::default(),
                    states: PaneStates::new(&document),
                    folder: Rc::default(),
                    tree_paths: Rc::default(),
                    workspace_ids: Rc::default(),
                    preview: Rc::default(),
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
                let weak = window.as_weak();
                let scroll_cache = live.cache.clone();
                window.on_pane_scroll_changed(move |pane, offset| {
                    if let Some(window) = weak.upgrade() {
                        // 窓の配線と同じ：Slintの紙のスクロールを、組版の座標のスクロールへ。
                        let id = PaneId::from_index(pane);
                        let offset = id.scroll_from_page(&window, offset);
                        refresh_after_scroll(&window, &scroll_cache, id, offset);
                    }
                });
                id.update_screen(&window, |screen| {
                    screen.width = 1050.0;
                    screen.height = 640.0;
                    screen.shown_width = 1050.0;
                    screen.shown_height = 540.0;
                    screen.preview = preview;
                    // 面ごとの縦横は組版エンジンが正で、新しいエンジンは横書きから始まる。前の文書の
                    // 縦書きが行に残っていると、横書きへの切り替えが「変わっていない」と見なされる。
                    screen.vertical = false;
                    screen.scroll_x = 0.0;
                    screen.scroll_y = 0.0;
                });
                set_pane_direction(&window, &live.cache, id, vertical);
                let name = format!(
                    "{stem}-{}-{}",
                    if vertical { "v" } else { "h" },
                    if preview { "p" } else { "s" }
                );
                let state = live.states.of(id);
                let refresh = |live: &Live| {
                    let source = document.text.borrow().clone();
                    refresh_pane_from_state(
                        &window,
                        &live.cache,
                        &document,
                        id,
                        &live.states.of(id),
                        &source,
                    );
                    wait_for_layout(&window, &live.states, &live.cache);
                };

                // 1. 頭にカーソル。
                {
                    let mut state = state.borrow_mut();
                    state.caret_source_byte = Some(0);
                    state.selection_anchor_source_byte = Some(0);
                    state.active_line_start = Some(0);
                }
                refresh(&live);
                // **文書の頭から。**前の文書のスクロールが行に残っているので、明示的に送る。
                let content = live
                    .cache
                    .borrow_mut()
                    .pane(id)
                    .graphics
                    .engine
                    .total_flow_size() as f32;
                let start = id.start_scroll(&window, id.viewport_flow(&window), content);
                id.set_scroll(&window, start);
                refresh_after_scroll(&window, &live.cache, id, start);
                wait_for_layout(&window, &live.states, &live.cache);
                snapshot(
                    &surface,
                    &window,
                    &live,
                    &document,
                    &output,
                    &format!("{name}-1start"),
                    &mut geometry,
                );

                // 2. 真ん中にカーソル、少し前から選ぶ。
                let middle = floor_char_boundary(&text, text.len() / 2);
                let anchor = floor_char_boundary(&text, middle.saturating_sub(40));
                {
                    let mut state = state.borrow_mut();
                    state.caret_source_byte = Some(middle);
                    state.selection_anchor_source_byte = Some(anchor);
                    state.active_line_start = Some(source_line_start(&text, middle));
                }
                refresh(&live);
                snapshot(
                    &surface,
                    &window,
                    &live,
                    &document,
                    &output,
                    &format!("{name}-2middle"),
                    &mut geometry,
                );

                // 3. そこでEnter（列・行が1つ増える）。
                enter_in_pane(&window, &live, id, false);
                wait_for_layout(&window, &live.states, &live.cache);
                snapshot(
                    &surface,
                    &window,
                    &live,
                    &document,
                    &output,
                    &format!("{name}-3enter"),
                    &mut geometry,
                );

                // 4. 読み進める向きへ300pxスクロール。
                let scroll = id.scroll(&window);
                let target = if vertical {
                    scroll + 300.0
                } else {
                    scroll - 300.0
                };
                let content = live
                    .cache
                    .borrow_mut()
                    .pane(id)
                    .graphics
                    .engine
                    .total_flow_size() as f32;
                let (low, high) = id.scroll_range(&window, id.shown_flow(&window), content);
                let target = target.clamp(low, high);
                id.set_scroll(&window, target);
                refresh_after_scroll(&window, &live.cache, id, target);
                wait_for_layout(&window, &live.states, &live.cache);
                snapshot(
                    &surface,
                    &window,
                    &live,
                    &document,
                    &output,
                    &format!("{name}-4scrolled"),
                    &mut geometry,
                );
            }
        }
    }
    std::fs::write(output.join("geometry.txt"), geometry).unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
}
