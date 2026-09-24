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

/// **選べばその面だけが替わり、値を替えればどのプリセットにも合わなくなる。**
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

    // 替えれば「なし」。
    window.set_paper_random(2);
    window.set_terminal_size(20);
    window.set_language(1);
    save_settings(window, &r.live.cache);
    assert_eq!(current(window, 0), "");
    assert_eq!(current(window, 1), "");

    // Editorを選ぶ：Editorの値だけが戻り、Terminal・Generalはそのまま。
    window.invoke_preset_chosen(0, "執筆用".into());
    assert_eq!(window.get_paper_random(), 1);
    assert_eq!(window.get_terminal_size(), 20);
    assert_eq!(window.get_language(), 1);
    assert_eq!(current(window, 0), "執筆用");
    window.invoke_preset_chosen(1, "黒".into());
    assert_eq!(window.get_terminal_size(), 15);

    // 書いたファイルを次の起動が読む。
    let directory = app_data::app_directory().unwrap();
    let names: Vec<_> = settings_transfer::read_presets(&directory)
        .into_iter()
        .map(|preset| preset.name)
        .collect();
    assert_eq!(names, ["執筆用", "黒"]);

    // Keys。
    window.set_shortcut_bindings("0=Ctrl+Alt+O".into());
    settings_transfer::save_as(window, &r.live, PresetKind::Keys, "自分の");
    window.set_shortcut_bindings("".into());
    crate::shortcuts::publish(window);
    assert_eq!(current(window, 2), "");
    window.invoke_preset_chosen(2, "自分の".into());
    assert_eq!(window.get_shortcut_bindings(), "0=Ctrl+Alt+O");
    assert_eq!(current(window, 2), "自分の");

    // 同じ名前で残せば上書き、消せば無くなる。
    window.set_paper_random(0);
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "執筆用");
    assert_eq!(window.get_editor_presets().row_count(), 1);
    window.invoke_preset_delete_requested(0, "執筆用".into());
    assert_eq!(window.get_editor_presets().row_count(), 0);
    assert_eq!(current(window, 0), "");
}

/// **書き出したファイルを取り込めば、設定・キー・プリセットが戻る。**取り込む前の姿は
/// `before-import.rfnexport`に残る。
#[test]
fn an_export_brings_everything_back() {
    let r = rig("本文。\n", (900, 500));
    let window = &r.window;
    settings_transfer::wire(window, &r.live);

    window.set_paper_random(1);
    window.set_language(1);
    window.set_shortcut_bindings("0=Ctrl+Alt+O".into());
    save_settings(window, &r.live.cache);
    settings_transfer::save_as(window, &r.live, PresetKind::Editor, "執筆用");
    let written = settings_transfer::encode_export(&settings_transfer::export_now(window));

    window.set_paper_random(2);
    window.set_language(2);
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
    assert_eq!(window.get_language(), 1);
    assert_eq!(window.get_shortcut_bindings(), "0=Ctrl+Alt+O");
    assert_eq!(window.get_editor_presets().row_count(), 1);
    assert_eq!(current(window, 0), "執筆用");

    // 控えは取り込む前の姿（言語は2）。
    let directory = app_data::app_directory().unwrap();
    let kept = std::fs::read_to_string(settings_transfer::backup_path(&directory)).unwrap();
    let kept = settings_transfer::decode_export(&kept).unwrap();
    assert!(kept.settings.contains(&("language".into(), "2".into())));
    assert_eq!(kept.keys.as_deref(), Some(""));
    // 設定ファイルにも書かれている。
    let saved = app_data::read_settings(&directory).unwrap();
    assert!(saved.contains(&("paper.random".into(), "1".into())));
}
