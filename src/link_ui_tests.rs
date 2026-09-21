use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

/// 2026-09-21（実画面の報告への対応）: 開いている Target.md の見出しを保存せずに
/// 変えたとき、リンク補完の見出し候補がその未保存の値になること。索引はディスク
/// 上の古い見出しのままにして、開いている本文の新しい見出しだけが候補に出る状況
/// を作る（RFN01-24 の確認手順4）。
#[test]
#[ignore = "offscreen link completion verification"]
fn an_unsaved_heading_reaches_the_completion_popup() {
    let directory = std::env::temp_dir().join(format!(
        "editor-link-heading-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root = directory.join("manuscript");
    std::fs::create_dir_all(root.join("notes")).unwrap();
    let target_path = root.join("notes").join("Target.md");
    std::fs::write(&target_path, "# Target heading\n").unwrap();
    let review_path = root.join("Review.md");
    std::fs::write(&review_path, "[[Target#\n").unwrap();
    let appdata = directory.join("appdata");
    std::fs::create_dir_all(&appdata).unwrap();
    app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(appdata.clone()));
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
    window.set_sheet_numbers(ModelRc::from(numbers));
    window.set_palette(ModelRc::from(palette));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
        screen.shown_width = 950.0;
        screen.shown_height = 620.0;
    });
    window.set_autosave(false);

    let (file, text) = DocumentFile::open(&review_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let review = OpenDocument::new(file, text, window.as_weak());
    let (target_file, target_text) =
        DocumentFile::open(&target_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let target = OpenDocument::new(target_file, target_text, window.as_weak());
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&review),
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
                let in_front = PaneTab::showing(&window, id, review.clone());
                // Open but not in front: the caret stays in Review.md while
                // Target.md is held with unsaved edits.
                let held = PaneTab::showing(&window, id, target.clone());
                PaneTabs {
                    history: vec![NavigationPlace::from(&in_front)],
                    tabs: vec![in_front, held],
                    ..Default::default()
                }
            }],
        })),
        writer: Rc::new(FileWriter::start()),
        searcher: Rc::new(Searcher::start(|| {})),
        searched: Rc::default(),
    };

    live.states.of(id).borrow_mut().caret_source_byte = Some("[[Target#".len());

    let runtime = Rc::new(RefCell::new(workspace_ui::Runtime::open(appdata)));
    let workspace = runtime
        .borrow_mut()
        .edit(|registry| {
            let id = registry.create_workspace("RFN24".into())?;
            registry.add_root(id, &root)?;
            Ok(id)
        })
        .unwrap();
    runtime.borrow_mut().set_active_silently(Some(workspace));
    live.folder.borrow_mut().workspace = Some(runtime);
    window.show().unwrap();

    let shown = |live: &Live| -> Vec<String> {
        workspace_link_ui(live)
            .borrow()
            .completion
            .candidates()
            .iter()
            .map(|item| item.display.clone())
            .collect()
    };
    // 1. The writer types `[[Target#` while Target.md is still as it was on
    //    disk: the file's own heading is what the file holds.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        workspace_links_tick(&window, &live);
        if !shown(&live).is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the link popup never opened");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(shown(&live), vec!["Target heading".to_owned()]);

    // 2. The writer switches to Target.md and changes its heading without
    //    saving.
    live.states.show(id, &target);
    live.tabs.borrow_mut().panes[0].active = 1;
    live.states.of(id).borrow_mut().caret_source_byte = Some(0);
    for _ in 0..8 {
        workspace_links_tick(&window, &live);
        std::thread::sleep(Duration::from_millis(10));
    }
    *target.text.borrow_mut() = "# Target heading TEST\n".to_owned();
    for _ in 0..40 {
        workspace_links_tick(&window, &live);
        std::thread::sleep(Duration::from_millis(10));
    }

    // 3. Back in Review.md, the same trigger has to offer the unsaved heading.
    live.states.show(id, &review);
    live.tabs.borrow_mut().panes[0].active = 0;
    live.states.of(id).borrow_mut().caret_source_byte = Some("[[Target#".len());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        workspace_links_tick(&window, &live);
        let offered = shown(&live);
        if offered.iter().any(|text| text == "Target heading TEST") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after the unsaved edit the popup offered {offered:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // 4. Re-picking a heading with the caret inside the written target
    //    replaces the whole target, rather than leaving its tail behind
    //    (書き手の報告 2026-09-21).
    *review.text.borrow_mut() = "[[Target#Target heading]]\n".to_owned();
    live.states.of(id).borrow_mut().caret_source_byte = Some("[[Target#Target head".len());
    for _ in 0..8 {
        workspace_links_tick(&window, &live);
        std::thread::sleep(Duration::from_millis(5));
    }
    accept_link_completion(&window, &live, id);
    assert_eq!(
        &*review.text.borrow(),
        "[[Target#Target%20heading%20TEST]]\n",
        "the written target is replaced whole, with one closer"
    );
    let _ = std::fs::remove_dir_all(directory);
}

// Run separately from other font rendering tests.

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
#[ignore = "offscreen link navigation verification"]
fn local_links_open_without_losing_the_source() {
    let directory = std::env::temp_dir().join(format!(
        "editor-links-{}-{}",
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
    window.set_sheet_numbers(ModelRc::from(numbers));
    window.set_palette(ModelRc::from(palette));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
    });
    window.set_autosave(true);
    let source = "[次へ](次の原稿.md)\n[不明](存在しない.md)\n[[未解決]]\n";
    let source_path = directory.join("本文.md");
    let target_path = directory.join("次の原稿.md");
    std::fs::write(&source_path, source).unwrap();
    std::fs::write(&target_path, "リンク先の本文").unwrap();
    let (file, text) = DocumentFile::open(&source_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let memo = OpenDocument::new(file, text, window.as_weak());
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&memo),
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
                let tab = PaneTab::showing(&window, id, memo.clone());
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
    for vertical in [false, true] {
        set_pane_direction(&window, &live.cache, id, vertical);
        refresh_pane_from_state(&window, &live.cache, &memo, id, &live.states.of(id), source);
        // Locate the actual glyph through the same rendered hit test as a mouse click.
        // 紙の上の点を、クリックと同じく組版の座標へ直して訊く（縦書きの原点は右端）。
        let shift = id.page_shift(&window);
        let mut point = None;
        'search: for y in (1..600).step_by(4) {
            for x in (1..900).step_by(4) {
                let x = x as f32 - shift;
                let hit = hit_test_pane(
                    &window,
                    &mut live.cache.borrow_mut(),
                    &memo,
                    id,
                    source,
                    None,
                    x,
                    y as f32,
                );
                if hit.is_some_and(|h| {
                    h.is_inside
                        && document::link_target_at(source, h.letter)
                            == Some(("次の原稿.md", false))
                }) {
                    point = Some((x, y as f32));
                    break 'search;
                }
            }
        }
        let (x, y) = point.expect("link glyph is rendered");
        assert!(open_link_at(&window, &live, id, x, y));
        assert_eq!(&*live.states.document(id).text.borrow(), "リンク先の本文");
        assert_eq!(&*memo.text.borrow(), source);
        assert_eq!(live.tabs.borrow().of(id).tabs.len(), 2);
        // Keep unsaved text when the same target is opened again under another path spelling.
        *live.states.document(id).text.borrow_mut() = "未保存の変更".into();
        switch_to_tab(&window, &live, id, 0);
        refresh_pane_from_state(&window, &live.cache, &memo, id, &live.states.of(id), source);
        assert!(open_link_at(&window, &live, id, x, y));
        assert_eq!(&*live.states.document(id).text.borrow(), "未保存の変更");
        assert_eq!(live.tabs.borrow().of(id).tabs.len(), 2);
        *live.states.document(id).text.borrow_mut() = "リンク先の本文".into();
        switch_to_tab(&window, &live, id, 0);
    }
    // Completion is one history step and cannot act on stale caret/text or IME.
    wire_workspace_links(&window, &live);
    let typing = "# 見出し\n[[#";
    *memo.text.borrow_mut() = typing.into();
    memo.history.borrow_mut().forget();
    {
        let state = live.states.of(id);
        state.borrow_mut().caret_source_byte = Some(typing.len());
        state.borrow_mut().selection_anchor_source_byte = Some(typing.len());
    }
    workspace_links_tick(&window, &live);
    assert!(window.get_link_popup_open());
    assert!(window.invoke_pane_link_popup_key(id.index(), 3));
    assert_eq!(&*memo.text.borrow(), "# 見出し\n[[#見出し]]");
    assert!(
        memo.history
            .borrow_mut()
            .undo_into(&mut memo.text.borrow_mut())
            .is_some()
    );
    assert_eq!(&*memo.text.borrow(), typing);

    live.states.of(id).borrow_mut().caret_source_byte = Some(typing.len());
    workspace_links_tick(&window, &live);
    assert!(window.get_link_popup_open());
    assert!(window.invoke_pane_link_popup_key(id.index(), 4));
    workspace_links_tick(&window, &live);
    assert!(!window.get_link_popup_open());
    assert_eq!(&*memo.text.borrow(), typing);

    *memo.text.borrow_mut() = format!("{typing}見");
    live.states.of(id).borrow_mut().caret_source_byte = Some(memo.text.borrow().len());
    workspace_links_tick(&window, &live);
    assert!(window.get_link_popup_open());
    *memo.text.borrow_mut() = "changed after popup".into();
    accept_link_completion(&window, &live, id);
    assert_eq!(&*memo.text.borrow(), "changed after popup");
    *memo.text.borrow_mut() = typing.into();
    live.states.of(id).borrow_mut().caret_source_byte = Some(typing.len());
    live.states.of(id).borrow_mut().preedit = "変換中".into();
    workspace_links_tick(&window, &live);
    assert!(!window.get_link_popup_open());
    window.hide().unwrap();
}

#[test]
fn completion_rejects_fenced_and_inline_code() {
    for source in ["```md\n[[", "~~~\n[[", "before `[[", "before ``[["] {
        assert!(link_trigger_in_code(source, source.len()), "{source}");
    }
    for source in ["[[", "`closed` [[", "```\ncode\n```\n[["] {
        assert!(!link_trigger_in_code(source, source.len()), "{source}");
    }
}
