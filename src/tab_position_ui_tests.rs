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

/// 書き手の報告 2026-09-15: **縦書きで、閉じたTABを開き直すと表示が末尾になる。**
///
/// まだ誰も見ていない表示の位置を`scroll`の0で持っていた——縦書きの0は左端＝末尾で、
/// 同じ面のまま別の文書へ替われば、前の文書との長さの差だけ右端から離れた途中に出た。
/// 開いたばかりのTABは、向きを問わず文書の先頭に立つ。
#[test]
fn a_new_vertical_tab_opens_at_the_start() {
    let directory = std::env::temp_dir().join(format!(
        "editor-tab-position-{}-{}",
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
    window.set_autosave(false);
    let untitled = |number: u32, text: String| {
        let document = OpenDocument::untitled(number, window.as_weak());
        *document.text.borrow_mut() = text;
        document
    };
    let short = untitled(1, "春はあけぼの。\n".repeat(60));
    let long = untitled(2, "夏は夜。月のころはさらなり。\n".repeat(200));
    let live = Live {
        preview: Rc::default(),
        closed_tabs: Rc::default(),
        states: PaneStates::new(&short),
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
            panes: vec![PaneTabs::default()],
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
        screen.preview = false;
    });
    set_pane_direction(&window, &live.cache, id, true);
    let start = || {
        let content = live
            .cache
            .borrow_mut()
            .pane(id)
            .graphics
            .engine
            .total_flow_size() as f32;
        (
            content,
            id.start_scroll(&window, id.viewport_flow(&window), content),
        )
    };
    let at_start = |what: &str| {
        let (content, start) = start();
        assert!(
            content - id.viewport_flow(&window) > 100.0,
            "{what}: the document is wider than the pane"
        );
        assert!(
            (id.scroll(&window) - start).abs() < 1.0,
            "{what}: scroll {} should be the start {start}",
            id.scroll(&window),
        );
    };

    add_tab(
        &window,
        &live,
        id,
        PaneTab::showing(&window, id, short.clone()),
    );
    assert!(id.vertical(&window));
    at_start("the first tab");
    // 同じ面のまま、長い文書へ（前の文書との長さの差で途中に出ていた）。
    add_tab(
        &window,
        &live,
        id,
        PaneTab::showing(&window, id, long.clone()),
    );
    at_start("a longer document after a shorter one");
    // 閉じて、開き直す。
    assert!(close_tab(&window, &live, id, 1));
    add_tab(&window, &live, id, PaneTab::showing(&window, id, long));
    at_start("reopened");
}

/// 書き手の報告 2026-09-15: **Pane1で保存した「Saved」が、Active PaneをPane2へ切り替えても残る。**
///
/// 知らせは窓に1つで、どのPaneの話かを持っていなかった。**TAB・Pane・窓全体に分け**（書き手の判断）、
/// 頭に誰の話かを付け、TAB・Paneの話は別のPaneで操作が始まれば畳む。同じPaneで描き直すだけなら残し、
/// 窓全体の話はPaneを移っても残す。
#[test]
fn a_message_goes_when_another_pane_takes_over() {
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
    publish_panes(&window, 2);
    let document = OpenDocument::new(DocumentFile::untitled(1), "本文\n".into(), window.as_weak());
    let states = PaneStates::new(&document);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    cache.borrow_mut().add_pane(WritingMode::Horizontal);
    let (first, second) = (PaneId::from_index(0), PaneId::from_index(1));
    for id in [first, second] {
        id.update_screen(&window, |screen| {
            screen.width = 480.0;
            screen.height = 600.0;
            screen.shown_width = 480.0;
            screen.shown_height = 600.0;
        });
    }
    let refresh = |id: PaneId| {
        window.set_focused_pane(id.index());
        refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), "本文\n");
    };

    refresh(first);
    window.tell_tab("保存しました".into());
    refresh(first);
    assert_eq!(
        window.get_render_status(),
        "保存しました",
        "the same pane keeps it"
    );
    refresh(second);
    assert_eq!(window.get_render_status(), "", "another pane takes over");
    window.tell_pane("分割: ペインが多すぎます".into());
    refresh(second);
    // **区切りの字は画面の言葉で変わる**（日本語は全角のコロン）。訳は工程に1つで、
    // ほかの試験が切り替えることもある（`settings_ui_tests`の言語の試験）——どちらの
    // 綴りも受ける。ここで見たいのは、**誰の話かが頭に付くこと**である。
    let shown = window.get_render_status_shown();
    assert!(
        shown == "Pane 2: 分割: ペインが多すぎます" || shown == "Pane 2：分割: ペインが多すぎます",
        "a pane's message names the pane: {shown}"
    );
    refresh(first);
    assert_eq!(window.get_render_status(), "");
    window.tell("すべての設定を既定に戻しました".into());
    refresh(second);
    assert_eq!(
        window.get_render_status_shown(),
        "すべての設定を既定に戻しました",
        "the window's message stays, with no name"
    );
}

/// 書き手の報告 2026-09-15: **余白をクリックしてもActive Paneが動かない。**
///
/// 本文の押下は入力欄が鍵盤を取ってPaneを替えるが、紙の周りと紙の外には受け手が無かった。
#[test]
fn a_click_on_the_margin_makes_the_pane_active() {
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
    publish_panes(&window, 2);
    for (id, x) in [(PaneId::from_index(0), 0.0), (PaneId::from_index(1), 530.0)] {
        id.update_screen(&window, |screen| {
            screen.x = x;
            screen.width = 500.0;
            screen.height = 640.0;
        });
    }
    window.set_focused_pane(0);
    // 本文の押下が届いたら数える——余白の押下は本文に届かないことも見る。
    let selected = Rc::new(std::cell::Cell::new(0));
    let seen = selected.clone();
    window.on_pane_selection_start(move |_, _, _, _, _| seen.set(seen.get() + 1));
    window.show().unwrap();
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
    // 紙の左端は窓の x=60（本文の押下が返す位置から逆算）。そのすぐ左は紙の周りの余白である。
    click(588.0, 500.0);
    assert_eq!(window.get_focused_pane(), 1, "the right pane's margin");
    click(58.0, 500.0);
    assert_eq!(window.get_focused_pane(), 0, "the left pane's margin");
    assert_eq!(selected.get(), 0, "the margin is not the text");
}

/// 書き手の報告 2026-09-15: **横書きでTABを切り替えると、一瞬描画がカクっとする**（幅を固定すると
/// 起きない、縦書きでは起きない）。
///
/// 左右の余白は横書きの面にだけ付く。縦書きのTABから替わった直後の組版は、まだ縦書きのときの
/// 見えている幅（余白なし）で折り返し、画面に余白が付いて幅が24px縮んだ知らせで、200ms後に
/// 全段落を折り返し直していた（記録：`extent=1183`の直後に`extent=1159`、どちらも全段落を測る）。
/// **向きを替えるときに、新しい向きの余白で見えている大きさを置き直す。**置いた値が、画面が
/// あとで知らせてくる値と同じなら、組み直しは1回で済む。
#[test]
fn switching_direction_expects_the_new_margins() {
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
    // 縦書きと横書きで上下の余白も違う場合。
    Setting::PageMargin.write(&numbers, 0, 20);
    Setting::PageMargin.write(&numbers, 1, 32);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(fonts));
    surface.set_size(slint::PhysicalSize::new(1000, 740));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 640.0;
    });
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let draw = || {
        surface.draw_if_needed(|renderer| {
            let mut buffer =
                vec![slint::platform::software_renderer::Rgb565Pixel::default(); 1000 * 740];
            renderer.render(&mut buffer, 1000);
        });
        slint::platform::update_timers_and_animations();
        let screen = id.screen(&window);
        (screen.shown_width, screen.shown_height, screen.wrap_height)
    };
    let reported = || {
        let screen = id.screen(&window);
        (screen.shown_width, screen.shown_height, screen.wrap_height)
    };
    for vertical in [true, false, true] {
        draw();
        set_pane_direction(&window, &cache, id, vertical);
        let expected = reported();
        let shown = draw();
        assert_eq!(
            expected, shown,
            "vertical={vertical}: expected before the pane says"
        );
    }
}
