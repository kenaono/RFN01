//! RFN01-58: Alt+ドラッグで矩形選択。
use super::*;
use crate::table_ui_tests::{Rig, rig};
use slint::platform::{Key, PointerEventButton, WindowEvent};

/// 窓の押す・引く・離すを、アプリと同じ段階でペインの選択へ渡す。**Altの旗はSlintが
/// 届けたものをそのまま使う**——試験の窓ではWindowsのキーは押されていないので、
/// `alt_held`との突き合わせ（`main`の配線）は通さない。押した点の窓の座標とペインの
/// 座標の差も返す。
fn wire(r: &Rig) -> Rc<Cell<Option<(f32, f32)>>> {
    let pressed = Rc::new(Cell::new(None));
    let seen = pressed.clone();
    let weak = r.window.as_weak();
    let live = r.live.clone();
    r.window
        .on_pane_selection_start(move |pane, x, y, extend, rectangle| {
            seen.set(Some((x, y)));
            let window = weak.upgrade().unwrap();
            let id = PaneId::from_index(pane);
            let phase = press_phase(rectangle, extend);
            let (document, state) = (live.states.document(id), live.states.of(id));
            let x = id.flow_x(&window, x);
            update_pane_selection(&window, &document, &state, &live.cache, id, x, y, phase);
        });
    for end in [false, true] {
        let weak = r.window.as_weak();
        let live = r.live.clone();
        let handler = move |pane: i32, x: f32, y: f32| {
            let window = weak.upgrade().unwrap();
            let id = PaneId::from_index(pane);
            let phase = if end {
                SelectionPhase::End
            } else {
                SelectionPhase::Update
            };
            let (document, state) = (live.states.document(id), live.states.of(id));
            let x = id.flow_x(&window, x);
            update_pane_selection(&window, &document, &state, &live.cache, id, x, y, phase);
        };
        if end {
            r.window.on_pane_selection_end(handler);
        } else {
            r.window.on_pane_selection_update(handler);
        }
    }
    pressed
}

/// ペインの座標で、キャレットが`byte`に立つ点（行を横切る向きの真ん中）。
fn point_at(r: &Rig, byte: usize) -> (f32, f32) {
    let source = r.source();
    let mut hits = Vec::new();
    for row in 0..120 {
        let y = row as f32 * 3.0;
        for column in 0..300 {
            let x = column as f32 * 3.0;
            let mut borrowed = r.live.cache.borrow_mut();
            let hit = hit_test_pane(
                &r.window,
                &mut borrowed,
                &r.document,
                r.id,
                &source,
                None,
                r.id.flow_x(&r.window, x),
                y,
            );
            if hit.is_some_and(|hit| hit.byte == byte && hit.is_inside) {
                hits.push((x, y));
            }
        }
    }
    assert!(!hits.is_empty(), "no point lands on {byte}");
    hits[hits.len() / 2]
}

fn drag(r: &Rig, from: (f32, f32), to: (f32, f32), alt: bool) {
    let at = |(x, y): (f32, f32)| slint::LogicalPosition::new(x, y);
    let window = r.window.window();
    if alt {
        window.dispatch_event(WindowEvent::KeyPressed {
            text: Key::Alt.into(),
        });
    }
    window.dispatch_event(WindowEvent::PointerMoved { position: at(from) });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: at(from),
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerMoved { position: at(to) });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: at(to),
        button: PointerEventButton::Left,
    });
    if alt {
        window.dispatch_event(WindowEvent::KeyReleased {
            text: Key::Alt.into(),
        });
    }
}

/// **Altを押して引けば矩形、押さずに引けば今までどおりの選択。**横書き・縦書きの
/// どちらでも、囲んだ行の同じ桁だけが選ばれる（要件 7.1）。
#[test]
fn alt_drag_selects_a_rectangle() {
    let line = "あいうえおかきくけこ";
    let text = format!("{line}\n{line}\n{line}\n{line}\n");
    let r = rig(&text, (1000, 600));
    let pressed = wire(&r);
    let char_at = |row: usize, column: usize| {
        (line.len() + 1) * row + line.char_indices().nth(column).unwrap().0
    };

    for vertical in [false, true] {
        set_pane_direction(&r.window, &r.live.cache, r.id, vertical);
        r.put(0);
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1000 * 600];
        r.window.window().request_redraw();
        r.surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1000);
        });
        // 窓の座標とペインの座標の差を、1回押して測る。縦書きの短い文書は右に寄るので、
        // 紙に当たる所を右から探す。
        let (probe, (seen_x, seen_y)) = [(900.0, 200.0), (600.0, 200.0), (200.0, 200.0)]
            .into_iter()
            .find_map(|probe| {
                pressed.set(None);
                drag(&r, probe, probe, false);
                pressed.get().map(|seen| (probe, seen))
            })
            .expect("the pane saw the press");
        let window_point = |(x, y): (f32, f32)| (x + probe.0 - seen_x, y + probe.1 - seen_y);
        r.put(0);

        let from = window_point(point_at(&r, char_at(0, 1)));
        let to = window_point(point_at(&r, char_at(2, 4)));

        drag(&r, from, to, true);
        let state = r.live.states.of(r.id);
        assert!(
            state.borrow().rectangular,
            "vertical={vertical}: a rectangle"
        );
        let runs = selected_runs(&r.live.cache, r.id);
        assert_eq!(
            selected_text(&r.source(), &runs),
            "いうえ\nいうえ\nいうえ",
            "vertical={vertical}: {runs:?}"
        );

        // Altを押さずに引けば、字の並びの選択に戻る。
        drag(&r, from, to, false);
        assert!(!state.borrow().rectangular, "vertical={vertical}: a run");
        let runs = selected_runs(&r.live.cache, r.id);
        assert_eq!(
            runs,
            vec![(char_at(0, 1), char_at(2, 4))],
            "vertical={vertical}"
        );
    }
}
