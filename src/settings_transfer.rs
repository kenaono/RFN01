//! RFN01-31・RFN01-56（書き手と決めた 2026-09-24）: 設定のプリセットと、全体の書き出し・取り込み。
//!
//! **プリセットはEditor・Terminal・Keysの3つの面ごと。**選べばその面の値だけがすぐ替わる。
//! どの設定がどの面のものかは[`kind_of`]の1か所で決める——書き出す値そのものは
//! `settings_values`が作る（設定ファイルと同じ名前と値）ので、ここは振り分けるだけである。
//!
//! **書き出しは1つのファイル**：全設定・キー割り当て・プリセット・単語帳。取り込みは全体を
//! 置き換え、その直前にいまの姿を同じ形で控える（`before-import.rfnexport`）。
//!
//! 入れないもの：作業コピーを残すかどうか（`work.autosave`、守りのための設定をプリセットで
//! 切らない）、文字色のセットと最近の色
//! （セットの置き場で、いまの姿ではない）。**単語帳と文書ごとのモードもプリセットは触らない**
//! （RFN01-31の本文）。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel};

use crate::{AppWindow, Live, StatusBar, app_data, file_dialog, file_io, ime, pick, say};

/// プリセットを持つ面。番号は画面（`preset-*`）と1対1。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresetKind {
    Editor,
    Terminal,
    Keys,
}

impl PresetKind {
    pub(crate) const ALL: [Self; 3] = [Self::Editor, Self::Terminal, Self::Keys];

    pub(crate) fn from_number(number: i32) -> Option<Self> {
        Self::ALL.get(usize::try_from(number).ok()?).copied()
    }

    fn number(self) -> usize {
        self as usize
    }

    fn written(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Terminal => "terminal",
            Self::Keys => "keys",
        }
    }

    fn read(said: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.written() == said)
    }
}

/// キー割り当ての名前。**設定ファイルではなく作業の記録（session）にある**ので、書き出しと
/// プリセットでは設定と同じ「名前: 値」の1行にして運ぶ。
const KEYS_SETTING: &str = "shortcut-bindings";

/// Editorの面の設定のうち、横書き・縦書きのシート（`h.`・`v.`）以外のもの。
const EDITOR_SETTINGS: &[&str] = &[
    "text.shared",
    "layout.shared",
    "paper.shared",
    "paper.random",
    "ruby.marks",
    "list.marks",
    "count.ruby",
    "wallpaper.kind",
    "wallpaper.path",
    "wallpaper.fit",
    "wallpaper.strength",
    "background.transparency",
];

/// その設定がどの面のプリセットに入るか。**どれにも入らないものは`None`**
/// （General・Left Pane・印刷と、上で外したもの）。
pub(crate) fn kind_of(name: &str) -> Option<PresetKind> {
    if name == KEYS_SETTING {
        return Some(PresetKind::Keys);
    }
    if name.starts_with("h.") || name.starts_with("v.") || EDITOR_SETTINGS.contains(&name) {
        return Some(PresetKind::Editor);
    }
    // **Terminalのページにあるものは全部**（書き手の報告 2026-09-24：既定のシェルも、
    // プロファイルのコマンドや引数も、変えても「変更あり」にならなかった）。切り替えれば
    // プロファイルの一覧もそのプリセットの中身に置き換わる。既定は名前で持つので、その
    // 名前のプロファイルが無ければ最初のものになる（`apply_settings`）。
    if name.starts_with("terminal.") {
        return Some(PresetKind::Terminal);
    }
    None
}

/// 名前の付いた1組の値。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preset {
    pub(crate) kind: PresetKind,
    pub(crate) name: String,
    pub(crate) values: Vec<(String, String)>,
}

/// プリセットの一覧と、面ごとに選んでいるプリセット（書き手の報告 2026-09-24）。
///
/// **選んだものを覚える**のは、値だけでは答えが決まらないから：同じ値を別の名前で残すと、
/// 値から探せばいつも上のほうが出て、下のほうを選べなかった。値を替えたあとも名前が残るので、
/// 「Save」でそのプリセットを直せる。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PresetBook {
    pub(crate) presets: Vec<Preset>,
    /// `PresetKind`の番号ごと。
    pub(crate) chosen: [Option<String>; 3],
}

impl PresetBook {
    fn find(&self, kind: PresetKind, name: &str) -> Option<&Preset> {
        self.presets
            .iter()
            .find(|preset| preset.kind == kind && preset.name == name)
    }
}

const PRESETS_FILE: &str = "presets.rfnpresets";
const PRESETS_MAGIC: &str = "RFN-EDIT-PRESETS 1";
/// プリセットの頭の行。**これで始まる行だけが見出し**で、面と名前をタブで分ける。
const PRESET_KEY: &str = "preset: ";
/// 選んでいるプリセットの行（面と名前をタブで分ける）。プリセットの外、先頭に書く。
const CHOSEN_KEY: &str = "chosen: ";

pub(crate) fn encode_presets(book: &PresetBook) -> String {
    let mut out = format!("{PRESETS_MAGIC}\n");
    for kind in PresetKind::ALL {
        if let Some(name) = &book.chosen[kind.number()] {
            let name = name.replace(['\n', '\r', '\t'], " ");
            out.push_str(&format!("{CHOSEN_KEY}{}\t{name}\n", kind.written()));
        }
    }
    for preset in &book.presets {
        let name = preset.name.replace(['\n', '\r', '\t'], " ");
        out.push_str(&format!("{PRESET_KEY}{}\t{name}\n", preset.kind.written()));
        for (name, value) in &preset.values {
            out.push_str(&format!("{name}: {}\n", value.replace(['\n', '\r'], "")));
        }
    }
    out
}

/// 読めない行は飛ばす——設定ファイルと同じ寛容さ。先頭の1行が合わなければ`None`。
pub(crate) fn decode_presets(raw: &str) -> Option<PresetBook> {
    let mut lines = raw.split('\n').map(|line| line.trim_end_matches('\r'));
    if lines.next()? != PRESETS_MAGIC {
        return None;
    }
    let mut presets: Vec<Preset> = Vec::new();
    let mut chosen: [Option<String>; 3] = Default::default();
    // 読めない見出しの下の値は、どのプリセットにも入れない。
    let mut open = false;
    for line in lines {
        if let Some(head) = line.strip_prefix(CHOSEN_KEY) {
            open = false;
            if let Some((kind, name)) = head.split_once('\t')
                && let Some(kind) = PresetKind::read(kind)
            {
                chosen[kind.number()] = Some(name.trim().to_owned());
            }
            continue;
        }
        if let Some(head) = line.strip_prefix(PRESET_KEY) {
            open = false;
            if let Some((kind, name)) = head.split_once('\t')
                && let Some(kind) = PresetKind::read(kind)
                && !name.trim().is_empty()
            {
                presets.push(Preset {
                    kind,
                    name: name.trim().to_owned(),
                    values: Vec::new(),
                });
                open = true;
            }
            continue;
        }
        if open
            && let Some((name, value)) = line.split_once(": ")
            && let Some(preset) = presets.last_mut()
        {
            preset.values.push((name.to_owned(), value.to_owned()));
        }
    }
    let mut book = PresetBook { presets, chosen };
    // 無くなったプリセットを指していたら忘れる。
    for kind in PresetKind::ALL {
        if let Some(name) = book.chosen[kind.number()].clone()
            && book.find(kind, &name).is_none()
        {
            book.chosen[kind.number()] = None;
        }
    }
    Some(book)
}

/// 書き出しの1ファイル。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Export {
    pub(crate) settings: Vec<(String, String)>,
    pub(crate) keys: Option<String>,
    pub(crate) presets: PresetBook,
    /// 単語帳のファイルの中身そのまま（`app_data::encode_words`の形）。
    pub(crate) words: Option<String>,
}

pub(crate) const EXPORT_EXTENSION: &str = "rfnexport";
const EXPORT_MAGIC: &str = "RFN-EDIT-EXPORT 1";
/// 区切りの行。**単語帳の行は1行1語で何でも書ける**ので、語として書かれることの
/// 無い形にしてある。
const SECTION: &str = "@@RFN-EDIT-SECTION ";

pub(crate) fn encode_export(export: &Export) -> String {
    let mut out = format!("{EXPORT_MAGIC}\n");
    out.push_str(&format!("{SECTION}settings\n"));
    out.push_str(&app_data::encode_settings(&export.settings));
    if let Some(keys) = &export.keys {
        out.push_str(&format!("{SECTION}keys\n"));
        out.push_str(&format!(
            "{KEYS_SETTING}: {}\n",
            keys.replace(['\n', '\r'], "")
        ));
    }
    out.push_str(&format!("{SECTION}presets\n"));
    out.push_str(&encode_presets(&export.presets));
    if let Some(words) = &export.words {
        out.push_str(&format!("{SECTION}words\n"));
        out.push_str(words);
        if !words.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// **どの区切りも中身が読めなければ`None`**——読めない塊を「空」として取り込むと、
/// その塊が持っていたものを黙って消すことになる。
pub(crate) fn decode_export(raw: &str) -> Option<Export> {
    let raw = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n");
    let mut lines = raw.split('\n');
    if lines.next()? != EXPORT_MAGIC {
        return None;
    }
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some(name) = line.strip_prefix(SECTION) {
            sections.push((name.trim().to_owned(), String::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    let mut export = Export::default();
    for (name, body) in sections {
        match name.as_str() {
            "settings" => export.settings = app_data::decode_settings(&body)?,
            "keys" => {
                let line = body.lines().next().unwrap_or_default();
                export.keys = Some(line.strip_prefix(&format!("{KEYS_SETTING}: "))?.to_owned());
            }
            "presets" => export.presets = decode_presets(&body)?,
            "words" => {
                app_data::decode_words(&body)?;
                export.words = Some(body);
            }
            // 後の版が足した塊は飛ばす。
            _ => {}
        }
    }
    Some(export)
}

pub(crate) fn read_presets(directory: &Path) -> PresetBook {
    std::fs::read_to_string(directory.join(PRESETS_FILE))
        .ok()
        .and_then(|raw| decode_presets(&raw))
        .unwrap_or_default()
}

fn write_presets(directory: &Path, book: &PresetBook) -> io::Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(PRESETS_FILE);
    file_io::write_atomically(&path, encode_presets(book).as_bytes())?;
    Ok(path)
}

thread_local! {
    /// 読んだプリセット。**窓は1つ**なので、単語帳（`WORD_MODES`）と同じくここに持つ。
    static PRESETS: RefCell<PresetBook> = RefCell::new(PresetBook::default());
}

fn presets_now() -> PresetBook {
    PRESETS.with(|held| held.borrow().clone())
}

/// 置き換えて、書いて、画面へ出す。
fn hold_presets(window: &AppWindow, live: &Live, book: PresetBook) {
    if let Some(directory) = app_data::app_directory()
        && let Err(error) = write_presets(&directory, &book)
    {
        live.cache
            .borrow_mut()
            .log_diag("spec", &format!("presets not saved error={error}"));
        window.tell(
            say!(
                "プリセットを書けません: {error}",
                "Cannot write the presets: {error}"
            )
            .into(),
        );
    }
    PRESETS.with(|held| *held.borrow_mut() = book);
    publish(window);
}

/// その面のいまの値。
pub(crate) fn current_values(window: &AppWindow, kind: PresetKind) -> Vec<(String, String)> {
    if kind == PresetKind::Keys {
        return vec![(
            KEYS_SETTING.to_owned(),
            window.get_shortcut_bindings().to_string(),
        )];
    }
    crate::settings_values(window)
        .into_iter()
        .filter(|(name, _)| kind_of(name) == Some(kind))
        .collect()
}

/// 名前の一覧、選んでいるプリセット、その値から変えたかを画面へ出す。
///
/// **選んでいるものが無ければ、いまの値に合うものを出す**（取り込んだあとや、選ぶ前に
/// 残したファイル）。それも無ければ空で、画面は「None」と出す。
pub(crate) fn publish(window: &AppWindow) {
    let book = presets_now();
    let mut current = Vec::new();
    let mut modified = Vec::new();
    for kind in PresetKind::ALL {
        let now: BTreeMap<String, String> = current_values(window, kind).into_iter().collect();
        let same =
            |preset: &Preset| preset.values.iter().cloned().collect::<BTreeMap<_, _>>() == now;
        let names: Vec<SharedString> = book
            .presets
            .iter()
            .filter(|preset| preset.kind == kind)
            .map(|preset| SharedString::from(preset.name.as_str()))
            .collect();
        let chosen = book.chosen[kind.number()]
            .as_deref()
            .and_then(|name| book.find(kind, name));
        let (name, changed) = match chosen {
            Some(preset) => (preset.name.clone(), !same(preset)),
            None => (
                book.presets
                    .iter()
                    .filter(|preset| preset.kind == kind)
                    .find(|preset| same(preset))
                    .map(|preset| preset.name.clone())
                    .unwrap_or_default(),
                false,
            ),
        };
        current.push(SharedString::from(name));
        modified.push(changed);
        let model = ModelRc::new(VecModel::from(names));
        match kind {
            PresetKind::Editor => window.set_editor_presets(model),
            PresetKind::Terminal => window.set_terminal_presets(model),
            PresetKind::Keys => window.set_keys_presets(model),
        }
    }
    window.set_preset_current(ModelRc::new(VecModel::from(current)));
    window.set_preset_modified(ModelRc::new(VecModel::from(modified)));
}

/// いまの値を、その名前のプリセットとして残す。**同じ面に同じ名前があれば上書き**。
pub(crate) fn save_as(window: &AppWindow, live: &Live, kind: PresetKind, name: &str) {
    let name = name.trim().replace(['\n', '\r', '\t'], " ");
    if name.is_empty() {
        return;
    }
    let values = current_values(window, kind);
    let mut book = presets_now();
    match book
        .presets
        .iter_mut()
        .find(|preset| preset.kind == kind && preset.name == name)
    {
        Some(preset) => preset.values = values,
        None => book.presets.push(Preset {
            kind,
            name: name.clone(),
            values,
        }),
    }
    // 残したものが、選んでいるものになる。
    book.chosen[kind.number()] = Some(name.clone());
    hold_presets(window, live, book);
    window.tell(
        say!(
            "プリセット「{name}」を保存しました",
            "Saved the preset \"{name}\""
        )
        .into(),
    );
}

/// 選んでいるプリセットを、いまの値で上書きする（「Save」）。
fn overwrite(window: &AppWindow, live: &Live, kind: PresetKind) {
    let chosen = window
        .get_preset_current()
        .row_data(kind.number())
        .unwrap_or_default();
    if !chosen.is_empty() {
        save_as(window, live, kind, &chosen);
    }
}

fn delete(window: &AppWindow, live: &Live, kind: PresetKind, name: &str) {
    let mut book = presets_now();
    book.presets
        .retain(|preset| !(preset.kind == kind && preset.name == name));
    if book.chosen[kind.number()].as_deref() == Some(name) {
        book.chosen[kind.number()] = None;
    }
    hold_presets(window, live, book);
}

/// そのプリセットの値を当てる。
fn choose(window: &AppWindow, live: &Live, kind: PresetKind, name: &str) {
    let mut book = presets_now();
    let Some(preset) = book.find(kind, name).cloned() else {
        return;
    };
    // **その面のものだけ**：手で書き換えられたファイルに他の面の値があっても当てない。
    let values: Vec<(String, String)> = preset
        .values
        .into_iter()
        .filter(|(name, _)| kind_of(name) == Some(kind))
        .collect();
    if kind == PresetKind::Keys {
        if let Some((_, keys)) = values.first() {
            apply_keys(window, live, keys);
        }
    } else {
        apply_values(window, live, &values);
    }
    // 選んだことを覚える——同じ値のプリセットが他にあっても、選んだほうを出す。
    book.chosen[kind.number()] = Some(name.to_owned());
    hold_presets(window, live, book);
}

fn apply_keys(window: &AppWindow, live: &Live, keys: &str) {
    window.set_shortcut_bindings(keys.into());
    window.set_shortcut_selected(-1);
    window.set_shortcut_edit("".into());
    crate::shortcuts::publish(window);
    crate::write_session(window, live);
}

/// 設定の値を画面に当てて、組み直し、書く。**起動のときと同じ`apply_settings`を通る**。
fn apply_values(window: &AppWindow, live: &Live, values: &[(String, String)]) {
    let numbers = window.get_sheet_numbers();
    let palette = window.get_palette();
    let fonts = window.get_sheet_fonts();
    let (Some(numbers), Some(palette), Some(fonts)) = (
        numbers.as_any().downcast_ref::<VecModel<i32>>(),
        palette.as_any().downcast_ref::<VecModel<slint::Color>>(),
        fonts.as_any().downcast_ref::<VecModel<SharedString>>(),
    ) else {
        return;
    };
    let language = window.get_language();
    crate::apply_settings(window, numbers, palette, fonts, values);
    // シェルの一覧と、開いているプロファイルの欄を出し直す（既定を選んだときと同じ）。
    crate::publish_shells(window);
    window.invoke_shell_profile_action(0, window.get_shell_profile_index());
    // 言語は**変わったときだけ**当て直す——翻訳はプロセスで1つなので、同じ値でも当て直せば
    // 「System」の解決をやり直すことになる。
    if window.get_language() != language {
        crate::i18n::apply(window.get_language());
        crate::publish_word_modes(window);
    }
    crate::shortcuts::publish(window);
    crate::match_ink_set(window);
    crate::show_wallpaper(window, &live.cache);
    crate::publish_tabs(window, live);
    crate::publish_left(window, live);
    window.invoke_terminal_woken();
    crate::relayout_panes(window, &live.states, &live.cache);
    crate::save_settings(window, &live.cache);
}

/// いまの姿の書き出し。
pub(crate) fn export_now(window: &AppWindow) -> Export {
    Export {
        settings: crate::settings_values(window),
        keys: Some(window.get_shortcut_bindings().to_string()),
        presets: presets_now(),
        words: crate::words_now_encoded(),
    }
}

/// 取り込む前の姿を控える場所。
pub(crate) fn backup_path(directory: &Path) -> PathBuf {
    directory.join(format!("before-import.{EXPORT_EXTENSION}"))
}

/// 読んだ書き出しで全体を置き換える。**その前に、いまの姿を控える**——控えを書けなければ
/// 取り込まない（戻れない置き換えをしない）。
pub(crate) fn import(window: &AppWindow, live: &Live, export: Export) -> io::Result<()> {
    if let Some(directory) = app_data::app_directory() {
        std::fs::create_dir_all(&directory)?;
        file_io::write_atomically(
            &backup_path(&directory),
            encode_export(&export_now(window)).as_bytes(),
        )
        .map(|_| ())?;
    }
    apply_values(window, live, &export.settings);
    if let Some(keys) = &export.keys {
        apply_keys(window, live, keys);
    }
    if let Some(words) = &export.words
        && let Some((stored, _)) = app_data::decode_words(words)
    {
        crate::take_word_modes(window, live, stored);
    }
    hold_presets(window, live, export.presets);
    Ok(())
}

/// 起動のとき：プリセットを読み、画面へ出し、操作をつなぐ。
pub(crate) fn wire(window: &AppWindow, live: &Live) {
    let presets = app_data::app_directory()
        .map(|directory| read_presets(&directory))
        .unwrap_or_default();
    PRESETS.with(|held| *held.borrow_mut() = presets);
    publish(window);

    let weak = window.as_weak();
    let held = live.clone();
    window.on_preset_overwrite_requested(move |kind| {
        if let (Some(window), Some(kind)) = (weak.upgrade(), PresetKind::from_number(kind)) {
            overwrite(&window, &held, kind);
        }
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_preset_chosen(move |kind, name| {
        if let (Some(window), Some(kind)) = (weak.upgrade(), PresetKind::from_number(kind)) {
            choose(&window, &held, kind, &name);
        }
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_preset_save_requested(move |kind| {
        if let (Some(window), Some(kind)) = (weak.upgrade(), PresetKind::from_number(kind)) {
            let suggested = window
                .get_preset_current()
                .row_data(kind.number())
                .unwrap_or_default();
            crate::ask_for_name(
                &window,
                &held,
                crate::Question::SavePreset(kind),
                pick(
                    "プリセットの名前\n\nいまの値をこの名前で残します。同じ名前があれば上書きします。",
                    "Preset name\n\nSaves the current values under this name, replacing a preset of the same name.",
                )
                .to_owned(),
                &suggested,
            );
        }
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_preset_delete_requested(move |kind, name| {
        if let (Some(window), Some(kind)) = (weak.upgrade(), PresetKind::from_number(kind)) {
            delete(&window, &held, kind, &name);
        }
    });

    // ダイアログは押した合図の外で開く（語群の書き出しと同じ）。
    let weak = window.as_weak();
    window.on_settings_export_requested(move || {
        let weak = weak.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                export_to_file(&window);
            }
        });
    });
    let weak = window.as_weak();
    let held = live.clone();
    window.on_settings_import_requested(move || {
        let weak = weak.clone();
        let held = held.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                ask_import(&window, &held);
            }
        });
    });
}

fn export_to_file(window: &AppWindow) {
    let Some(choice) = file_dialog::save_document_as(
        ime::window_handle(window),
        &format!("RFN Edit Settings.{EXPORT_EXTENSION}"),
        file_dialog::SaveFields::none(),
    ) else {
        return;
    };
    let text = encode_export(&export_now(window));
    match file_io::write_atomically(&choice.path, text.as_bytes()) {
        Ok(_) => window.tell(pick("設定を書き出しました", "Exported the settings").into()),
        Err(error) => window.tell(
            say!(
                "設定を書き出せません: {error}",
                "Cannot export the settings: {error}"
            )
            .into(),
        ),
    }
}

fn ask_import(window: &AppWindow, live: &Live) {
    let Some(path) = file_dialog::open_document_named(
        ime::window_handle(window),
        pick(
            "取り込む設定のファイルを選ぶ",
            "Choose a Settings File to Import",
        ),
    ) else {
        return;
    };
    let read = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| decode_export(&raw));
    let Some(export) = read else {
        window.tell(
            pick(
                "RFN Editの設定のファイルとして読めませんでした",
                "Could not read it as an RFN Edit settings file",
            )
            .into(),
        );
        return;
    };
    crate::ask_question(
        window,
        live,
        crate::Question::ImportSettings(Rc::new(export)),
        pick(
            "設定をすべて置き換えますか？\n\n設定・キー・プリセット・単語帳が、選んだファイルの中身に置き換わります。いまの設定は before-import.rfnexport に控えます。",
            "Replace all settings?\n\nSettings, keys, presets and word sets are replaced with the file's contents. The current ones are kept in before-import.rfnexport.",
        )
        .to_owned(),
        &[pick("置き換える", "Replace"), crate::cancel()],
        0,
    );
}

/// 問いに「置き換える」と答えた。
pub(crate) fn import_answered(window: &AppWindow, live: &Live, export: &Export) {
    let export = Export {
        settings: export.settings.clone(),
        keys: export.keys.clone(),
        presets: export.presets.clone(),
        words: export.words.clone(),
    };
    match import(window, live, export) {
        Ok(()) => window.tell(pick("設定を取り込みました", "Imported the settings").into()),
        Err(error) => window.tell(
            say!(
                "いまの設定を控えられないので、取り込みませんでした: {error}",
                "Did not import, because the current settings could not be kept: {error}"
            )
            .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_setting_belongs_to_one_page_or_none() {
        assert_eq!(kind_of("h.size"), Some(PresetKind::Editor));
        assert_eq!(kind_of("v.ink"), Some(PresetKind::Editor));
        assert_eq!(kind_of("wallpaper.path"), Some(PresetKind::Editor));
        assert_eq!(kind_of("terminal.font"), Some(PresetKind::Terminal));
        assert_eq!(kind_of("terminal.appearance.0"), Some(PresetKind::Terminal));
        assert_eq!(kind_of(KEYS_SETTING), Some(PresetKind::Keys));
        // 守りの設定・この機械のもの・General。
        assert_eq!(kind_of("work.autosave"), None);
        assert_eq!(kind_of("terminal.shell.0"), Some(PresetKind::Terminal));
        assert_eq!(kind_of("terminal.default"), Some(PresetKind::Terminal));
        assert_eq!(kind_of("language"), None);
        assert_eq!(kind_of("ink.set.1"), None);
        assert_eq!(kind_of("left.size"), None);
    }

    fn sample() -> PresetBook {
        let presets = vec![
            Preset {
                kind: PresetKind::Editor,
                name: "執筆用".into(),
                values: vec![
                    ("h.size".into(), "18".into()),
                    ("h.paper".into(), "#fffff0".into()),
                ],
            },
            Preset {
                kind: PresetKind::Keys,
                name: "Emacs風".into(),
                values: vec![(KEYS_SETTING.into(), "0=Ctrl+Alt+O;3=Ctrl+Q".into())],
            },
        ];
        PresetBook {
            presets,
            chosen: [Some("執筆用".into()), None, None],
        }
    }

    #[test]
    fn presets_come_back_as_written() {
        let book = sample();
        assert_eq!(decode_presets(&encode_presets(&book)), Some(book));
        assert_eq!(decode_presets("something else\n"), None);
        // 無いプリセットを指す「選んでいる」は忘れる。
        let raw = format!("{PRESETS_MAGIC}\nchosen: terminal\t黒\n");
        assert_eq!(decode_presets(&raw).unwrap().chosen, [None, None, None]);
    }

    #[test]
    fn an_unreadable_preset_heading_takes_its_values_with_it() {
        let raw = format!(
            "{PRESETS_MAGIC}\npreset: nowhere\tx\nh.size: 1\npreset: editor\t読み返し用\nh.size: 20\n"
        );
        let presets = decode_presets(&raw).unwrap().presets;
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].values, vec![("h.size".into(), "20".into())]);
    }

    #[test]
    fn an_export_carries_every_part() {
        let words = "RFN-EDIT-WORDS 3\nnext: 3\nmode: 1 | 小説\ngroup: 2 | 人物 | #336699\nリオン\n[words]\n";
        let export = Export {
            settings: vec![
                ("h.size".into(), "18".into()),
                ("language".into(), "1".into()),
            ],
            keys: Some("0=Ctrl+Alt+O".into()),
            presets: sample(),
            words: Some(words.into()),
        };
        let written = encode_export(&export);
        let read = decode_export(&written).expect("reads back");
        assert_eq!(read.settings, export.settings);
        assert_eq!(read.keys, export.keys);
        assert_eq!(read.presets, export.presets);
        // 単語帳は中身のまま戻る（語に`[words]`があっても区切りにならない）。
        assert_eq!(
            app_data::decode_words(read.words.as_deref().unwrap()),
            app_data::decode_words(words)
        );
        // Windowsの改行とBOMで保存し直されても読める。
        let crlf = format!("\u{feff}{}", written.replace('\n', "\r\n"));
        assert_eq!(decode_export(&crlf).unwrap().presets, export.presets);
    }

    #[test]
    fn an_export_with_a_broken_part_is_refused() {
        assert_eq!(decode_export("RFN-EDIT-SETTINGS 1\n"), None);
        let broken = format!("{EXPORT_MAGIC}\n{SECTION}presets\nnot presets\n");
        assert_eq!(decode_export(&broken), None);
    }
}
