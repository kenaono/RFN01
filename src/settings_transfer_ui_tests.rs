//! RFN01-31・RFN01-56: プリセットの保存と切り替え、全体の書き出しと取り込み。
use super::*;
use crate::settings_transfer::{self, PresetKind};
use crate::table_ui_tests::rig;

fn current(window: &AppWindow, kind: usize) -> String {
    window
        .get_preset_current()
        .row_data(kind)
        .unwrap_or_default()
        .to_string()
}

fn modified(window: &AppWindow, kind: usize) -> bool {
    window
        .get_preset_modified()
        .row_data(kind)
        .unwrap_or_default()
}

/// **選べばその面だけが替わる。値を替えても選んだ名前は残り、「変更あり」になる。**
/// Keysのプリセットはキー割り当てを戻す。
#[test]
fn a_preset_puts_back_only_its_own_page() {
    let r = rig("本文。\n", (900, 500));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);

    window.set_paper_random(1);
    window.set_terminal_size(15);
    save_settings(window, &r.live.cache);
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "執筆用");
    settings_transfer::save_as(window, &r.live, PresetKind::Terminal, "黒");
    assert_eq!(current(window, 0), "執筆用");
    assert_eq!(current(window, 1), "黒");

    assert!(!modified(window, 0));
    // 替えれば「変更あり」。名前は残る。
    window.set_paper_random(2);
    window.set_terminal_size(20);
    window.set_left_size(15);
    save_settings(window, &r.live.cache);
    assert_eq!(current(window, 0), "執筆用");
    assert!(modified(window, 0));
    assert!(modified(window, 1));

    // Editorを選ぶ：Editorの値だけが戻り、Terminal・Left Paneはそのまま。
    // （言語は使わない——翻訳はプロセスで1つで、並んで走る試験の文言を変える。）
    window.invoke_preset_chosen(0, "執筆用".into());
    assert_eq!(window.get_paper_random(), 1);
    assert_eq!(window.get_terminal_size(), 20);
    assert_eq!(window.get_left_size(), 15);
    assert_eq!(current(window, 0), "執筆用");
    assert!(!modified(window, 0));
    window.invoke_preset_chosen(1, "黒".into());
    assert_eq!(window.get_terminal_size(), 15);

    // 書いたファイルを次の起動が読む。
    let directory = app_data::app_directory().unwrap();
    let names: Vec<_> = settings_transfer::read_presets(&directory)
        .presets
        .into_iter()
        .map(|preset| preset.name)
        .collect();
    assert_eq!(names, ["執筆用", "黒"]);

    // Keys。
    window.set_shortcut_bindings("0=Ctrl+Alt+O".into());
    settings_transfer::save_as(window, &r.live, PresetKind::Keys, "自分の");
    window.set_shortcut_bindings("".into());
    crate::shortcuts::publish(window);
    assert!(modified(window, 2));
    window.invoke_preset_chosen(2, "自分の".into());
    assert_eq!(window.get_shortcut_bindings(), "0=Ctrl+Alt+O");
    assert_eq!(current(window, 2), "自分の");

    // 「Save」で選んでいるプリセットを上書き。
    window.set_paper_random(0);
    save_settings(window, &r.live.cache);
    assert!(modified(window, 0));
    window.invoke_preset_overwrite_requested(0);
    assert!(!modified(window, 0));
    window.set_paper_random(2);
    window.invoke_preset_chosen(0, "執筆用".into());
    assert_eq!(window.get_paper_random(), 0);
    assert_eq!(window.get_editor_presets().row_count(), 1);
    // 消せば無くなり、「None」。
    window.invoke_preset_delete_requested(0, "執筆用".into());
    assert_eq!(window.get_editor_presets().row_count(), 0);
    assert_eq!(current(window, 0), "");
}

/// 書き手の報告 2026-09-24: **同じ値を別の名前で残しても、どちらも選べる。**選んだほうの
/// 名前が出て、開き直しても残る。
#[test]
fn presets_with_the_same_values_are_each_chosen() {
    let r = rig("本文。\n", (900, 500));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "A");
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "B");
    assert_eq!(current(window, 0), "B");
    window.invoke_preset_chosen(0, "A".into());
    assert_eq!(current(window, 0), "A");
    window.invoke_preset_chosen(0, "B".into());
    assert_eq!(current(window, 0), "B");
    assert!(!modified(window, 0));
    let directory = app_data::app_directory().unwrap();
    let book = settings_transfer::read_presets(&directory);
    assert_eq!(book.chosen[0].as_deref(), Some("B"));
}

/// **書き出したファイルを取り込めば、設定・キー・プリセットが戻る。**取り込む前の姿は
/// `before-import.rfnexport`に残る。
#[test]
fn an_export_brings_everything_back() {
    let r = rig("本文。\n", (900, 500));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);

    window.set_paper_random(1);
    window.set_left_size(15);
    window.set_shortcut_bindings("0=Ctrl+Alt+O".into());
    save_settings(window, &r.live.cache);
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "執筆用");
    let written = settings_transfer::encode_export(&settings_transfer::export_now(window));

    window.set_paper_random(2);
    window.set_left_size(18);
    window.set_shortcut_bindings("".into());
    window.invoke_preset_delete_requested(0, "執筆用".into());
    save_settings(window, &r.live.cache);

    let mut export = settings_transfer::decode_export(&written).expect("reads back");
    // 単語帳も置き換わる（1つのモードに1つの語群）。
    export.words = Some(
        "RFN-EDIT-WORDS 3\nnext: 3\nmode: 1 | 小説\ngroup: 2 | 人物 | #336699\nリオン\n".into(),
    );
    settings_transfer::import(window, &r.live, export).unwrap();
    let modes = word_modes_now();
    assert_eq!(modes.len(), 1);
    assert_eq!(modes[0].name, "小説");
    assert_eq!(window.get_paper_random(), 1);
    assert_eq!(window.get_left_size(), 15);
    assert_eq!(window.get_shortcut_bindings(), "0=Ctrl+Alt+O");
    assert_eq!(window.get_editor_presets().row_count(), 1);
    assert_eq!(current(window, 0), "執筆用");

    // 控えは取り込む前の姿（Left Paneの字は18）。
    let directory = app_data::app_directory().unwrap();
    let kept = std::fs::read_to_string(settings_transfer::backup_path(&directory)).unwrap();
    let kept = settings_transfer::decode_export(&kept).unwrap();
    assert!(kept.settings.contains(&("left.size".into(), "18".into())));
    assert_eq!(kept.keys.as_deref(), Some(""));
    // 設定ファイルにも書かれている。
    let saved = app_data::read_settings(&directory).unwrap();
    assert!(saved.contains(&("paper.random".into(), "1".into())));
}

/// 書き手の報告 2026-09-24（Terminal・Keysで変えてもSaveできない）の確かめ：設定の画面と同じ
/// 配線で変えれば、Terminal・Keysのプリセットも「変更あり」になる。
#[test]
fn terminal_and_key_changes_mark_their_presets_changed() {
    let r = rig("本文。\n", (1000, 740));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);
    crate::terminal_appearance::install(window, &r.live);
    crate::shortcuts::wire(window, &r.live);
    settings_transfer::save_as(window, &r.live, PresetKind::Terminal, "T");
    settings_transfer::save_as(window, &r.live, PresetKind::Keys, "K");
    open_settings(window, &r.live);
    window.set_settings_tab(1);
    // 画面の入切と同じ：変わったら200ms待って書く。
    window.set_terminal_confirm_paste(false);
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
    }
    assert_eq!(current(window, 1), "T");
    assert!(modified(window, 1));
    window.set_shortcut_selected(1);
    window.set_shortcut_edit("Ctrl+Alt+S".into());
    window.invoke_shortcut_save(false);
    assert_eq!(current(window, 2), "K");
    assert!(modified(window, 2));
    window.invoke_preset_overwrite_requested(2);
    assert!(!modified(window, 2));
}

/// 書き手の報告 2026-09-24（プロファイルの引数を変えても保存できない）：**シェルのプロファイルも
/// Terminalのプリセットに入る。**変えれば「変更あり」になり、選べば一覧ごと戻る。
#[test]
fn a_terminal_preset_carries_the_shell_profiles() {
    let r = rig("本文。\n", (900, 500));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);
    crate::terminal_shells::install(window, &r.live);
    let before = configured_shells(window);
    settings_transfer::save_as(window, &r.live, PresetKind::Terminal, "T");

    let mut changed = before.clone();
    changed[0].command.push_str(" -NoLogo");
    hold_shells(window, &changed);
    save_settings(window, &r.live.cache);
    assert!(modified(window, 1));

    window.invoke_preset_chosen(1, "T".into());
    assert_eq!(
        configured_shells(window)
            .iter()
            .map(|shell| shell.command.clone())
            .collect::<Vec<_>>(),
        before
            .iter()
            .map(|shell| shell.command.clone())
            .collect::<Vec<_>>()
    );
    assert!(!modified(window, 1));
}
