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
    window.set_settings_tab(3);
    window.invoke_show_settings();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("settings.ppm"), ppm).unwrap();
    // Drag the settings header, keeping the mouse grab across movement.
    use slint::platform::{PointerEventButton, WindowEvent};
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(410.0, 62.0),
        button: PointerEventButton::Left,
    });
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(510.0, 112.0),
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(510.0, 112.0),
            button: PointerEventButton::Left,
        });
    assert!((window.get_settings_offset_x() - 100.0).abs() < 1.0);
    assert!((window.get_settings_offset_y() - 50.0).abs() < 1.0);
    slint::platform::update_timers_and_animations();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("settings-moved.ppm"), ppm).unwrap();
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
    click(480.0, 140.0);
    assert!(
        window.get_settings_open(),
        "clicking the frame must not close settings"
    );
    // H1's background menu follows the moved settings panel.
    click(670.0, 328.0);
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1100);
    });
    let mut ppm = b"P6\n1100 760\n255\n".to_vec();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(output.join("background-menu.ppm"), ppm).unwrap();
    click(530.0, 364.0);
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
    click(50.0, 700.0);
    assert!(
        !window.get_settings_open(),
        "clicking outside closes settings"
    );
    window.invoke_show_settings();
    assert!((window.get_settings_offset_x() - 100.0).abs() < 1.0);
    assert_eq!(&*document.text.borrow(), source);
    window.set_settings_open(false);
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
    click(930.0, 426.0);
    assert_eq!(
        goto_requests.get(),
        1,
        "Go to Line must be reachable from the pane menu"
    );
    click(1077.0, 24.0);
    menu_snapshot("pane-menu.ppm");
    click(930.0, 478.0);
    assert_eq!(
        saved_requests.get(),
        1,
        "saved comparison is in the pane menu"
    );
    click(1077.0, 24.0);
    menu_snapshot("pane-menu.ppm");
    click(930.0, 507.0);
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
    let position = slint::LogicalPosition::new(200.0, 200.0);
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
    click(240.0, 376.0);
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
    click(240.0, 219.0);
    assert_eq!(undo_calls.get(), 0, "disabled Undo must not dispatch");
    click(700.0, 700.0);
    insert_pane_text(&window, id, &document, &states, &cache, "追加", false);
    let edited = document.text.borrow().clone();
    assert_ne!(edited, source);
    assert!(id.screen(&window).can_undo);
    open_menu();
    click(240.0, 219.0);
    assert_eq!(undo_calls.get(), 1);
    assert_eq!(&*document.text.borrow(), source);
    assert!(id.screen(&window).can_redo);
    open_menu();
    click(240.0, 248.0);
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
    replace_document(&window, &states, &cache, &document, "置換".into());
    undo_in_pane(&window, id, &document, &states, &cache, false);
    undo_in_pane(&window, id, &document, &states, &cache, true);
    set_pane_preedit(&window, id, &document, &states.of(id), &cache, "変換中");
    assert_eq!(&*document.text.borrow(), source);
    assert!(states.of(id).borrow().preedit.is_empty());
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    assert!(id.screen(&window).viewer);
    let mut preview_slot = PreviewSlot::default();
    let completed = pane_text(&window, id, &mut preview_slot, source, Some(0))
        .text()
        .to_owned();
    assert!(
        !completed.starts_with('#'),
        "Viewer must not reveal the clicked heading's source"
    );
    assert_eq!(
        completed,
        pane_text(&window, id, &mut preview_slot, source, None).text()
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
