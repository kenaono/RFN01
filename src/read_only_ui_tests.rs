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

fn log_lines(from: usize, to: usize) -> String {
    (from..to)
        .map(|number| format!("2026-09-15 12:00:{number:02} ログの行 {number}\n"))
        .collect()
}

/// 追加要件 2026-09-15（書き手）: **ソース表示のReadOnly。**
///
/// ソース表示で本のボタンを押すと、横書きに回って最下行を追う。書き足しは0.5秒の
/// 見回りで読み直し、上へスクロールすると止まり、最下行へ戻すと再開する。
/// 編集・上書き保存・縦書き／プレビューへの切り替えは断り、別名で保存すると解ける。
/// プレビューで押せば今までどおりViewerで、Viewerのあいだの外部変更も本文に届く。
#[test]
fn read_only_follows_a_growing_file_until_scrolled_away() {
    let directory = std::env::temp_dir().join(format!(
        "editor-readonly-{}-{}",
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
    let log_path = directory.join("app.log");
    std::fs::write(&log_path, log_lines(0, 80)).unwrap();
    let (file, text) = DocumentFile::open(&log_path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let document = OpenDocument::new(file, text, window.as_weak());
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
    id.update_screen(&window, |screen| {
        screen.width = 950.0;
        screen.height = 620.0;
        screen.shown_width = 950.0;
        screen.shown_height = 620.0;
        screen.preview = false;
    });
    set_pane_direction(&window, &live.cache, id, true);
    let (states, cache) = (&live.states, &live.cache);
    let source = document.text.borrow().clone();
    refresh_pane_from_state(&window, cache, &document, id, &states.of(id), &source);
    let end = || {
        let content = cache
            .borrow_mut()
            .pane(id)
            .graphics
            .engine
            .total_flow_size() as f32;
        -(content - id.viewport_flow(&window)).max(0.0)
    };
    let append = |from: usize, to: usize| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        file.write_all(log_lines(from, to).as_bytes()).unwrap();
    };

    // ソース表示の縦書きで押す → 横書きのReadOnlyで、最下行にいる。
    toggle_viewer(&window, states, cache, id);
    assert!(id.reads_only(&window));
    assert!(!id.vertical(&window), "ReadOnly is horizontal only");
    assert!(states.of(id).borrow().follow);
    assert!(end() < -100.0, "the log is longer than the pane");
    assert!(
        (id.scroll(&window) - end()).abs() < 1.0,
        "starts at the last line"
    );
    assert!(read_only_documents(&window, &live).len() == 1);

    // 編集は断る。
    insert_pane_text(&window, id, &document, states, cache, "追加", false);
    assert_eq!(&*document.text.borrow(), &source);

    // 書き足し → 0.5秒の見回りで読み直し、最下行まで付いて行く。
    let before = id.scroll(&window);
    append(80, 120);
    saving::check_read_only_change(&window, &live);
    assert!(document.text.borrow().ends_with("ログの行 119\n"));
    assert!(id.reads_only(&window), "a reload keeps ReadOnly");
    assert!(id.scroll(&window) < before - 100.0);
    assert!(
        (id.scroll(&window) - end()).abs() < 1.0,
        "follows to the new end"
    );

    // 上へスクロール → 止まる。書き足されても位置はそのまま、選択も残る。
    id.set_scroll(&window, -40.0);
    assert!(!follow_scroll(&window, states, cache, id, -40.0));
    assert!(!states.of(id).borrow().follow);
    {
        let state = states.of(id);
        let mut state = state.borrow_mut();
        state.selection_anchor_source_byte = Some(0);
        state.caret_source_byte = Some(10);
    }
    append(120, 140);
    saving::check_read_only_change(&window, &live);
    assert!(document.text.borrow().ends_with("ログの行 139\n"));
    assert!(
        (id.scroll(&window) + 40.0).abs() < 1.0,
        "stays where it was left"
    );
    assert_eq!(states.of(id).borrow().caret_source_byte, Some(10));
    assert_eq!(states.of(id).borrow().selection_anchor_source_byte, Some(0));

    // 面が縮んだだけでは止まらない（書き手はスクロールしていない）。
    let bottom = end();
    id.set_scroll(&window, bottom);
    assert!(follow_scroll(&window, states, cache, id, bottom));
    id.update_screen(&window, |screen| screen.shown_height = 500.0);
    assert!(follow_scroll(&window, states, cache, id, bottom));
    assert!(
        states.of(id).borrow().follow,
        "a smaller pane is not a scroll"
    );
    id.set_scroll(&window, -40.0);
    assert!(!follow_scroll(&window, states, cache, id, -40.0));

    // 最下行まで戻す → 再開。
    let bottom = end();
    id.set_scroll(&window, bottom);
    assert!(follow_scroll(&window, states, cache, id, bottom));
    append(140, 160);
    saving::check_read_only_change(&window, &live);
    assert!((id.scroll(&window) - end()).abs() < 1.0);

    // 上書き保存は断る（ファイルは書き足されたまま）。
    append(160, 161);
    saving::save_document(&window, &live, false);
    assert!(window.get_render_status().contains("ReadOnly"));
    assert!(
        std::fs::read_to_string(&log_path)
            .unwrap()
            .ends_with("ログの行 160\n")
    );

    // 別名で保存 → 断面になり、ReadOnlyは解ける。元のログはもう届かない。
    saving::check_external_change(&window, &live);
    let snapshot = directory.join("断面.log");
    let form = document.file.borrow().form();
    assert!(saving::write_document_in(
        &window,
        &live,
        &document,
        snapshot.clone(),
        form
    ));
    assert!(!id.reads_only(&window) && !states.of(id).borrow().viewer);
    assert!(window.get_render_status().contains("ReadOnlyモードを解除"));
    append(161, 170);
    saving::check_external_change(&window, &live);
    assert!(document.text.borrow().ends_with("ログの行 160\n"));

    // 書き手の判断 2026-09-15: **編集モードでは取り込まない。**知らせを1度出して印を
    // 立て、読み直すかReadOnlyで読むかは印の問いで選ぶ。
    use std::io::Write;
    let grow = |from: usize, to: usize| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&snapshot)
            .unwrap();
        file.write_all(log_lines(from, to).as_bytes()).unwrap();
    };
    id.set_scroll(&window, -40.0);
    grow(170, 200);
    saving::check_external_change(&window, &live);
    assert!(
        document.text.borrow().ends_with("ログの行 160\n"),
        "not taken in"
    );
    assert!(document.outside.get());
    assert!(window.get_render_status().contains("外で変更"));
    window.set_render_status("".into());
    grow(200, 210);
    saving::check_external_change(&window, &live);
    assert_eq!(window.get_render_status(), "", "told once");

    // 「外部の変更を読み込む」→ 書き足されただけなので位置はそのまま。
    ask_outside_change(&window, &live);
    assert!(matches!(
        live.pending.borrow().as_ref(),
        Some(Question::OutsideChanged(_))
    ));
    answer_question(&window, &live, 0);
    assert!(document.text.borrow().ends_with("ログの行 209\n"));
    assert!(!document.outside.get());
    assert!((id.scroll(&window) + 40.0).abs() < 1.0, "keeps its place");

    // もう一度変わる →「ReadOnlyモードで読む」→ 取り込んで、ReadOnlyで最下行。
    grow(210, 220);
    saving::check_external_change(&window, &live);
    assert!(document.outside.get());
    ask_outside_change(&window, &live);
    answer_question(&window, &live, 1);
    assert!(document.text.borrow().ends_with("ログの行 219\n"));
    assert!(id.reads_only(&window) && states.of(id).borrow().follow);
    assert!((id.scroll(&window) - end()).abs() < 1.0);
    toggle_viewer(&window, states, cache, id);
    assert!(!states.of(id).borrow().viewer);

    // プレビューで押せば今までどおりViewer。Viewerのあいだの外部変更も本文に届く。
    id.set_shows_preview(&window, true);
    toggle_viewer(&window, states, cache, id);
    assert!(states.of(id).borrow().viewer && !id.reads_only(&window));
    std::fs::write(&snapshot, "# 見出し\n\n書き換えた\n").unwrap();
    saving::check_external_change(&window, &live);
    assert_eq!(&*document.text.borrow(), "# 見出し\n\n書き換えた\n");
    assert!(states.of(id).borrow().viewer, "a reload keeps Viewer");
    toggle_viewer(&window, states, cache, id);
    assert!(!states.of(id).borrow().viewer);
}
