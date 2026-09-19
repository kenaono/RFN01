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

/// A minimal, offscreen `AppWindow`/`Live` pair, the same shape the other
/// `*_ui_tests` files build — just enough to hold one document in one
/// pane so [`open_documents`] and [`write_document_in`] work normally.
struct Harness {
    window: AppWindow,
    live: Live,
}

impl Harness {
    /// Builds the offscreen window and platform first — `make_document`
    /// is only handed the window's `Weak` afterwards, because
    /// `AppWindow::new` needs a platform already set, and the document a
    /// test wants has to be built with this window's own handle.
    fn new(
        make_document: impl FnOnce(slint::Weak<AppWindow>) -> Rc<OpenDocument>,
    ) -> (Self, Rc<OpenDocument>) {
        let directory = std::env::temp_dir().join(format!(
            "editor-terminal-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(directory.clone()));
        let surface = MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
        let window = AppWindow::new().unwrap();
        let numbers = Rc::new(VecModel::from(vec![0; 2 * crate::SHEET_NUMBERS]));
        let palette = Rc::new(VecModel::from(vec![
            slint::Color::default();
            2 * crate::SHEET_COLOURS
        ]));
        let fonts = Rc::new(VecModel::from(vec![
            SharedString::default();
            2 * crate::SHEET_FONTS
        ]));
        crate::reset_settings(&numbers, &palette, &fonts);
        window.set_sheet_stride(crate::SHEET_NUMBERS as i32);
        window.set_sheet_numbers(ModelRc::from(numbers));
        window.set_palette(ModelRc::from(palette));
        window.set_sheet_fonts(ModelRc::from(fonts));
        surface.set_size(slint::PhysicalSize::new(1000, 740));
        crate::publish_panes(&window, 1);
        let id = PaneId::from_index(0);
        id.update_screen(&window, |screen| {
            screen.width = 950.0;
            screen.height = 620.0;
        });
        let document = make_document(window.as_weak());
        let tab = crate::PaneTab::showing(&window, id, document.clone());
        let live = Live {
            preview: Rc::default(),
            closed_tabs: Rc::default(),
            states: crate::PaneStates::new(&document),
            folder: Rc::default(),
            tree_paths: Rc::default(),
            workspace_ids: Rc::default(),
            results: Rc::default(),
            recent: Rc::default(),
            recent_folders: Rc::default(),
            find_terms: Rc::new(RefCell::new(crate::find::Terms::restored(Vec::new()))),
            replace_terms: Rc::new(RefCell::new(crate::find::Terms::restored(Vec::new()))),
            layout: Rc::new(RefCell::new(crate::pane_layout::Layout::single(0))),
            pending: Rc::default(),
            close_run: Rc::default(),
            cache: Rc::new(RefCell::new(crate::RenderCache::default())),
            tabs: Rc::new(RefCell::new(crate::Tabs {
                panes: vec![crate::PaneTabs {
                    history: vec![crate::NavigationPlace::from(&tab)],
                    tabs: vec![tab],
                    ..Default::default()
                }],
            })),
            writer: Rc::new(writer::FileWriter::start()),
            searcher: Rc::new(crate::searcher::Searcher::start(|| {})),
            searched: Rc::default(),
        };
        let weak = window.as_weak();
        let notified = live.clone();
        window.on_republish_tabs(move || {
            if let Some(window) = weak.upgrade() {
                publish_tabs(&window, &notified);
            }
        });
        (Self { window, live }, document)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.live.writer.finish();
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
    }
}

#[test]
fn terminal_panel_restored_draft_can_be_saved_and_edited_with_live_notifications() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    h.live.tabs.borrow_mut().of_mut(id).tabs[0].below.draft = "restored draft".into();
    terminal_panels::ensure(&h.window, &h.live, id);
    let panel = terminal_panels::current(&h.live, id).unwrap();
    let document = panel.borrow().document.clone();
    assert_eq!(&*document.text.borrow(), "restored draft");
    let path = app_data::app_directory().unwrap().join("saved-panel.md");
    assert!(saving::write_document_to(&h.window, &h.live, &document, path.clone()));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "restored draft");
    assert!(!document.text.edited());
    // Capture/edit updates hold the panel while changing its text, as in the UI.
    panel.borrow_mut().document.text.borrow_mut().push_str(" edited");
    terminal_panels::edited(&h.window, &h.live, id);
    assert!(document.text.edited());
    assert!(id.screen(&h.window).panel_tabs.row_data(0).unwrap().ends_with('*'));
    assert!(terminal_panels::save(&h.window, &h.live, &panel, false));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "restored draft edited");
}

#[test]
#[ignore = "starts a real ConPTY shell; run explicitly"]
fn terminal_panels_keep_capture_target_and_cancel_without_stopping() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    let session = Rc::new(RefCell::new(
        TerminalSession::start("QA", "cmd.exe /Q /D /K", 80, 25, || {}).unwrap(),
    ));
    session.borrow_mut().wait(Duration::from_millis(100));
    terminal_workflow::input(
        &h.window,
        &h.live,
        session.clone(),
        "echo RFN_NEVER_SEND\r",
        None,
    );
    assert!(h.live.pending.borrow().is_some());
    answer_question(&h.window, &h.live, 1);
    session.borrow_mut().wait(Duration::from_millis(100));
    assert!(
        !session
            .borrow()
            .screen()
            .retained_text()
            .contains("RFN_NEVER_SEND")
    );
    {
        let mut tabs = h.live.tabs.borrow_mut();
        tabs.of_mut(id).tabs[0].terminal = Some(session.clone());
    }
    let upper = h.live.tabs.borrow().of(id).tabs[0].clone();
    h.live.show_tab(&h.window, id, &upper);
    terminal_panels::ensure(&h.window, &h.live, id);
    show_draft(&h.window, id, "first draft");
    terminal_panels::edited(&h.window, &h.live, id);
    terminal_panels::action(&h.window, &h.live, id, 1, 0);
    show_draft(&h.window, id, "second draft");
    terminal_panels::edited(&h.window, &h.live, id);
    terminal_panels::action(&h.window, &h.live, id, 0, 0);
    assert_eq!(id.screen(&h.window).below_draft.as_str(), "first draft");
    terminal_panels::action(&h.window, &h.live, id, 7, 0);
    let target = terminal_panels::current(&h.live, id).unwrap();
    assert!(target.borrow().view.viewer);
    terminal_panels::action(&h.window, &h.live, id, 2, 0);
    assert!(h.live.pending.borrow().is_some());
    answer_question(&h.window, &h.live, 2);
    assert!(target.borrow().capture.is_some());
    toggle_below(&h.window, &h.live, id);
    new_file_tab(&h.window, &h.live, id);
    session.borrow_mut().type_text("echo RFN_CAPTURE_TEST\r");
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        session.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
        if target
            .borrow()
            .document
            .text
            .borrow()
            .contains("RFN_CAPTURE_TEST")
        {
            break;
        }
    }
    assert!(
        target
            .borrow()
            .document
            .text
            .borrow()
            .contains("RFN_CAPTURE_TEST")
    );
    assert!(h.live.active(&h.window).text.borrow().is_empty());
    target.borrow_mut().stop();
    assert!(!target.borrow().view.viewer);
    assert!(target.borrow().capture.is_none());
}

#[test]
fn terminal_appearance_roundtrip_and_pane_reset_preserve_formatting() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    terminal_panels::ensure(&h.window, &h.live, id);
    let mut style = terminal_appearance::default_style(&h.window, 1);
    style.transparency = 65;
    style.bold = true;
    style.italic = true;
    style.paper = Color::from_rgb_u8(20, 40, 60);
    let encoded = terminal_appearance::encode(&style);
    assert_eq!(
        terminal_appearance::encode(&terminal_appearance::decode(&encoded).unwrap()),
        encoded
    );
    let entry = terminal_panels::current(&h.live, id).unwrap();
    entry.borrow_mut().style = Some(style.clone());
    {
        let mut tabs = h.live.tabs.borrow_mut();
        let tab = &mut tabs.of_mut(id).tabs[0];
        tab.below.front_style = Some(style);
        terminal_appearance::clear_paper(tab);
        assert!(!tab.below.front_style.as_ref().unwrap().paper_own);
        assert_eq!(tab.below.front_style.as_ref().unwrap().transparency, 65);
    }
    let style = entry.borrow().style.clone().unwrap();
    assert!(!style.paper_own);
    assert!(style.bold && style.italic);
    assert_eq!(style.transparency, 65);
}

#[test]
#[ignore = "starts real Windows/WSL shells; run explicitly"]
fn terminal_start_folder_handles_spaces_and_japanese() {
    let directory = std::env::temp_dir()
        .join(format!("rfn-terminal-cwd-{}", std::process::id()))
        .join("原稿 作業");
    std::fs::create_dir_all(&directory).unwrap();
    for command in [
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"[Console]::Write((Get-Location).Path)\"",
        "wsl.exe --exec pwd",
    ] {
        let mut session =
            TerminalSession::start_in("QA", command, Some(&directory), 160, 12, || {}).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until && !session.screen().retained_text().contains("原稿 作業")
        {
            session.wait(Duration::from_millis(50));
        }
        assert!(
            session.screen().retained_text().contains("原稿 作業"),
            "working directory was not reported by {command}"
        );
    }
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while let Err(error) = std::fs::remove_dir(&directory) {
        assert!(Instant::now() < cleanup_deadline, "cleanup: {error}");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_dir(directory.parent().unwrap()).unwrap();
}

#[test]
fn terminal_transparency_keeps_glyphs_opaque() {
    let mut terminal = terminal::Terminal::new(20, 3);
    terminal.feed(b"Terminal text");
    let look = cells::TerminalLook {
        transparent: true,
        ..Default::default()
    };
    let cell = cells::terminal_cell_size(&look).unwrap();
    let width = (20.0 * cell.advance).ceil() as u32;
    let height = (3.0 * cell.line).ceil() as u32;
    let mut pixels = vec![0; (width * height * 4) as usize];
    cells::draw_terminal(
        terminal.screen.lines(),
        &[],
        None,
        "",
        &look,
        cell,
        &mut pixels,
        width,
        height,
    )
    .unwrap();
    assert_eq!(pixels[pixels.len() - 1], 0, "blank paper is transparent");
    assert!(
        pixels.chunks_exact(4).filter(|p| p[3] == 255).count() > 20,
        "text remains solid"
    );
}
