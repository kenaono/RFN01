//! Shared pointer and caret decisions. Hosts handle drawing and notifications.
use super::*;

pub(crate) fn move_caret(
    engine: &mut TextEngine,
    shown: PaneText<'_>,
    caret: usize,
    direction: i32,
    preferred_line: Option<f32>,
    vertical: bool,
) -> windows::core::Result<(usize, Option<f32>)> {
    let at = shown.utf16_at_source_byte(caret) as u32;
    match direction {
        -1 => Ok((shown.previous_grapheme(caret), None)),
        1 => Ok((shown.next_grapheme(caret), None)),
        // 下の`hidden_indent`で跨ぐ。ここは1歩ぶんの答えだけを出す。
        // 要件 11.4's `Alt+B` and `Alt+F`. **Asked of the text the pane
        // laid out**, like the grapheme steps beside them (技術検証 3.12):
        // a word is a run of the characters the writer can see, and in the
        // preview the markers are not among them.
        -3 | 3 => {
            let text = shown.text();
            let at = shown.shown_byte_at_utf16(at as usize);
            let moved = if direction < 0 {
                document::previous_word_boundary(text, at)
            } else {
                document::next_word_boundary(text, at)
            };
            let landed = shown.source_byte_at_utf16(utf16_at_byte(text, moved));
            Ok((landed, None))
        }
        -2 | 2 => {
            let anchor = match preferred_line {
                Some(anchor) => Ok(anchor),
                None => {
                    let geometry = engine.caret_geometry(at);
                    geometry.map(|caret| if vertical { caret.y } else { caret.x })
                }
            };
            anchor.and_then(|anchor| {
                engine
                    .move_caret_by_line(at, direction, Some(anchor))
                    .map(|hit| {
                        let position = hit.utf16_position as usize;
                        (shown.source_byte_at_utf16(position), Some(anchor))
                    })
            })
        }
        _ => Ok((caret, None)),
    }
}

pub(crate) fn select_range(
    state: &Rc<RefCell<EditorState>>,
    source: &str,
    start: usize,
    end: usize,
) {
    {
        let mut state = state.borrow_mut();
        state.selection_anchor_source_byte = Some(start);
        state.caret_source_byte = Some(end);
        state.active_line_start = Some(source_line_start(source, end));
        state.preferred_line = None;
        state.preedit.clear();
        state.rectangular = false;
        state.search_selection = None;
    }
}

pub(crate) enum PointerResult {
    NoChange,
    Range(usize, usize),
    Caret {
        hit: usize,
        next_active_line_start: usize,
        selection: PaneSelection,
    },
}

pub(crate) fn pointer(
    state: &Rc<RefCell<EditorState>>,
    source: &str,
    hit: PaneHit,
    x: f32,
    y: f32,
    phase: SelectionPhase,
    vertical: bool,
) -> PointerResult {
    // E3: **行番号を押したら、その論理行が選ばれる。**番号は本文ではないので、
    // そこへカーソルを置いても書き手の言ったことにならない——押した先が行その
    // ものであるほうが、次にすること（動かす・複製する・消す）に繋がる。
    // **押した瞬間だけ**：そのまま引けば、行の頭から普通の選択が伸びる。
    let from_numbers = match phase {
        SelectionPhase::Begin => hit.in_numbers.then_some(hit.byte),
        // 引いているあいだは、始まりが番号だったかどうかで決まる——途中で
        // ポインタが本文へ入っても、選んでいるのは行のままである。
        _ => state.borrow().line_drag,
    };
    if from_numbers.is_none() && phase == SelectionPhase::Begin {
        // **本文で押し直したら、行の選択は終わり。**離した合図（`End`）は
        // 窓の外へポインタが出ると来ないことがあるので、次に押した回でも畳む。
        state.borrow_mut().line_drag = None;
    }
    // E3: ダブルクリックが選んだ語（書き手の報告 2026-09-10）。
    let chosen_word = match phase {
        SelectionPhase::Begin => None,
        _ => state.borrow().word_drag,
    };
    if let Some((first_start, first_end)) = chosen_word {
        if phase == SelectionPhase::Update {
            // **押したまま動かせば、語ごと伸びる。**押した語は必ず入る。
            let (start, end) = document::word_around(&source, hit.letter);
            let (start, end) = (first_start.min(start), first_end.max(end));
            return PointerResult::Range(start, end);
        }
        // **離した合図では、何もしない**（書き手の報告 2026-09-10：「white catは
        // 2語です」「不安定に感じました」）。ここでカーソルを押した点へ置くと、
        // アンカーは語の頭のままなので語の途中までの選択になり、離した点が隣の語に
        // 寄っていれば2語ぶんに広がる——**選んだ語は、選んだそのままでよい。**
        let mut state = state.borrow_mut();
        state.word_drag = None;
        state.mark = false;
        return PointerResult::NoChange;
    }
    // 2回目の押下は語を選ぶ（E3）。**数えるのはここ**——窓の`double-clicked`は
    // 離した合図の前後どちらで来るか決まっておらず、3回目・4回目にも来る
    // （それが「契機がわからないのですが、選択がはずれなくなります」であった）。
    // ここで数えれば、2回目で区切って3回目は普通の押下に戻せる。
    if phase == SelectionPhase::Begin {
        let doubled = {
            let mut state = state.borrow_mut();
            state.word_drag = None;
            state.double_click(x, y)
        };
        if doubled && !hit.in_numbers {
            let (start, end) = document::word_around(&source, hit.letter);
            state.borrow_mut().word_drag = Some((start, end));

            return PointerResult::Range(start, end);
        }
    }
    if let Some(anchor) = from_numbers {
        let (first, _) = document::line_span(&source, anchor);
        let (start, end) = document::line_span(&source, hit.byte);
        // 上へ引けば上の行まで、下へ引けば下の行まで。**始めた行は必ず入る。**
        let (start, end) = (first.min(start), first.max(end));
        {
            let mut state = state.borrow_mut();
            state.line_drag = (phase != SelectionPhase::End).then_some(anchor);
        }

        return PointerResult::Range(start, end);
    }
    let hit = hit.byte;

    let next_active_line_start = source_line_start(&source, hit);

    let selection = {
        let mut state = state.borrow_mut();
        // **A standing mark answers the mouse too** (書き手の報告 2026-09-07).
        // 要件 11.4's mark is a Shift nobody is holding, and a Shift that only
        // the arrow keys could extend was half a key: the writer who asked for
        // a selection and then pointed at where it should end had said the
        // whole of it. So the press keeps the anchor the mark dropped — the
        // rectangle with it (要件 7.1) — and the release puts the mark down,
        // which is what makes the *next* click plain again and so the way to
        // let a selection go.
        let held = state.mark && phase != SelectionPhase::Extend;
        if !held && (phase == SelectionPhase::Begin || state.selection_anchor_source_byte.is_none())
        {
            state.selection_anchor_source_byte = Some(hit);
            state.rectangular = false;
        }
        if phase == SelectionPhase::End {
            state.mark = false;
        }
        state.caret_source_byte = Some(hit);
        // Only the vertical pane keeps the revealed line: the horizontal one
        // derives it from its caret on every lookup, so writing it there would
        // be storing an answer that is recomputed anyway.
        //
        // **縦書きは離したときに開く**（書き手の報告 2026-09-16：「縦書きだと、2行目を選択する
        // ことが難しかった」）。縦書きは文書の終わり側から積むので、押した行が開いて広がると
        // その行が右へずれる——画像の行なら絵の幅ぶん——、離した点は隣の行に落ち、カーソルが
        // 行を出てまた閉じていた。押したあいだは押したときの組みのまま答える。
        if vertical && phase == SelectionPhase::End {
            state.active_line_start = Some(next_active_line_start);
        }
        state.preedit.clear();
        state.preferred_line = None;
        pane_selection(&state)
    };
    PointerResult::Caret {
        hit,
        next_active_line_start,
        selection,
    }
}
