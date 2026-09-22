//! Editing state shared by all editor surfaces. No window or TAB ownership.
use crate::input_platform::double_click_time;
use std::{borrow::Cow, time::Instant};
pub(crate) const READ_ONLY_END_SLACK: f32 = 2.0;

/// One pane's caret, selection and pending IME text, all in source bytes.
///
/// Both panes keep one of these. Everything here is about the document, not
/// about how a pane draws it, so the same struct serves either writing
/// direction: `preferred_line` is the coordinate on the *line* axis to hold on
/// to when stepping between lines, which is a y in the vertical pane and an x in
/// the horizontal one. `active_line_start` is only read back by the vertical
/// pane: it decides which line shows its Markdown and lags the caret on
/// purpose, while the horizontal pane works the same answer out from its own
/// caret (the host resolves the currently revealed line).
#[derive(Clone, Debug, Default)]
pub(crate) struct EditorState {
    pub(crate) viewer: bool,
    /// 追加要件 2026-09-15（書き手）: **ReadOnlyで最下行を追っているか。**
    ///
    /// ReadOnly（Viewerをソース表示で開いた形）だけが読む。入った時点で立ち、
    /// 書き手が上へスクロールすると下り、最下行まで戻すとまた立つ——ログを
    /// 読む人が`tail -f`でしていることを、スクロールだけで言えるようにする。
    /// **TABが持つ**のは、同じ文書を別のTABで止めて読めるように。
    pub(crate) follow: bool,
    pub(crate) caret_source_byte: Option<usize>,
    pub(crate) selection_anchor_source_byte: Option<usize>,
    pub(crate) active_line_start: Option<usize>,
    pub(crate) preedit: String,
    pub(crate) preferred_line: Option<f32>,
    /// Whether the selection is a rectangle rather than a run (要件 7.1).
    ///
    /// **The two ends are the same two bytes either way** — what changes is
    /// what is read out of them: a rectangle takes the lines they sit on and
    /// the columns they sit at, and covers every line between (`selection_ranges`).
    pub(crate) rectangular: bool,
    /// Whether `Ctrl+Space` has been pressed and not yet answered (要件 11.4).
    ///
    /// **A held Shift that the writer does not have to hold.** While it is on,
    /// every move extends the selection the way Shift does; it goes off when
    /// the writer does anything but move — an edit, or a click — and when
    /// `Ctrl+Space` is pressed again.
    pub(crate) mark: bool,
    /// 検索が置いた選択（E1、書き手の指摘 2026-09-09）。
    ///
    /// **「その範囲は書き手が選んだものか」を答えるためだけにある。**範囲内検索
    /// （`[ ]`）は書き手が選んだ範囲を覚えているが、選び直したら新しい範囲に
    /// なってほしい——ところが検索そのものも選択を動かす（一致を選ぶ）ので、
    /// 「選択が変わったら取り直す」では**2回目の検索で範囲が一致そのものに
    /// 潰れる**。ここに置いた最後の一致と今の選択を比べれば、書き手の手が
    /// 入ったかどうかが分かる。
    pub(crate) search_selection: Option<(usize, usize)>,
    /// 行番号から始まった選択（E3）。**押した行の頭のバイト。**
    ///
    /// **これがあるあいだ、引くと行ごと選ばれる。**番号を押すことは行を指す
    /// ことなので、そのまま引いた書き手が指しているのも行である——1画素の
    /// ぶれで行の選択が字の選択へ変わってしまうと、押しただけのつもりが
    /// 選び直しになる。ボタンを離すと消える。
    pub(crate) line_drag: Option<usize>,
    /// ダブルクリックが選んだ語（E3、書き手の報告 2026-09-10）。
    ///
    /// **2回目を離した合図から、選んだ語を守る。**離した合図はカーソルを押した
    /// 点へ置くので、そのままでは語の途中までしか残らない——「不安定に感じました」
    /// 「英語では単語選択にならない感じ」の半分はこれである。**引けば語ごと
    /// 伸びる**のも同じ印で、押し直すまで残る。
    pub(crate) word_drag: Option<(usize, usize)>,
    /// 直前の押下——いつ、どこを（E3）。**2回目かどうかを数えるためだけにある。**
    ///
    /// 2回目を数えたら空に戻す：**3回目は普通の押下**である。窓の
    /// `double-clicked`に任せていたときは3回目・4回目にも来ていて、
    /// 押すたびに語が選び直されるので選択が外れなくなった（書き手の報告
    /// 2026-09-10：「契機がわからないのですが、選択がはずれなくなります」）。
    pub(crate) last_click: Option<(Instant, f32, f32)>,
}

impl EditorState {
    /// Shared ReadOnly policy; renderers only adapt selection and scroll coordinates.
    pub(crate) fn set_read_only(&mut self, viewer: bool, source: bool) {
        self.viewer = viewer;
        self.follow = viewer && source;
        self.preedit.clear();
    }

    pub(crate) fn follow_at(&mut self, position: f32, end: f32, resized: bool) -> bool {
        if !self.viewer {
            return false;
        }
        if !(self.follow && resized) {
            self.follow = position >= end.max(0.0) - READ_ONLY_END_SLACK;
        }
        self.follow
    }
    /// この押下は「2回目」か——ダブルクリックの判定（E3）。
    ///
    /// **速さはWindowsのもの**（`GetDoubleClickTime`）。この編集器が独自の秒数を
    /// 持てば、書き手が他のアプリで慣れた速さと違う反応をすることになる。
    ///
    /// **場所も見る。**離れたところを2回押したのは、同じものを2回押したのではない。
    pub(crate) fn double_click(&mut self, x: f32, y: f32) -> bool {
        let now = Instant::now();
        let doubled = self.last_click.is_some_and(|(when, at_x, at_y)| {
            now.duration_since(when) <= double_click_time()
                && (x - at_x).abs() <= DOUBLE_CLICK_SLACK
                && (y - at_y).abs() <= DOUBLE_CLICK_SLACK
        });
        // 2回目で区切る。3回目は、次の1回目である。
        self.last_click = (!doubled).then_some((now, x, y));
        doubled
    }
}

/// 2回目とみなす、押した場所のずれ（画素、E3）。**手は完全には止まらない。**
const DOUBLE_CLICK_SLACK: f32 = 4.0;

pub(crate) fn selection_source_range(state: &EditorState) -> Option<(usize, usize)> {
    let anchor = state.selection_anchor_source_byte?;
    let focus = state.caret_source_byte?;
    (anchor != focus).then_some((anchor.min(focus), anchor.max(focus)))
}

/// 書き手が選んだ範囲。**選んでいなければキャレット1つ**（頭と尻が同じ所）で、
/// 本文の長さ`len`を越えない。挿入・行の操作・メニューの有効条件が同じ答えを読む。
pub(crate) fn chosen_source_range(state: &EditorState, len: usize) -> (usize, usize) {
    selection_source_range(state).unwrap_or_else(|| {
        let caret = state.caret_source_byte.unwrap_or(0).min(len);
        (caret, caret)
    })
}

pub(crate) fn update_selection_after_move(
    state: &mut EditorState,
    source_byte: usize,
    next_source_byte: usize,
    extend_selection: bool,
) -> Option<(usize, usize)> {
    // **The mark is a Shift nobody is holding** (要件 11.4), so it is read in
    // the one place a move decides what the selection becomes.
    if extend_selection || state.mark {
        if state.selection_anchor_source_byte.is_none() {
            state.selection_anchor_source_byte = Some(source_byte);
        }
    } else {
        state.selection_anchor_source_byte = Some(next_source_byte);
    }
    state.caret_source_byte = Some(next_source_byte);
    selection_source_range(state)
}

pub(crate) fn release_selection(state: &mut EditorState) -> bool {
    let selecting = state.mark
        || state.line_drag.is_some()
        || state.word_drag.is_some()
        || !state.preedit.is_empty()
        || selection_source_range(state).is_some();
    if !selecting {
        return false;
    }
    state.mark = false;
    state.rectangular = false;
    state.line_drag = None;
    state.word_drag = None;
    // 選択はカーソルのところへ畳む。**本文は動かさない**——打った字が選択を
    // 置き換えるのは`insert_pane_text`のほうで、ここは何も消さない。
    state.selection_anchor_source_byte = state.caret_source_byte;
    state.preedit.clear();
    true
}

pub(crate) fn source_line_start(source: &str, source_byte: usize) -> usize {
    let source_byte = source_byte.min(source.len());
    source[..source_byte]
        .rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(0)
}

pub(crate) fn normalize_typed_input(input: &str) -> String {
    // A Windows clipboard hands over CRLF, and mapping each half of it to a line
    // break would double every one. Typed input never contains a carriage
    // return, so the common path allocates nothing.
    let input = if input.contains('\r') {
        Cow::Owned(input.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(input)
    };
    input
        .chars()
        .filter_map(|character| match character {
            '\r' => Some('\n'),
            '\n' | '\t' => Some(character),
            character
                if !character.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&character) =>
            {
                Some(character)
            }
            _ => None,
        })
        .collect()
}
