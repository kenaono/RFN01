//! Render the real Slint pane without opening a native window or touching a session.
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

#[test]
fn comparison_renders_and_protects_the_snapshot_in_both_directions() {
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
    surface.set_size(slint::PhysicalSize::new(1100, 760));
    window.set_tree_open(false);
    publish_panes(&window, 1);
    let id = PaneId::from_index(0);
    let original = "# 朝の庭\n猫が眠っている。\n風が木々を揺らす。\n夜になった。\n";
    let outside = "# 朝の庭\n犬が眠っている。\n風が木々を揺らす。\n鳥が鳴いた。\n夜になった。\n";
    let source = OpenDocument::new(DocumentFile::untitled(1), original.into(), window.as_weak());
    let snapshot = OpenDocument::snapshot(
        "原稿.md".into(),
        file_io::TextForm::default(),
        outside.into(),
        window.as_weak(),
    );
    source.compare_with(&snapshot);
    let states = PaneStates::new(&snapshot);
    let cache = Rc::new(RefCell::new(RenderCache::default()));
    window.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1100 * 760];
    for vertical in [false, true] {
        id.update_screen(&window, |screen| {
            screen.width = 1050.0;
            screen.height = 640.0;
            screen.shown_width = 1050.0;
            screen.shown_height = 540.0;
            screen.preview = true;
            screen.tabs = ModelRc::new(VecModel::from(vec![TabInfo {
                title: "原稿.md［外部版・読み取り専用］".into(),
                ..Default::default()
            }]));
        });
        set_pane_direction(&window, &cache, id, vertical);
        refresh_pane_from_state(&window, &cache, &snapshot, id, &states.of(id), outside);
        let screen = id.screen(&window);
        assert!(
            screen.comparison_note.contains("差分2箇所"),
            "{}",
            screen.comparison_note
        );
        assert!(screen.difference_rects.row_count() >= 2);
        for index in 0..screen.difference_rects.row_count() {
            let rect = screen.difference_rects.row_data(index).unwrap();
            assert!(rect.width > 0.0 && rect.height > 0.0);
        }
        insert_pane_text(&window, id, &snapshot, &states, &cache, "編集しない", false);
        splice_source(&window, id, &snapshot, &states, &cache, 0, 1, "", 0);
        undo_in_pane(&window, id, &snapshot, &states, &cache, false);
        assert_eq!(snapshot.text.borrow().as_str(), outside);
        assert!(!snapshot.text.edited());
        window.set_render_status(Default::default());
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1100);
        });
        if let Ok(directory) = std::env::var("EDITOR_COMPARISON_SNAPSHOT") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let name = if vertical { "vertical" } else { "horizontal" };
            let mut ppm = b"P6\n1100 760\n255\n".to_vec();
            for pixel in &pixels {
                ppm.extend([pixel.r, pixel.g, pixel.b]);
            }
            std::fs::write(directory.join(format!("comparison-{name}.ppm")), ppm).unwrap();
        }
        // The shorter side still shows a narrow marker at the insertion point.
        states.show(id, &source);
        refresh_pane_from_state(&window, &cache, &source, id, &states.of(id), original);
        let rects = id.screen(&window).difference_rects;
        assert!(rects.row_count() >= 2);
        assert!((0..rects.row_count()).any(|index| {
            let rect = rects.row_data(index).unwrap();
            if vertical {
                rect.height <= 3.0
            } else {
                rect.width <= 3.0
            }
        }));
        states.show(id, &snapshot);
    }
    snapshot.stop_comparison();
    refresh_pane_from_state(&window, &cache, &snapshot, id, &states.of(id), outside);
    assert_eq!(id.screen(&window).difference_rects.row_count(), 0);
    assert!(id.screen(&window).comparison_note.is_empty());
}
