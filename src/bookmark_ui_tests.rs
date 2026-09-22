//! 書き手の求め 2026-09-22: the Bookmark View, the outline's folds and the
//! Add Bookmark dialog, on a real window.
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

const WIDTH: u32 = 900;
const HEIGHT: u32 = 520;

fn row(title: &str, group: bool, open: bool, depth: i32, tip: &str) -> BookmarkRow {
    BookmarkRow {
        title: title.into(),
        group,
        open,
        depth,
        tip: tip.into(),
    }
}

fn left_row(name: &str, depth: i32, folder: bool, open: bool) -> LeftRow {
    LeftRow {
        name: name.into(),
        depth,
        folder,
        open,
        parent: -1,
        is_root: false,
    }
}

fn window_with(surface: &Rc<MinimalSoftwareWindow>) -> AppWindow {
    let window = AppWindow::new().unwrap();
    surface.set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));
    publish_panes(&window, 1);
    window.set_tree_open(true);
    window.show().unwrap();
    window
}

fn snapshot(window: &AppWindow, surface: &MinimalSoftwareWindow, name: &str) {
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
    surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, WIDTH as usize);
    });
    let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
    for pixel in pixels {
        ppm.extend([pixel.r, pixel.g, pixel.b]);
    }
    let output = PathBuf::from("target/bookmark-qa");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join(name), ppm).unwrap();
}

fn click(window: &AppWindow, x: f32, y: f32, button: PointerEventButton) {
    let position = slint::LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    slint::platform::update_timers_and_animations();
}

fn sample_rows() -> Vec<BookmarkRow> {
    vec![
        row("Drafts", true, true, 0, ""),
        row("第一章", false, false, 1, "章/一.md # 第一章"),
        row("Notes", true, false, 0, ""),
        row("Alpha", false, false, 0, "a.md"),
    ]
}

#[test]
#[ignore = "offscreen visual verification of the Bookmark View, outline folds and dialog"]
fn bookmark_view_outline_and_dialog_render() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = window_with(&surface);

    window.set_left_tab(3);
    window.set_left_rows(ModelRc::new(VecModel::from(vec![
        left_row("一", 0, true, true),
        left_row("二", 1, true, false),
        left_row("四", 1, false, false),
        left_row("五", 0, false, false),
    ])));
    snapshot(&window, &surface, "outline.ppm");

    window.set_left_tab(5);
    window.set_bookmark_available(false);
    snapshot(&window, &surface, "bookmarks-no-workspace.ppm");
    window.set_bookmark_available(true);
    snapshot(&window, &surface, "bookmarks-empty.ppm");
    window.set_bookmark_rows(ModelRc::new(VecModel::from(sample_rows())));
    window.set_bookmark_selected(1);
    window.set_bookmark_groups(ModelRc::new(VecModel::from(vec![
        SharedString::from("Drafts"),
        SharedString::from("Notes"),
    ])));
    window.set_bookmark_filter_visible(true);
    snapshot(&window, &surface, "bookmarks.ppm");

    window.set_question_bookmark_link("章/一.md # 第一章 / 場面".into());
    window.set_question_bookmark_groups(ModelRc::new(VecModel::from(vec![
        SharedString::from("(Root)"),
        SharedString::from("Drafts"),
    ])));
    window.set_question_bookmark_group(1);
    window.set_question_name("場面".into());
    window.set_question_text("Add Bookmark".into());
    window.set_question_choices(ModelRc::new(VecModel::from(vec![
        SharedString::from("OK"),
        SharedString::from("Cancel"),
    ])));
    window.set_question_asks_name(true);
    window.set_question_is_bookmark(true);
    window.set_question_open(true);
    snapshot(&window, &surface, "dialog.ppm");
}

/// What a click on each part of the view and the outline asks for: a group's
/// row opens or closes it, a bookmark's row opens the bookmark, the empty space
/// lets go of it, the right button's menu deletes and moves; in the outline the
/// ▸ folds and the rest of the row goes to the heading.
#[test]
fn the_bookmark_view_and_the_outline_answer_their_clicks() {
    let surface = MinimalSoftwareWindow::new(Default::default());
    slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
    let window = window_with(&surface);
    let heard = Rc::new(RefCell::new(Vec::<String>::new()));
    macro_rules! hear {
        ($callback:ident, $name:literal $(, $argument:ident)*) => {{
            let heard = heard.clone();
            window.$callback(move |$($argument),*| {
                heard
                    .borrow_mut()
                    .push(format!(concat!($name $(, " {", stringify!($argument), "}")*) $(, $argument = $argument)*));
            });
        }};
    }
    hear!(on_bookmark_toggled, "toggled", row);
    hear!(on_bookmark_activated, "activated", row);
    hear!(on_bookmark_cleared, "cleared");
    hear!(on_bookmark_remove, "remove", row);
    hear!(on_bookmark_move_to, "move", row, group);
    hear!(on_bookmark_add_current, "add-current");
    hear!(on_outline_fold_toggled, "fold", row);
    hear!(on_left_row_activated, "heading", row);
    hear!(on_outline_bookmark_requested, "outline-bookmark", row);

    window.set_left_tab(5);
    window.set_bookmark_available(true);
    window.set_bookmark_filter_visible(true);
    window.set_bookmark_rows(ModelRc::new(VecModel::from(sample_rows())));
    window.set_bookmark_groups(ModelRc::new(VecModel::from(vec![
        SharedString::from("Drafts"),
        SharedString::from("Notes"),
    ])));
    snapshot(&window, &surface, "clicks.ppm");
    click(&window, 150.0, 86.0, PointerEventButton::Left);
    click(&window, 150.0, 110.0, PointerEventButton::Left);
    click(&window, 150.0, 300.0, PointerEventButton::Left);
    click(&window, 68.0, 27.0, PointerEventButton::Left);
    // 栞の行の右クリック → Delete（その場で消す）。
    click(&window, 150.0, 158.0, PointerEventButton::Right);
    click(
        &window,
        180.0,
        158.0 + 6.0 + 29.0 + 14.0,
        PointerEventButton::Left,
    );
    assert_eq!(
        *heard.borrow(),
        [
            "toggled 0",
            "activated 1",
            "cleared",
            "add-current",
            "remove 3"
        ]
    );
    heard.borrow_mut().clear();

    window.set_left_tab(3);
    window.set_left_rows(ModelRc::new(VecModel::from(vec![
        left_row("一", 0, true, true),
        left_row("二", 1, true, false),
        left_row("五", 0, false, false),
    ])));
    click(&window, 60.0, 57.0, PointerEventButton::Left);
    click(&window, 150.0, 57.0, PointerEventButton::Left);
    click(&window, 76.0, 81.0, PointerEventButton::Left);
    click(&window, 150.0, 105.0, PointerEventButton::Right);
    click(&window, 190.0, 105.0 + 6.0 + 14.0, PointerEventButton::Left);
    assert_eq!(
        *heard.borrow(),
        ["fold 0", "heading 0", "fold 1", "outline-bookmark 2"]
    );
}

/// The whole of it on a real window, document and Workspace: add a bookmark
/// from the caret's heading, make a group and move it in, open it — the file's
/// section is lit and its row chosen — then an edit, and Escape, let it go.
/// What was made is in the Workspace's file afterwards.
#[test]
fn a_bookmark_is_added_moved_opened_lit_and_let_go() {
    let directory = std::env::temp_dir().join(format!(
        "editor-bookmarks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root = directory.join("manuscript");
    std::fs::create_dir_all(root.join("章")).unwrap();
    let path = root.join("章").join("一.md");
    let text = "# 一\nintro\n## 二\nbody\n# 三\nend\n";
    std::fs::write(&path, text).unwrap();
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
    live.folder.borrow_mut().workspace = Some(runtime.clone());
    window.set_left_tab(5);
    let refresh = |live: &Live| {
        let source = document.text.borrow().clone();
        refresh_pane_from_state(
            &window,
            &live.cache,
            &document,
            id,
            &live.states.of(id),
            &source,
        );
    };
    refresh(&live);
    bookmark_ui::publish(&window, &live);
    assert!(window.get_bookmark_available());

    // 1. The caret in "body" is under 二, so the dialog offers 二 in its chain.
    live.states.of(id).borrow_mut().caret_source_byte = Some(text.find("body").unwrap());
    bookmark_ui::offer_from_pane(&window, &live, 0);
    assert!(window.get_question_open() && window.get_question_is_bookmark());
    assert_eq!(window.get_question_bookmark_link(), "章/一.md # 一 / 二");
    assert_eq!(window.get_question_name(), "二");
    window.set_question_name("Scene two".into());
    answer_question(&window, &live, 0);
    assert!(!window.get_question_open());

    // 2. A group, and the bookmark moved into it.
    bookmark_ui::ask_new_group(&window, &live);
    assert!(!window.get_question_is_bookmark());
    window.set_question_name("Drafts".into());
    answer_question(&window, &live, 0);
    let titles = |window: &AppWindow| {
        window
            .get_bookmark_rows()
            .iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth as usize), row.title))
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(&window), ["Drafts", "Scene two"]);
    bookmark_ui::dropped(&window, &live, 1, 0);
    assert_eq!(titles(&window), ["Drafts", "  Scene two"]);

    // 3. Opening it lights 二's section and chooses its row.
    live.states.of(id).borrow_mut().caret_source_byte = Some(0);
    bookmark_ui::activate(&window, &live, 1);
    {
        let cache = live.cache.borrow();
        let mark = cache.bookmark_mark.as_ref().expect("lit");
        assert_eq!(&text[mark.start..mark.end], "## 二\nbody\n");
    }
    assert_eq!(window.get_bookmark_selected(), 1);
    assert!(id.screen(&window).bookmark_rects.row_count() > 0);
    // Switching the left pane to another view keeps it.
    window.set_left_tab(3);
    refresh(&live);
    assert!(live.cache.borrow().bookmark_mark.is_some());
    window.set_left_tab(5);

    // 4. An edit lets it go.
    document.text.borrow_mut().push_str("more\n");
    refresh(&live);
    assert!(live.cache.borrow().bookmark_mark.is_none());
    assert_eq!(window.get_bookmark_selected(), -1);
    assert_eq!(id.screen(&window).bookmark_rects.row_count(), 0);

    // 5. So does Escape, and a second Escape has nothing left to let go of.
    bookmark_ui::activate(&window, &live, 1);
    assert!(live.cache.borrow().bookmark_mark.is_some());
    assert!(bookmark_ui::let_go_in(
        &window,
        &live.cache,
        &live.states,
        id
    ));
    assert!(!bookmark_ui::let_go_in(
        &window,
        &live.cache,
        &live.states,
        id
    ));
    assert_eq!(id.screen(&window).bookmark_rects.row_count(), 0);

    // 6. The outline folds 一, and its second row is then 三 — a click there
    //    goes to 三, not to what was second before the fold.
    window.set_left_tab(3);
    publish_outline(&window, &live);
    let names = |window: &AppWindow| {
        window
            .get_left_rows()
            .iter()
            .map(|row| (row.name.to_string(), row.folder, row.open))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&window),
        [
            ("一".to_owned(), true, true),
            ("二".to_owned(), false, true),
            ("三".to_owned(), false, true)
        ]
    );
    outline_fold_toggled(&window, &live, 0);
    assert_eq!(
        names(&window),
        [
            ("一".to_owned(), true, false),
            ("三".to_owned(), false, true)
        ]
    );
    go_to_heading(&window, &live, 1);
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(&live.states.of(id), &source);
    assert_eq!(caret, source.find("# 三").unwrap());
    fold_whole_outline(&window, &live, false);
    assert_eq!(window.get_left_rows().row_count(), 3);

    // 7. It is all in the Workspace's own file.
    let saved = bookmarks::load(&appdata, workspace);
    assert_eq!(saved.groups.len(), 1);
    assert_eq!(saved.groups[0].name, "Drafts");
    assert_eq!(
        saved.groups[0].items,
        [bookmarks::Bookmark {
            title: "Scene two".to_owned(),
            path: path.clone(),
            heading: vec!["一".to_owned(), "二".to_owned()],
        }]
    );
}
