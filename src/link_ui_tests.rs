use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;

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
