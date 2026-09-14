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
    }

    // Nothing to reopen next time: the session names the document only.
    let session = session::capture_session(&window, &live);
    assert_eq!(session.panes[0].tabs.len(), 1);
    assert_eq!(&*document.text.borrow(), "本文");
    drop(live);
    std::fs::remove_dir_all(&directory).unwrap();
}
