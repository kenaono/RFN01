//! 書き手の求め 2026-09-23: the Tag View, the `tag:` search it runs and the
//! `#` completion, on a real window.
use super::*;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn window_with(surface: &Rc<MinimalSoftwareWindow>) -> AppWindow {
    let window = AppWindow::new().unwrap();
    surface.set_size(slint::PhysicalSize::new(900, 520));
    publish_panes(&window, 1);
    window.set_tree_open(true);
    window.show().unwrap();
    window
}

fn click(window: &AppWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    slint::platform::update_timers_and_animations();
}

fn names(window: &AppWindow) -> Vec<String> {
    window
        .get_left_rows()
        .iter()
        .map(|row| format!("{}{}", "  ".repeat(row.depth as usize), row.name))
        .collect()
}

/// The ▸ of a Tag View row folds it; the rest of the row runs its search —
/// the same places the outline's rows answer.
#[test]
fn the_tag_view_answers_its_clicks() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = window_with(&surface);
    let heard = Rc::new(RefCell::new(Vec::<String>::new()));
    let fold = heard.clone();
    window.on_tag_fold_toggled(move |row| fold.borrow_mut().push(format!("fold {row}")));
    let row = heard.clone();
    window.on_left_row_activated(move |index| row.borrow_mut().push(format!("row {index}")));
    let all = heard.clone();
    window.on_tag_fold_all(move |close| all.borrow_mut().push(format!("all {close}")));

    window.set_left_tab(6);
    window.set_tag_available(true);
    window.set_left_rows(ModelRc::new(VecModel::from(vec![
        LeftRow {
            name: "小説".into(),
            depth: 0,
            folder: true,
            open: false,
            parent: -1,
            is_root: false,
        },
        LeftRow {
            name: "メモ".into(),
            depth: 0,
            folder: false,
            open: false,
            parent: -1,
            is_root: false,
        },
    ])));
    window.set_tag_counts(ModelRc::new(VecModel::from(vec![
        SharedString::from("2"),
        SharedString::from("1"),
    ])));
    click(&window, 60.0, 57.0);
    click(&window, 150.0, 57.0);
    click(&window, 150.0, 81.0);
    // Collapse All is the first button over the rows.
    click(&window, 68.0, 27.0);
    assert_eq!(*heard.borrow(), ["fold 0", "row 0", "row 1", "all true"]);
}

/// A look at the view itself: `target/tag-qa/tags.ppm`.
#[test]
#[ignore = "offscreen visual verification of the Tag View"]
fn the_tag_view_renders() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = window_with(&surface);
    let row = |name: &str, depth: i32, folder: bool, open: bool| LeftRow {
        name: name.into(),
        depth,
        folder,
        open,
        parent: -1,
        is_root: false,
    };
    window.set_left_tab(6);
    window.set_tag_available(true);
    window.set_tag_filter_visible(true);
    window.set_left_rows(ModelRc::new(VecModel::from(vec![
        row("メモ", 0, false, false),
        row("小説", 0, true, true),
        row("人物", 1, true, false),
        row("下書き", 1, false, false),
        row("TODO", 0, false, false),
    ])));
    let counts = ["1", "12", "3", "5", "2"].map(SharedString::from);
    window.set_tag_counts(ModelRc::new(VecModel::from(counts.to_vec())));
    window.window().request_redraw();
    let (width, height) = (900usize, 520usize);
    let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    let output = PathBuf::from("target/tag-qa");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("tags.ppm"), ppm).unwrap();
}

/// The whole of it on a real Workspace: the index reads the tags, the view
/// shows them as a tree with counts, folds and filters, a row searches the
/// work folder with `tag:#…`, a result opens on the tag, and `#` offers the
/// Workspace's tags.
#[test]
fn tags_are_indexed_shown_searched_and_offered() {
    let directory = std::env::temp_dir().join(format!(
        "editor-tags-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root = directory.join("manuscript");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("a.md");
    let text = "# 一\n本文 #小説/人物 と #メモ\n";
    std::fs::write(&path, text).unwrap();
    std::fs::write(root.join("b.md"), "---\ntags: [小説]\n---\n脇役\n").unwrap();
    let appdata = directory.join("appdata");
    std::fs::create_dir_all(&appdata).unwrap();
    app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(appdata.clone()));
    struct Reset(PathBuf);
    impl Drop for Reset {
        fn drop(&mut self) {
            app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _reset = Reset(directory.clone());

    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = window_with(&surface);
    let id = PaneId::from_index(0);
    id.update_screen(&window, |screen| {
        screen.width = 560.0;
        screen.height = 440.0;
        screen.shown_width = 560.0;
        screen.shown_height = 440.0;
    });
    window.set_autosave(false);
    let (file, read) = DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS).unwrap();
    let document = OpenDocument::new(file, read, window.as_weak());
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
                let in_front = PaneTab::showing(&window, id, document.clone());
                PaneTabs {
                    history: vec![NavigationPlace::from(&in_front)],
                    tabs: vec![in_front],
                    ..Default::default()
                }
            }],
        })),
        writer: Rc::new(FileWriter::start()),
        searcher: Rc::new(Searcher::start(|| {})),
        searched: Rc::default(),
    };
    let runtime = Rc::new(RefCell::new(workspace_ui::Runtime::open(appdata.clone())));
    let workspace = runtime
        .borrow_mut()
        .edit(|registry| {
            let id = registry.create_workspace("Novel".into())?;
            registry.add_root(id, &root)?;
            Ok(id)
        })
        .unwrap();
    runtime.borrow_mut().set_active_silently(Some(workspace));
    live.folder.borrow_mut().workspace = Some(runtime);
    // The Explorer was the tree shown last, and it has no folder: a `tag:`
    // search still walks the Workspace, whose tags the view counts.
    window.set_workspace_active(true);
    live.states.of(id).borrow_mut().caret_source_byte = Some(0);

    // 1. The view fills in as the index reads the files: closed, with counts.
    window.set_left_tab(6);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        workspace_links_tick(&window, &live);
        if names(&window) == ["メモ", "小説"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the view showed {:?}",
            names(&window)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(window.get_tag_available());
    let counts: Vec<String> = window.get_tag_counts().iter().map(Into::into).collect();
    assert_eq!(counts, ["1", "2"]);

    // 2. Folds and the filter.
    tag_fold_toggled(&window, &live, 1);
    assert_eq!(names(&window), ["メモ", "小説", "  人物"]);
    fold_all_tags(&window, &live, true);
    assert_eq!(names(&window), ["メモ", "小説"]);
    fold_all_tags(&window, &live, false);
    assert_eq!(names(&window), ["メモ", "小説", "  人物"]);
    fold_all_tags(&window, &live, true);
    window.set_tag_filter("人".into());
    publish_tags(&window, &live);
    assert_eq!(names(&window), ["小説", "  人物"]);
    window.set_tag_filter("".into());
    publish_tags(&window, &live);

    // 3. `#` offers the Workspace's tags; the one typed in full is not offered.
    let typed = format!("{text}#小");
    *document.text.borrow_mut() = typed.clone();
    live.states.of(id).borrow_mut().caret_source_byte = Some(typed.len());
    let offered = |live: &Live| -> Vec<String> {
        workspace_link_ui(live)
            .borrow()
            .completion
            .candidates()
            .iter()
            .map(|item| item.display.clone())
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        workspace_links_tick(&window, &live);
        if !offered(&live).is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the tag popup never opened");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(offered(&live), ["#小説", "#小説/人物"]);
    accept_link_completion(&window, &live, id);
    assert_eq!(*document.text.borrow(), format!("{text}#小説"));
    *document.text.borrow_mut() = text.to_owned();
    live.states.of(id).borrow_mut().caret_source_byte = Some(0);

    // 4. A row searches the work folder for its tag, in the search panel.
    search_tag_row(&window, &live, 1);
    assert_eq!(window.get_left_tab(), 1);
    assert_eq!(window.get_folder_needle(), "tag:#小説");
    let deadline = Instant::now() + Duration::from_secs(20);
    while live.results.borrow().is_empty() {
        assert!(Instant::now() < deadline, "the tag search never answered");
        std::thread::sleep(Duration::from_millis(5));
        collect_search(&window, &live);
    }
    let rows: Vec<String> = live
        .results
        .borrow()
        .iter()
        .map(|row| row.text.clone())
        .collect();
    assert_eq!(
        rows,
        [
            "a.md",
            "2: 本文 #小説/人物 と #メモ",
            "b.md",
            "2: tags: [小説]"
        ]
    );

    // 5. Opening the result selects the tag itself.
    open_result(&window, &live, 1);
    let start = text.find("#小説").unwrap();
    assert_eq!(
        live.states.of(id).borrow().search_selection,
        Some((start, start + "#小説/人物".len()))
    );
}
