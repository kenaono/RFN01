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

// Run separately from other rendering tests; also produces reviewable real UI images.
#[test]
#[ignore = "offscreen typography and settings visual verification"]
fn typography_settings_roundtrip_and_render() {
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
    set_picked_colour(&numbers, &palette, 1, 9, [0.5, 1.0, 1.0]);
    assert_eq!(
        numbers.row_data(SHEET_NUMBERS + Setting::Decoration(1, 3).row_in_sheet()),
        Some(1)
    );
    assert_eq!(
        numbers.row_data(Setting::Decoration(1, 3).row_in_sheet()),
        Some(0)
    );
    set_picked_colour(&numbers, &palette, 0, 0, [0.1, 0.2, 0.3]);
    assert_eq!(
        numbers.row_data(Setting::Decoration(0, 3).row_in_sheet()),
        Some(0)
    );
    reset_settings(&numbers, &palette, &fonts);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(fonts.clone()));
    let weak = window.as_weak();
    let steps = numbers.clone();
    window.on_sheet_step(move |index, by| {
        if let (Some(window), Some(setting)) = (weak.upgrade(), Setting::from_index(index)) {
            step_setting(&window, &steps, setting, by);
        }
    });
    for sheet in 0..2 {
        for kind in [0, 3, 4] {
            Setting::Decoration(1, kind).write(&numbers, sheet, 1);
        }
        Setting::Decoration(2, 1).write(&numbers, sheet, 1);
        Setting::Decoration(3, 2).write(&numbers, sheet, 1);
        set_colour(&palette, sheet, 9, [0.8, 0.9, 1.0]);
    }
    let stored = settings_values(&window);
    reset_settings(&numbers, &palette, &fonts);
    apply_settings(&window, &numbers, &palette, &fonts, &stored);
    assert_eq!(Setting::Decoration(1, 4).read(&window, 0), 1);
    assert_eq!(Setting::Decoration(3, 2).read(&window, 1), 1);
    // Body settings are independent of headings and of the other writing direction.
    for kind in 0..4 {
        Setting::Decoration(0, kind).write(&numbers, 1, 1);
    }
    set_colour(&palette, 1, 8, [1.0, 0.9, 0.9]);
    assert_eq!(
        hex_colour(palette.row_data(colour_row(1, 9)).unwrap()),
        "#cce6ff"
    );
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let source = "# 太字12の見出しと背景・罫線\n本文の装飾は方向別です。\n## 斜体の見出し Italic\n本文の **強調** と *斜体* と ~~取消線~~。\n### 取り消し線12の見出し\n未解決の[[不明|リンク]]と[通常リンク](章.md)。\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let output = PathBuf::from("target/typography-qa");
    std::fs::create_dir_all(&output).unwrap();
    for vertical in [false, true] {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
        });
        set_pane_direction(&window, &cache, id, vertical);
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(
            output.join(if vertical {
                "vertical.ppm"
            } else {
                "horizontal.ppm"
            }),
            ppm,
        )
        .unwrap();
    }
    // E14: compare the same H1 with its source markers visible while editing.
    for vertical in [false, true] {
        set_pane_direction(&window, &cache, id, vertical);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(2);
            state.active_line_start = Some(0);
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(
            output.join(if vertical {
                "heading-edit-vertical.ppm"
            } else {
                "heading-edit-horizontal.ppm"
            }),
            ppm,
        )
        .unwrap();
    }
    {
        let state = states.of(id);
        let mut state = state.borrow_mut();
        state.caret_source_byte = None;
        state.active_line_start = None;
    }
    // 追加要件 2026-09-14: the settings stand in the pane, as its tab.
    window.set_settings_tab(3);
    id.update_screen(&window, |screen| screen.settings = true);
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("settings.ppm"), ppm).unwrap();
    use slint::platform::{PointerEventButton, WindowEvent};
    let click = |x, y| {
        let position = slint::LogicalPosition::new(x, y);
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
    };
    click(446.0, 277.0);
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("background-menu.ppm"), ppm).unwrap();
    // 「Paperに合わせる」
    click(307.0, 313.0);
    assert_eq!(Setting::Decoration(1, 3).read(&window, 0), 0);
    let saved_background = palette.row_data(colour_row(0, 9)).unwrap();
    set_colour(&palette, 0, PAPER_SLOT, [0.9, 1.0, 0.9]);
    assert_eq!(
        typography_for(&window, 100, false, true).paper,
        channels(palette.row_data(colour_row(0, PAPER_SLOT)).unwrap())
    );
    let stored = settings_values(&window);
    reset_settings(&numbers, &palette, &fonts);
    apply_settings(&window, &numbers, &palette, &fonts, &stored);
    assert_eq!(Setting::Decoration(1, 3).read(&window, 0), 0);
    assert_eq!(palette.row_data(colour_row(0, 9)), Some(saved_background));
    id.update_screen(&window, |screen| screen.settings = false);
    assert_eq!(&*document.text.borrow(), source);
    let copied = Rc::new(Cell::new(0));
    let saved_requests = Rc::new(Cell::new(0));
    let received = saved_requests.clone();
    window.on_compare_saved_requested(move || received.set(received.get() + 1));
    let compare_requests = Rc::new(Cell::new(0));
    let received = compare_requests.clone();
    window.on_compare_files_requested(move || received.set(received.get() + 1));
    let menu_snapshot = |name: &str| {
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join(name), ppm).unwrap();
    };
    let goto_requests = Rc::new(Cell::new(0));
    let received = goto_requests.clone();
    window.on_goto_requested(move |taking| {
        assert!(taking);
        received.set(received.get() + 1);
    });
    click(1077.0, 24.0);
    menu_snapshot("pane-menu.ppm");
    click(930.0, 484.0);
    assert_eq!(
        goto_requests.get(),
        1,
        "Go to Line must be reachable from the pane menu"
    );
    click(1077.0, 24.0);
    menu_snapshot("pane-menu.ppm");
    click(930.0, 542.0);
    assert_eq!(
        saved_requests.get(),
        1,
        "saved comparison is in the pane menu"
    );
    click(1077.0, 24.0);
    menu_snapshot("pane-menu.ppm");
    click(930.0, 600.0);
    assert_eq!(compare_requests.get(), 1, "comparison is in the pane menu");
    let navigated = Rc::new(RefCell::new(Vec::new()));
    let received_navigation = navigated.clone();
    window.on_pane_navigate(move |pane, forward| {
        received_navigation.borrow_mut().push((pane, forward));
    });
    for button in [PointerEventButton::Back, PointerEventButton::Forward] {
        let position = slint::LogicalPosition::new(200.0, 200.0);
        for event in [
            WindowEvent::PointerPressed { position, button },
            WindowEvent::PointerReleased { position, button },
        ] {
            window.window().dispatch_event(event);
        }
    }
    assert_eq!(
        &*navigated.borrow(),
        &[(0, false), (0, true)],
        "mouse navigation must dispatch once per click to the pane under the pointer"
    );
    let received = copied.clone();
    window.on_pane_copy_body(move |_| received.set(received.get() + 1));
    // **紙の上で押す。**右クリックのメニューは本文のものなので、紙の外（縦書きの短い文書では
    // 左の余白）では出ない。戻る／進むは面のどこでも効くので、上ではその外を押している。
    let position = slint::LogicalPosition::new(800.0, 200.0);
    for event in [
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Right,
        },
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Right,
        },
    ] {
        window.window().dispatch_event(event);
    }
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("body-copy-menu.ppm"), ppm).unwrap();
    click(840.0, 376.0);
    assert_eq!(
        copied.get(),
        1,
        "body copy must reach Rust before its conditional menu row is destroyed"
    );
    let undo_calls = Rc::new(Cell::new(0));
    let calls = undo_calls.clone();
    let weak = window.as_weak();
    let undo_document = document.clone();
    let undo_states = states.clone();
    let undo_cache = cache.clone();
    window.on_pane_undo(move |_, forwards| {
        calls.set(calls.get() + 1);
        undo_in_pane(
            &weak.upgrade().unwrap(),
            id,
            &undo_document,
            &undo_states,
            &undo_cache,
            forwards,
        );
    });
    let open_menu = || {
        for event in [
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Right,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Right,
            },
        ] {
            window.window().dispatch_event(event);
        }
    };
    assert!(!id.screen(&window).can_undo);
    open_menu();
    click(840.0, 219.0);
    assert_eq!(undo_calls.get(), 0, "disabled Undo must not dispatch");
    click(700.0, 700.0);
    insert_pane_text(&window, id, &document, &states, &cache, "追加", false);
    // The debug build draws this in about 14ms, over `PACE_FREE_MS`, and the
    // undo below would then be drawn by a timer this test never runs. What is
    // checked here is the menu, not the pacing.
    cache.borrow_mut().pace_of(id).took = 0.0;
    let edited = document.text.borrow().clone();
    assert_ne!(edited, source);
    assert!(id.screen(&window).can_undo);
    open_menu();
    click(840.0, 219.0);
    assert_eq!(undo_calls.get(), 1);
    assert_eq!(&*document.text.borrow(), source);
    assert!(id.screen(&window).can_redo);
    open_menu();
    click(840.0, 248.0);
    assert_eq!(undo_calls.get(), 2);
    assert_eq!(&*document.text.borrow(), &edited);
    undo_in_pane(&window, id, &document, &states, &cache, false);
    states.of(id).borrow_mut().viewer = true;
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    assert!(!id.screen(&window).can_undo && !id.screen(&window).can_redo);
    assert!(
        !document.read_only(),
        "user lock must not disable saving or session recovery"
    );
    insert_pane_text(&window, id, &document, &states, &cache, "追加", false);
    splice_source(&window, id, &document, &states, &cache, 0, 1, "", 0);
    undo_in_pane(&window, id, &document, &states, &cache, false);
    undo_in_pane(&window, id, &document, &states, &cache, true);
    set_pane_preedit(&window, id, &document, &states.of(id), &cache, "変換中");
    assert_eq!(&*document.text.borrow(), source);
    assert!(states.of(id).borrow().preedit.is_empty());
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    assert!(id.screen(&window).viewer);
    let mut preview_slot = PreviewSlot::default();
    let completed = pane_text(&window, &document, id, &mut preview_slot, source, Some(0))
        .text()
        .to_owned();
    assert!(
        !completed.starts_with('#'),
        "Viewer must not reveal the clicked heading's source"
    );
    assert_eq!(
        completed,
        pane_text(&window, &document, id, &mut preview_slot, source, None).text()
    );
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("viewer.ppm"), ppm).unwrap();
    states.add(&document);
    publish_panes(&window, 2);
    let other = PaneId::from_index(1);
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    refresh_pane_from_state(&window, &cache, &document, other, &states.of(other), source);
    assert!(id.screen(&window).viewer);
    assert!(!other.screen(&window).viewer);
    insert_pane_text(&window, other, &document, &states, &cache, "別TAB", false);
    assert_ne!(
        &*document.text.borrow(),
        source,
        "another TAB can edit the shared document"
    );
    assert!(
        states.of(id).borrow().viewer,
        "shared edits preserve Viewer mode"
    );
    states.of(id).borrow_mut().viewer = false;
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    insert_pane_text(&window, id, &document, &states, &cache, "追加", false);
    assert_ne!(&*document.text.borrow(), source, "unlock restores editing");
    let divisions = Rc::new(RefCell::new(Vec::new()));
    let received = divisions.clone();
    window.on_divide_requested(move |right| received.borrow_mut().push(right));
    click(1060.0, 738.0);
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("status-split-menu.ppm"), ppm).unwrap();
    click(980.0, 665.0);
    assert_eq!(&*divisions.borrow(), &[true]);
    click(1060.0, 738.0);
    click(980.0, 694.0);
    assert_eq!(&*divisions.borrow(), &[true, false]);
    let whitespace_source =
        "半角 空白\n全角　空白\nタブ\t区切り\n末尾の空白  \n\n折り返しの確認です。";
    *document.text.borrow_mut() = whitespace_source.into();
    *states.of(id).borrow_mut() = EditorState::default();
    let history = document.history.borrow().done.len();
    for sheet in 0..2 {
        assert_eq!(Setting::Whitespace.read(&window, sheet), 0);
        Setting::Whitespace.write(&numbers, sheet, 1);
    }
    let saved = settings_values(&window);
    reset_settings(&numbers, &palette, &fonts);
    apply_settings(&window, &numbers, &palette, &fonts, &saved);
    for vertical in [false, true] {
        assert_eq!(Setting::Whitespace.read(&window, usize::from(vertical)), 1);
        set_pane_direction(&window, &cache, id, vertical);
        refresh_pane_from_state(
            &window,
            &cache,
            &document,
            id,
            &states.of(id),
            whitespace_source,
        );
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(
            output.join(if vertical {
                "whitespace-vertical.ppm"
            } else {
                "whitespace-horizontal.ppm"
            }),
            ppm,
        )
        .unwrap();
    }
    assert_eq!(&*document.text.borrow(), whitespace_source);
    assert_eq!(document.history.borrow().done.len(), history);
}

/// 追加要件 2026-09-16（書き手「組版の表現拡大」）: **長い読みは前後の仮名へかけ、かけられない
/// ときは親文字を広げる。**前者では本文の送りが変わらず、後者では親文字のぶんだけ行が伸びる。
#[test]
fn a_long_reading_hangs_over_kana_and_widens_only_when_it_cannot() {
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    // 行末のカーソルがどこに立つか＝その行がどこまで伸びたか。
    let line_reach = |source: &str, vertical: bool| {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = false;
        });
        let document =
            OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
        let states = PaneStates::new(&document);
        set_pane_direction(&window, &cache, id, vertical);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            // カーソルは次の行に置く：ルビの行は編集中にせず、整形したまま測る。
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        let mut borrowed = cache.borrow_mut();
        let pane = borrowed.pane(id);
        let shown = pane.view.preview_slot.preview.text.clone();
        let at = shown.find('\n').unwrap_or(shown.len());
        let utf16 = shown[..at].encode_utf16().count() as u32;
        let caret = pane.graphics.engine.caret_geometry(utf16).unwrap();
        if vertical { caret.y } else { caret.x }
    };
    for vertical in [false, true] {
        let plain = line_reach("漢字漢字\n\n", vertical);
        // 前後が漢字なので、どちらへもかけられない：親文字（字）が広がる。
        let spread = line_reach("漢｜字《ながいよみです》漢字\n\n", vertical);
        assert!(
            spread > plain + 20.0,
            "vertical={vertical}: 親文字が広がっていない（{plain} → {spread}）"
        );
        // 前後が仮名なら、かけて済む：本文の送りは変わらない。
        let kana = line_reach("のののの\n\n", vertical);
        let hung = line_reach("の｜の《ながいよみ》のの\n\n", vertical);
        assert!(
            (hung - kana).abs() < 1.0,
            "vertical={vertical}: 仮名へかけずに広げている（{kana} → {hung}）"
        );
    }
}

/// 追加要件 2026-09-16（書き手の決定）: **段落の先頭の行でもルビが切れない。**
///
/// ルビの帯は行の箱の中に収まる（`Typography::ruby_room`）ので、ブロック＝タイルの外へ出ない。
/// 帯が行からはみ出していたころは、段落の先頭の列の読みだけが前の段落のタイルへ出て切れていた
/// （同じルビで墨10画素対15画素）。
#[test]
fn a_reading_at_the_head_of_a_paragraph_is_not_cut_off() {
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
    let (width, height) = (1100usize, 760usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    // 同じルビが、段落の先頭の行と、段落の2行目に出る。
    let source = "｜宿《やど》です。\n\n本文の行。\n｜宿《やど》です。\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    for vertical in [true, false] {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = false;
        });
        set_pane_direction(&window, &cache, id, vertical);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        // 読みの墨は本文より小さい字なので、行（列）ごとに数えれば2つの山になる。
        // 山を大きい順に2つ取り、同じ量であることを見る。
        let mut bands = std::collections::BTreeMap::<usize, usize>::new();
        for (index, pixel) in pixels.iter().enumerate() {
            if pixel.r < 150 && pixel.g < 150 && pixel.b < 150 {
                let (x, y) = (index % width, index / width);
                if (60..620).contains(&y) && x > 120 {
                    *bands.entry(if vertical { x } else { y }).or_default() += 1;
                }
            }
        }
        // 山を切れ目で分ける。
        let mut groups: Vec<usize> = Vec::new();
        let mut last = None;
        for (at, ink) in &bands {
            match last {
                Some(previous) if at - previous <= 1 => *groups.last_mut().unwrap() += ink,
                _ => groups.push(*ink),
            }
            last = Some(*at);
        }
        groups.sort_unstable();
        // いちばん小さい2つの山が、2つの読み（本文の列より墨が少ない）。
        let readings = &groups[..2];
        assert!(
            readings[0] * 4 >= readings[1] * 3,
            "vertical={vertical}: 読みの墨が揃わない（{readings:?}、全部で{groups:?}）"
        );
    }
}

/// 要件 7.8（2026-09-16、書き手「組版の表現拡大」）: **注記が名指した印が、画素に届く。**
///
/// 種類ごとに墨の量が違う（丸は白丸より多い）ので、どれか1つでも既定の点に落ちていれば分かる。
/// 傍線は点ではなく続いた線なので、字の並びと同じ長さの筋になる。
#[test]
fn every_kind_of_mark_beside_the_word_reaches_the_pixels() {
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
    let (width, height) = (1100usize, 760usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let ink_of = |note: &str| {
        let source = format!("ここに印{note}を振る。\n");
        let document = OpenDocument::new(
            DocumentFile::untitled(1),
            source.clone().into(),
            window.as_weak(),
        );
        let states = PaneStates::new(&document);
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = false;
        });
        set_pane_direction(&window, &cache, id, false);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &source);
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        pixels
            .iter()
            .filter(|pixel| pixel.r < 150 && pixel.g < 150 && pixel.b < 150)
            .count()
    };
    let plain = ink_of("");
    let mut seen = Vec::new();
    for note in [
        "［＃「印」に傍点］",
        "［＃「印」に丸傍点］",
        "［＃「印」に白丸傍点］",
        "［＃「印」に二重丸傍点］",
        "［＃「印」にゴマ傍点］",
        "［＃「印」に×傍点］",
        "［＃「印」に傍線］",
    ] {
        let ink = ink_of(note);
        assert!(ink > plain, "{note}: 印の墨が出ていない（{plain} → {ink}）");
        seen.push((note, ink));
    }
    let ink = |name: &str| {
        seen.iter()
            .find(|(note, _)| note.contains(name))
            .map(|(_, ink)| *ink)
            .unwrap()
    };
    assert!(
        ink("丸傍点］") > ink("白丸傍点］"),
        "塗った丸のほうが墨が多い: {seen:?}"
    );
    // 線の種類（2026-09-17、書き手「組版の表現拡大」②）。**刻みのある線は実線より墨が
    // 少なく、二重傍線は多い**——どれも同じ実線に落ちていないことがこれで分かる。
    // **長い語で見る。**1字ぶんの線では刻みが1つも入らず、どの種類も同じ墨になる。
    let mut lines = Vec::new();
    for note in [
        "［＃「ここに印」に傍線］",
        "［＃「ここに印」に二重傍線］",
        "［＃「ここに印」に波線］",
        "［＃「ここに印」に鎖線］",
        "［＃「ここに印」に破線］",
    ] {
        let ink = ink_of(note);
        assert!(ink > plain, "{note}: 線の墨が出ていない（{plain} → {ink}）");
        lines.push((note, ink));
    }
    let line = |name: &str| {
        lines
            .iter()
            .find(|(note, _)| note.contains(name))
            .map(|(_, ink)| *ink)
            .unwrap()
    };
    // **どれも同じ実線に落ちていない。**5種類とも墨の量が違う。
    for (at, (note, ink)) in lines.iter().enumerate() {
        assert!(
            lines[..at].iter().all(|(_, other)| other != ink),
            "{note}: ほかの線と同じ墨になっている: {lines:?}"
        );
    }
    assert!(
        line("二重傍線］") > line("に傍線］"),
        "2本のほうが墨が多い: {lines:?}"
    );
    // 刻んだぶんだけ墨が減る——破線がいちばん空いている。
    assert!(
        line("破線］") < line("鎖線］") && line("鎖線］") < line("に傍線］"),
        "刻みの多さと墨の量が合わない: {lines:?}"
    );
}

/// 要件 7.8（2026-09-17、書き手「組版の表現拡大」①③）: **組み方の注記が、幾何に届く。**
///
/// 行末のカーソルがどこに立つか＝その行がどこまで伸びたか、で測る。縦中横は寝ていた
/// 2字を1マスに、割注は4字を1マスに、文字の大きさは字そのものを縮める（広げる）。
#[test]
fn the_notes_that_set_a_stretch_change_how_far_the_line_reaches() {
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
    // 縦中横の旗は2つとも入れる（既定は切）。縦書きのシートだけが持つ。
    Setting::UprightDigits.write(&numbers, 1, 1);
    Setting::UprightMarks.write(&numbers, 1, 1);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers));
    window.set_palette(ModelRc::from(palette));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let line_reach = |source: &str, vertical: bool| {
        // **面の向きは画面にも言う。**縦書きにしか無い設定（縦中横）は縦書きのシートから
        // 読まれるので、`screen.vertical`が横のままだと横書きのシートの値で組まれる。
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = vertical;
        });
        let document =
            OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
        let states = PaneStates::new(&document);
        set_pane_direction(&window, &cache, id, vertical);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            // カーソルは次の行に置く：組んだ姿のまま測る。
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        let mut borrowed = cache.borrow_mut();
        let pane = borrowed.pane(id);
        let shown = pane.view.preview_slot.preview.text.clone();
        let at = shown.find('\n').unwrap_or(shown.len());
        let utf16 = shown[..at].encode_utf16().count() as u32;
        let caret = pane.graphics.engine.caret_geometry(utf16).unwrap();
        if vertical { caret.y } else { caret.x }
    };

    // **枡目は差で測る。**カーソルの座標には紙の余白が入っているので、1字増やしたときの
    // 伸びぶんが1マスであり、そこから余白を割り出す。
    let cell = line_reach("あいうえおか\n\n", true) - line_reach("あいうえお\n\n", true);
    let margin = line_reach("あいうえお\n\n", true) - cell * 5.0;
    let cells = |source: &str, vertical: bool| (line_reach(source, vertical) - margin) / cell;

    // ① 半角記号の縦中横。**全角の`！？`は2マス、半角の`!?`は1マス。**
    let sideways = cells("えっ！？と\n\n", true);
    let upright = cells("えっ!?と\n\n", true);
    assert!(
        (sideways - 5.0).abs() < 0.2,
        "全角5字が5マスになっていない（{sideways}マス）"
    );
    assert!(
        (upright - 4.0).abs() < 0.2,
        "`!?`が1マスに収まっていない（{upright}マス）"
    );

    // ① 書き手が名指した縦中横も1マス。3字でも枡目からはみ出さない。
    let named = cells("第［＃縦中横］10［＃縦中横終わり］章\n\n", true);
    assert!(
        (named - 3.0).abs() < 0.2,
        "名指した縦中横が1マスに収まっていない（{named}マス）"
    );

    // ③ 割注。4字が半分の大きさで2行になるので、1マスぶんしか取らない。
    let plain = cells("本文注記ですの続き\n\n", true);
    let warichu = cells("本文［＃割り注］注記です［＃割り注終わり］の続き\n\n", true);
    assert!(
        (plain - 9.0).abs() < 0.2,
        "地の文が9マスになっていない（{plain}マス）"
    );
    assert!(
        (warichu - 6.0).abs() < 0.3,
        "割注が1マスに収まっていない（{warichu}マス）"
    );

    // ③ 文字の大きさ。**どちらの書字方向でも効く**——箱ではなく字そのものの大きさである。
    for vertical in [false, true] {
        let body = line_reach("ここは細かい話です\n\n", vertical);
        let small = line_reach(
            "ここは［＃小さな文字］細かい話［＃小さな文字終わり］です\n\n",
            vertical,
        );
        let large = line_reach(
            "ここは［＃大きな文字］細かい話［＃大きな文字終わり］です\n\n",
            vertical,
        );
        assert!(
            small < body && body < large,
            "vertical={vertical}: 字の大きさが効いていない（小{small} 本文{body} 大{large}）"
        );
    }
}

/// 要件 7.8（2026-09-17、書き手「組版の表現拡大」②）: **左の注記が画素に届く。**
///
/// どちら側に出るかは帯の置き場所を決める`beside_the_line`の仕事で、そちらはその場で
/// 測ってある（`the_band_of_a_left_note_is_on_the_other_side`）。ここで見るのは、
/// 注記の字がちゃんと組まれて墨になっていること——注記が消えて何も出ない、が起きないこと。
#[test]
fn a_left_note_lands_on_the_other_side_of_the_word() {
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
    let (width, height) = (1100usize, 760usize);
    surface.set_size(slint::PhysicalSize::new(width as u32, height as u32));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    // 1列のうち、どのxに墨があるか。縦書きなので列は右から並ぶ。
    let columns_of = |source: &str| {
        let document = OpenDocument::new(
            DocumentFile::untitled(1),
            source.to_owned().into(),
            window.as_weak(),
        );
        let states = PaneStates::new(&document);
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = true;
        });
        set_pane_direction(&window, &cache, id, true);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        let mut ink = vec![0usize; width];
        for (at, pixel) in pixels.iter().enumerate() {
            if pixel.r < 150 && pixel.g < 150 && pixel.b < 150 {
                ink[at % width] += 1;
            }
        }
        ink
    };
    let total = |ink: &[usize]| -> usize { ink.iter().sum() };

    let bare = total(&columns_of("東京\n"));
    let ruby = total(&columns_of("｜東京《とうきょう》\n"));
    let note = total(&columns_of(
        "東京［＃「東京」の左に「とうきょう」の注記］\n",
    ));
    // 右と左に1本ずつ。同じ5字を組むので、増える墨はどちらも同じくらいになる。
    let both = total(&columns_of(
        "｜東京《とうきょう》［＃「東京」の左に「とうけい」の注記］\n",
    ));

    assert!(ruby > bare, "ルビの墨が出ていない（{bare} → {ruby}）");
    assert!(note > bare, "左の注の墨が出ていない（{bare} → {note}）");
    assert!(
        both > ruby && both > note,
        "両側に出していない（右{ruby} 左{note} 両方{both}）"
    );
}

/// 要件 7.8（2026-09-16、書き手「組版の表現拡大」）: **体裁の注記の字下げが、組みに届く。**
///
/// `［＃ここからN字下げ］`の中の行と`［＃N字下げ］`の行は、地の文より字数ぶん内側から始まる。
#[test]
fn a_note_indent_moves_where_the_line_starts() {
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let source = "地の文。\n［＃ここから2字下げ］\n手紙の行。\n［＃ここで字下げ終わり］\n地の文。\n［＃1字下げ］この行だけ。\n［＃地付き］署名。\n［＃地から2字上げ］結び。\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    for vertical in [true, false] {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.vertical = false;
        });
        set_pane_direction(&window, &cache, id, vertical);
        {
            let state = states.of(id);
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(source.len());
            state.active_line_start = Some(source.len());
        }
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        // 行の頭がどこから始まるか——行の軸（縦書きはy、横書きはx）で見る。
        let head_of = |needle: &str| {
            let mut borrowed = cache.borrow_mut();
            let pane = borrowed.pane(id);
            let shown = pane.view.preview_slot.preview.text.clone();
            let at = shown.find(needle).unwrap();
            let utf16 = shown[..at].encode_utf16().count() as u32;
            let caret = pane.graphics.engine.caret_geometry(utf16).unwrap();
            if vertical { caret.y } else { caret.x }
        };
        let body = head_of("地の文。");
        let letter = head_of("手紙の行。");
        let single = head_of("この行だけ。");
        let cell = 22.0 * 1.3;
        assert!(
            (letter - body - cell * 2.0).abs() < cell * 0.6,
            "vertical={vertical}: 2字下げになっていない（地の文{body}、手紙{letter}）"
        );
        assert!(
            (single - body - cell).abs() < cell * 0.6,
            "vertical={vertical}: 1字下げになっていない（地の文{body}、その行{single}）"
        );
        // 地付きは行の終わりへ寄る。地から2字上げは、そこから2字ぶん手前で終わる。
        let end_of = |needle: &str| {
            let mut borrowed = cache.borrow_mut();
            let pane = borrowed.pane(id);
            let shown = pane.view.preview_slot.preview.text.clone();
            let at = shown.find(needle).unwrap() + needle.len();
            let utf16 = shown[..at].encode_utf16().count() as u32;
            let caret = pane.graphics.engine.caret_geometry(utf16).unwrap();
            if vertical { caret.y } else { caret.x }
        };
        let plain_end = end_of("地の文。");
        let flush = end_of("署名。");
        let raised = end_of("結び。");
        assert!(
            flush > plain_end + cell * 2.0,
            "vertical={vertical}: 地付きが行末へ寄っていない（地の文{plain_end}、署名{flush}）"
        );
        assert!(
            (flush - raised - cell * 2.0).abs() < cell * 0.6,
            "vertical={vertical}: 地から2字上げになっていない（地付き{flush}、結び{raised}）"
        );
    }
}

/// 書き手の報告 2026-09-22: **右クリックの右へ開いた面の行が押せる。**Slintは`show()`の
/// 時点の幅をpopupへ書き込むので、開いてから枠を広げても、右の面は「外」と見なされ、
/// 押せば行に届かずメニューが閉じていた。
#[test]
fn a_row_in_the_right_click_flyout_answers_the_click() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let source = "本文の一行目。\n二行目。\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    id.update_screen(&window, |screen| {
        screen.width = 1050.0;
        screen.height = 640.0;
        screen.shown_width = 1050.0;
        screen.shown_height = 540.0;
        screen.preview = true;
    });
    set_pane_direction(&window, &cache, id, false);
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    let listed = Rc::new(RefCell::new(Vec::new()));
    let received = listed.clone();
    window.on_pane_list_edit(move |_, what, mark| received.borrow_mut().push((what, mark)));
    let copied = Rc::new(Cell::new(0));
    let received = copied.clone();
    window.on_pane_copy_body(move |_| received.set(received.get() + 1));
    let press = |x, y, button| {
        let position = slint::LogicalPosition::new(x, y);
        window
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
    };
    // 窓の右寄りで開く——枠が右の面ぶん広いので、左へ寄せて窓に収まる（x=668）。
    press(800.0, 200.0, PointerEventButton::Right);
    // 「箇条書き▸」に乗せて右へ開き、開いた面のいちばん上（`-`）を押す。
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(840.0, 430.0),
    });
    press(1000.0, 430.0, PointerEventButton::Left);
    assert_eq!(
        &*listed.borrow(),
        &[(0, 0)],
        "a row in the flyout must answer the click, not close the menu"
    );
    // 枠の右の空きは今までどおり「外」——押せば閉じ、下の行の位置はもう押せない。
    press(800.0, 200.0, PointerEventButton::Right);
    press(1060.0, 220.0, PointerEventButton::Left);
    press(840.0, 376.0, PointerEventButton::Left);
    assert_eq!(copied.get(), 0, "the empty right of the menu must close it");
    // 閉じずに押せば、同じ位置の「本文だけをコピー」に届く（位置の確かめ）。
    press(800.0, 200.0, PointerEventButton::Right);
    press(840.0, 376.0, PointerEventButton::Left);
    assert_eq!(copied.get(), 1);
}
