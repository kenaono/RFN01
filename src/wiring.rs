//! 窓のコールバックを繋ぐ。
//!
//! **ここには判断が無い。**どのコールバックがどの関数を呼ぶか、それだけが
//! 書いてある——中身は`saving.rs`や`session.rs`や`main.rs`の側にあり、この
//! ファイルはそこへの配線である。
//!
//! `main()`から出したのは長さのためだけではない。**繋ぎ方が一箇所に集まると、
//! 「このコールバックは`Live`の何を触るのか」が読める**ようになる——それが
//! 分かって初めて`Live`を細くでき、クレートへ分ける道が開く（技術検証 9.3）。
//!
//! 分け方は画面の区画ではなく**要件の区画**にした。1つの要件に手を入れるとき、
//! 開く場所が1つになる。
//!
//! **引数の名前は`main()`での名前のまま**にしてある（`pane_states`、
//! `render_cache`）。閉包の中で何度もクローンされる名前なので、ここで短い名へ
//! 付け替えると、内側と外側で同じ名前が別のものを指す形になる。

use crate::{LeftTab, PaneTab, double_click_time, duplicate_tab};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

type ExplorerClick = Option<(usize, PaneId, Instant, bool)>;

fn reuse_explorer_destination(new_tab: bool, was_open: bool, provisional: bool) -> bool {
    new_tab || (!was_open && provisional)
}

#[test]
fn an_already_open_provisional_tab_also_gets_another_view() {
    assert!(!reuse_explorer_destination(false, true, true));
    assert!(!reuse_explorer_destination(false, true, false));
    assert!(reuse_explorer_destination(false, false, true));
    assert!(reuse_explorer_destination(true, true, false));
}

fn take_explorer_double(click: &mut ExplorerClick, row: usize, pane: PaneId) -> Option<bool> {
    let same = click.as_ref().is_some_and(|(previous, owner, at, _)| {
        *previous == row && *owner == pane && at.elapsed() <= double_click_time()
    });
    if same {
        click.take().map(|(_, _, _, reuse)| reuse)
    } else {
        *click = None;
        None
    }
}

#[test]
fn explorer_double_survives_row_recreation_and_is_consumed_once() {
    let pane = PaneId::from_index(0);
    for reuse in [false, true] {
        let mut click = None;
        remember_explorer_destination(&mut click, 2, pane, reuse);
        // Only the controller survives when the first click replaces the rows.
        assert_eq!(take_explorer_double(&mut click, 2, pane), Some(reuse));
        assert_eq!(take_explorer_double(&mut click, 2, pane), None);
        remember_explorer_destination(&mut click, 2, pane, reuse);
        assert_eq!(take_explorer_double(&mut click, 3, pane), None);
        remember_explorer_destination(&mut click, 2, pane, reuse);
        click.as_mut().unwrap().2 = Instant::now() - double_click_time() - Duration::from_millis(1);
        assert_eq!(take_explorer_double(&mut click, 2, pane), None);
    }
}

fn remember_explorer_destination(click: &mut ExplorerClick, row: usize, pane: PaneId, reuse: bool) {
    let same = click.as_ref().is_some_and(|(previous, owner, at, _)| {
        *previous == row && *owner == pane && at.elapsed() <= double_click_time()
    });
    if !same {
        *click = Some((row, pane, Instant::now(), reuse));
    }
}

#[test]
fn explorer_second_click_preserves_the_first_destination() {
    let pane = PaneId::from_index(0);
    for reuse_first in [false, true] {
        let mut click = None;
        remember_explorer_destination(&mut click, 3, pane, reuse_first);
        remember_explorer_destination(&mut click, 3, pane, !reuse_first);
        assert_eq!(click.unwrap().3, reuse_first);
        remember_explorer_destination(&mut click, 4, pane, !reuse_first);
        assert_eq!(click.unwrap().3, !reuse_first);
        click.as_mut().unwrap().2 = Instant::now() - double_click_time() - Duration::from_millis(1);
        remember_explorer_destination(&mut click, 4, pane, reuse_first);
        assert_eq!(click.unwrap().3, reuse_first);
    }
}

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel};

use crate::directwrite_render;
use crate::document;
use crate::saving::{open_document, reveal_active_document, save_all, save_document};
use crate::word_marks;
use crate::{
    AppWindow, Live, NO_TARGET, PAPER_SLOT, PaneId, PaneStates, RenderCache, Setting, TreeCommand,
    activate_left_row, add_word_to_group, bullet_marks_of, choose_find_option, clear_find,
    close_word_naming, collect_search, colour_row, count_in_pane, drop_tree_row, edit_word_file,
    export_word_group, file_dialog, file_tree, find_in_pane, focused_pane, font_name, font_row,
    go_to_line, go_to_remembered_folder, hold_word_modes, ime, navigate, new_word_group,
    new_word_mode, next_word_colour, open_work_folder, pane_word_mode, paste_into_tab,
    paste_targets, pick_tree_row, publish_left, publish_tabs, publish_word_mode_of,
    publish_word_modes, quick_draft, read_word_source, remove_word_from_group, rename_word_group,
    rename_word_mode, replace_all_in_pane, replace_in_pane, reset_settings_group,
    restore_editor_focus, save_settings, schedule_relayout, search_in_folder, search_work_folder,
    selected_runs, set_picked_colour, set_word_mode_of, show_bullet_marks, shown_sheet,
    slint_colour, step_setting, tell_goto, toggle_goto, tree_command, walk_find_history,
    word_modes_now,
};

/// 追加要件 2026-09-08（要件 6.8）: 端末の見た目。
///
/// **紙の設定（要件 9）とは別の口。**端末を黒地で使う人が多く、原稿の紙と同じ値で
/// 決めるものではない。16色はここに無い——背景の明るさからRustが選ぶ。
pub fn wire_terminal_look(
    window: &AppWindow,
    live: &Live,
    render_cache: &Rc<RefCell<RenderCache>>,
) {
    // **黒地と紙を、ひとまとめで置く。**背景だけ黒くして文字が黒のままの画面は
    // 読めない——その状態を通らせない。
    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_terminal_theme_chosen(move |dark| {
        if let Some(window) = weak.upgrade() {
            let (paper, ink) = if dark {
                ([0.09, 0.09, 0.10], [0.88, 0.87, 0.85])
            } else {
                (
                    crate::text_blocks::DEFAULT_PAPER,
                    crate::text_blocks::DEFAULT_INK,
                )
            };
            window.set_terminal_paper(slint_colour(paper));
            window.set_terminal_ink(slint_colour(ink));
            cache
                .borrow_mut()
                .log_diag("spec", &format!("terminal theme dark={}", u8::from(dark)));
            after_terminal_look(&window, &cache);
        }
    });

    let weak = window.as_weak();
    window.on_terminal_font_picked(move || {
        if let Some(window) = weak.upgrade() {
            fill_font_names(&window);
            // **どちらの欄のために開いたかを旗で言う。**一覧は要件 9 のものと
            // 同じ`font-menu`で、選ばれた家名が戻る先だけが違う。
            window.set_font_for_terminal(true);
            let standing = window.get_terminal_font();
            window.set_font_current(standing);
        }
    });

    // 書き手の求め 2026-09-15: Terminal の面の Reset。**この面の値だけ**——
    // 既定の接続先、紙の地、書体と大きさ。シェルの一覧は設定ファイルのもので、
    // 画面から足したものではないので触らない。
    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_terminal_reset(move || {
        if let Some(window) = weak.upgrade() {
            window.set_default_shell(0);
            window.set_terminal_font(crate::TERMINAL_FONT_DEFAULT.into());
            window.set_terminal_size(crate::TERMINAL_SIZE_DEFAULT);
            cache.borrow_mut().log_diag("spec", "terminal reset");
            // 地と字をひとまとめで戻し、書き出しと描き直しもそこで済む。
            window.invoke_terminal_theme_chosen(false);
        }
    });

    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_terminal_size_stepped(move |by| {
        if let Some(window) = weak.upgrade() {
            let (low, high) = crate::TERMINAL_SIZE_RANGE;
            let size = (window.get_terminal_size() + by).clamp(low, high);
            window.set_terminal_size(size);
            after_terminal_look(&window, &cache);
        }
    });

    let _ = live;
}

/// C5（書き手の求め 2026-09-15：「Windows標準は小さくわかりにくいので、MS-Officeの
/// 形式がいい」）: 色を選ぶ口。パレットで選ぶ、既定へ戻す、その他の色で混ぜる。
///
/// **どの色かは (kind, slot) の2つで足りる**——kind 0 は紙のシート（シートは
/// 押した欄が`sheet`へ置いてある）、1 は端末（0 地、1 字）、2 は語群。
/// 選んだ色は「最近使用した色」の頭へ入り、設定ファイルに残る。
pub fn wire_colours(
    window: &AppWindow,
    live: &Live,
    pane_states: &PaneStates,
    render_cache: &Rc<RefCell<RenderCache>>,
    spec_timer: Rc<Timer>,
    numbers: Rc<VecModel<i32>>,
    palette: Rc<VecModel<Color>>,
) {
    let doors = Rc::new(ColourDoors {
        live: live.clone(),
        states: pane_states.clone(),
        cache: render_cache.clone(),
        timer: spec_timer,
        numbers,
        palette,
        target: std::cell::Cell::new((0, 0, 0)),
    });

    let weak = window.as_weak();
    let held = doors.clone();
    window.on_colour_set(move |kind, slot, colour| {
        if let Some(window) = weak.upgrade() {
            held.apply(
                &window,
                kind,
                slot,
                shown_sheet(&window),
                Some(channels_of(colour)),
            );
        }
    });

    let weak = window.as_weak();
    let held = doors.clone();
    window.on_colour_default(move |kind, slot| {
        if let Some(window) = weak.upgrade() {
            held.apply(&window, kind, slot, shown_sheet(&window), None);
        }
    });

    let weak = window.as_weak();
    let held = doors.clone();
    window.on_colour_more(move |kind, slot| {
        if let Some(window) = weak.upgrade() {
            let sheet = shown_sheet(&window);
            held.target.set((kind, slot, sheet));
            let current = held.current(&window, kind, slot, sheet);
            window.set_colour_mixer_current(current);
            show_in_mixer(&window, current);
            window.set_colour_mixer_open(true);
        }
    });

    // 追加要件 2026-09-15: 文字色のセット。適用は色を置いて組み直す（組み直しが
    // 設定を書く）。保存と削除は色を変えないので、その場で書く。
    let weak = window.as_weak();
    let held = doors.clone();
    window.on_ink_set_apply(move |index| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let said = window.get_ink_sets().row_data(index.max(0) as usize);
        if said.is_some_and(|said| crate::apply_ink_set(&held.palette, &said)) {
            schedule_relayout(&window, &held.states, &held.cache, &held.timer);
        }
    });
    let weak = window.as_weak();
    let held = doors.clone();
    window.on_ink_set_save(move |index| {
        if let Some(window) = weak.upgrade() {
            let said = crate::ink_set_of(&*held.palette);
            window
                .get_ink_sets()
                .set_row_data(index.max(0) as usize, said.into());
            save_settings(&window, &held.cache);
        }
    });
    let weak = window.as_weak();
    let held = doors.clone();
    window.on_ink_set_delete(move |index| {
        if let Some(window) = weak.upgrade() {
            let sets = window.get_ink_sets();
            sets.set_row_data(index.max(0) as usize, SharedString::new());
            save_settings(&window, &held.cache);
        }
    });

    let weak = window.as_weak();
    let held = doors;
    window.on_colour_mixer_accepted(move |colour| {
        if let Some(window) = weak.upgrade() {
            let (kind, slot, sheet) = held.target.get();
            held.apply(&window, kind, slot, sheet, Some(channels_of(colour)));
        }
    });

    window.on_colour_hex_of(|colour| crate::hex_colour(colour).trim_start_matches('#').into());

    let weak = window.as_weak();
    window.on_colour_hex_typed(move |text| {
        if let Some(window) = weak.upgrade()
            && let Some(rgb) = crate::parse_hex_colour(&text)
        {
            show_in_mixer(&window, slint_colour(rgb));
        }
    });

    let weak = window.as_weak();
    window.on_colour_channel_typed(move |channel, text| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(typed) = crate::typed_number(&text) else {
            return;
        };
        let mixed = Color::from_hsva(
            window.get_colour_mixer_hue(),
            window.get_colour_mixer_saturation(),
            window.get_colour_mixer_value(),
            1.0,
        );
        let mut rgb = [mixed.red(), mixed.green(), mixed.blue()];
        rgb[channel.clamp(0, 2) as usize] = typed.round().clamp(0.0, 255.0) as u8;
        show_in_mixer(&window, Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]));
    });
}

/// What the colour doors need to reach every place a colour lives.
struct ColourDoors {
    live: Live,
    states: PaneStates,
    cache: Rc<RefCell<RenderCache>>,
    timer: Rc<Timer>,
    numbers: Rc<VecModel<i32>>,
    palette: Rc<VecModel<Color>>,
    /// Which colour the mixer was opened for: kind, slot, sheet.
    target: std::cell::Cell<(i32, i32, usize)>,
}

impl ColourDoors {
    /// Put a colour where (kind, slot) names, or its default when `rgb` is `None`.
    fn apply(&self, window: &AppWindow, kind: i32, slot: i32, sheet: usize, rgb: Option<[f32; 3]>) {
        let slot_at = slot.max(0) as usize;
        match kind {
            0 => {
                match rgb {
                    Some(rgb) => {
                        set_picked_colour(&self.numbers, &self.palette, sheet, slot_at, rgb)
                    }
                    None if (8..15).contains(&slot_at) => {
                        Setting::Decoration(slot_at - 8, 3).write(&self.numbers, sheet, 0)
                    }
                    None => crate::set_colour(
                        &self.palette,
                        sheet,
                        slot_at,
                        crate::default_colour(sheet, slot_at),
                    ),
                }
                schedule_relayout(window, &self.states, &self.cache, &self.timer);
            }
            1 => {
                let fallback = if slot == 0 {
                    crate::text_blocks::DEFAULT_PAPER
                } else {
                    crate::text_blocks::DEFAULT_INK
                };
                let colour = slint_colour(rgb.unwrap_or(fallback));
                if slot == 0 {
                    window.set_terminal_paper(colour);
                } else {
                    window.set_terminal_ink(colour);
                }
                after_terminal_look(window, &self.cache);
            }
            2 => {
                let Some(rgb) = rgb else {
                    return;
                };
                let mode = window.get_word_mode_opened_at().max(0) as usize;
                let mut modes = word_modes_now();
                let Some(group) = modes
                    .get_mut(mode)
                    .and_then(|held| held.groups.get_mut(slot_at))
                else {
                    return;
                };
                // 色を持たない語群（除外語群）から選び直すこともできる。そのときは
                // **色を選んだことが、色を付けると言ったことである。**
                group.colour = Some(rgb);
                hold_word_modes(window, &self.live, modes, true);
            }
            _ => return,
        }
        if let Some(rgb) = rgb {
            remember_colour(window, slint_colour(rgb));
            save_settings(window, &self.cache);
        }
    }

    /// The colour (kind, slot) has now, which the mixer shows as 現在.
    fn current(&self, window: &AppWindow, kind: i32, slot: i32, sheet: usize) -> Color {
        let slot_at = slot.max(0) as usize;
        match kind {
            0 => {
                // 背景が Paper に合わせてあるなら、今見えているのは紙の色。
                let shown = if (8..15).contains(&slot_at)
                    && Setting::Decoration(slot_at - 8, 3).read(window, sheet) == 0
                {
                    PAPER_SLOT
                } else {
                    slot_at
                };
                window
                    .get_palette()
                    .row_data(colour_row(sheet, shown))
                    .unwrap_or_default()
            }
            1 if slot == 0 => window.get_terminal_paper(),
            1 => window.get_terminal_ink(),
            _ => word_modes_now()
                .get(window.get_word_mode_opened_at().max(0) as usize)
                .and_then(|mode| mode.groups.get(slot_at))
                .and_then(|group| group.colour)
                .map(slint_colour)
                .unwrap_or(Color::from_rgb_u8(128, 128, 128)),
        }
    }
}

/// Channels as the settings hold them.
fn channels_of(colour: Color) -> [f32; 3] {
    [
        f32::from(colour.red()) / 255.0,
        f32::from(colour.green()) / 255.0,
        f32::from(colour.blue()) / 255.0,
    ]
}

/// Point the mixer's field and hue bar at a colour.
fn show_in_mixer(window: &AppWindow, colour: Color) {
    let hsva = colour.to_hsva();
    // 灰色には色相が無い。いま帯が指している色相を残す——打ち直すたびに
    // 帯が赤へ跳ねると、次に面を触ったときに色が変わる。
    if hsva.saturation > 0.0 && hsva.value > 0.0 {
        window.set_colour_mixer_hue(hsva.hue.rem_euclid(360.0));
    }
    window.set_colour_mixer_saturation(hsva.saturation);
    window.set_colour_mixer_value(hsva.value);
}

/// Put a colour at the head of 最近使用した色 (C5): newest first, no repeats,
/// ten at most — one row of the palette.
pub fn remember_colour(window: &AppWindow, colour: Color) {
    let colours = window.global::<crate::Colours>();
    let mut recent: Vec<Color> = colours.get_recent().iter().collect();
    recent.retain(|held| *held != colour);
    recent.insert(0, colour);
    recent.truncate(RECENT_COLOURS);
    colours.set_recent(ModelRc::new(VecModel::from(recent)));
}

/// How many recent colours the palette keeps: one row, like Office.
pub const RECENT_COLOURS: usize = 10;

/// システムの書体の一覧を、初めて必要になったときだけ読む。
///
/// **起動時には読まない**（要件 9）：数百の家名をシステムのコレクションから読むので、
/// 一度も開かない書き手を待たせる理由が無い。一度読んだら持っておく——一覧は編集器が
/// 走っているあいだ変わらず、書体を決めるときは続けて何度も開く。
fn fill_font_names(window: &AppWindow) {
    let names = window.get_font_names();
    if names.row_count() > 0 {
        return;
    }
    let read: Vec<SharedString> = directwrite_render::font_families()
        .into_iter()
        .map(SharedString::from)
        .collect();
    window.set_font_names(ModelRc::new(VecModel::from(read)));
}

/// 見た目が変わったので、書き直して覚える（追加要件 2026-09-08）。
///
/// **描き直しは帯の署名が起こす**（`refresh_terminal`が見た目を混ぜている）ので、
/// ここがすることは「もう一度描け」と言うことと、設定を書くことの2つだけ。
fn after_terminal_look(window: &AppWindow, cache: &Rc<RefCell<RenderCache>>) {
    save_settings(window, cache);
    window.invoke_terminal_woken();
}

/// 要件 7.9（2026-09-08）: 単語チェックモード——**書き手が自分で作る「言語モード」**。
///
/// **考え方はソースの予約語の色分けと同じ**（書き手の指摘）。モードが語群を持ち、
/// 語群が色を持つ。モードは文書ごとで、ステータスバーから切り替える。
///
/// **どの操作も`hold_word_modes`一本を通る**：表を置き換え、重複を数え直し、木を
/// 建て直し、画面へ出し、書き出す。順番を守る場所が1つで済む。
pub fn wire_word_modes(window: &AppWindow, live: &Live) {
    // **モードを1つ作る。**ファイルは要らない——辞書は編集器が持つもので、
    // 書き手がファイルを管理する必要は無い。
    //
    // **名前は設定画面の中の欄で受ける**（書き手の求め 2026-09-08、単語チェック
    // モード要件 7.4）。問いは窓の中に描くもの（要件 8.3）で、設定画面はその上に
    // 浮いている別の窓——**訊くたびに設定画面が消えて戻る**ことになり、「決定
    // すると次の画面」が続くと何をしているのか分からなくなる。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_mode_added(move |name| {
        if let Some(window) = weak.upgrade() {
            match new_word_mode(&window, &held, &name) {
                // **できたら欄を畳む。**次にすることは、この画面の中にある。
                Ok(()) => close_word_naming(&window),
                Err(told) => window.set_word_naming_trouble(told.into()),
            }
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_group_added(move |mode, name| {
        if let Some(window) = weak.upgrade() {
            match new_word_group(&window, &held, mode.max(0) as usize, &name) {
                Ok(()) => close_word_naming(&window),
                Err(told) => window.set_word_naming_trouble(told.into()),
            }
        }
    });

    // 書き手の報告 2026-09-08: **モードを選んでも何も出なかった。**
    // 語群も語も重複もRustが作って窓へ渡すモデルで、画面側で数字を変えても
    // **誰も作り直さない**——開く先が変わったら、ここで作り直す。
    let weak = window.as_weak();
    window.on_word_mode_opened(move |at| {
        if let Some(window) = weak.upgrade() {
            window.set_word_mode_opened_at(at);
            // 別のモードを開いたら、前のモードの語群を開いたままにしない。
            window.set_word_group_opened_at(-1);
            publish_word_modes(&window);
        }
    });

    let weak = window.as_weak();
    window.on_word_group_opened(move |at| {
        if let Some(window) = weak.upgrade() {
            window.set_word_group_opened_at(at);
            publish_word_modes(&window);
        }
    });

    // **長押しで名前を変える**（書き手の求め 2026-09-08、単語チェックモード要件
    // 7.4）。作るときと同じ欄が同じ場所に出るので、Rustから見れば同じ形の仕事で
    // ある——できたら畳み、駄目なら理由を欄の下へ返す。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_mode_renamed(move |at, name| {
        if let Some(window) = weak.upgrade() {
            match rename_word_mode(&window, &held, at.max(0) as usize, &name) {
                Ok(()) => close_word_naming(&window),
                Err(told) => window.set_word_naming_trouble(told.into()),
            }
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_group_renamed(move |at, name| {
        if let Some(window) = weak.upgrade() {
            let mode = window.get_word_mode_opened_at().max(0) as usize;
            match rename_word_group(&window, &held, mode, at.max(0) as usize, &name) {
                Ok(()) => close_word_naming(&window),
                Err(told) => window.set_word_naming_trouble(told.into()),
            }
        }
    });

    // 単語チェックモード要件 5.4: **辞書そのものをタブで開く。**
    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_file_edited(move || {
        if let Some(window) = weak.upgrade() {
            close_word_naming(&window);
            edit_word_file(&window, &held);
        }
    });

    // 要件 10: **ステータスバーから切り替える。**コードエディタが言語モードを
    // 出しているのと同じ場所で、**モードは文書ごと**に付く。
    // **どちらの口も同じところへ来る。**ステータスバーは「いま書いている文書」、
    // タブのメニューは「そのタブ」——指す先が違うだけで、することは同じである。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_pane_word_mode_chosen(move |pane, mode| {
        if let Some(window) = weak.upgrade() {
            set_word_mode_of(&window, &held, PaneId::from_index(pane), mode.max(0) as u32);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_mode_chosen(move |mode| {
        if let Some(window) = weak.upgrade() {
            set_word_mode_of(&window, &held, focused_pane(&window), mode.max(0) as u32);
        }
    });

    // 要件 E2 の②: **ステータスバーの文字コードから、開き直す。**
    // 相手は「いま鍵盤を持っているペインが見ている文書」——帯に出ている
    // 文字コードがその文書のものだからで、押した言葉と動く先が同じものを
    // 指している。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_reopen_encoding(move |id| {
        if let Some(window) = weak.upgrade() {
            if let Some(encoding) = crate::encoding_of_id(id) {
                crate::reopen_as_asked(&window, &held, encoding);
            }
        }
    });

    // 要件 8.3（書き手のレビュー 2026-09-11、S2）: **押せる印。**外で変わったことは
    // 片付くまで画面に出ていて、押せばどうするかを訊く——保存のときに初めて
    // 気づくのでは、そこでどちらかを捨てることになる。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_outside_asked(move || {
        if let Some(window) = weak.upgrade() {
            crate::ask_outside_change(&window, &held);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_comparison_dismissed(move |pane| {
        if let Some(window) = weak.upgrade() {
            held.states
                .document(crate::PaneId::from_index(pane))
                .stop_comparison();
            crate::relayout_panes(&window, &held.states, &held.cache);
            crate::restore_editor_focus(&window);
        }
    });

    // 要件 7.9: **書きながら語を足す、いちばん普通の道。**
    let weak = window.as_weak();
    let held = live.clone();
    window.on_pane_word_added(move |pane, at| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = held.states.document(id);
            let source = document.text.borrow().clone();
            let ranges = selected_runs(&held.cache, id);
            // **選んだものをそのまま語にする。**矩形選択なら行ごとに切れている
            // ので、先頭の1本だけを採る——語は1行に収まるものである。
            let Some((start, end)) = ranges.first().copied() else {
                return;
            };
            let word = source[start..end].to_owned();
            let mode = pane_word_mode(&held, id);
            add_word_to_group(&window, &held, mode, at.max(0) as usize, &word);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_removed(move |group, word| {
        if let Some(window) = weak.upgrade() {
            let mode = window.get_word_mode_opened_at().max(0) as usize;
            remove_word_from_group(&window, &held, mode, group.max(0) as usize, &word);
        }
    });

    // 書き手と決めた 2026-09-08: **除外語群。**色を持たない語群は木に積まれて
    // 最長一致で勝ち、しかし何も塗らない——`リオン`を色分けしている書き手が
    // `カリオン`をここへ入れると、`カリオン`の中で`リオン`が光らなくなる。
    // **照合の仕組みは1行も変えずに、包む語を書き手が言える。**
    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_group_paint_toggled(move |at| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let at = at.max(0) as usize;
        let mut modes = word_modes_now();
        let Some(mode) = modes.get_mut(window.get_word_mode_opened_at().max(0) as usize) else {
            return;
        };
        let Some(standing) = mode.groups.get(at).map(|group| group.colour) else {
            return;
        };
        // **色を戻すときは、いま空いている色を配る**（消したときの色を覚えて
        // おくより役に立つ）。**自分は数に入れない**——自分の古い色が「使われて
        // いる」ことになると、隣の語群と同じ色を配ってしまう。
        let given = standing.is_none().then(|| {
            let others: Vec<word_marks::WordGroup> = mode
                .groups
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != at)
                .map(|(_, group)| group.clone())
                .collect();
            next_word_colour(&others)
        });
        if let Some(group) = mode.groups.get_mut(at) {
            group.colour = given;
        }
        hold_word_modes(&window, &held, modes, true);
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_group_removed(move |at| {
        if let Some(window) = weak.upgrade() {
            let mode = window.get_word_mode_opened_at().max(0) as usize;
            let mut modes = word_modes_now();
            let Some(held_mode) = modes.get_mut(mode) else {
                return;
            };
            let at = at.max(0) as usize;
            if at >= held_mode.groups.len() {
                return;
            }
            held_mode.groups.remove(at);
            window.set_word_group_opened_at(-1);
            hold_word_modes(&window, &held, modes, true);
        }
    });

    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_mode_removed(move |at| {
        if let Some(window) = weak.upgrade() {
            let mut modes = word_modes_now();
            let at = at.max(0) as usize;
            if at >= modes.len() {
                return;
            }
            modes.remove(at);
            window.set_word_mode_opened_at(-1);
            window.set_word_group_opened_at(-1);
            // **文書のほうは触らない。**消したモードを指していたタブは「なし」
            // として扱われる（`word_mode_named`）——名前を書き換えて回るより、
            // 引けなかったときの答えが1つあるほうが確かである。
            hold_word_modes(&window, &held, modes, true);
            publish_word_mode_of(&window, &held);
        }
    });

    // **取り込みと書き出しは「あってもよい道」**（要件 7.9）。他の道具で作った
    // 一覧を持ち込むとき、控えを取るとき、別の機械へ移すときのためにある。
    let weak = window.as_weak();
    let held = live.clone();
    window.on_word_group_imported(move |at| {
        let weak = weak.clone();
        let held = held.clone();
        Timer::single_shot(Duration::ZERO, move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let Some(source) = file_dialog::open_word_set(ime::window_handle(&window)) else {
                return;
            };
            let Some(words) = read_word_source(&source) else {
                window.set_render_status("取り込めませんでした".into());
                return;
            };
            let mode = window.get_word_mode_opened_at().max(0) as usize;
            let mut modes = word_modes_now();
            let Some(group) = modes
                .get_mut(mode)
                .and_then(|held| held.groups.get_mut(at.max(0) as usize))
            else {
                return;
            };
            // **足す**（置き換えない）。取り込みは書き手が起こす操作で、いまある
            // 語を黙って捨てる理由が無い——重なったぶんは「二重」と言われる。
            //
            // **覚え書きだけは重ねない**（2026-09-08）。語の「二重」は画面に出て
            // 掃除できるが、同じ見出しが取り込むたびに増えるのは、ただの散らかり
            // である。
            let mut taken = 0usize;
            for line in words {
                if word_marks::is_note(&line) {
                    if !group.words.contains(&line) {
                        group.words.push(line);
                    }
                    continue;
                }
                taken += 1;
                group.words.push(line);
            }
            hold_word_modes(&window, &held, modes, true);
            window.set_render_status(format!("{taken}語を取り込みました").into());
        });
    });

    let weak = window.as_weak();
    window.on_word_group_exported(move |at| {
        let weak = weak.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                let mode = window.get_word_mode_opened_at().max(0) as usize;
                export_word_group(&window, mode, at.max(0) as usize);
            }
        });
    });
}

/// 要件 7.7: 文書の中を探して置き換える。
///
/// **三つとも前に出ているペインに効き、三つとも普通の編集の道を通る**ので、
/// 取り消しも他の面の追従も勝手に付いてくる。
pub fn wire_find(window: &AppWindow, live: &Live) {
    // 要件 7.7: finding and replacing inside the document in front of the
    // writer. All three act on the focused pane, and all three go through the
    // ordinary editing path so that undo and the other panes follow.
    let weak = window.as_weak();
    let find_live = live.clone();
    window.on_find_requested(move |forwards| {
        if let Some(window) = weak.upgrade() {
            find_in_pane(&window, &find_live, forwards);
        }
    });

    // E1: 打っているあいだの数え直し。**動かさないのはここだけの約束ではない**
    // ——`count_in_pane`がカーソルに触らないので、検索欄でのIME変換中に本文が
    // 飛ばない。
    let weak = window.as_weak();
    let typed_live = live.clone();
    window.on_find_typed(move || {
        if let Some(window) = weak.upgrade() {
            count_in_pane(&window, &typed_live);
        }
    });

    // E1: 探し方の3つの入切。**範囲だけはRustが答える**——押した瞬間の選択を
    // 写し取るのは、窓の側では書けない。
    let weak = window.as_weak();
    let option_live = live.clone();
    window.on_find_option(move |which| {
        if let Some(window) = weak.upgrade() {
            choose_find_option(&window, &option_live, which);
        }
    });

    // E1の④: 履歴を↑↓で歩く。**帯には何も足していない**（書き手の選択
    // 2026-09-10）——半分の窓に収まる帯を保ったまま、打ち直しだけを減らす。
    let weak = window.as_weak();
    let history_live = live.clone();
    window.on_find_history(move |back, replacing| {
        if let Some(window) = weak.upgrade() {
            walk_find_history(&window, &history_live, back, replacing);
        }
    });

    let weak = window.as_weak();
    let clear_live = live.clone();
    window.on_find_cleared(move || {
        if let Some(window) = weak.upgrade() {
            clear_find(&window, &clear_live);
        }
    });

    // E4: `Ctrl+G`。**3つとも同じ帯の口**——出す・打つ・行く。
    let weak = window.as_weak();
    let goto_live = live.clone();
    window.on_goto_requested(move |taking| {
        if let Some(window) = weak.upgrade() {
            toggle_goto(&window, &goto_live, taking);
        }
    });

    let weak = window.as_weak();
    let goto_typed_live = live.clone();
    window.on_goto_typed(move || {
        if let Some(window) = weak.upgrade() {
            tell_goto(&window, &goto_typed_live);
        }
    });

    let weak = window.as_weak();
    let goto_accepted_live = live.clone();
    window.on_goto_accepted(move || {
        if let Some(window) = weak.upgrade() {
            go_to_line(&window, &goto_accepted_live);
        }
    });

    let weak = window.as_weak();
    let replace_live = live.clone();
    window.on_replace_requested(move || {
        if let Some(window) = weak.upgrade() {
            replace_in_pane(&window, &replace_live);
        }
    });

    let weak = window.as_weak();
    let replace_all_live = live.clone();
    window.on_replace_all_requested(move || {
        if let Some(window) = weak.upgrade() {
            replace_all_in_pane(&window, &replace_all_live);
        }
    });
}

/// 要件 9: 表示設定を動かす口。**要件 E9 のルビ記法もここにいる**——紙の
/// シートの設定ではないが、切り替えると同じ組み直しが要る（`spec_timer`は
/// ここにしか無い）。
///
/// **`spec_timer`は借りずに受け取る。**最後の`typography-reset`がそれを閉包へ
/// 入れて持っていくので、ここから先で誰も使わない——`main()`でもそうだった。
pub fn wire_typography(
    window: &AppWindow,
    pane_states: &PaneStates,
    render_cache: &Rc<RefCell<RenderCache>>,
    spec_timer: Rc<Timer>,
    numbers: Rc<VecModel<i32>>,
    palette: Rc<VecModel<Color>>,
    sheet_fonts: Rc<VecModel<SharedString>>,
) {
    // **借りたものを、その場で自分のものにする。**下の閉包はどれも`'static`で、
    // 参照は入っていけない——`main()`ではこれらが局所変数だった、そこだけが違う。
    let pane_states = pane_states.clone();
    let render_cache = render_cache.clone();
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let steps = numbers.clone();
    window.on_sheet_step(move |setting, by| {
        let Some(setting) = Setting::from_index(setting) else {
            return;
        };
        if let Some(window) = weak.upgrade() {
            step_setting(&window, &steps, setting, by);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 書き手の求め 2026-09-15: 大きさを打って決める——px／% と pt の2つの口。
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let typed = numbers.clone();
    window.on_sheet_typed(move |setting, text| {
        let Some(setting) = Setting::from_index(setting) else {
            return;
        };
        if let Some(window) = weak.upgrade() {
            crate::type_setting(&window, &typed, setting, &text);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let typed = numbers.clone();
    window.on_sheet_typed_points(move |slot, text| {
        if let Some(window) = weak.upgrade() {
            crate::type_points(&window, &typed, slot, &text);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 要件 9: a setting whose values are a choice rather than a quantity —
    // the same door as `sheet-step`, told what to be instead of by how
    // much to move.
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let chosen = numbers.clone();
    window.on_sheet_chose(move |setting, value| {
        let Some(setting) = Setting::from_index(setting) else {
            return;
        };
        if let Some(window) = weak.upgrade() {
            let (low, high) = setting.range();
            setting.write(&chosen, shown_sheet(&window), value.clamp(low, high));
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 要件 E9: ルビと傍点の記法を読むか。**紙のシートではなく Settings → General の
    // FILES に
    // ある**（`count.ruby`と同じ理由の一歩先——シートが持つのは組み方で、これは
    // 読み方である）が、**口はここにある**：切り替えると本文そのものが変わるので、
    // 表示設定と同じ組み直しが要る。`spec_timer`はここにしか無い。
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_ruby_marks_toggled(move |wanted| {
        if let Some(window) = weak.upgrade() {
            window.set_ruby_marks(wanted);
            save_settings(&window, &cache);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 書き手の決定 2026-09-11: どの記号を箇条書きの印として読むか。**組み直しが
    // 要る**——`* 項目`が項目なのか本文の1行なのかが変わるので、ルビの旗とまったく
    // 同じ道を通る（`spec_timer`はここにしか無い）。
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_bullet_mark_toggled(move |index| {
        if let Some(window) = weak.upgrade() {
            let Some(mark) = document::BULLET_MARKS.get(index.max(0) as usize) else {
                return;
            };
            let marks = bullet_marks_of(&window).toggled(*mark);
            show_bullet_marks(&window, marks);
            save_settings(&window, &cache);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 要件 9: which families this machine has, and which one was chosen.
    // **一覧は`fill_font_names`が窓へ入れる**——同じ一覧を端末の書体（6.8）も
    // 使うので、持ち主は窓のほうにいる。
    let weak = window.as_weak();
    window.on_font_picked(move |slot| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        fill_font_names(&window);
        let sheet = shown_sheet(&window);
        let row = font_row(sheet, slot.max(0) as usize);
        let standing = window.get_sheet_fonts().row_data(row).unwrap_or_default();
        window.set_font_slot(slot);
        window.set_font_current(standing);
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let families = sheet_fonts.clone();
    window.on_font_chosen(move |slot, family| {
        if let Some(window) = weak.upgrade() {
            // 追加要件 2026-09-08: **同じ一覧が2つの欄のために開く。**どちらの
            // ために開いたかは、開いた側が旗で言う（要件 6.8 の端末の書体）。
            if window.get_font_for_terminal() {
                window.set_font_for_terminal(false);
                window.set_terminal_font(family.clone());
                cache
                    .borrow_mut()
                    .log_diag("spec", &format!("terminal font={family}"));
                after_terminal_look(&window, &cache);
                return;
            }
            let sheet = shown_sheet(&window);
            let slot = slot.max(0) as usize;
            families.set_row_data(font_row(sheet, slot), family.clone());
            cache.borrow_mut().log_diag(
                "spec",
                &format!("font sheet={sheet} {}={family}", font_name(slot)),
            );
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer;
    let steps = numbers;
    let colours = palette;
    let families = sheet_fonts;
    window.on_typography_reset(move |group| {
        if let Some(window) = weak.upgrade() {
            // **Both sheets**, which are both on the page now (書き手の求め
            // 2026-09-15) — and **only the page's own values**: the Reset on
            // Text leaves the page margin where the writer put it.
            reset_settings_group(&steps, &colours, &families, group);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });
}

/// 要件 6.5 の「開く」と、要件 12 のクイック下書き。
///
/// **下書きの窓に渡すのは、タブの一覧と、そこへ字を入れる手段だけ**である
/// (`quick_draft::Editor`)。あちらがこちら側について知っているのは、
/// `searcher.rs` が起こす窓について知っているのと同じ量——つまりほとんど何も。
pub fn wire_open_and_draft(
    window: &AppWindow,
    live: &Live,
    draft: &Rc<RefCell<quick_draft::QuickDraftWindow>>,
) {
    crate::diff_view::wire(window, live);
    crate::shortcuts::wire(window, live);
    let weak = window.as_weak();
    let reopened_live = live.clone();
    window.on_reopen_closed_requested(move || {
        let weak = weak.clone();
        let live = reopened_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                crate::reopen_closed_tab(&window, &live);
            }
        });
    });
    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_open_file_requested(move || {
        if let Some(window) = weak.upgrade() {
            open_document(&window, &file_live);
            restore_editor_focus(&window);
        }
    });

    // 要件 12: the quick draft. **The menu asks for it; it does not open it** —
    // what 要件 12.2 asks of this is that a global shortcut be able to ask the
    // same way later.
    let weak = window.as_weak();
    let held_draft = draft.clone();
    let draft_live = live.clone();
    window.on_quick_draft_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // **What the draft window is given is a list of tabs and a way to put
        // text in one**, not the editor. It knows no more about this side than
        // `searcher.rs` knows about the window it wakes.
        //
        // The two share what the list resolved to, so the place a row stands
        // for is decided once (`paste_targets`).
        let resolved: Rc<RefCell<Vec<(PaneId, usize)>>> = Rc::default();
        let editor = quick_draft::Editor {
            tabs: {
                let weak = window.as_weak();
                let live = draft_live.clone();
                let resolved = resolved.clone();
                Box::new(move |aimed| {
                    let Some(window) = weak.upgrade() else {
                        return quick_draft::TabList {
                            rows: Vec::new(),
                            target: -1,
                            target_name: NO_TARGET.to_owned(),
                        };
                    };
                    paste_targets(&window, &live, aimed, &mut resolved.borrow_mut())
                })
            },
            paste: {
                let weak = window.as_weak();
                let live = draft_live.clone();
                let resolved = resolved.clone();
                Box::new(move |at, text| {
                    let Some(&(id, index)) = resolved.borrow().get(at) else {
                        return String::new();
                    };
                    let Some(window) = weak.upgrade() else {
                        return String::new();
                    };
                    paste_into_tab(&window, &live, id, index, text)
                })
            },
        };
        quick_draft::QuickDraftWindow::open(&held_draft, &window, editor);
    });
}

/// 要件 8.2 の明示的な保存。
pub fn wire_saving(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_document(&window, &file_live, false);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_as_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_document(&window, &file_live, true);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_all_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_all(&window, &file_live);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_reveal_requested(move || {
        if let Some(window) = weak.upgrade() {
            reveal_active_document(&window, &file_live);
            restore_editor_focus(&window);
        }
    });
}

/// 要件 5 と 6.2 と 7.7: 左の面と、そこから開くもの。
///
/// **四つの面（Explorer・検索・履歴・アウトライン）と、作業フォルダそのもの。**
/// 行を押す・掴む・落とすの三つが要件 5.2 の運搬で、**掴んだ位置は即答する**
/// ——描き直すと掴んでいる当の行が消える（技術検証 6.18）。
///
/// フォルダ全文検索（要件 7.7）はスレッドへ出ていて、返事は世代番号つきで
/// 戻ってくる。**古い問いへの答えは捨てる**：追い越しは画面に出ない。
pub fn wire_left_panel(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let excluded_live = live.clone();
    window.on_search_exclusions_changed(move || {
        if let Some(window) = weak.upgrade() {
            excluded_live.searched.set(excluded_live.searched.get() + 1);
            excluded_live.results.borrow_mut().clear();
            publish_left(&window, &excluded_live);
            window.set_folder_status("条件を変更しました。検索してください".into());
            crate::write_session(&window, &excluded_live);
        }
    });
    let weak = window.as_weak();
    let new_live = live.clone();
    window.on_tree_new_finished(move |accept| {
        let weak = weak.clone();
        let live = new_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                crate::finish_tree_entry(&window, &live, accept);
            }
        });
    });
    let weak = window.as_weak();
    let explorer_live = live.clone();
    window.on_explorer_command(move |command| {
        if let Some(window) = weak.upgrade() {
            crate::explorer_command(&window, &explorer_live, command);
        }
    });
    // 要件 5.1, 5.2: the work folder and its tree.
    let weak = window.as_weak();
    let folder_live = live.clone();
    window.on_work_folder_requested(move || {
        if let Some(window) = weak.upgrade() {
            let owner = ime::window_handle(&window);
            let Some(chosen) = file_dialog::open_folder(owner) else {
                return;
            };
            open_work_folder(&window, &folder_live, &chosen);
        }
    });

    // 要件 5.1: back to a folder worked in before, chosen by name rather than
    // found again in the dialog.
    //
    // **Handed to the event loop rather than done here.** The rows are a
    // repeater inside an open popup and switching folders redraws them, which
    // is 6.18 again: the element whose handler is running would be replaced
    // underneath it.
    let weak = window.as_weak();
    let history_live = live.clone();
    window.on_recent_folder_chosen(move |index| {
        let index = index.max(0) as usize;
        let weak = weak.clone();
        let live = history_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                go_to_remembered_folder(&window, &live, index);
            }
        });
    });

    let weak = window.as_weak();
    let tree_live = live.clone();
    // Preserve the destination chosen by the first click until double-click.
    let explorer_click = Rc::new(RefCell::new(None::<(usize, PaneId, Instant, bool)>));
    let first_click = explorer_click.clone();
    window.on_left_row_activated(move |index| {
        let index = index.max(0) as usize;
        let weak = weak.clone();
        let live = tree_live.clone();
        let first_click = first_click.clone();
        // Opening a file replaces the model this row is drawn from. 6.18 again:
        // not from inside the click that is on it.
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                let id = focused_pane(&window);
                let explorer = LeftTab::from_index(window.get_left_tab()) == LeftTab::Explorer;
                let file = explorer
                    && live
                        .tree_paths
                        .borrow()
                        .get(index)
                        .is_some_and(|path| path.is_file());
                let doubled = if file {
                    take_explorer_double(&mut first_click.borrow_mut(), index, id)
                } else {
                    *first_click.borrow_mut() = None;
                    None
                };
                if let Some(reuse) = doubled {
                    live.cache.borrow_mut().log_diag(
                        "explorer",
                        &format!("double pane={} row={index} reuse={reuse}", id.log_name()),
                    );
                    if reuse {
                        if let Some(tab) = live.tabs.borrow().of(id).current() {
                            tab.provisional.set(false);
                        }
                        publish_tabs(&window, &live);
                    } else {
                        let active = live.tabs.borrow().of(id).active;
                        duplicate_tab(&window, &live, id, active);
                    }
                    return;
                }
                let new_tab = live
                    .tabs
                    .borrow()
                    .of(id)
                    .current()
                    .is_some_and(|tab| tab.empty);
                let was_open = live.tree_paths.borrow().get(index).is_some_and(|path| {
                    live.tabs
                        .borrow()
                        .of(id)
                        .tabs
                        .iter()
                        .any(|tab| tab.document.file.borrow().path() == Some(path.as_path()))
                });
                activate_left_row(&window, &live, index);
                if file {
                    let provisional = live
                        .tabs
                        .borrow()
                        .of(id)
                        .current()
                        .is_some_and(PaneTab::is_provisional);
                    remember_explorer_destination(
                        &mut first_click.borrow_mut(),
                        index,
                        id,
                        reuse_explorer_destination(new_tab, was_open, provisional),
                    );
                }
            }
        });
    });

    let weak = window.as_weak();
    let kept_live = live.clone();
    // Explorer doubles create another view unless the first click already
    // filled a New Tab or made a provisional view. Other lists still pin.
    window.on_left_row_kept(move |_index| {
        let weak = weak.clone();
        let live = kept_live.clone();
        // 一度目のクリックが仕掛けた`Timer`より後に走らなければ、まだ無い
        // タブに旗を立てることになる。0の単発は入れた順に走る。
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                let id = focused_pane(&window);
                if LeftTab::from_index(window.get_left_tab()) == LeftTab::Explorer {
                    // Explorer is counted by the persistent click handler above.
                    return;
                }
                let tabs = live.tabs.borrow();
                if let Some(tab) = tabs.of(id).current() {
                    tab.provisional.set(false);
                }
                drop(tabs);
                publish_tabs(&window, &live);
            }
        });
    });

    let weak = window.as_weak();
    let navigate_live = live.clone();
    window.on_pane_navigate(move |pane, forward| {
        if let Some(window) = weak.upgrade() {
            navigate(&window, &navigate_live, PaneId::from_index(pane), forward);
        }
    });

    // 要件 6.2: which of the left pane's three things is showing. Rust holds
    // it, because the rows it puts there have to agree with it.
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_left_tab_chosen(move |_tab| {
        let weak = weak.clone();
        let live = tab_live.clone();
        // The tab itself is set in the window, so the highlight moves at once.
        // Filling the panel replaces the model the rows are drawn from, and
        // this is a click inside a repeater — 6.18's rule, from the other side:
        // a different repeater, but not worth being clever about.
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                publish_left(&window, &live);
            }
        });
    });

    // 要件 7.7: the whole work folder, not the document in front.
    let weak = window.as_weak();
    let search_live = live.clone();
    window.on_folder_search_requested(move || {
        if let Some(window) = weak.upgrade() {
            search_work_folder(&window, &search_live);
        }
    });

    // 要件 7.7（2026-09-07追加）: which folder that search walks. **The dialog
    // opens where the search stands now**, so narrowing twice walks down rather
    // than starting over from the disk.
    let weak = window.as_weak();
    let scope_live = live.clone();
    window.on_search_folder_requested(move || {
        if let Some(window) = weak.upgrade() {
            let owner = ime::window_handle(&window);
            let from = scope_live.folder.borrow().searched_root();
            let Some(chosen) = file_dialog::open_folder_at(owner, from.as_deref()) else {
                return;
            };
            search_in_folder(&window, &scope_live, Some(chosen));
        }
    });

    let weak = window.as_weak();
    let scope_live = live.clone();
    window.on_search_folder_reset(move || {
        if let Some(window) = weak.upgrade() {
            search_in_folder(&window, &scope_live, None);
        }
    });

    // 要件 2: and the answer, whenever the searching thread has one.
    let weak = window.as_weak();
    let found_live = live.clone();
    window.on_folder_search_finished(move || {
        if let Some(window) = weak.upgrade() {
            collect_search(&window, &found_live);
        }
    });

    let weak = window.as_weak();
    let command_live = live.clone();
    // From the event loop rather than from the callback, for 6.18's reason: the
    // commands are asked for from a menu that hangs off a row of the tree, and
    // half of them draw the tree again — which takes that row, and the menu on
    // it, away while the click is still being handled.
    window.on_tree_command(move |command| {
        let Some(command) = TreeCommand::from_index(command) else {
            return;
        };
        let weak = weak.clone();
        let live = command_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                tree_command(&window, &live, command);
            }
        });
    });

    let picked_live = live.clone();
    window.on_left_row_picked(move |index| {
        pick_tree_row(&picked_live, index.max(0) as usize);
    });

    // 要件 5.2: a row was picked up. **Answered on the spot** — the answer is
    // one number and setting it draws nothing, which is what makes it safe to
    // ask Rust in the middle of a drag at all (drawing the rows again would
    // take the row the drag is running in, 6.18).
    let weak = window.as_weak();
    let grab_live = live.clone();
    window.on_tree_row_grabbed(move |index| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let paths = grab_live.tree_paths.borrow();
        let at = index.max(0) as usize;
        window.set_tree_carry_end(file_tree::subtree_end(&paths, at) as i32);
        // Which row holds it now, and `-1` when that is the work folder — the
        // heading stands for the work folder, so the two answers are the two
        // kinds of place a row can be let go in, and neither may be given what
        // it already has.
        let holder = paths.get(at).and_then(|path| path.parent());
        let row = holder.and_then(|held| paths.iter().position(|path| path == held));
        window.set_tree_carry_parent(row.map_or(-1, |at| at as i32));
    });

    // 要件 5.2: one line for one gesture on a row, so that "it does not move"
    // has an answer to be read rather than guessed at. **The rows are inside a
    // `ScrollView`**, and a Flickable holds a press back for 100ms and takes
    // the drag for itself when it can scroll that way (`items/flickable.rs`),
    // so a carry can fail before any of this code runs.
    let trace_live = live.clone();
    window.on_tree_row_traced(move |row, moves, pressed, carried, drop, cancelled| {
        // A press that never moved is a row being chosen, and that is the other
        // callback's story. This one is only for gestures that meant to carry.
        if moves == 0 && !cancelled {
            return;
        }
        let told = format!(
            "drag row={row} moves={moves} pressed={} carried={} drop={drop} cancel={}",
            u8::from(pressed),
            u8::from(carried),
            u8::from(cancelled),
        );
        trace_live.cache.borrow_mut().log_diag("folder", &told);
    });

    // 要件 5.2: a carried row was let go. Put off to the next tick for the
    // reason every tab command is: the move draws the tree again, and the row
    // the drag ran in is one of the rows that is rebuilt (6.18).
    let weak = window.as_weak();
    let drop_live = live.clone();
    window.on_tree_row_dropped(move |from, onto| {
        let from = from.max(0) as usize;
        let weak = weak.clone();
        let live = drop_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                drop_tree_row(&window, &live, from, onto);
            }
        });
    });
}
