//! RFN01-63: 綴りの確認を、実際の窓（画面外）で確かめる。Runで始めて、赤い波線が
//! 描かれ、右クリックの行で直せて、終えれば消える。
use super::*;
use crate::saving::{Harness, open_under, scratch_directory};
use slint::Model;

const WIDTH: usize = 1000;
const HEIGHT: usize = 740;

/// 綴りの印の赤（`draw_spelling`の色）。
fn spelling_red(pixel: &slint::Rgb8Pixel) -> bool {
    pixel.r > 180 && pixel.g < 90 && pixel.b < 90
}

fn render(harness: &Harness, document: &OpenDocument, id: PaneId) -> Vec<slint::Rgb8Pixel> {
    let source = document.text.borrow().clone();
    refresh_pane_from_state(
        &harness.window,
        &harness.live.cache,
        document,
        id,
        &harness.live.states.of(id),
        &source,
    );
    harness.window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); WIDTH * HEIGHT];
    harness.surface.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, WIDTH);
    });
    if let Ok(output) = std::env::var("EDITOR_SETTINGS_SNAPSHOT") {
        let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
        for pixel in &pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        let name = if id.vertical(&harness.window) {
            "vertical"
        } else {
            "horizontal"
        };
        std::fs::write(
            PathBuf::from(output).join(format!("spelling-{name}.ppm")),
            ppm,
        )
        .unwrap();
    }
    pixels
}

fn red_count(pixels: &[slint::Rgb8Pixel]) -> usize {
    pixels.iter().filter(|pixel| spelling_red(pixel)).count()
}

/// キャレットを`byte`へ置いて描き、その字の上を右クリックしたことにする。
fn right_click_at(harness: &Harness, document: &OpenDocument, id: PaneId, byte: usize) {
    {
        let state = harness.live.states.of(id);
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(byte);
        state.selection_anchor_source_byte = Some(byte);
        state.active_line_start = Some(0);
    }
    render(harness, document, id);
    let screen = id.screen(&harness.window);
    let x = id.flow_x(&harness.window, screen.caret_x + 2.0);
    let y = screen.caret_y + 6.0;
    spelling_ui::at_pointer(&harness.window, &harness.live, id, x, y);
}

fn status(window: &AppWindow) -> String {
    window.get_spelling_status().to_string()
}

#[test]
fn spelling_is_checked_marked_and_fixed_from_the_right_click() {
    let root = scratch_directory("spelling");
    let text = "I recieve teh the the letter.\n\n```\nteh in code\n```\n";
    let (harness, document) = Harness::new(|weak| open_under(&root, "draft.md", text, weak));
    let window = &harness.window;
    let live = &harness.live;
    let id = PaneId::from_index(0);
    id.update_screen(window, |screen| {
        screen.width = 950.0;
        screen.height = 600.0;
        screen.shown_width = 950.0;
        screen.shown_height = 560.0;
        screen.zoom = 100;
    });
    // 左のパネルは閉じる。縦書きの行は右端から始まり、開いていると画面の外へ出る。
    window.set_tree_open(false);
    window.show().unwrap();
    if !spelling_ui::available() {
        eprintln!("en-US spell checker not installed; skipped");
        return;
    }

    // 頼むまでは何も調べない。
    assert_eq!(red_count(&render(&harness, &document, id)), 0);
    assert_eq!(status(window), "");

    // Run → Check Spelling。recieve・teh・繰り返しのthe の3つ。コードの行は読まない。
    spelling_ui::toggle(window, live, id);
    assert_eq!(status(window), say!("綴りの誤り {}", "Spelling {}", 3));
    let horizontal = red_count(&render(&harness, &document, id));
    assert!(horizontal > 30, "red wave pixels: {horizontal}");

    // 縦書きでも描く。
    set_pane_direction(window, &live.cache, id, true);
    let vertical = red_count(&render(&harness, &document, id));
    assert!(vertical > 30, "vertical red wave pixels: {vertical}");
    set_pane_direction(window, &live.cache, id, false);

    // 右クリックの先頭に候補。選べば置き換わり、1回の取り消しで戻る。
    right_click_at(&harness, &document, id, text.find("recieve").unwrap() + 2);
    assert_eq!(window.get_spell_word(), "recieve");
    assert!(!window.get_spell_repeated());
    let suggestions = window.get_spell_suggestions();
    assert_eq!(
        suggestions.row_data(0).map(|s| s.to_string()).as_deref(),
        Some("receive")
    );
    spelling_ui::chosen(window, live, 0);
    assert!(document.text.borrow().starts_with("I receive teh the the"));
    undo_in_pane(window, id, &document, &live.states, &live.cache, false);
    assert!(document.text.borrow().starts_with("I recieve teh"));
    undo_in_pane(window, id, &document, &live.states, &live.cache, true);
    spelling_ui::recount_all(window, live);
    assert_eq!(status(window), say!("綴りの誤り {}", "Spelling {}", 2));

    // 繰り返しの語は「Delete Repeated Word」で、前の空白ごと消える。
    let second_the = text.find("the the").unwrap() + "the t".len();
    right_click_at(&harness, &document, id, second_the);
    assert!(window.get_spell_repeated());
    spelling_ui::chosen(window, live, -1);
    assert!(
        document
            .text
            .borrow()
            .starts_with("I receive teh the letter.")
    );
    spelling_ui::recount_all(window, live);
    assert_eq!(status(window), say!("綴りの誤り {}", "Spelling {}", 1));

    // Ignoreした語には、この文書の中ではもう印が付かない。
    right_click_at(
        &harness,
        &document,
        id,
        document.text.borrow().find("teh").unwrap() + 1,
    );
    assert_eq!(window.get_spell_word(), "teh");
    spelling_ui::chosen(window, live, -2);
    assert_eq!(status(window), say!("綴りの誤り {}", "Spelling {}", 0));
    assert_eq!(red_count(&render(&harness, &document, id)), 0);

    // 印の無い語を右クリックしても行は出ない。
    right_click_at(&harness, &document, id, 0);
    assert_eq!(window.get_spell_word(), "");

    // 終えれば状態ごと消える。
    spelling_ui::toggle(window, live, id);
    assert!(document.spelling.borrow().is_none());
    assert_eq!(status(window), "");

    // Viewerでは始めない。
    id.update_screen(window, |screen| screen.viewer = true);
    spelling_ui::toggle(window, live, id);
    assert!(document.spelling.borrow().is_none());
}
