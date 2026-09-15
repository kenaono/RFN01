//! 追加要件 2026-09-15: 画像だけの行は、ライブプレビューで絵として出る。
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

/// 1色塗りの24ビットBMP（WICが読む、いちばん簡単な形）。
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

/// 条件に合う画素の数と、それを囲む矩形の幅・高さ。
fn found(
    pixels: &[slint::Rgb8Pixel],
    stride: usize,
    hit: impl Fn(&slint::Rgb8Pixel) -> bool,
) -> (usize, usize, usize) {
    let (mut count, mut left, mut top, mut right, mut bottom) = (0, usize::MAX, usize::MAX, 0, 0);
    for (index, pixel) in pixels.iter().enumerate() {
        if hit(pixel) {
            let (x, y) = (index % stride, index / stride);
            count += 1;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    (
        count,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
}

/// 画像の行は**元の大きさで**、入らなければ**行の長さへ縮めて**出る。縦書きでも絵は立ったまま。
/// 読めない行き先は記法のまま。`EDITOR_SETTINGS_SNAPSHOT`があれば画像も書く。
#[test]
fn an_image_line_is_shown_as_the_picture() {
    let directory = std::env::temp_dir().join(format!(
        "editor-image-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(directory.join("img")).unwrap();
    std::fs::write(
        directory.join("img/red dot.bmp"),
        solid_bmp(200, 100, [255, 0, 0]),
    )
    .unwrap();
    std::fs::write(directory.join("wide.bmp"), solid_bmp(3000, 60, [0, 0, 255])).unwrap();
    let text =
        "# 庭の写真\n![赤](img/red%20dot.bmp)\n本文の行。\n![[wide.bmp]]\n![無い](missing.bmp)\n";
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
    let (width, height) = (1000usize, 700usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let (file, source) = DocumentFile::open(&path, usize::MAX).unwrap();
    let document = OpenDocument::new(file, source.clone(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
    let red = |pixel: &slint::Rgb8Pixel| pixel.r > 200 && pixel.g < 60 && pixel.b < 60;
    let blue = |pixel: &slint::Rgb8Pixel| pixel.b > 200 && pixel.r < 60 && pixel.g < 60;
    for vertical in [true, false] {
        id.update_screen(&window, |screen| {
            screen.width = 950.0;
            screen.height = 600.0;
            screen.shown_width = 950.0;
            screen.shown_height = 560.0;
            screen.preview = true;
            screen.tabs = ModelRc::new(VecModel::from(vec![TabInfo {
                title: "原稿.md".into(),
                ..Default::default()
            }]));
        });
        set_pane_direction(&window, &cache, id, vertical);
        // カーソルは見出しに置く：画像の行はどれも編集中ではない。
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(0);
            state.active_line_start = Some(0);
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &source);
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        let name = if vertical { "vertical" } else { "horizontal" };
        if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
            let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
            for pixel in &pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(PathBuf::from(output).join(format!("image-{name}.ppm")), ppm).unwrap();
        }
        // 赤は入るので元の大きさ（200×100）、回さない。
        let (count, across, along) = found(&pixels, width, red);
        assert!(
            (190..=210).contains(&across) && (95..=105).contains(&along) && count > 18_000,
            "{name}: red {count} px, {across}x{along}"
        );
        // 青（3000×60）は行に入らないので、行の長さへ縮む。
        let (count, across, along) = found(&pixels, width, blue);
        if vertical {
            // 縦書きの行は上下に伸びる。60は入るので縮まない（幅は面の外まで出る）。
            assert!(
                count > 10_000 && along >= 55,
                "{name}: blue {count} px, {across}x{along}"
            );
        } else {
            assert!(
                (300..950).contains(&across) && along < 30 && count > 3_000,
                "{name}: blue {count} px, {across}x{along}"
            );
        }
        // 画像の行にカーソルを置いても絵は出たまま、記法はその下に見える。
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(source.find("![赤]").unwrap() + 2);
            // 縦書きは止まったカーソルの行を開く（`PaneId::revealed_line`）。
            state.active_line_start = source.find("![赤]");
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &source);
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
            let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
            for pixel in &pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(
                PathBuf::from(output).join(format!("image-active-{name}.ppm")),
                ppm,
            )
            .unwrap();
        }
        let (count, across, along) = found(&pixels, width, red);
        assert!(
            (190..=210).contains(&across) && (95..=105).contains(&along) && count > 18_000,
            "{name} active: red {count} px, {across}x{along}"
        );
    }
    // 読めない行き先の行は記法のまま（字が消えていない）。
    let mut borrowed = cache.borrow_mut();
    let shown = &borrowed.pane(id).view.preview_slot.preview.text;
    assert!(shown.contains("![無い](missing.bmp)"), "{shown}");
    assert!(shown.contains("![赤](img/red%20dot.bmp)"), "{shown}");
    drop(borrowed);
    let _ = std::fs::remove_dir_all(&directory);
}

/// 縦書きで画像の行を押して離すと、カーソルはその行に残り、行が開く（書き手の報告 2026-09-16：
/// 「縦書きだと、2行目を選択することが難しかった」）。押した瞬間に開くと、行が絵の幅ぶん右へずれ、
/// 離した点が隣の空行に落ちていた。
#[test]
fn a_click_on_a_vertical_picture_keeps_the_caret_on_its_line() {
    let directory = std::env::temp_dir().join(format!(
        "editor-image-click-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("a.bmp"), solid_bmp(640, 400, [255, 0, 0])).unwrap();
    let text = "![テスト|300](a.bmp)\n\n![[a.bmp|200]]\n";
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
    surface.set_size(slint::PhysicalSize::new(1000, 700));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let (file, source) = DocumentFile::open(&path, usize::MAX).unwrap();
    let document = Rc::new(OpenDocument::new(file, source.clone(), window.as_weak()));
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 600.0;
        screen.shown_width = 950.0;
        screen.shown_height = 560.0;
        screen.preview = true;
        screen.tabs = ModelRc::new(VecModel::from(vec![TabInfo {
            title: "原稿.md".into(),
            ..Default::default()
        }]));
    });
    set_pane_direction(&window, &cache, id, true);
    let state = states.of(id);
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(0);
        state.active_line_start = Some(0);
    }
    refresh_pane_from_state(&window, &cache, &document, id, &state, &source);

    // 2枚目の絵の真ん中（開く前の組みで、その行に当たる列の中央）。
    let start = source.find("![[").unwrap();
    let end = start + "![[a.bmp|200]]".len();
    let on_line = (0..100)
        .map(|step| step as f32 * 9.0)
        .filter(|&x| {
            let mut borrowed = cache.borrow_mut();
            let hit = hit_test_pane(
                &window,
                &mut borrowed,
                &document,
                id,
                &source,
                Some(0),
                x,
                200.0,
            );
            hit.is_some_and(|hit| (start..=end).contains(&hit.byte))
        })
        .collect::<Vec<_>>();
    assert!(on_line.len() > 10, "picture column: {on_line:?}");
    let x = on_line[on_line.len() / 2];

    for phase in [SelectionPhase::Begin, SelectionPhase::End] {
        update_pane_selection(&window, &document, &state, &cache, id, x, 200.0, phase);
    }
    let (caret, active) = {
        let state = state.borrow();
        (state.caret_source_byte, state.active_line_start)
    };
    assert!(
        caret.is_some_and(|caret| (start..=end).contains(&caret)),
        "caret {caret:?}, line {start}..{end}"
    );
    assert_eq!(active, Some(start));
    // 離したら開く：記法が見える。
    let mut borrowed = cache.borrow_mut();
    let shown = &borrowed.pane(id).view.preview_slot.preview.text;
    assert!(shown.contains("![[a.bmp|200]]"), "{shown}");
    drop(borrowed);
    let _ = std::fs::remove_dir_all(&directory);
}
