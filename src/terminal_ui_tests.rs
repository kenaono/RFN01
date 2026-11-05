use super::*;

#[test]
fn read_only_policy_is_shared_by_editor_and_panel() {
    let mut state = EditorState::default();
    state.preedit = "composition".into();
    state.set_read_only(true, true);
    assert!(state.viewer && state.follow && state.preedit.is_empty());
    assert!(!state.follow_at(10.0, 100.0, false));
    assert!(!state.follow_at(10.0, 120.0, true));
    assert!(state.follow_at(118.0, 120.0, false));
    assert!(state.follow_at(118.0, 160.0, true));
    state.set_read_only(false, true);
    assert!(!state.viewer && !state.follow);
    state.set_read_only(true, false);
    assert!(state.viewer && !state.follow);
}

#[test]
fn editor_panel_uses_source_engine_document_and_selection() {
    let (h, original) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let owner = PaneId::FIRST;
    owner.update_screen(&h.window, |screen| {
        screen.terminal = true;
        screen.below_kind = 2;
        screen.below_height = 240.;
        screen.panel_style = terminal_appearance::initial_style();
    });
    terminal_panels::ensure(&h.window, &h.live, owner);
    let entry = terminal_panels::current(&h.live, owner).unwrap();
    let document = entry.borrow().document.clone();
    *document.text.borrow_mut() = "# source\n日本語".into();
    panel_source::sync(&h.window, &h.live);
    let id = entry.borrow().source_id.unwrap();
    assert!(id.screen(&h.window).embedded_panel);
    assert!(!id.screen(&h.window).preview && !id.vertical(&h.window));
    assert!(Rc::ptr_eq(&h.live.states.document(id), &document));
    let state = h.live.states.of(id);
    assert!(Rc::ptr_eq(&state, &entry.borrow().view));
    state.borrow_mut().caret_source_byte = Some(document.text.borrow().len());
    insert_pane_text(
        &h.window,
        id,
        &document,
        &h.live.states,
        &h.live.cache,
        "追記",
        false,
    );
    assert_eq!(&*document.text.borrow(), "# source\n日本語追記");
    assert!(original.text.borrow().is_empty());
    undo_in_pane(
        &h.window,
        id,
        &document,
        &h.live.states,
        &h.live.cache,
        false,
    );
    assert_eq!(&*document.text.borrow(), "# source\n日本語");
    undo_in_pane(
        &h.window,
        id,
        &document,
        &h.live.states,
        &h.live.cache,
        true,
    );
    assert_eq!(&*document.text.borrow(), "# source\n日本語追記");
    state.borrow_mut().selection_anchor_source_byte = Some(0);
    assert!(release_selection(&mut state.borrow_mut()));
    assert!(selection_source_range(&state.borrow()).is_none());
    panel_source::sync(&h.window, &h.live);
    assert_eq!(entry.borrow().source_id, Some(id));
    assert!(Rc::ptr_eq(&h.live.states.of(id), &state));
    entry
        .borrow_mut()
        .view
        .borrow_mut()
        .set_read_only(true, true);
    panel_source::sync(&h.window, &h.live);
    insert_pane_text(
        &h.window,
        id,
        &document,
        &h.live.states,
        &h.live.cache,
        "blocked",
        false,
    );
    assert_eq!(&*document.text.borrow(), "# source\n日本語追記");
    panel_source::focus(&h.window, &h.live, owner, true);
    assert_eq!(focused_pane(&h.window), id);
    panel_source::focus(&h.window, &h.live, owner, false);
    assert_eq!(focused_pane(&h.window), owner);
    h.live.tabs.borrow_mut().of_mut(owner).unwrap().tabs[0]
        .below
        .entries
        .clear();
    panel_source::sync(&h.window, &h.live);
    assert!(h.live.states.panels.borrow().is_empty());
    assert_eq!(h.live.tabs.borrow().of(owner).tabs.len(), 1);
    assert!(h.live.cache.borrow().panel_panes.is_empty());
}

#[test]
fn embedded_editor_host_releases_all_resources_without_reusing_ids() {
    let (h, original) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let host = editor_host::EditorHost::new(&h.window, &h.live);
    let document = OpenDocument::with_events(
        DocumentFile::untitled(2),
        "独立した面".into(),
        Default::default(),
    );
    let session = editor_session::EditorSession::new(document.clone());
    let held_state = Rc::downgrade(&session.state);
    let id = host.attach(PaneId::FIRST, session).unwrap();
    assert!(h.live.tabs.borrow().of(id).tabs.is_empty());
    assert!(h.live.tabs.borrow_mut().of_mut(id).is_none());
    assert_eq!(h.live.tabs.borrow().of(PaneId::FIRST).tabs.len(), 1);
    assert!(Rc::ptr_eq(&h.live.states.document(id), &document));
    assert!(Rc::ptr_eq(
        &h.live.states.document(PaneId::FIRST),
        &original
    ));
    id.update_screen(&h.window, |screen| screen.width = 200.);
    h.live.cache.borrow_mut().pane(id);
    h.live.cache.borrow_mut().pace_of(id);
    host.hide_except(&[]);
    assert_eq!(id.screen(&h.window).width, 0.);
    host.retain(&[]);
    assert!(held_state.upgrade().is_none());
    assert!(!h.live.states.panels.borrow().contains_key(&id.0));
    assert!(h.live.tabs.borrow().of(id).tabs.is_empty());
    assert!(h.live.tabs.borrow_mut().of_mut(id).is_none());
    assert!(!h.live.cache.borrow().panel_panes.contains_key(&id.0));
    assert!(!h.live.cache.borrow().panel_pace.contains_key(&id.0));
    assert!(
        !h.window
            .get_panes()
            .iter()
            .any(|screen| screen.id == id.index())
    );
    let next = host
        .attach(PaneId::FIRST, editor_session::EditorSession::new(document))
        .unwrap();
    assert_ne!(id, next);
    host.retain(&[next]);
    assert!(h.live.states.panels.borrow().contains_key(&next.0));
}

#[test]
#[ignore = "starts real ConPTY shells; run explicitly"]
fn terminal_new_tab_random_and_direct_file_capture() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    let mut defaults = vec![terminal_appearance::initial_style(); 3];
    for style in &mut defaults {
        style.random = 1;
    }
    h.window
        .set_panel_defaults(ModelRc::new(VecModel::from(defaults)));
    new_terminal_tab(
        &h.window,
        &h.live,
        id,
        TerminalShell {
            name: "QA".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        },
    );
    let tab = h.live.tabs.borrow().of(id).current().unwrap().clone();
    let source = tab.terminal.as_ref().unwrap().clone();
    let colour = tab.below.front_style.as_ref().unwrap().paper;
    publish_tabs(&h.window, &h.live);
    let screen = id.screen(&h.window);
    let info = screen.tabs.row_data(screen.active_tab as usize).unwrap();
    assert!(info.colour_own);
    assert_eq!(info.colour, colour);
    assert_eq!(info.dark, luminance(channels(colour)) < 0.45);
    assert_ne!(colour, terminal_appearance::initial_style().paper);
    assert_eq!(id.screen(&h.window).front_style.paper, colour);
    assert_eq!(
        terminal_appearance::look(&h.window, id, TerminalSpot::Front).paper,
        channels(colour)
    );
    assert!(tab_title(&tab).starts_with("QA:"));
    assert!(tab.below.entries.is_empty());
    let path =
        app_data::TEST_DIRECTORY.with(|p| p.borrow().as_ref().unwrap().join("direct-log.txt"));
    source
        .borrow_mut()
        .start_file_log(path.clone(), std::fs::File::create(&path).unwrap(), None);
    terminal_panels::publish_source(&h.window, &h.live, id);
    assert!(id.screen(&h.window).terminal_file_logging);
    assert!(!id.screen(&h.window).terminal_capturing);
    assert!(
        h.live
            .tabs
            .borrow()
            .of(id)
            .current()
            .unwrap()
            .below
            .entries
            .is_empty()
    );
    assert!(!h.live.cache.borrow_mut().pane(id).below_open);
    source.borrow_mut().type_text("echo DIRECT_CAPTURE\r");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        source.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
        if std::fs::read_to_string(&path)
            .unwrap()
            .contains("DIRECT_CAPTURE")
        {
            break;
        }
    }
    terminal_panels::action(&h.window, &h.live, id, 14, 0);
    assert!(!id.screen(&h.window).terminal_file_logging);
    assert!(source.borrow().file_log_path().is_none());
    assert!(
        h.live
            .tabs
            .borrow()
            .of(id)
            .current()
            .unwrap()
            .below
            .entries
            .is_empty()
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("DIRECT_CAPTURE")
    );
}
/// RFN01-95: a bell while the keyboard is elsewhere puts the bell on the tab;
/// the keyboard coming to the shell takes it away.
#[test]
#[ignore = "starts real ConPTY shells; run explicitly"]
fn a_bell_marks_the_tab_until_the_keyboard_comes() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    terminal_workflow::install(&h.window, &h.live);
    let id = PaneId::FIRST;
    new_terminal_tab(
        &h.window,
        &h.live,
        id,
        TerminalShell {
            name: "QA".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        },
    );
    let source = h
        .live
        .tabs
        .borrow()
        .of(id)
        .current()
        .unwrap()
        .terminal
        .clone()
        .unwrap();
    let shell_tab = h.live.tabs.borrow().of(id).active;
    let marked = || {
        publish_tabs(&h.window, &h.live);
        let screen = id.screen(&h.window);
        screen.tabs.row_data(shell_tab).unwrap().attention
    };
    assert!(!marked());
    // The writer goes back to the document; the shell rings behind it.
    switch_to_tab(&h.window, &h.live, id, 0);
    source.borrow_mut().type_text("echo \x07\r");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until && !source.borrow().wants_attention() {
        source.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
    }
    assert!(marked(), "the bell is on the tab");
    // **Opening the tab is the keyboard coming**: the same field keeps the
    // focus, so no focus signal says so (measured in the app, 2026-11-04).
    switch_to_tab(&h.window, &h.live, id, shell_tab);
    assert!(!marked(), "the keyboard came, and the bell went");
    source.borrow_mut().type_text("echo \x07\r");
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        source.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
    }
    assert!(!marked(), "a bell where the writer is typing is not a call");
}

/// RFN01-98: in a shell, the palette's keys are the window's; every other
/// chord still goes to the shell.
#[test]
fn the_palette_keys_are_the_windows_in_a_shell() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    crate::shortcuts::wire(&h.window, &h.live);
    let key = |text: &str, shift| {
        h.window
            .invoke_terminal_shortcut_key(text.into(), true, false, shift)
    };
    assert!(key("p", false), "Ctrl+P: files");
    assert!(key("P", true), "Ctrl+Shift+P: commands");
    // RFN01-102: New File, and the way out to the next and previous tab.
    assert!(key("n", false), "Ctrl+N: a new document");
    let tab: SharedString = slint::platform::Key::Tab.into();
    assert!(
        h.window
            .invoke_terminal_shortcut_key(tab.clone(), true, false, false),
        "Ctrl+Tab"
    );
    assert!(
        h.window
            .invoke_terminal_shortcut_key(tab, true, false, true),
        "Ctrl+Shift+Tab"
    );
    assert!(!key("w", false), "Ctrl+W stays bash's (delete a word)");
    assert!(!key("c", false), "Ctrl+C stays the shell's");
    assert!(!key("s", false), "so does Ctrl+S");
    assert!(!key("r", false), "and bash's Ctrl+R");
}

/// RFN01-97: the shells along a document's foot come back as that strip's
/// Terminal Panel tabs, in order, with the one in front in front — and are
/// written down again the same way. **No shell is started**: they wait.
#[test]
fn the_shells_along_a_foot_come_back_waiting() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let shell = |name: &str| app_data::SessionShell {
        name: name.to_owned(),
        command: "cmd.exe /Q /D /K".to_owned(),
        directory: String::new(),
        output: None,
        // RFN01-99: the name the program had given it last run.
        title: (name == "B").then(|| "ken@KN01: /mnt/d/原稿/第一部".to_owned()),
    };
    let stored = app_data::Session {
        layout: "P 0".to_owned(),
        panes: vec![app_data::SessionPane {
            tabs: vec![app_data::SessionTab {
                // A file: an untitled tab with no work copy is not put back.
                origin: Some(
                    std::env::current_dir()
                        .unwrap()
                        .join("testdata")
                        .join("00_この一式について.md"),
                ),
                below: true,
                below_shells: vec![shell("A"), shell("B")],
                below_active: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let (tabs, _) = session::open_session(&h.window, Some(stored), Vec::new());
    let tab = &tabs.panes[0].tabs[0];
    assert!(tab.below.open);
    assert_eq!(tab.below.entries.len(), 2);
    assert_eq!(tab.below.active, 1);
    let front = tab.below.shell.clone().expect("the one in front");
    assert_eq!(front.borrow().name(), "B");
    assert!(front.borrow().is_waiting(), "nothing has started");
    // RFN01-99: named as the program named it, cut to the folder, with the
    // whole of it for the tooltip; and the icon of the shell it was.
    assert_eq!(
        front.borrow().tab_name(),
        (
            "第一部".to_owned(),
            "ken@KN01: /mnt/d/原稿/第一部".to_owned()
        )
    );
    assert_eq!(front.borrow().shell_icon(), 3);
    let first = tab.below.entries[0].borrow().shell.clone().unwrap();
    assert!(
        first.borrow().tab_name().0.starts_with("A:"),
        "no title, the shell's name"
    );
    let numbers: Vec<u32> = std::iter::once(tab.document.file.borrow().untitled_number())
        .chain(
            tab.below
                .entries
                .iter()
                .map(|e| e.borrow().document.file.borrow().untitled_number()),
        )
        .collect();
    assert_eq!(numbers, vec![0, 1, 2], "no untitled number is taken twice");
    let again = session::session_tab(tab, true);
    assert_eq!(
        again
            .below_shells
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_eq!(again.below_active, 1);
    assert!(again.below_shells.iter().all(|s| s.output.is_some()));
    assert!(
        session::session_tab(tab, false)
            .below_shells
            .iter()
            .all(|s| s.output.is_none()),
        "off, no output is named"
    );
}

/// RFN01-97: a shell tab is written into the session with what it printed;
/// read back, it waits until it is drawn, then starts below that output and
/// a line saying where it ends.
#[test]
#[ignore = "starts real ConPTY shells; run explicitly"]
fn a_shell_comes_back_after_a_restart_with_its_output() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    new_terminal_tab(
        &h.window,
        &h.live,
        id,
        TerminalShell {
            name: "QA".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        },
    );
    let source = h
        .live
        .tabs
        .borrow()
        .of(id)
        .current()
        .unwrap()
        .terminal
        .clone()
        .unwrap();
    source.borrow_mut().type_text("echo BEFORE_RESTART\r");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until
        && !source
            .borrow()
            .output_lines(100)
            .iter()
            .any(|line| line == "BEFORE_RESTART")
    {
        source.borrow_mut().wait(Duration::from_millis(30));
    }
    session::write_terminal_outputs(&h.window, &h.live);
    let stored = session::capture_session(&h.window, &h.live);
    let tab = stored.panes[0]
        .tabs
        .iter()
        .find(|tab| tab.shell.is_some())
        .expect("the shell tab is written down");
    assert_eq!(tab.shell.as_ref().unwrap().command, "cmd.exe /Q /D /K");
    assert!(tab.shell.as_ref().unwrap().output.is_some());

    // Off, only the arrangement is kept, and the files go.
    h.window.set_terminal_restore_output(false);
    let without = session::capture_session(&h.window, &h.live);
    assert!(
        without.panes[0]
            .tabs
            .iter()
            .filter_map(|tab| tab.shell.as_ref())
            .all(|shell| shell.output.is_none())
    );
    h.window.set_terminal_restore_output(true);

    let (tabs, _) = session::open_session(&h.window, Some(stored), Vec::new());
    let back = tabs.panes[0]
        .tabs
        .iter()
        .find_map(|tab| tab.terminal.clone())
        .expect("the shell tab comes back");
    assert!(back.borrow().is_waiting(), "it waits to be drawn");
    assert!(
        back.borrow()
            .output_lines(100)
            .iter()
            .any(|line| line == "BEFORE_RESTART")
    );
    back.borrow_mut().start_waiting(60, 12, || {}).unwrap();
    assert!(!back.borrow().is_waiting());
    back.borrow_mut().type_text("echo AFTER_RESTART\r");
    let until = Instant::now() + Duration::from_secs(5);
    let read = || back.borrow().output_lines(100);
    while Instant::now() < until && !read().iter().any(|line| line == "AFTER_RESTART") {
        back.borrow_mut().wait(Duration::from_millis(30));
    }
    let lines = read();
    let before = lines.iter().position(|l| l == "BEFORE_RESTART").unwrap();
    let rule = lines
        .iter()
        .position(|l| l.starts_with("──── Restored · "))
        .expect("the line where the last run ends");
    let after = lines.iter().position(|l| l == "AFTER_RESTART").unwrap();
    assert!(before < rule && rule < after, "{lines:#?}");
}

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
pub(super) struct Harness {
    pub(super) window: AppWindow,
    pub(super) live: Live,
    surface: Rc<MinimalSoftwareWindow>,
}

impl Harness {
    /// Builds the offscreen window and platform first — `make_document`
    /// is only handed the window's `Weak` afterwards, because
    /// `AppWindow::new` needs a platform already set, and the document a
    /// test wants has to be built with this window's own handle.
    pub(super) fn new(
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
            composing: Rc::default(),
            settled: Rc::default(),
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
                no_tabs: Default::default(),
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
        (
            Self {
                window,
                live,
                surface,
            },
            document,
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.live.writer.finish();
        app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
    }
}

#[test]
fn terminal_panel_background_survives_typing_and_log_updates() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    h.window.set_tree_open(false);
    id.update_screen(&h.window, |s| {
        s.height = 700.0;
        s.terminal = true;
        s.empty = false;
        s.front_style = terminal_appearance::initial_style();
        s.panel_style = terminal_appearance::initial_style();
        s.panel_style.paper = Color::from_rgb_u8(30, 55, 70);
    });
    id.set_below(&h.window, 2, 240.0);
    terminal_panels::ensure(&h.window, &h.live, id);
    let entry = terminal_panels::current(&h.live, id).unwrap();
    entry.borrow_mut().style = Some(id.screen(&h.window).panel_style.clone());
    h.window.show().unwrap();
    for text in [
        "",
        "input",
        "input\nnext",
        "log 1\nlog 2\nlog 3\nlog 4\nlog 5\n",
    ] {
        *entry.borrow().document.text.borrow_mut() = text.into();
        panel_source::sync(&h.window, &h.live);
        h.window.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 740];
        h.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        // Right of the short text, inside each input/log line: neither the
        // old command-preview fill nor newline rules may cover the paper.
        for y in 535..665 {
            let p = pixels[y * 1000 + 800];
            assert_eq!(
                (p.r, p.g, p.b),
                (30, 55, 70),
                "paper changed at y={y}, text={text:?}"
            );
        }
    }
}

#[test]
fn terminal_opaque_background_covers_entire_pane() {
    use slint::platform::software_renderer::PremultipliedRgbaColor;
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    h.window.set_tree_open(false);
    h.window.set_background_transparency(0);
    let id = PaneId::FIRST;
    id.update_screen(&h.window, |s| {
        s.height = 700.0;
        s.terminal = true;
        s.empty = false;
        s.front_style = terminal_appearance::initial_style();
        s.panel_style = terminal_appearance::initial_style();
    });
    h.window.show().unwrap();
    for below in [0, 2, 0] {
        id.set_below(&h.window, below, 200.0);
        h.window.window().request_redraw();
        let mut pixels = vec![PremultipliedRgbaColor::default(); 1000 * 740];
        h.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        for y in (50..680).step_by(10) {
            for x in (100..930).step_by(10) {
                assert_eq!(
                    pixels[y * 1000 + x].alpha,
                    255,
                    "transparent hole at {x},{y}, below={below}"
                );
            }
        }
    }
    // Each surface owns its alpha; the upper surface must not shine through
    // a transparent lower panel (or make it opaque).
    id.set_below(&h.window, 1, 200.0);
    for (front, panel) in [(0, 50), (50, 0), (100, 50), (0, 100)] {
        id.update_screen(&h.window, |s| {
            s.front_style.transparency = front;
            s.panel_style.transparency = panel;
        });
        h.window.window().request_redraw();
        let mut pixels = vec![PremultipliedRgbaColor::default(); 1000 * 740];
        h.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        for (y, value) in [(120, front), (620, panel)] {
            let alpha = pixels[y * 1000 + 500].alpha as i32;
            let expected = (255.0 * (100 - value) as f32 / 100.0).round() as i32;
            assert!(
                (alpha - expected).abs() <= 1,
                "alpha at y={y}: {alpha}, expected {expected}"
            );
        }
        assert_eq!(pixels[50 * 1000 + 500].alpha, 255, "toolbar remains opaque");
    }
}

#[test]
fn terminal_settings_refresh_profiles_loaded_after_install() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    terminal_shells::install(&h.window, &h.live);
    hold_shells(
        &h.window,
        &[TerminalShell {
            name: "Loaded QA".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        }],
    );
    publish_shells(&h.window);
    h.window.set_default_shell(0);
    open_settings(&h.window, &h.live);
    assert_eq!(h.window.get_shell_profile_names().row_count(), 1);
    assert!(
        h.window
            .get_shell_profile_names()
            .row_data(0)
            .unwrap()
            .starts_with("Loaded QA (")
    );
    assert_eq!(h.window.get_shell_profile_name(), "Loaded QA");
    assert!(h.window.get_shell_profile_default());
}

#[test]
fn terminal_panel_long_text_click_and_end_render() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    id.update_screen(&h.window, |screen| {
        screen.terminal = true;
        screen.panel_style = terminal_appearance::initial_style();
    });
    let text = (0..3000).map(|i| format!("line {i}\n")).collect::<String>();
    show_draft(&h.window, id, &text);
    id.set_below(&h.window, 2, 240.0);
    terminal_panels::ensure(&h.window, &h.live, id);
    let entry = terminal_panels::current(&h.live, id).unwrap();
    *entry.borrow().document.text.borrow_mut() = text.clone();
    panel_source::sync(&h.window, &h.live);
    let source_id = entry.borrow().source_id.unwrap();
    let weak = h.window.as_weak();
    let live = h.live.clone();
    h.window.on_pane_home_end(move |pane, end, edge, extend| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            move_pane_to_line_edge(
                &window,
                id,
                &live.states.document(id),
                &live.states.of(id),
                &live.cache,
                &Rc::new(Timer::default()),
                end,
                edge,
                extend,
            );
        }
    });
    let weak = h.window.as_weak();
    let live = h.live.clone();
    h.window
        .on_pane_selection_start(move |pane, x, y, extend, _| {
            if let Some(window) = weak.upgrade() {
                let id = PaneId::from_index(pane);
                update_pane_selection(
                    &window,
                    &live.states.document(id),
                    &live.states.of(id),
                    &live.cache,
                    id,
                    id.flow_x(&window, x),
                    y,
                    if extend {
                        SelectionPhase::Extend
                    } else {
                        SelectionPhase::Begin
                    },
                );
            }
        });
    h.window.show().unwrap();
    let render = || {
        h.window.window().request_redraw();
        h.surface.draw_if_needed(|renderer| {
            renderer.render(&mut vec![slint::Rgb8Pixel::default(); 1000 * 740], 1000);
        });
    };
    render();
    let position = slint::LogicalPosition::new(400.0, 600.0);
    h.window
        .window()
        .dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    h.window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    render();
    h.window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    h.window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::End.into(),
    });
    h.window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::End.into(),
    });
    h.window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    render();
    assert_eq!(
        h.live.states.of(source_id).borrow().caret_source_byte,
        Some(text.len())
    );
    // Selection rectangles and ReadOnly use the same large scroll coordinates.
    h.window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    h.window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "a".into() });
    h.window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: "a".into() });
    h.window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    render();
    entry
        .borrow_mut()
        .view
        .borrow_mut()
        .set_read_only(true, true);
    panel_source::sync(&h.window, &h.live);
    h.window
        .window()
        .dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    h.window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    render();
    assert_eq!(id.screen(&h.window).below_draft.as_str(), text);
}

#[test]
fn terminal_panel_restored_draft_can_be_saved_and_edited_with_live_notifications() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    h.live.tabs.borrow_mut().of_mut(id).unwrap().tabs[0]
        .below
        .draft = "restored draft".into();
    terminal_panels::ensure(&h.window, &h.live, id);
    let panel = terminal_panels::current(&h.live, id).unwrap();
    let document = panel.borrow().document.clone();
    assert_eq!(&*document.text.borrow(), "restored draft");
    let path = app_data::app_directory().unwrap().join("saved-panel.md");
    assert!(saving::write_document_to(
        &h.window,
        &h.live,
        &document,
        path.clone()
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "restored draft");
    assert!(!document.text.edited());
    // Capture/edit updates hold the panel while changing its text, as in the UI.
    panel
        .borrow_mut()
        .document
        .text
        .borrow_mut()
        .push_str(" edited");
    terminal_panels::edited(&h.window, &h.live, id);
    assert!(document.text.edited());
    assert!(
        id.screen(&h.window)
            .panel_tabs
            .row_data(0)
            .unwrap()
            .ends_with('*')
    );
    assert!(terminal_panels::save(&h.window, &h.live, &panel, false));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "restored draft edited"
    );
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
        tabs.of_mut(id).unwrap().tabs[0].terminal = Some(session.clone());
    }
    let upper = h.live.tabs.borrow().of(id).tabs[0].clone();
    h.live.show_tab(&h.window, id, &upper);
    // Choosing the current profile is cancellation, including during capture.
    switch_shell(
        &h.window,
        &h.live,
        id,
        TerminalShell {
            name: "QA".into(),
            command: "invalid-command".into(),
            directory: String::new(),
        },
    );
    assert!(h.live.pending.borrow().is_none());
    assert!(Rc::ptr_eq(
        h.live.tabs.borrow().of(id).tabs[0]
            .terminal
            .as_ref()
            .unwrap(),
        &session
    ));
    terminal_panels::ensure(&h.window, &h.live, id);
    show_draft(&h.window, id, "first draft");
    terminal_panels::edited(&h.window, &h.live, id);
    terminal_panels::action(&h.window, &h.live, id, 1, 0);
    show_draft(&h.window, id, "second draft");
    terminal_panels::edited(&h.window, &h.live, id);
    terminal_panels::action(&h.window, &h.live, id, 0, 0);
    assert_eq!(id.screen(&h.window).below_draft.as_str(), "first draft");
    // Starting a capture asks about timestamps first; cancelling starts nothing.
    let panels = |h: &Harness| {
        h.live
            .tabs
            .borrow()
            .of(id)
            .current()
            .unwrap()
            .below
            .entries
            .len()
    };
    let before = panels(&h);
    terminal_panels::action(&h.window, &h.live, id, 7, 0);
    assert!(matches!(
        *h.live.pending.borrow(),
        Some(Question::PanelLogStamp { .. })
    ));
    answer_question(&h.window, &h.live, 2);
    assert_eq!(panels(&h), before);
    assert!(session.borrow_mut().capture_update().is_none());
    terminal_panels::action(&h.window, &h.live, id, 7, 0);
    answer_question(&h.window, &h.live, 1);
    let target = terminal_panels::current(&h.live, id).unwrap();
    assert!(target.borrow().view.borrow().viewer);
    switch_shell(
        &h.window,
        &h.live,
        id,
        TerminalShell {
            name: "QA".into(),
            command: "invalid-command".into(),
            directory: String::new(),
        },
    );
    assert!(h.live.pending.borrow().is_none());
    assert!(target.borrow().capture.is_some());
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
    h.window.set_terminal_history_limit(3);
    session
        .borrow_mut()
        .type_text("for /L %i in (1,1,20) do @echo RFN_LIMIT_%i\r");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        session.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
        if target
            .borrow()
            .document
            .text
            .borrow()
            .contains("RFN_LIMIT_20")
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
            .contains("RFN_LIMIT_20"),
        "panel={:?}, screen={:?}",
        target.borrow().document.text.borrow().as_str(),
        session.borrow().screen().retained_text()
    );
    assert!(target.borrow().document.text.borrow().lines().count() <= 3);
    assert!(target.borrow().document.file.borrow().path().is_none());
    // Stop at the source even while a different Panel is selected.
    switch_to_tab(&h.window, &h.live, id, 0);
    terminal_panels::action(&h.window, &h.live, id, 0, 0);
    terminal_panels::action(&h.window, &h.live, id, 13, 0);
    assert!(!target.borrow().view.borrow().viewer);
    assert!(target.borrow().capture.is_none());
    assert!(
        !target.borrow().document.text.borrow().starts_with('['),
        "a capture started without timestamps has none"
    );

    // With timestamps: every finished line starts with one, and stopping
    // stamps the unfinished prompt as well.
    h.window.set_terminal_history_limit(1000);
    h.window
        .set_terminal_timestamp_format("<yyyy-MM-dd HH:mm:ss> ".into());
    terminal_panels::action(&h.window, &h.live, id, 7, 0);
    answer_question(&h.window, &h.live, 0);
    let stamped = terminal_panels::current(&h.live, id).unwrap();
    assert!(!Rc::ptr_eq(&stamped, &target));
    session.borrow_mut().type_text("echo RFN_STAMPED\r");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        session.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
        if stamped.borrow().document.text.borrow().contains("\n") {
            let text = stamped.borrow().document.text.borrow().clone();
            if text
                .lines()
                .any(|l| l.ends_with("RFN_STAMPED") && !l.contains("echo"))
            {
                break;
            }
        }
    }
    // Typed but not entered: the unfinished line the capture stops on.
    session.borrow_mut().type_text("RFN_TAIL");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until
        && !stamped
            .borrow()
            .document
            .text
            .borrow()
            .ends_with("RFN_TAIL")
    {
        session.borrow_mut().wait(Duration::from_millis(30));
        terminal_panels::drain(&h.window, &h.live);
    }
    stamped.borrow_mut().stop();
    let text = stamped.borrow().document.text.borrow().clone();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.iter().any(|l| l.ends_with("RFN_STAMPED")), "{text:?}");
    let shape = |line: &str| {
        let b = line.as_bytes();
        b.len() >= 22
            && b[0] == b'<'
            && b[20] == b'>'
            && b[21] == b' '
            && line[1..20].bytes().enumerate().all(|(i, c)| match i {
                4 | 7 => c == b'-',
                10 => c == b' ',
                13 | 16 => c == b':',
                _ => c.is_ascii_digit(),
            })
    };
    assert!(lines.iter().all(|l| shape(l)), "{text:?}");
    assert!(lines.last().unwrap().ends_with(">RFN_TAIL"), "{text:?}");
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
        let tab = &mut tabs.of_mut(id).unwrap().tabs[0];
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

#[test]
fn terminal_panel_idle_publication_preserves_models_and_saved_title() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    terminal_panels::ensure(&h.window, &h.live, id);
    terminal_panels::drain(&h.window, &h.live);
    let before = id.screen(&h.window);
    for _ in 0..100 {
        terminal_panels::drain(&h.window, &h.live);
    }
    assert_eq!(
        before,
        id.screen(&h.window),
        "idle ticks must not rebuild the UI models"
    );
    let panel = terminal_panels::current(&h.live, id).unwrap();
    let doc = panel.borrow().document.clone();
    let path = app_data::app_directory().unwrap().join("Panel Title.md");
    doc.text.borrow_mut().push_str("content");
    assert!(saving::write_document_to(&h.window, &h.live, &doc, path));
    terminal_panels::drain(&h.window, &h.live);
    assert_eq!(
        id.screen(&h.window).panel_tabs.row_data(0).unwrap(),
        "Panel Title.md"
    );
}

#[test]
fn terminal_random_override_wins_until_pane_background_changed() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    let mut defaults = vec![terminal_appearance::initial_style(); 3];
    for style in &mut defaults {
        style.random = 1;
    }
    h.window
        .set_panel_defaults(ModelRc::new(VecModel::from(defaults)));
    id.update_screen(&h.window, |s| {
        s.pane_paper_own = true;
        s.pane_paper = Color::from_rgb_u8(1, 2, 3);
    });
    terminal_panels::ensure(&h.window, &h.live, id);
    let mut tabs = h.live.tabs.borrow_mut();
    let tab = &mut tabs.of_mut(id).unwrap().tabs[0];
    tab.below.front_style = terminal_appearance::random_style(&h.window, 0);
    terminal_panels::publish(&h.window, id, &tab.below);
    assert_ne!(
        id.screen(&h.window).front_style.paper,
        id.screen(&h.window).pane_paper
    );
    assert_ne!(
        id.screen(&h.window).panel_style.paper,
        id.screen(&h.window).pane_paper
    );
    terminal_appearance::clear_paper(tab);
    terminal_panels::publish(&h.window, id, &tab.below);
    assert_eq!(
        id.screen(&h.window).front_style.paper,
        id.screen(&h.window).pane_paper
    );
    assert_eq!(
        id.screen(&h.window).panel_style.paper,
        id.screen(&h.window).pane_paper
    );
}

#[test]
#[ignore = "starts a real ConPTY shell; run explicitly"]
fn terminal_search_preserves_output_and_toggle_clears_state() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    let source = Rc::new(RefCell::new(
        TerminalSession::start("QA", "cmd.exe /Q /D /K", 80, 25, || {}).unwrap(),
    ));
    h.live.tabs.borrow_mut().of_mut(id).unwrap().tabs[0].terminal = Some(source.clone());
    let tab = h.live.tabs.borrow().of(id).tabs[0].clone();
    h.live.show_tab(&h.window, id, &tab);
    source.borrow_mut().wait(Duration::from_millis(200));
    source.borrow_mut().type_text("echo editor_spike.exe\r");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        source.borrow_mut().wait(Duration::from_millis(50));
        if source
            .borrow()
            .screen()
            .retained_text()
            .contains("\neditor_spike.exe\n")
        {
            break;
        }
    }
    source.borrow_mut().wait(Duration::from_millis(200));
    refresh_terminal_panes(&h.window, &h.live);
    let before = source.borrow().screen().retained_text();
    assert!(before.contains("editor_spike.exe"));
    let rows = source.borrow().screen().rows();
    terminal_workflow::action(&h.window, &h.live, id, TerminalSpot::Front, 0);
    id.update_screen(&h.window, |s| s.terminal_query = "edit".into());
    terminal_workflow::action(&h.window, &h.live, id, TerminalSpot::Front, 1);
    assert_ne!(id.screen(&h.window).terminal_search_status, "0 / 0");
    assert!(
        h.live
            .cache
            .borrow_mut()
            .pane(id)
            .terminal
            .as_ref()
            .unwrap()
            .selection
            .is_some()
    );
    terminal_workflow::action(&h.window, &h.live, id, TerminalSpot::Front, 0);
    assert!(!id.screen(&h.window).terminal_search_open);
    assert!(id.screen(&h.window).terminal_query.is_empty());
    assert!(
        h.live
            .cache
            .borrow_mut()
            .pane(id)
            .terminal
            .as_ref()
            .unwrap()
            .selection
            .is_none()
    );
    assert_eq!(source.borrow().screen().rows(), rows);
    assert_eq!(source.borrow().screen().retained_text(), before);
    // Opening/renaming a panel file uses the same file move rules without
    // replacing the upper terminal or placing dirty markers in the filename.
    let path = app_data::app_directory().unwrap().join("Open Panel.md");
    std::fs::write(&path, "panel file contents").unwrap();
    terminal_panels::install(&h.window, &h.live);
    terminal_panels::open_file(&h.window, &h.live, id, &path);
    let panel = terminal_panels::current(&h.live, id).unwrap();
    assert_eq!(
        &*panel.borrow().document.text.borrow(),
        "panel file contents"
    );
    h.window.invoke_panel_renamed(
        id.index(),
        id.screen(&h.window).panel_active,
        "Renamed Panel.md".into(),
    );
    assert!(!path.exists());
    assert_eq!(
        panel
            .borrow()
            .document
            .file
            .borrow()
            .path()
            .unwrap()
            .file_name()
            .unwrap(),
        "Renamed Panel.md"
    );
    assert!(Rc::ptr_eq(
        h.live.tabs.borrow().of(id).tabs[0]
            .terminal
            .as_ref()
            .unwrap(),
        &source
    ));
}

#[test]
#[ignore = "starts a real ConPTY shell; run explicitly"]
fn terminal_panel_shell_selection_same_cancel_and_replace() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let id = PaneId::FIRST;
    let profiles = vec![
        TerminalShell {
            name: "QA A".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        },
        TerminalShell {
            name: "QA B".into(),
            command: "cmd.exe /Q /D /K".into(),
            directory: String::new(),
        },
    ];
    hold_shells(&h.window, &profiles);
    publish_shells(&h.window);
    let old = Rc::new(RefCell::new(
        TerminalSession::start("QA A", "cmd.exe /Q /D /K", 80, 12, || {}).unwrap(),
    ));
    {
        let mut tabs = h.live.tabs.borrow_mut();
        tabs.of_mut(id).unwrap().tabs[0].below.shell = Some(old.clone());
        tabs.of_mut(id).unwrap().tabs[0].below.open = true;
    }
    terminal_panels::ensure(&h.window, &h.live, id);
    let tab = h.live.tabs.borrow().of(id).tabs[0].clone();
    h.live.show_tab(&h.window, id, &tab);
    let panel = terminal_panels::current(&h.live, id).unwrap();
    terminal_panels::action(&h.window, &h.live, id, 12, 0);
    assert!(h.live.pending.borrow().is_none());
    assert!(Rc::ptr_eq(panel.borrow().shell.as_ref().unwrap(), &old));
    terminal_panels::action(&h.window, &h.live, id, 12, 1);
    assert!(matches!(
        *h.live.pending.borrow(),
        Some(Question::PanelSwitch { .. })
    ));
    answer_question(&h.window, &h.live, 1);
    assert!(Rc::ptr_eq(panel.borrow().shell.as_ref().unwrap(), &old));
    terminal_panels::action(&h.window, &h.live, id, 12, 1);
    answer_question(&h.window, &h.live, 0);
    assert_eq!(
        panel.borrow().shell.as_ref().unwrap().borrow().name(),
        "QA B"
    );
    assert_eq!(h.live.tabs.borrow().of(id).tabs[0].below.active, 0);
}

/// 書き手の求め 2026-11-05: **倍率はペインに1つで、Terminalと下段にも効く。**字の大きさは
/// 書式の文字サイズ × ペインの倍率。下段のEditor Panelから倍率を動かしても、持ち主のペインの
/// 倍率が動く。
#[test]
fn the_pane_zoom_reaches_the_terminal_and_its_panels() {
    let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
    let owner = PaneId::FIRST;
    owner.update_screen(&h.window, |screen| {
        screen.terminal = true;
        screen.below_kind = 2;
        screen.below_height = 240.;
        screen.front_style = terminal_appearance::initial_style();
        screen.panel_style = terminal_appearance::initial_style();
    });
    let size = terminal_appearance::initial_style().size as f32;
    owner.set_zoom(&h.window, 100);
    terminal_panels::ensure(&h.window, &h.live, owner);
    panel_source::sync(&h.window, &h.live);
    let entry = terminal_panels::current(&h.live, owner).unwrap();
    let panel = entry.borrow().source_id.unwrap();
    let font = |spot| terminal_appearance::look(&h.window, owner, spot).font_size;
    assert_eq!(font(TerminalSpot::Front), size);
    assert_eq!(font(TerminalSpot::Below), size);
    assert_eq!(pane_typography(&h.window, panel).font_size, size);

    owner.set_zoom(&h.window, 200);
    panel_source::sync(&h.window, &h.live);
    assert_eq!(font(TerminalSpot::Front), size * 2.0, "the shell above");
    assert_eq!(font(TerminalSpot::Below), size * 2.0, "the shell below");
    assert_eq!(pane_typography(&h.window, panel).font_size, size * 2.0);
    assert_eq!(
        panel.zoom(&h.window),
        200,
        "the status bar reads the owner's"
    );

    // 下段から動かしても、ペインの倍率が1つ動く。
    let timer = Rc::new(Timer::default());
    zoom_pane(&h.window, &h.live.states, &h.live.cache, &timer, panel, 120);
    assert_eq!(owner.zoom(&h.window), 120);
    panel_source::sync(&h.window, &h.live);
    assert_eq!(panel.zoom(&h.window), 120);
    assert_eq!(font(TerminalSpot::Front), size * 1.2);
}
