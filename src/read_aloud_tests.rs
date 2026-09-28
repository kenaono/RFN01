//! RFN01-62: 読み上げを実際の窓（画面外）で確かめる。声はWindowsのものを使うが、
//! 試験では消音にしてある。
use super::*;
use crate::saving::{Harness, open_under, scratch_directory};

fn caret(live: &Live, id: PaneId) -> Option<usize> {
    live.states.of(id).borrow().caret_source_byte
}

#[test]
fn reading_aloud_goes_paragraph_by_paragraph_and_stops_on_an_edit() {
    let root = scratch_directory("read-aloud");
    let text = "前の段落。\n｜漢字《かんじ》を読む。\n\n次の段落。\n最後の段落。\n";
    let (harness, document) = Harness::new(|weak| open_under(&root, "draft.md", text, weak));
    let window = &harness.window;
    let live = &harness.live;
    let id = PaneId::from_index(0);
    id.update_screen(window, |screen| {
        screen.width = 950.0;
        screen.height = 600.0;
        screen.shown_width = 950.0;
        screen.shown_height = 560.0;
    });
    window.show().unwrap();
    read_aloud::install(window, live);
    read_aloud::publish_voices(window);
    if !window.get_speech_available() {
        eprintln!("no Japanese voice installed; skipped");
        return;
    }
    // 声の一覧が出て、既定の声が選ばれている。
    assert!(window.get_speech_voice() >= 0);

    // キャレットは2つめの段落の中。そこから読み、前の段落は読まない。
    let second = text.find("｜漢字").unwrap();
    {
        let state = live.states.of(id);
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(second);
        state.selection_anchor_source_byte = Some(second);
    }
    read_aloud::toggle(window, live, id);
    assert!(read_aloud::reading());
    assert_eq!(read_aloud::reading_at(), Some(0));
    let lit = read_aloud::mark_in(id, &document).unwrap();
    assert_eq!(&text[lit.0..lit.1], "｜漢字《かんじ》を読む。");
    assert_eq!(caret(live, id), Some(second));

    // 鳴り終わると次の段落へ。空行は飛ばし、キャレットが段落の頭へ付いて行く。
    read_aloud::finish_paragraph_for_test();
    let lit = read_aloud::mark_in(id, &document).unwrap();
    assert_eq!(&text[lit.0..lit.1], "次の段落。");
    assert_eq!(caret(live, id), Some(text.find("次の段落").unwrap()));

    // 書き換えたら止まる（書き換えが済んでから）。
    document.text.borrow_mut().push('x');
    slint::platform::update_timers_and_animations();
    assert!(!read_aloud::reading());
    assert!(read_aloud::mark_in(id, &document).is_none());

    // 書き足した字を戻す（戻すのも書き換えなので、読んでいなくても何も起きない）。
    document.text.borrow_mut().pop();
    slint::platform::update_timers_and_animations();

    // 最後まで読むと止まる。
    {
        let state = live.states.of(id);
        let mut state = state.borrow_mut();
        let last = text.find("最後の段落").unwrap();
        state.caret_source_byte = Some(last);
        state.selection_anchor_source_byte = Some(last);
    }
    read_aloud::toggle(window, live, id);
    assert!(read_aloud::reading());
    read_aloud::finish_paragraph_for_test();
    assert!(!read_aloud::reading());

    // 「読み上げを停止」で止まる。
    read_aloud::toggle(window, live, id);
    assert!(read_aloud::reading());
    read_aloud::toggle(window, live, id);
    assert!(!read_aloud::reading());
}

#[test]
fn speed_snaps_to_the_slider_and_old_steps_carry_over() {
    assert_eq!(read_aloud::speed_of(1.26), 1.25);
    assert_eq!(read_aloud::speed_of(0.1), 0.5);
    assert_eq!(read_aloud::speed_of(9.0), 2.0);
    assert_eq!(read_aloud::speed_of(1.0), 1.0);
    // 以前の5段の番号は、同じ速さとして読む。
    assert_eq!(read_aloud::speed_from_old_rate("3"), 1.5);
    assert_eq!(read_aloud::speed_from_old_rate("x"), 1.0);
}

#[test]
fn the_sample_plays_and_stops_and_gives_way_to_reading() {
    let root = scratch_directory("read-aloud-sample");
    let (harness, _document) =
        Harness::new(|weak| open_under(&root, "draft.md", "本文の段落。\n", weak));
    let window = &harness.window;
    let live = &harness.live;
    let id = PaneId::from_index(0);
    read_aloud::install(window, live);
    read_aloud::publish_voices(window);
    if !window.get_speech_available() {
        eprintln!("no Japanese voice installed; skipped");
        return;
    }
    window.set_speech_speed(1.5);

    // 押すと鳴り、釦は「■ Stop」になる。もう一度押すと止まる。
    read_aloud::sample(window, live);
    assert!(read_aloud::sampling());
    assert!(window.get_speech_sampling());
    read_aloud::sample(window, live);
    assert!(!read_aloud::sampling());
    assert!(!window.get_speech_sampling());

    // 文書を読み上げるときは、Sampleを止めてから。
    read_aloud::sample(window, live);
    read_aloud::toggle(window, live, id);
    assert!(!read_aloud::sampling());
    assert!(read_aloud::reading());
    // 読み上げ中にSampleを押すと、読み上げを止めて鳴らす。
    read_aloud::sample(window, live);
    assert!(!read_aloud::reading());
    assert!(read_aloud::sampling());
    read_aloud::stop_all(window, live);
    assert!(!read_aloud::sampling());
}
