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

#[test]
fn real_transparency_preserves_text_chrome_and_saved_settings() {
    use slint::platform::software_renderer::PremultipliedRgbaColor;
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
    window.set_tree_open(false);
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    publish_panes(&window, 1);
    let id = PaneId::FIRST;
    let source = "# 透過の確認\n背景だけが透けます。文字は不透明です。\n";
    let document = OpenDocument::new(DocumentFile::untitled(1), source.into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let draw = || {
        window.window().request_redraw();
        let mut pixels = vec![PremultipliedRgbaColor::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        pixels
    };
    for vertical in [false, true] {
        id.update_screen(&window, |s| {
            s.width = 1050.;
            s.height = 640.;
            s.shown_width = 1050.;
            s.shown_height = 540.;
            s.preview = true;
            s.vertical = vertical;
        });
        set_pane_direction(&window, &cache, id, vertical);
        window.set_background_transparency(0);
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        let plain = draw();
        assert_eq!(plain[520 * 1100 + 600].alpha, 255);
        window.set_background_transparency(50);
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
        assert!(!pane_typography(&window, id).paper_painted);
        let transparent = draw();
        assert!(
            (126..=129).contains(&transparent[520 * 1100 + 600].alpha),
            "paper alone is 50% opaque, vertical={vertical}: {:?}",
            transparent[520 * 1100 + 600]
        );
        assert_eq!(
            transparent[10 * 1100 + 600].alpha,
            255,
            "tab strip is opaque"
        );
        let solid_text = (70..500)
            .flat_map(|y| (80..1050).map(move |x| y * 1100 + x))
            .filter(|&at| {
                let (a, b) = (plain[at], transparent[at]);
                a.alpha == 255
                    && a.red < 100
                    && a.green < 100
                    && a.blue < 100
                    && b.alpha == 255
                    && (a.red, a.green, a.blue) == (b.red, b.green, b.blue)
            })
            .count();
        assert!(
            solid_text > 50,
            "solid text keeps its color and alpha, vertical={vertical}"
        );
        window.set_background_transparency(100);
        let clear = draw();
        assert_eq!(clear[520 * 1100 + 600].alpha, 0);
        assert_eq!(clear[10 * 1100 + 600].alpha, 255);
        id.update_screen(&window, |s| s.settings = true);
        assert_eq!(draw()[520 * 1100 + 600].alpha, 255, "settings stay opaque");
        id.update_screen(&window, |s| {
            s.settings = false;
            s.terminal = true;
        });
        assert_eq!(draw()[520 * 1100 + 600].alpha, 255, "terminal stays opaque");
        id.update_screen(&window, |s| s.terminal = false);
    }
    // New Tab has no document yet. Focusing it must not reveal paper edges
    // behind the transparent start-page overlay, in either writing direction.
    for vertical in [false, true] {
        id.update_screen(&window, |s| {
            s.vertical = vertical;
            s.empty = true;
            s.content_width = 700;
            s.content_height = 500;
            s.tiles = ModelRc::default();
            s.caret_visible = false;
        });
        for transparency in [0, 50, 100] {
            window.set_background_transparency(transparency);
            window.set_focused_pane(-1);
            let unfocused = draw();
            window.set_focused_pane(0);
            let focused = draw();
            for y in 80..600 {
                for x in 50..1050 {
                    let at = y * 1100 + x;
                    let a = unfocused[at];
                    let b = focused[at];
                    assert_eq!(
                        (a.red, a.green, a.blue, a.alpha),
                        (b.red, b.green, b.blue, b.alpha),
                        "New Tab must not show focus edges: vertical={vertical}, transparency={transparency}, at=({x},{y})"
                    );
                }
            }
        }
    }
    id.update_screen(&window, |s| s.empty = false);
    // A long page exceeds i16 coordinates. Its visible paper and focus edges
    // must be clipped before the software renderer receives rectangles.
    for vertical in [false, true] {
        for transparency in [0, 50, 100] {
            window.set_background_transparency(transparency);
            for offset in [0., -90_000.] {
                id.update_screen(&window, |s| {
                    s.vertical = vertical;
                    s.content_width = if vertical { 100_000 } else { 1000 };
                    s.content_height = if vertical { 500 } else { 100_000 };
                    s.scroll_x = if vertical { offset } else { 0. };
                    s.scroll_y = if vertical { 0. } else { offset };
                    s.scroll_generation += 1;
                    s.tiles = ModelRc::default();
                });
                let pixels = draw();
                assert_eq!(pixels[10 * 1100 + 600].alpha, 255);
            }
        }
    }
    // Restore the value through the real settings parser; image choice survives.
    window.set_background_transparency(37);
    window.set_wall_kind(wallpaper::FILE);
    window.set_wall_path("missing-image-does-not-need-loading.bmp".into());
    wallpaper::publish(&window).expect("real transparency does not load the hidden wallpaper");
    let saved = settings_values(&window);
    window.set_background_transparency(0);
    window.set_wall_kind(0);
    apply_settings(&window, &numbers, &palette, &fonts, &saved);
    assert_eq!(window.get_background_transparency(), 37);
    assert_eq!(window.get_wall_kind(), wallpaper::FILE);
    for (written, expected) in [("-1", 0), ("120", 100), ("broken", 0)] {
        apply_settings(
            &window,
            &numbers,
            &palette,
            &fonts,
            &[(BACKGROUND_TRANSPARENCY_SETTING.into(), written.into())],
        );
        assert_eq!(window.get_background_transparency(), expected);
    }
}

/// 追加要件 2026-09-14（書き手）: 設定はダイアログではなくTABで開く。
///
/// **窓に1つ**——もう一度頼めば開いているTABへ移り、New Tab はそれになる。
/// 前にある間、本文に効く鍵（保存など）は代役の文書へ届かず、TABを移る鍵は
/// 届く。セッションには残らない。
#[test]
fn settings_open_as_the_one_tab_and_keep_document_keys_away() {
    let directory = std::env::temp_dir().join(format!(
        "editor-settings-{}-{}",
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
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 700.0;
    });
    window.set_autosave(false);
    let document = OpenDocument::untitled(1, window.as_weak());
    *document.text.borrow_mut() = "本文".into();
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
    shortcuts::wire(&window, &live);
    let saves = Rc::new(Cell::new(0));
    let seen = saves.clone();
    window.on_save_requested(move || seen.set(seen.get() + 1));
    let steps = Rc::new(Cell::new(0));
    let seen = steps.clone();
    window.on_pane_tab_stepped(move |_, _| seen.set(seen.get() + 1));
    let strip = || {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        (
            strip
                .tabs
                .iter()
                .map(|tab| tab.settings)
                .collect::<Vec<_>>(),
            strip.active,
        )
    };

    // A New Tab becomes the settings rather than gaining a neighbour.
    new_tab(&window, &live, id);
    open_settings(&window, &live);
    assert_eq!(strip(), (vec![false, true], 1));
    assert!(id.screen(&window).settings);
    let shown = id.screen(&window).tabs.row_data(1).unwrap();
    assert_eq!(shown.title, settings_tab_name());
    assert!(!shown.edited && !shown.renamable);

    // Asked again from another tab, the one already open comes to the front.
    switch_to_tab(&window, &live, id, 0);
    assert!(!id.screen(&window).settings);
    open_settings(&window, &live);
    assert_eq!(strip(), (vec![false, true], 1));
    assert!(id.screen(&window).settings);

    // Saving is not for a stand-in; moving between tabs is.
    assert!(window.invoke_shortcut_key("s".into(), true, false, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(saves.get(), 0);
    let tab: SharedString = slint::platform::Key::Tab.into();
    assert!(window.invoke_shortcut_key(tab, true, false, false));
    slint::platform::update_timers_and_animations();
    assert_eq!(steps.get(), 1);

    // A size typed into the field takes effect on Enter (書き手の求め 2026-09-15).
    {
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        let typed = Rc::new(RefCell::new(Vec::new()));
        let seen = typed.clone();
        let sheets = window.as_weak();
        window.on_sheet_typed(move |setting, text| {
            let sheet = sheets.upgrade().unwrap().get_sheet();
            seen.borrow_mut().push((sheet, setting, text.to_string()));
        });
        window.set_tree_open(false);
        window.set_settings_tab(3);
        window.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.window().request_redraw();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
                renderer.render(&mut pixels, 1000);
            });
        };
        draw();
        // H1 の大きさの欄（上に COLOR SETS の行がある）。
        let position = slint::LogicalPosition::new(322.0, 248.0);
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
        draw();
        let key = |text: SharedString| {
            window
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            window
                .window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        };
        key(Key::End.into());
        for _ in 0..3 {
            key(Key::Backspace.into());
        }
        assert!(typed.borrow().is_empty(), "nothing is set while typing");
        for c in ["2", "5", "0"] {
            key(c.into());
        }
        key(Key::Return.into());
        draw();
        assert_eq!(&*typed.borrow(), &[(0, 4, "250".to_owned())]);

        // 一覧が指すセットは、覚えた番号ではなく今の文字色から決まる（書き手の報告 2026-09-15）。
        let sets = VecModel::from(vec![SharedString::new(); 10]);
        window.set_ink_sets(ModelRc::new(sets));
        crate::match_ink_set(&window);
        assert_eq!(
            window.get_ink_set_chosen(),
            -1,
            "no set holds these colours"
        );
        let now = crate::ink_set_of(&*palette);
        window.get_ink_sets().set_row_data(3, now.as_str().into());
        window.get_ink_sets().set_row_data(6, now.as_str().into());
        crate::match_ink_set(&window);
        assert_eq!(window.get_ink_set_chosen(), 3);
        window.set_ink_set_chosen(6);
        crate::match_ink_set(&window);
        assert_eq!(window.get_ink_set_chosen(), 6, "the one pointed at is kept");
        palette.set_row_data(0, Color::from_rgb_u8(1, 2, 3));
        crate::match_ink_set(&window);
        assert_eq!(
            window.get_ink_set_chosen(),
            -1,
            "a colour changed by hand is Custom"
        );
        window.hide().unwrap();
    }

    // Every group, as the writer will see it: `EDITOR_SETTINGS_SNAPSHOT` names
    // a folder for the images.
    if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
        let output = PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        window.set_tree_open(false);
        window.show().unwrap();
        for group in 0..8 {
            window.set_settings_tab(group);
            if group == 6 {
                for category in 0..4 {
                    window.invoke_shortcut_fold(category);
                }
                window.set_shortcut_selected(11);
                window.set_shortcut_edit("Ctrl+Tab".into());
                window.invoke_shortcut_filter();
            }
            slint::platform::update_timers_and_animations();
            window.window().request_redraw();
            let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 1000);
            });
            let mut ppm = b"P6\n1000 740\n255\n".to_vec();
            for pixel in pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(output.join(format!("settings-{group}.ppm")), ppm).unwrap();
        }
        window.set_colour_mixer_current(Color::from_rgb_u8(0x44, 0x72, 0xc4));
        window.set_colour_mixer_hue(20.0);
        window.set_colour_mixer_saturation(0.8);
        window.set_colour_mixer_value(0.9);
        window.set_colour_mixer_open(true);
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let mut ppm = b"P6\n1000 740\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join("colour-mixer.ppm"), ppm).unwrap();
        window.set_colour_mixer_open(false);
    }

    // C5 (書き手の求め 2026-09-15): the palette's doors.
    {
        wiring::wire_colours(
            &window,
            &live,
            &live.states,
            &live.cache,
            Rc::new(Timer::default()),
            numbers.clone(),
            palette.clone(),
        );
        let red = Color::from_rgb_u8(255, 0, 0);
        let row = |sheet, slot| palette.row_data(colour_row(sheet, slot)).unwrap();
        window.set_sheet(1);
        window.invoke_colour_set(0, 1, red);
        assert_eq!(row(1, 1), red);
        assert_ne!(row(0, 1), red, "the other sheet stays");
        assert_eq!(
            window.global::<Colours>().get_recent().row_data(0),
            Some(red)
        );
        // A background chosen from the palette is a background that is on.
        window.invoke_colour_set(0, 9, Color::from_rgb_u8(0, 0, 255));
        assert_eq!(Setting::Decoration(1, 3).read(&window, 1), 1);
        window.invoke_colour_default(0, 9);
        assert_eq!(Setting::Decoration(1, 3).read(&window, 1), 0);
        window.invoke_colour_default(0, 1);
        assert_eq!(row(1, 1), slint_colour(default_colour(1, 1)));

        // その他の色: opened on the colour there now, typed into, then accepted.
        window.invoke_colour_set(0, 2, red);
        window.set_sheet(0);
        window.invoke_colour_more(0, 2);
        assert!(window.get_colour_mixer_open());
        window.set_sheet(1);
        window.invoke_colour_hex_typed("00ff00".into());
        assert!((window.get_colour_mixer_hue() - 120.0).abs() < 0.5);
        window.invoke_colour_channel_typed(0, "２５５".into());
        assert!((window.get_colour_mixer_hue() - 60.0).abs() < 0.5);
        assert_eq!(
            window.invoke_colour_hex_of(Color::from_rgb_u8(255, 255, 0)),
            "ffff00"
        );
        window.invoke_colour_mixer_accepted(Color::from_rgb_u8(255, 255, 0));
        assert_eq!(
            row(0, 2),
            Color::from_rgb_u8(255, 255, 0),
            "the sheet it was opened from, not the one now in `sheet`"
        );

        // Recent colours: newest first, no repeats, ten at most, and kept.
        for shade in 0..12u8 {
            window.invoke_colour_set(1, 1, Color::from_rgb_u8(shade, shade, shade));
        }
        window.invoke_colour_set(1, 1, Color::from_rgb_u8(11, 11, 11));
        let recent: Vec<Color> = window.global::<Colours>().get_recent().iter().collect();
        assert_eq!(recent.len(), wiring::RECENT_COLOURS);
        assert_eq!(recent[0], Color::from_rgb_u8(11, 11, 11));
        assert_eq!(recent[1], Color::from_rgb_u8(10, 10, 10));
        assert_eq!(window.get_terminal_ink(), Color::from_rgb_u8(11, 11, 11));
        let stored = settings_values(&window);
        window
            .global::<Colours>()
            .set_recent(ModelRc::new(VecModel::from(Vec::<Color>::new())));
        let fonts = VecModel::from(vec![SharedString::default(); 2 * SHEET_FONTS]);
        apply_settings(&window, &numbers, &palette, &fonts, &stored);
        assert_eq!(
            window
                .global::<Colours>()
                .get_recent()
                .iter()
                .collect::<Vec<_>>(),
            recent
        );
    }

    // Reset All puts every document's mode back to none (書き手の求め 2026-09-15).
    live.tabs.borrow_mut().panes[0].tabs[0].word_mode = 3;
    id.update_screen(&window, |screen| screen.word_mode = 3);
    reset_all_settings(&window, &live);
    assert_eq!(live.tabs.borrow().panes[0].tabs[0].word_mode, 0);
    assert_eq!(id.screen(&window).word_mode, 0);

    // Nothing to reopen next time: the session names the document only.
    let session = session::capture_session(&window, &live);
    assert_eq!(session.panes[0].tabs.len(), 1);
    assert_eq!(&*document.text.borrow(), "本文");
    drop(live);
    std::fs::remove_dir_all(&directory).unwrap();
}

/// 書き手の求め 2026-09-15: 各面の Reset はその面の値だけを、両方のシートで戻す。
#[test]
fn each_page_reset_stays_on_its_page() {
    let numbers = VecModel::from(vec![0; 2 * SHEET_NUMBERS]);
    let palette = VecModel::from(vec![Color::default(); 2 * SHEET_COLOURS]);
    let fonts = VecModel::from(vec![SharedString::default(); 2 * SHEET_FONTS]);
    reset_settings(&numbers, &palette, &fonts);
    let number = |setting: Setting, sheet: usize| {
        numbers
            .row_data(sheet * SHEET_NUMBERS + setting.row_in_sheet())
            .unwrap()
    };
    let change = || {
        for sheet in 0..2 {
            Setting::BodySize.write(&numbers, sheet, 40);
            Setting::LineAdvance.write(&numbers, sheet, 300);
            set_colour(&palette, sheet, 0, [1.0, 0.0, 0.0]);
            set_colour(&palette, sheet, PAPER_SLOT, [0.0, 1.0, 0.0]);
            fonts.set_row_data(font_row(sheet, 0), "Meiryo".into());
            fonts.set_row_data(font_row(sheet, CODE_SLOT), "Meiryo".into());
        }
    };
    let paper = |sheet| palette.row_data(colour_row(sheet, PAPER_SLOT)).unwrap();
    let ink = |sheet| palette.row_data(colour_row(sheet, 0)).unwrap();

    change();
    reset_settings_group(&numbers, &palette, &fonts, 3);
    for sheet in 0..2 {
        assert_eq!(number(Setting::BodySize, sheet), BASE_FONT_SIZE);
        assert_eq!(ink(sheet), slint_colour(default_colour(sheet, 0)));
        assert_eq!(number(Setting::LineAdvance, sheet), 300, "Layout stays");
        assert_ne!(
            paper(sheet),
            slint_colour(default_colour(sheet, PAPER_SLOT))
        );
        assert_eq!(
            fonts.row_data(font_row(sheet, CODE_SLOT)).unwrap(),
            default_font(CODE_SLOT),
            "the code font is Text's"
        );
    }

    change();
    reset_settings_group(&numbers, &palette, &fonts, 4);
    assert_eq!(number(Setting::LineAdvance, 1), 100);
    assert_eq!(number(Setting::BodySize, 1), 40, "Text stays");

    change();
    reset_settings_group(&numbers, &palette, &fonts, 5);
    assert_eq!(paper(0), slint_colour(default_colour(0, PAPER_SLOT)));
    assert_eq!(
        fonts.row_data(font_row(0, CODE_SLOT)).unwrap(),
        "Meiryo",
        "Text stays"
    );
    assert_eq!(Setting::UprightDigits.default_value(), 0);
    assert_eq!(
        fonts.row_data(font_row(0, 0)).unwrap(),
        "Meiryo",
        "Text stays"
    );
}

/// 書き手の求め 2026-09-15: 打った数は全角でも単位付きでも読み、pt は px と % へ直す。
#[test]
fn typed_sizes_read_full_width_and_points() {
    assert_eq!(typed_number("30"), Some(30.0));
    assert_eq!(typed_number("３０"), Some(30.0));
    assert_eq!(typed_number(" 16.5pt "), Some(16.5));
    assert_eq!(typed_number("１２．５"), Some(12.5));
    assert_eq!(typed_number("200%"), Some(200.0));
    assert_eq!(typed_number("abc"), None);
    assert_eq!(typed_number(""), None);

    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = AppWindow::new().unwrap();
    let numbers = Rc::new(VecModel::from(vec![0; 2 * SHEET_NUMBERS]));
    let palette = VecModel::from(vec![Color::default(); 2 * SHEET_COLOURS]);
    let fonts = VecModel::from(vec![SharedString::default(); 2 * SHEET_FONTS]);
    reset_settings(&numbers, &palette, &fonts);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_sheet(1);
    type_setting(&window, &numbers, Setting::Heading(0), "３０");
    assert_eq!(
        Setting::Heading(0).read(&window, 1),
        50,
        "held to the range"
    );
    type_setting(&window, &numbers, Setting::Heading(0), "250");
    assert_eq!(Setting::Heading(0).read(&window, 1), 250);
    assert_eq!(
        Setting::Heading(0).read(&window, 0),
        200,
        "other sheet stays"
    );
    type_points(&window, &numbers, 0, "12");
    assert_eq!(Setting::BodySize.read(&window, 1), 16);
    type_points(&window, &numbers, 1, "24");
    assert_eq!(Setting::Heading(0).read(&window, 1), 200);
    type_setting(&window, &numbers, Setting::BodySize, "不正");
    assert_eq!(
        Setting::BodySize.read(&window, 1),
        16,
        "unreadable changes nothing"
    );
}

/// 追加要件 2026-09-15（書き手）: TAB毎・Pane毎の紙の色。
///
/// **TAB > Pane > 全体**で、横書き・縦書きは別。付けた色は組版の紙（`pane_typography`）と
/// 画面の行の両方に出て、TABを切り替えればそのTABの色に、既定へ戻せば下の段の色になる。
/// セッションにも残る。ランダムは暗い字に対して淡い色を選ぶ。
#[test]
fn a_tab_and_a_pane_carry_their_own_paper() {
    let directory = std::env::temp_dir().join(format!(
        "editor-paper-{}-{}",
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
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    window.set_autosave(false);
    let document = OpenDocument::untitled(1, window.as_weak());
    *document.text.borrow_mut() = "本文".into();
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
    wiring::wire_colours(
        &window,
        &live,
        &live.states,
        &live.cache,
        Rc::new(Timer::default()),
        numbers.clone(),
        palette.clone(),
    );
    let paper = || pane_typography(&window, id).paper;
    let global = channels(palette.row_data(colour_row(0, PAPER_SLOT)).unwrap());
    let red = Color::from_rgb_u8(255, 0, 0);
    let blue = Color::from_rgb_u8(0, 0, 255);
    assert_eq!(paper(), global);

    window.invoke_colour_set(4, 0, blue);
    assert_eq!(paper(), channels(blue), "the pane's paper");
    assert!(id.screen(&window).paper_h_own && id.screen(&window).paper_v_own);
    window.invoke_colour_set(3, 0, red);
    assert_eq!(paper(), channels(red), "the tab wins over the pane");
    let session = session::capture_session(&window, &live);
    assert_eq!(session.panes[0].paper, [Some([0, 0, 255]); 2]);
    assert_eq!(session.panes[0].tabs[0].paper, [Some([255, 0, 0]); 2]);

    // Another tab in the same pane shows the pane's paper; coming back shows the tab's.
    new_tab(&window, &live, id);
    assert_eq!(paper(), channels(blue), "a new tab has only the pane's");
    switch_to_tab(&window, &live, id, 0);
    assert_eq!(paper(), channels(red));

    // 書き手の判断 2026-09-15: Paneの色を変えれば、そのPaneのTABは全部塗り変わる——
    // TABに付けた紙の色も見出しの色も外れる。
    window.invoke_colour_set(5, 0, red);
    window.invoke_colour_set(4, 0, blue);
    assert_eq!(paper(), channels(blue), "the pane repaints its tabs");
    let tabs = live.tabs.borrow();
    assert!(
        tabs.of(id)
            .tabs
            .iter()
            .all(|tab| tab.paper == [None; 2] && tab.tab_colour.is_none())
    );
    drop(tabs);
    window.invoke_colour_set(3, 0, red);

    // Back to the default: the tab falls to the pane, the pane to the settings.
    window.invoke_colour_default(3, 0);
    assert_eq!(paper(), channels(blue));
    window.invoke_colour_default(4, 0);
    assert_eq!(paper(), global);

    // 書き手の判断 2026-09-15: TAB・Paneの色は縦書き・横書きにそろう。「縦書きを横書きに
    // 合わせる」を切り替えても変わらない。
    window.invoke_colour_set(4, 0, blue);
    window.invoke_colour_set(3, 0, red);
    for (vertical, shared) in [(true, false), (true, true), (false, true)] {
        id.update_screen(&window, |screen| screen.vertical = vertical);
        window.set_paper_shared(shared);
        assert_eq!(
            paper(),
            channels(red),
            "vertical={vertical} shared={shared}"
        );
    }
    window.set_paper_shared(false);
    id.update_screen(&window, |screen| screen.vertical = false);
    window.invoke_colour_default(3, 0);

    // 書き手の判断 2026-09-15: 全体の紙を変えたら、TAB・Paneに付けた色は外す。
    let green = Color::from_rgb_u8(0, 255, 0);
    window.invoke_colour_set(3, 0, red);
    window.invoke_colour_set(0, PAPER_SLOT as i32, green);
    assert_eq!(paper(), channels(green), "the global paper wins again");
    let tabs = live.tabs.borrow();
    assert_eq!(tabs.of(id).paper, [None; 2]);
    assert!(tabs.of(id).tabs.iter().all(|tab| tab.paper == [None; 2]));
    drop(tabs);
    window.invoke_colour_set(4, 0, blue);
    window.invoke_colour_default(0, PAPER_SLOT as i32);
    assert_eq!(paper(), global, "the default does the same");
    assert!(!id.screen(&window).paper_h_own);

    // 書き手の判断 2026-09-15: 全体の紙を既定から変えれば、選ばれていないTABもその色。
    // 既定のままなら今までの見た目（色を持たない）。
    let chips = || {
        let tabs = id.screen(&window).tabs;
        (0..tabs.row_count())
            .map(|at| tabs.row_data(at).unwrap())
            .collect::<Vec<_>>()
    };
    assert!(chips().len() > 1);
    window.invoke_colour_set(0, PAPER_SLOT as i32, green);
    assert!(
        chips()
            .iter()
            .all(|chip| chip.colour_own && chip.colour == green)
    );
    window.invoke_colour_default(0, PAPER_SLOT as i32);
    assert!(chips().iter().all(|chip| !chip.colour_own));

    // TABの色（書き手の求め 2026-09-15）: 既定は背景と同じ、個別に変えれば背景を変えても残る。
    let chip = || id.screen(&window).tabs.row_data(0).unwrap();
    switch_to_tab(&window, &live, id, 0);
    assert!(
        !chip().colour_own,
        "the global paper leaves the tab as it was"
    );
    window.invoke_colour_set(3, 0, red);
    assert!(
        chip().colour_own && chip().colour == red,
        "follows the tab's paper"
    );
    let yellow = Color::from_rgb_u8(255, 255, 0);
    window.invoke_colour_set(5, 0, yellow);
    window.invoke_colour_set(3, 0, blue);
    assert_eq!(chip().colour, yellow, "its own colour stays");
    assert!(chip().dark == false);
    assert_eq!(paper(), channels(blue), "the tab colour is not the paper");
    window.invoke_colour_default(5, 0);
    assert_eq!(chip().colour, blue, "back to following the paper");
    assert!(chip().dark, "a dark tab writes its name light");
    let session = session::capture_session(&window, &live);
    assert_eq!(session.panes[0].tabs[0].tab_colour, None);
    window.invoke_colour_default(3, 0);

    // Random paper is a global setting for new tabs (書き手の求め 2026-09-15): light or dark,
    // a new colour for each new tab, and nothing for tabs already open.
    let light = random_paper(true, 1);
    assert!(light.iter().all(|channel| *channel > 0.7), "{light:?}");
    let dark = random_paper(false, 12345);
    assert!(dark.iter().all(|channel| *channel < 0.3), "{dark:?}");
    window.invoke_paper_random_chosen(1);
    assert_eq!(paper(), global, "the tab already open keeps its paper");
    new_tab(&window, &live, id);
    let first = paper();
    // 明るい紙は彩度0.32まで・明度0.93からなので、いちばん暗い成分は 0.93×0.68≒0.63
    // まで下がる（`random_paper`）。0.7で見ていたときは、色によって落ちていた。
    assert!(first.iter().all(|channel| *channel > 0.6), "{first:?}");
    new_tab(&window, &live, id);
    assert_ne!(first, paper(), "each new tab its own");
    let tabs = live.tabs.borrow();
    let newest = tabs.of(id).current().unwrap();
    assert!(newest.paper[0].is_some() && newest.paper[0] == newest.paper[1]);
    drop(tabs);
    window.invoke_paper_random_chosen(0);
    new_tab(&window, &live, id);
    assert_eq!(paper(), global, "off: no colour");

    // 紙のReset（書き手の報告 2026-09-15）: **TAB・Paneに付けた色も外す。**残していたときは、
    // Resetが「横書きに合わせる」を切ったとたん前に縦書きへ付けた色が出て、紙が既定に戻らなかった。
    // 設定のTABは紙を持たないので、Paneの色を映さない。
    wiring::wire_typography(
        &window,
        &live,
        &live.states,
        &live.cache,
        Rc::new(Timer::default()),
        numbers.clone(),
        palette.clone(),
        fonts.clone(),
    );
    switch_to_tab(&window, &live, id, 0);
    window.invoke_colour_set(4, 0, blue);
    window.invoke_colour_set(3, 0, red);
    window.invoke_colour_set(5, 0, yellow);
    open_settings(&window, &live);
    let settings_chip = live.tabs.borrow().of(id).active;
    let chip_at = |at: usize| id.screen(&window).tabs.row_data(at).unwrap();
    assert!(
        !chip_at(settings_chip).colour_own,
        "the settings tab has no paper"
    );
    switch_to_tab(&window, &live, id, 0);
    window.set_paper_shared(true);
    window.invoke_typography_reset(5);
    assert!(!window.get_paper_shared());
    assert_eq!(paper(), global, "back to the default paper");
    let screen = id.screen(&window);
    assert!(!screen.paper_h_own && !screen.paper_v_own);
    let tabs = live.tabs.borrow();
    let strip = tabs.of(id);
    assert_eq!(strip.paper, [None; 2]);
    assert!(
        strip
            .tabs
            .iter()
            .all(|tab| tab.paper == [None; 2] && tab.tab_colour.is_none())
    );
    drop(tabs);
    assert!((0..id.screen(&window).tabs.row_count()).all(|at| !chip_at(at).colour_own));

    // 書式・レイアウトの「横書きに合わせる」（書き手の求め 2026-09-15）: 面ごと。縦書きにしか
    // 無い縦中横は共通にしない。保存するのは縦書き自身の値で、切れば戻る。
    let vertical = || typography_for(&window, 100, true, true);
    window.set_sheet(0);
    window.invoke_colour_set(0, 0, red);
    Setting::BodySize.write(&numbers, 0, 30);
    Setting::LineAdvance.write(&numbers, 0, 250);
    Setting::UprightDigits.write(&numbers, 1, 1);
    assert_ne!(vertical().ink, channels(red));
    window.invoke_text_shared_toggled(true);
    assert_eq!(vertical().ink, channels(red), "text: the horizontal ink");
    assert_eq!(Setting::BodySize.read(&window, 1), 30);
    assert_ne!(
        Setting::LineAdvance.read(&window, 1),
        250,
        "layout is its own page"
    );
    window.invoke_layout_shared_toggled(true);
    assert_eq!(Setting::LineAdvance.read(&window, 1), 250);
    assert_eq!(
        Setting::UprightDigits.read(&window, 1),
        1,
        "vertical-only stays"
    );
    let stored = settings_values(&window);
    let own = |name: &str| {
        stored
            .iter()
            .find(|(key, _)| key == name)
            .unwrap()
            .1
            .clone()
    };
    assert_ne!(
        own("v.body-size"),
        "30",
        "the vertical value is kept, not the shared one"
    );
    window.invoke_text_shared_toggled(false);
    window.invoke_layout_shared_toggled(false);
    assert_ne!(vertical().ink, channels(red));
    assert_ne!(Setting::BodySize.read(&window, 1), 30);
}

/// 書き手の報告 2026-09-15: **Paneメニューの「Terminal Below」が効かない。**
///
/// 設定のTAB化でこの行が`if !root.settings`に入り、「閉じてから頼む」の順が
/// 残った——閉じた時点で行ごと消え、頼みが届かない（技術検証 6.24）。
/// 本文の右クリックの同じ行も、押せば1回届くことを見る。
#[test]
fn terminal_below_rows_reach_the_pane() {
    use slint::platform::{PointerEventButton, WindowEvent};
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let document = OpenDocument::new(DocumentFile::untitled(1), "本文\n".into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    id.update_screen(&window, |screen| {
        screen.width = 1050.0;
        screen.height = 640.0;
        screen.shown_width = 1050.0;
        screen.shown_height = 540.0;
    });
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), "本文\n");
    let calls = Rc::new(std::cell::Cell::new(0));
    let seen = calls.clone();
    window.on_pane_below_toggled(move |_| seen.set(seen.get() + 1));
    let click = |x: f32, y: f32, button| {
        let position = slint::LogicalPosition::new(x, y);
        window
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
        slint::platform::update_timers_and_animations();
    };
    // ⋮ → Terminal Panel
    click(1077.0, 25.0, PointerEventButton::Left);
    click(900.0, 382.0, PointerEventButton::Left);
    assert_eq!(calls.get(), 1, "the pane menu row");
    // 本文の右クリック → Terminal Panel
    // **RFN01-47で挿入の行が増えた**ので、行は下へずれる（メニューは画面内へ
    // 寄せるので、上端は0に張り付く）。
    click(160.0, 150.0, PointerEventButton::Right);
    // **この試験は挿入の行を組まない**（`menu_commands`の配線をしない）ので、
    // メニューはアプリより少し短い——行の位置は実測で決めている。
    click(218.0, 540.0, PointerEventButton::Left);
    assert_eq!(calls.get(), 2, "the body menu row");
    let folder_calls = Rc::new(std::cell::Cell::new(0));
    let seen = folder_calls.clone();
    window.on_shell_profile_action(move |action, _| {
        if action == 8 {
            seen.set(seen.get() + 1);
        }
    });
    click(1077.0, 25.0, PointerEventButton::Left);
    click(900.0, 115.0, PointerEventButton::Left);
    assert_eq!(
        folder_calls.get(),
        1,
        "document-folder action survives popup closing"
    );
}

/// 小さなBMP（24bit、上から下）。WICが読める一番簡単な画像。
fn write_bmp(path: &std::path::Path, width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) {
    let row = (width * 3).div_ceil(4) * 4;
    let body = row * height;
    let mut bytes = Vec::new();
    bytes.extend(b"BM");
    bytes.extend((54 + body).to_le_bytes());
    bytes.extend([0u8; 4]);
    bytes.extend(54u32.to_le_bytes());
    bytes.extend(40u32.to_le_bytes());
    bytes.extend((width as i32).to_le_bytes());
    bytes.extend((-(height as i32)).to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(24u16.to_le_bytes());
    bytes.extend([0u8; 24]);
    for y in 0..height {
        let mut line = Vec::new();
        for x in 0..width {
            let [r, g, b] = pixel(x, y);
            line.extend([b, g, r]);
        }
        line.resize(row as usize, 0);
        bytes.extend(line);
    }
    std::fs::write(path, bytes).unwrap();
}

/// 追加要件 2026-09-15（書き手）: **背景の壁紙。**画像ファイルを敷くと、本文の紙が
/// 濃さぶん透けて画像が見え、字は濃いまま残る。タイルは紙を塗らない。
#[test]
fn a_background_image_shows_through_the_paper() {
    use slint::platform::{PointerEventButton, WindowEvent};
    let directory = std::env::temp_dir().join(format!(
        "editor-wall-{}-{}",
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let source = "# 見出し\n本文の字は壁紙の上でも濃いまま。\n";
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
    let draw = || {
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        pixels
    };
    let save = |name: &str, pixels: &[slint::Rgb8Pixel]| {
        let output = PathBuf::from("target/wallpaper-qa");
        std::fs::create_dir_all(&output).unwrap();
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join(name), ppm).unwrap();
    };
    // 本文の下の、字の無い所。
    let at = |pixels: &[slint::Rgb8Pixel], x: usize, y: usize| pixels[y * 1100 + x];
    let empty = (600, 520);

    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    let plain = draw();
    save("plain.ppm", &plain);
    assert!(pane_typography(&window, id).paper_painted);

    // 赤と青の市松（32px）。
    let image = directory.join("checker.bmp");
    write_bmp(&image, 64, 64, |x, y| {
        if (x / 32 + y / 32) % 2 == 0 {
            [220, 30, 30]
        } else {
            [30, 30, 220]
        }
    });
    window.set_wall_path(image.display().to_string().into());
    window.set_wall_kind(wallpaper::FILE);
    window.set_wall_strength(60);
    wallpaper::publish(&window).unwrap();
    assert_eq!(window.get_wall_image_width(), 64.0);
    assert!(
        !pane_typography(&window, id).paper_painted,
        "tiles leave the paper to the pane"
    );
    refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), source);
    let walled = draw();
    save("walled.ppm", &walled);
    let (before, after) = (at(&plain, empty.0, empty.1), at(&walled, empty.0, empty.1));
    assert_ne!(before, after, "the image shows through the paper");
    assert!(
        after.r.abs_diff(after.b) > 20,
        "a colour of the checker, not paper: {after:?}"
    );

    // 濃さ0：紙だけに戻る（画像は見えない）。
    window.set_wall_strength(0);
    let hidden = draw();
    let paper = at(&hidden, empty.0, empty.1);
    assert!(
        paper.r.abs_diff(before.r) <= 2 && paper.b.abs_diff(before.b) <= 2,
        "strength 0 is the plain paper: {paper:?} vs {before:?}"
    );

    // Settings → Page に出る。
    window.set_wall_strength(60);
    window.set_settings_tab(5);
    id.update_screen(&window, |screen| screen.settings = true);
    save("page-settings.ppm", &draw());
    let _ = (
        PointerEventButton::Left,
        WindowEvent::WindowActiveChanged(true),
    );
}

/// 追加要件 2026-09-15（書き手）: **表示の国際化。**同じ画面が、日本語を選べば日本語の
/// 文言で、英語なら英語で出る（訳は実行ファイルに埋め込まれている）。
#[test]
fn the_screen_speaks_japanese_and_english() {
    use slint::platform::{PointerEventButton, WindowEvent};
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    window.show().unwrap();
    id.update_screen(&window, |screen| {
        screen.width = 1050.0;
        screen.height = 640.0;
        screen.shown_width = 1050.0;
        screen.shown_height = 540.0;
        screen.settings = true;
    });
    window.set_settings_tab(0);
    let draw = |name: &str| {
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        let output = PathBuf::from("target/i18n-qa");
        std::fs::create_dir_all(&output).unwrap();
        let mut ppm = b"P6\n1100 760\n255\n".to_vec();
        for pixel in &pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join(name), ppm).unwrap();
        pixels
    };
    let click = |x: f32, y: f32| {
        let position = slint::LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        window
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
        slint::platform::update_timers_and_animations();
    };
    // **Rustの旗（`i18n::japanese`）は試験のあいだ共有なので触らない。**画面の訳だけを選ぶ。
    slint::select_bundled_translation("").unwrap();
    let english = draw("general-en.ppm");
    slint::select_bundled_translation("ja").unwrap();
    let japanese = draw("general-ja.ppm");
    assert_ne!(
        english, japanese,
        "the settings read differently in Japanese"
    );
    id.update_screen(&window, |screen| screen.settings = false);
    click(1077.0, 25.0);
    draw("pane-menu-ja.ppm");
    assert!(
        slint::select_bundled_translation("fr").is_err(),
        "only Japanese is bundled"
    );
    slint::select_bundled_translation("").unwrap();
}

/// 書き手の報告 2026-09-15: **設定のTABで検索欄を開いても検索が効かない。**「可能なら、検索した文字の含まれる
/// 設定のみがリストされると良い」。
///
/// 設定のTABで開いたCtrl+Fの欄の語で、全部の群から名前か見出しに語を含む行だけを出す。裏の代役の文書は探さず
/// （「見つかりません」を言わない）、Keysの一覧も同じ語で絞る。`EDITOR_SETTINGS_SNAPSHOT`があれば画像も書く。
#[test]
fn the_find_bar_searches_the_settings() {
    assert!(settings_match("font", ["Code Font".into()]));
    assert!(settings_match("Ｈ１", ["H1".into()]), "full width");
    assert!(settings_match(" 色 ", ["背景色".into()]), "trimmed");
    assert!(!settings_match(
        "margin",
        ["Line height".into(), "PAGE".into()]
    ));
    assert!(settings_match("", []), "nothing asked shows everything");

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
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 700.0;
    });
    window.set_autosave(false);
    let document = OpenDocument::untitled(1, window.as_weak());
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
    shortcuts::wire(&window, &live);
    window.on_settings_match(|query, labels| settings_match(&query, labels.iter()));
    open_settings(&window, &live);
    assert!(id.screen(&window).settings);
    window.show().unwrap();
    let shortcut_names = || {
        let rows = window.get_shortcut_rows();
        (0..rows.row_count())
            .filter_map(|at| rows.row_data(at))
            .filter(|row| !row.header)
            .map(|row| row.name.to_string())
            .collect::<Vec<_>>()
    };
    let all_shortcuts = shortcut_names().len();

    // Ctrl+Fが設定のTABまで届いて、欄が開く（書き手の報告 2026-09-15：効かなかった）。
    assert!(window.invoke_shortcut_key("f".into(), true, false, false));
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        window.get_find_open(),
        "Ctrl+F opens the bar on the settings"
    );
    assert_eq!(window.get_find_pane(), id.index());
    let ask = |needle: &str| {
        id.update_screen(&window, |screen| screen.find_needle = needle.into());
        slint::platform::update_timers_and_animations();
    };
    ask("Tab");
    assert_eq!(window.get_settings_query(), "Tab");
    count_in_pane(&window, &live);
    assert_eq!(
        window.get_count_find(),
        "",
        "the stand-in document is not searched"
    );
    let found = shortcut_names();
    assert!(
        !found.is_empty() && found.len() < all_shortcuts,
        "{found:?}"
    );
    assert!(
        found.iter().all(|name| name.to_lowercase().contains("tab")),
        "{found:?}"
    );

    if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
        let output = PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        for (name, needle) in [("font", "Font"), ("color", "Color"), ("margin", "margin")] {
            ask(needle);
            window.window().request_redraw();
            let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 1000);
            });
            let mut ppm = b"P6\n1000 740\n255\n".to_vec();
            for pixel in pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(output.join(format!("settings-search-{name}.ppm")), ppm).unwrap();
        }
    }

    // 欄を閉じれば、Keysの一覧も元に戻る。
    window.set_find_open(false);
    slint::platform::update_timers_and_animations();
    assert_eq!(window.get_settings_query(), "");
    assert_eq!(shortcut_names().len(), all_shortcuts);
    window.hide().unwrap();
}

/// 追加要件 2026-09-15（書き手）: **Left Pane の書式。**文字の大きさ・背景色・文字色と、フォルダ・ファイルの色。
///
/// フォルダ・ファイルの色は選ぶまで文字色に従い、「既定の色」で外れる。設定ファイルに残り、面のResetで戻る。
/// `EDITOR_SETTINGS_SNAPSHOT`があれば、色を付けたExplorerの画像も書く。
#[test]
fn the_left_pane_has_its_own_look() {
    let directory = std::env::temp_dir().join(format!(
        "editor-left-{}-{}",
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
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let document = OpenDocument::untitled(1, window.as_weak());
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
    wiring::wire_colours(
        &window,
        &live,
        &live.states,
        &live.cache,
        Rc::new(Timer::default()),
        numbers.clone(),
        palette.clone(),
    );
    // 窓の既定は、Rustの既定と同じ色。
    assert_eq!(window.get_left_size(), LEFT_SIZE_DEFAULT);
    assert_eq!(window.get_left_paper(), slint_colour(LEFT_PAPER_DEFAULT));
    assert_eq!(window.get_left_ink(), slint_colour(LEFT_INK_DEFAULT));

    let navy = Color::from_rgb_u8(0x1f, 0x2a, 0x44);
    let cream = Color::from_rgb_u8(0xf5, 0xef, 0xdc);
    let gold = Color::from_rgb_u8(0xe0, 0xb0, 0x40);
    window.invoke_colour_set(6, 0, navy);
    window.invoke_colour_set(6, 1, cream);
    assert!(
        !window.get_left_folder_own(),
        "folders follow the text color"
    );
    window.invoke_colour_set(6, 2, gold);
    assert!(window.get_left_folder_own() && window.get_left_folder_ink() == gold);
    window.invoke_left_size_stepped(3);
    assert_eq!(window.get_left_size(), 15);
    window.invoke_left_size_stepped(100);
    assert_eq!(window.get_left_size(), LEFT_SIZE_RANGE.1);
    window.invoke_left_size_stepped(-(LEFT_SIZE_RANGE.1 - 14));

    if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
        let output = PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        window.set_tree_open(true);
        window.set_left_tab(0);
        window.set_work_folder("D:\\原稿".into());
        let row = |name: &str, depth: i32, folder: bool, open: bool| LeftRow {
            name: name.into(),
            depth,
            folder,
            open,
            parent: -1,
            is_root: false,
        };
        window.set_left_rows(ModelRc::new(VecModel::from(vec![
            row("第一部", 0, true, true),
            row("第一章.md", 1, false, false),
            row("第二章.md", 1, false, false),
            row("資料", 0, true, false),
            row("あらすじ.md", 0, false, false),
        ])));
        // 画像のためだけに、ファイルにも色を付ける（読み戻しの前に外す）。
        window.invoke_colour_set(6, 3, Color::from_rgb_u8(0x9c, 0xc7, 0xe8));
        window.show().unwrap();
        slint::platform::update_timers_and_animations();
        window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        let mut ppm = b"P6\n1000 740\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join("left-pane.ppm"), ppm).unwrap();
        window.invoke_colour_default(6, 3);
        window.hide().unwrap();
    }

    // 設定ファイルへ出して、別の窓へ読み戻す。
    let values = settings_values(&window);
    let value = |name: &str| {
        values
            .iter()
            .find(|(key, _)| key == name)
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(value("left.size"), "14");
    assert_eq!(value("left.file"), "", "no file color chosen");
    let other = AppWindow::new().unwrap();
    other.set_sheet_stride(SHEET_NUMBERS as i32);
    other.set_sheet_numbers(ModelRc::from(numbers.clone()));
    other.set_palette(ModelRc::from(palette.clone()));
    other.set_sheet_fonts(ModelRc::from(fonts.clone()));
    apply_settings(&other, &numbers, &palette, &fonts, &values);
    assert_eq!(other.get_left_size(), 14);
    assert_eq!(other.get_left_paper(), navy);
    assert_eq!(other.get_left_ink(), cream);
    assert!(other.get_left_folder_own() && other.get_left_folder_ink() == gold);
    assert!(!other.get_left_file_own());

    // 「既定の色」でフォルダの色が外れ、面のResetで全部戻る。
    window.invoke_colour_default(6, 2);
    assert!(!window.get_left_folder_own());
    window.invoke_left_reset();
    assert_eq!(window.get_left_size(), LEFT_SIZE_DEFAULT);
    assert_eq!(window.get_left_paper(), slint_colour(LEFT_PAPER_DEFAULT));
    assert!(!window.get_left_file_own());
}
