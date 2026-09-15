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

fn render(surface: &MinimalSoftwareWindow, window: &AppWindow) -> Vec<u8> {
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    ppm
}

#[test]
#[ignore = "E17 offscreen UI, IME and viewport verification; run alone"]
fn incremental_ui_input_and_completion() {
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
    window.set_sheet_numbers(numbers.into());
    window.set_palette(palette.into());
    window.set_sheet_fonts(fonts.into());
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    id.update_screen(&window, |screen| {
        screen.width = 1050.0;
        screen.height = 640.0;
        screen.shown_width = 1050.0;
        screen.shown_height = 540.0;
        screen.preview = true;
    });
    // SoftwareRenderer uses i16 scene coordinates. Keep this rendered sample
    // below that limit; the engine tests separately exercise 50,000 characters.
    let original = format!(
        "# 長い段落の入力応答\n{}\n\n末尾の確認。",
        "本文の **強調** と句読点、括弧（かっこ）、日本語ABC123。".repeat(250)
    );
    let document = OpenDocument::new(
        DocumentFile::untitled(1),
        original.clone(),
        window.as_weak(),
    );
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let output = PathBuf::from("target/incremental-qa");
    std::fs::create_dir_all(&output).unwrap();
    cache.borrow_mut().perf_log.file = Some(File::create(output.join("perf.txt")).unwrap());

    for vertical in [false, true] {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
        });
        set_pane_direction(&window, &cache, id, vertical);
        let at = original.find("本文").unwrap();
        states.of(id).borrow_mut().caret_source_byte = Some(at);
        *document.text.borrow_mut() = original.clone();
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &original);
        let deadline = Instant::now() + Duration::from_secs(15);
        while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
            assert!(Instant::now() < deadline);
            collect_layout_results(&window, &states, &cache);
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = render(&surface, &window);
        let mut edited = original.clone();
        edited.insert_str(at, "追加した文字。");
        *document.text.borrow_mut() = edited.clone();
        states.of(id).borrow_mut().caret_source_byte = Some(at + "追加した文字。".len());
        let started = Instant::now();
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &edited);
        let foreground = elapsed_ms(started);
        assert!(cache.borrow_mut().pane(id).graphics.engine.layout_pending());
        let before = render(&surface, &window);
        let display = elapsed_ms(started);
        std::fs::write(output.join(format!("{vertical}-foreground.ppm")), &before).unwrap();
        let scroll = id.scroll(&window);
        while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
            assert!(started.elapsed() < Duration::from_secs(15));
            collect_layout_results(&window, &states, &cache);
            std::thread::sleep(Duration::from_millis(2));
        }
        let after = render(&surface, &window);
        std::fs::write(output.join(format!("{vertical}-complete.ppm")), &after).unwrap();
        assert_eq!(
            before, after,
            "background completion changed visible pixels"
        );
        // The viewport's pixels are checked in the artifacts too; no NG status
        // may be emitted while the worker completes or the IME changes.
        assert!(
            !window.get_render_status().contains("NG"),
            "{}",
            window.get_render_status()
        );
        eprintln!(
            "E17 UI vertical={vertical} prepare={foreground:.2}ms display={display:.2}ms full={:.2}ms scroll={scroll:.1}->{:.1}",
            elapsed_ms(started),
            id.scroll(&window)
        );
        for preedit in ["へんかん", "変換", ""] {
            states.of(id).borrow_mut().preedit = preedit.into();
            refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &edited);
            assert!(
                !window.get_render_status().contains("NG"),
                "{}",
                window.get_render_status()
            );
        }
        assert_eq!(
            &*document.text.borrow(),
            &edited,
            "IME layout must not edit saved text"
        );
        // Undo-like restoration while a job is outstanding must replace its
        // text before accepting any completion; history is owned elsewhere.
        *document.text.borrow_mut() = original.clone();
        states.of(id).borrow_mut().caret_source_byte = Some(at);
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &original);
        let deadline = Instant::now() + Duration::from_secs(15);
        while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
            assert!(Instant::now() < deadline);
            collect_layout_results(&window, &states, &cache);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            !window.get_render_status().contains("NG"),
            "{}",
            window.get_render_status()
        );
    }

    // Switching direction must retain the source caret and reveal it after
    // the deferred tail has completed, including a caret far into a paragraph.
    let weak = window.as_weak();
    let scroll_cache = cache.clone();
    window.on_pane_scroll_changed(move |pane, offset| {
        if let Some(window) = weak.upgrade() {
            // 窓の配線と同じ：Slintの紙のスクロールを、組版の座標のスクロールへ。
            let id = PaneId::from_index(pane);
            let offset = id.scroll_from_page(&window, offset);
            refresh_after_scroll(&window, &scroll_cache, id, offset);
        }
    });
    for fraction in [2, 4] {
        let at = floor_char_boundary(&original, original.len() * (fraction - 1) / fraction);
        states.of(id).borrow_mut().caret_source_byte = Some(at);
        for vertical in [false, true, false] {
            use slint::platform::{PointerEventButton, WindowEvent};
            let position = slint::LogicalPosition::new(500.0, 350.0);
            window.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            window
                .window()
                .dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
            if id.vertical(&window) != vertical {
                toggle_pane_direction(&window, &states, &cache, id);
            } else {
                refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &original);
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            // A ScrollView notification can arrive before the completion timer.
            if vertical && cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
                while !cache.borrow_mut().pane(id).graphics.engine.layout_ready() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                refresh_after_scroll(&window, &cache, id, 0.0);
            }
            while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
                assert!(Instant::now() < deadline);
                collect_layout_results(&window, &states, &cache);
                std::thread::sleep(Duration::from_millis(2));
            }
            collect_layout_results(&window, &states, &cache);
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(20));
                slint::platform::update_timers_and_animations();
                let _ = render(&surface, &window);
            }
            let mut borrowed = cache.borrow_mut();
            let pane = borrowed.pane(id);
            let caret = pane
                .graphics
                .engine
                .caret_geometry(pane.view.caret_utf16.unwrap())
                .unwrap();
            let flow = if vertical { caret.x } else { caret.y };
            let size = if vertical { caret.width } else { caret.height };
            let screen_flow = flow + id.scroll(&window);
            assert!(
                screen_flow >= -1.0 && screen_flow + size <= id.viewport_flow(&window) + 1.0,
                "direction={vertical} at={at} caret={flow} scroll={} viewport={}",
                id.scroll(&window),
                id.viewport_flow(&window)
            );
            assert_eq!(states.of(id).borrow().caret_source_byte, Some(at));
        }
    }

    // Preserve context, not merely visibility, in both directions. These
    // positions are away from document edges so no boundary clamp is needed.
    for fraction in [0.25, 0.5, 0.75] {
        for _ in 0..2 {
            let caret = {
                let mut borrowed = cache.borrow_mut();
                let pane = borrowed.pane(id);
                pane.graphics
                    .engine
                    .caret_geometry(pane.view.caret_utf16.unwrap())
                    .unwrap()
            };
            let total = cache
                .borrow_mut()
                .pane(id)
                .graphics
                .engine
                .total_flow_size() as f32;
            let viewport = id.viewport_flow(&window);
            let range = id.scroll_range(&window, viewport, total);
            id.set_scroll(
                &window,
                direction_caret_scroll(id.vertical(&window), &caret, fraction, viewport, range),
            );
            let before = caret_view_fraction(
                id.vertical(&window),
                &caret,
                id.scroll(&window),
                id.viewport_flow(&window),
            );
            assert!((before - fraction).abs() < 0.01);
            let source_caret = states.of(id).borrow().caret_source_byte;
            toggle_pane_direction(&window, &states, &cache, id);
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                collect_layout_results(&window, &states, &cache);
                if !cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = render(&surface, &window);
            // The native resize callback refreshes after ScrollView has settled.
            refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &original);
            while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
                assert!(Instant::now() < deadline);
                collect_layout_results(&window, &states, &cache);
                std::thread::sleep(Duration::from_millis(2));
            }
            let caret = {
                let mut borrowed = cache.borrow_mut();
                let pane = borrowed.pane(id);
                pane.graphics
                    .engine
                    .caret_geometry(pane.view.caret_utf16.unwrap())
                    .unwrap()
            };
            let after = caret_view_fraction(
                id.vertical(&window),
                &caret,
                id.scroll(&window),
                id.viewport_flow(&window),
            );
            assert!(
                (after - before).abs() < 0.02,
                "vertical={} before={before} after={after}",
                id.vertical(&window)
            );
            assert_eq!(states.of(id).borrow().caret_source_byte, source_caret);
            refresh_after_scroll(&window, &cache, id, id.scroll(&window) - 64.0);
            assert!(
                cache
                    .borrow_mut()
                    .pane(id)
                    .view
                    .direction_fraction
                    .is_none(),
                "manual scrolling releases the direction anchor"
            );
        }
    }
    // Zoomed/fixed-width lines exceed one 2048px raster slice. Scrolling
    // across the line must request the second slice via the real Slint event.
    let weak = window.as_weak();
    let across_cache = cache.clone();
    window.on_pane_across_scroll_changed(move |pane, offset| {
        if let Some(window) = weak.upgrade() {
            refresh_after_across_scroll(&window, &across_cache, PaneId::from_index(pane), offset);
        }
    });
    let wide = "右側まで表示する日本語の長い本文。".repeat(200);
    for vertical in [false, true] {
        set_pane_direction(&window, &cache, id, vertical);
        id.set_scroll_across(&window, 0.0);
        let typography = Typography {
            font_size: 33.0,
            ..Typography::default()
        };
        {
            let mut borrowed = cache.borrow_mut();
            let engine = &mut borrowed.pane(id).graphics.engine;
            engine
                .update(StyledText::plain(&wide), LineFit::Extent(3000), &typography)
                .unwrap();
            id.set_content_size(&window, engine.total_flow_size(), engine.line_extent());
        }
        cache
            .borrow_mut()
            .refresh_pane_tiles(&window, id, 0)
            .unwrap();
        id.set_scroll(&window, 0.0);
        let _ = render(&surface, &window);
        id.set_scroll_across(&window, -1800.0);
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(20));
            slint::platform::update_timers_and_animations();
            let _ = render(&surface, &window);
        }
        assert!(
            id.screen(&window).tiles.iter().any(|tile| {
                if vertical {
                    tile.y >= 2048
                } else {
                    tile.x >= 2048
                }
            }),
            "cross-axis scrolling must display the far slice, vertical={vertical}, across={}, flow={}, tiles={}",
            id.scroll_across(&window),
            id.scroll(&window),
            id.screen(&window).tiles.row_count()
        );
    }
}
