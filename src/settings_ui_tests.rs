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
    assert_eq!(shown.title, SETTINGS_TAB_NAME);
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
        for group in 0..7 {
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
