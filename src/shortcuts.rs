//! 設定のKeys（キーの割り当て）。RFN01-114: **割り当てられる操作はアクションの表
//! （`crate::actions`）の行で、保存はIDで書く**。ここに操作の名前の表は持たない。
use crate::actions::{self, Category, Scope};
use crate::menu_commands::Command;
use crate::{AppWindow, Live, ShortcutItem};
use slint::{ComponentHandle, ModelRc, VecModel};

/// 変えられないキー。**同じ一覧に並べる**——「このキーは何か」を探す書き手に
/// とって、変えられるかどうかは探したあとの話である。
const FIXED: &[(Category, (&str, &str), &str)] = &[
    (
        Category::Terminal,
        ("選択した文字をコピー", "Copy Selected Text"),
        "Ctrl+Shift+C",
    ),
    (Category::Terminal, ("貼り付け", "Paste"), "Ctrl+V"),
    (
        Category::Terminal,
        ("下段を表示・閉じる", "Show or Hide Strip Below"),
        "Ctrl+`",
    ),
    (
        Category::Terminal,
        ("下段から本文へ戻る", "Back to Text from Strip Below"),
        "Ctrl+Alt+Up",
    ),
    (
        Category::Edit,
        ("コピー（入力欄）", "Copy (in a field)"),
        "Ctrl+C",
    ),
    (
        Category::Edit,
        ("切り取り（入力欄）", "Cut (in a field)"),
        "Ctrl+X",
    ),
    (
        Category::Edit,
        ("貼り付け（入力欄）", "Paste (in a field)"),
        "Ctrl+V",
    ),
    (
        Category::Edit,
        ("全選択（入力欄）", "Select All (in a field)"),
        "Ctrl+A",
    ),
];

/// 日本語と英語の対から、いまの言語のほうを。
fn shown((japanese, english): (&'static str, &'static str)) -> &'static str {
    crate::i18n::pick(japanese, english)
}

/// 割り当ての一覧のid（アクションの表の中の位置）の操作。
fn command_at(id: usize) -> Option<&'static Command> {
    actions::all().get(id)?.command.as_ref()
}
/// 追加要件 2026-09-14: 設定のTABが前にあるときに通す操作。**TABとペインを移る・閉じる・
/// 開く**だけで、本文に効くもの（保存・置換・Undo・字の編集）は通さない——設定のTABの下に
/// あるのは代役の空文書で、保存すれば空の無題がディスクへ出ていく。**検索は通す**（書き手の
/// 報告 2026-09-15）。設定のTABでは検索欄が「設定を探す」欄になる。
fn passes_settings(command: i32) -> bool {
    usize::try_from(command)
        .ok()
        .and_then(command_at)
        .is_some_and(|command| {
            !matches!(
                command,
                // 本文に効くもの。左ペイン・設定・パレットなど、文書を持たない操作は通す。
                Command::Save(_)
                    | Command::Find(true)
                    | Command::Goto(_)
                    | Command::Undo(_)
                    | Command::Line(_)
                    | Command::Mark(_)
                    | Command::Kill(_)
                    | Command::Insert(_)
                    | Command::List(..)
                    | Command::DeleteNext
                    | Command::WordMove(..)
                    | Command::ScrollLine(_)
                    | Command::SelectLine
                    | Command::DirectionToggle
                    | Command::Preview
                    | Command::Viewer
                    | Command::Print
                    | Command::Below
                    | Command::CopyBody
                    | Command::Rename
                    | Command::Duplicate
                    | Command::CopyPath
                    | Command::Reveal
                    | Command::Encoding(_)
                    | Command::Compare(_)
                    | Command::Spelling
                    | Command::SpellingStep(_)
                    | Command::ReadAloud
                    | Command::Paper(_)
                    | Command::Direction(_)
                    | Command::Display(_)
                    | Command::Word(_)
                    | Command::WordModePicker
                    | Command::CloseOthers(_)
                    | Command::CloseClean
                    | Command::SaveAll
                    | Command::Split(_)
                    | Command::OtherPanes
                    | Command::Swap(_)
                    | Command::Zoom(_)
                    | Command::ZoomReset
                    | Command::Terminal(_)
                    | Command::Panel(_)
                    | Command::Appearance(_)
                    | Command::Above
                    | Command::CloseStripTab
            )
        })
}
/// 変更できる操作の数——アクションの表の行の数。
fn count() -> usize {
    actions::all().len()
}
fn name_at(id: usize) -> (&'static str, &'static str) {
    let action = &actions::all()[id];
    (action.title.0.as_str(), action.title.1.as_str())
}
fn default_at(id: usize) -> &'static str {
    actions::all()[id].key
}
fn category_at(id: usize) -> Category {
    actions::all()[id].category
}
const TREE_SCOPE: i32 = 2;

/// どこでキーを受けるか。
fn scope_at(id: usize) -> i32 {
    // RFN01-114: キーが固定の行は、割り当ての場面に入れない（resolveも重なりの検査も見ない）。
    if actions::all()[id].fixed {
        return -1;
    }
    match actions::all()[id].scope {
        Scope::Main => 0,
        Scope::QuickDraft => 1,
        Scope::Tree => TREE_SCOPE,
    }
}
/// いまの言語の名前（Keysの行、知らせ）。
pub(crate) fn name(id: usize) -> &'static str {
    shown(name_at(id))
}

/// 割り当てられない組み合わせ。
///
/// **固定の基本編集**（要件 11.1）と、**窓とアプリが先に使う鍵**——`F3`／
/// `Shift+F3`は本文の「次／前の一致」（要件 11.2）、`Alt+Tab`などは窓が先に取る。
const TAKEN: &[&str] = &[
    "Ctrl+C",
    "Ctrl+X",
    "Ctrl+V",
    "Ctrl+A",
    "Ctrl+Shift+C",
    "Alt+Tab",
    "Alt+Shift+Tab",
    "Alt+F4",
    "Alt+Space",
    "F3",
    "Shift+F3",
];
/// 綴りから、キー1つ分の名前にする。**綴りは`chord`と揃える。**
fn key_token(token: &str) -> Option<String> {
    let upper = token.trim().to_ascii_uppercase();
    if upper.len() == 1 && upper.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(upper);
    }
    if let Some(number) = upper.strip_prefix('F')
        && let Ok(number) = number.parse::<u8>()
        && (1..=12).contains(&number)
    {
        return Some(format!("F{number}"));
    }
    let named = match upper.as_str() {
        "TAB" => "Tab",
        "SPACE" => "Space",
        "`" => "`",
        "," => ",",
        "\\" => "\\",
        "LEFT" => "Left",
        "RIGHT" => "Right",
        "UP" => "Up",
        "DOWN" => "Down",
        "ENTER" => "Enter",
        "DELETE" => "Delete",
        _ => return None,
    };
    Some(named.to_owned())
}
/// F1〜F12の綴りか。**`F`1文字の鍵と混ざらないよう**、数も見る。
fn is_function(name: &str) -> bool {
    name.strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=12).contains(&number))
}
/// 押された鍵がF1〜F12なら、その名前。
///
/// **Slintは名前付きの鍵を私用面の文字で渡す**ので、綴りではなく鍵そのもので
/// 見分ける（[`modifier_only`]と同じ考え方）。
fn function_key(text: &str) -> Option<&'static str> {
    use slint::platform::Key;
    const KEYS: [(Key, &str); 12] = [
        (Key::F1, "F1"),
        (Key::F2, "F2"),
        (Key::F3, "F3"),
        (Key::F4, "F4"),
        (Key::F5, "F5"),
        (Key::F6, "F6"),
        (Key::F7, "F7"),
        (Key::F8, "F8"),
        (Key::F9, "F9"),
        (Key::F10, "F10"),
        (Key::F11, "F11"),
        (Key::F12, "F12"),
    ];
    KEYS.iter().find_map(|(key, name)| {
        let value: slint::SharedString = (*key).into();
        (value.as_str() == text).then_some(*name)
    })
}
#[cfg(test)]
fn normalize(value: &str) -> Option<String> {
    normalize_for(value, 0)
}

fn bare_tree_key(scope: i32, key: &str, shift: bool) -> bool {
    scope == TREE_SCOPE && !shift && matches!(key, "Enter" | "Delete" | "Left" | "Right" | "Up" | "Down")
}

/// 保存する綴りにそろえる。裸の操作キーはツリーに限り、固定キーは割り当てない。
fn normalize_for(value: &str, scope: i32) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    let upper = value.trim().to_ascii_uppercase();
    let mut parts: Vec<&str> = upper.split('+').collect();
    let key = key_token(parts.pop()?)?;
    let (mut control, mut alt, mut shift) = (false, false, false);
    for part in parts {
        match part {
            "CTRL" if !control => control = true,
            "ALT" if !alt => alt = true,
            "SHIFT" if !shift => shift = true,
            _ => return None,
        }
    }
    if scope != TREE_SCOPE && matches!(key.as_str(), "Enter" | "Delete") {
        return None;
    }
    if !control && !alt && !is_function(&key) && !bare_tree_key(scope, &key, shift) {
        return None;
    }
    let written = format!(
        "{}{}{}{key}",
        if control { "Ctrl+" } else { "" },
        if alt { "Alt+" } else { "" },
        if shift { "Shift+" } else { "" }
    );
    (!TAKEN.contains(&written.as_str())).then_some(written)
}
fn modifier_only(text: &str) -> bool {
    use slint::platform::Key;
    [
        Key::Shift,
        Key::ShiftR,
        Key::Control,
        Key::ControlR,
        Key::Alt,
        Key::AltGr,
        Key::Meta,
        Key::MetaR,
        Key::CapsLock,
    ]
    .iter()
    .any(|key| {
        let value: slint::SharedString = (*key).into();
        value.as_str() == text
    })
}
#[cfg(test)]
fn chord(text: &str, control: bool, alt: bool, shift: bool) -> Option<String> {
    chord_for(text, control, alt, shift, 0)
}

fn chord_for(text: &str, control: bool, alt: bool, shift: bool, scope: i32) -> Option<String> {
    use slint::platform::Key;
    let special = [
        (Key::Tab, "Tab"),
        (Key::LeftArrow, "Left"),
        (Key::RightArrow, "Right"),
        (Key::UpArrow, "Up"),
        (Key::DownArrow, "Down"),
        (Key::Return, "Enter"),
        (Key::Delete, "Delete"),
    ];
    let key = if let Some((_, name)) = special.iter().find(|(key, _)| {
        let value: slint::SharedString = (*key).into();
        value.as_str() == text
    }) {
        (*name).to_owned()
    } else if let Some(name) = function_key(text) {
        name.to_owned()
    } else {
        let mut chars = text.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        match c {
            ' ' => "Space".to_owned(),
            '`' => "`".to_owned(),
            ',' => ",".to_owned(),
            '\\' => "\\".to_owned(),
            _ if c.is_ascii_alphanumeric() => c.to_ascii_uppercase().to_string(),
            _ => return None,
        }
    };
    if scope != TREE_SCOPE && matches!(key.as_str(), "Enter" | "Delete") {
        return None;
    }
    if !control && !alt && !is_function(&key) && !bare_tree_key(scope, &key, shift) {
        return None;
    }
    Some(format!(
        "{}{}{}{key}",
        if control { "Ctrl+" } else { "" },
        if alt { "Alt+" } else { "" },
        if shift { "Shift+" } else { "" }
    ))
}
fn conflict(keys: &[String], i: usize, key: &str) -> Option<usize> {
    if key.is_empty() {
        return None;
    }
    keys.iter()
        .enumerate()
        .position(|(other, value)| other != i && scope_at(other) == scope_at(i) && value == key)
}
/// 保存するキー割り当ての形式。
const BINDINGS_VERSION: u32 = 5;

/// 保存する形。**IDで書く**（RFN01-114）。[`BINDINGS_VERSION`]を添える。
fn encode(keys: &[String]) -> String {
    actions::all()
        .iter()
        .zip(keys)
        .map(|(action, k)| format!("{}={k}", action.id))
        .chain(std::iter::once(format!("v={BINDINGS_VERSION}")))
        .collect::<Vec<_>>()
        .join(";")
}
fn bindings(raw: &str) -> Vec<String> {
    let mut keys: Vec<_> = (0..count()).map(|i| default_at(i).to_string()).collect();
    for entry in raw.split(';') {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if let Some(at) = actions::index_of(name)
            && let Some(key) = normalize_for(value, scope_at(at))
        {
            keys[at] = key;
        }
    }
    if keys
        .iter()
        .enumerate()
        .any(|(i, k)| conflict(&keys, i, k).is_some())
    {
        return (0..count()).map(|i| default_at(i).to_string()).collect();
    }
    keys
}

pub(crate) fn label(app: &AppWindow, id: usize) -> String {
    bindings(&app.get_shortcut_bindings())
        .get(id)
        .cloned()
        .unwrap_or_default()
}
/// その操作に、いま割り当てているキー（メニューの行に併記する）。
pub(crate) fn label_of(app: &AppWindow, command: &Command) -> String {
    actions::all()
        .iter()
        .position(|action| action.command.as_ref() == Some(command))
        .map(|id| label(app, id))
        .unwrap_or_default()
}
// -1 is unhandled; -2 consumes a removed default so legacy handlers cannot run it.
pub fn resolve(raw: &str, scope: i32, text: &str, control: bool, alt: bool, shift: bool) -> i32 {
    // Slint uses control characters for named keys. In particular Alt is
    // U+0012 and Shift is U+0010, not Ctrl+R and Ctrl+P. Consume them before
    // either configured shortcuts or older UI handlers see them as letters.
    if modifier_only(text) {
        return -2;
    }
    let Some(key) = chord_for(text, control, alt, shift, scope) else {
        return -1;
    };
    if let Some(i) = bindings(raw)
        .iter()
        .enumerate()
        .position(|(i, k)| scope_at(i) == scope && k == &key)
    {
        return i as i32;
    }
    if (0..count()).any(|i| scope_at(i) == scope && default_at(i) == key) {
        -2
    } else {
        -1
    }
}

/// The Keys page's list: a heading row per group, and under an open one its
/// keys (書き手の求め 2026-09-15).
///
/// **One flat list**, like the tree: the window draws rows and the rows say
/// what they are. While the writer is searching — by name or by pressing a key
/// — every group with something to show is open and the others are left out,
/// because a fold hiding the one match is a search that found nothing.
pub fn publish(app: &AppWindow) {
    let keys = bindings(&app.get_shortcut_bindings());
    // 書き手の報告 2026-11-05: 設定の項目と同じそろえ方で（全角・半角、大文字・小文字）。
    let query = crate::fold_for_search(app.get_shortcut_query().trim());
    let key_query = app.get_shortcut_key_query().to_string();
    let folded = app.get_shortcut_folded();
    let searching = !query.is_empty() || !key_query.is_empty();
    let matches = |name: &str, key: &str| {
        (query.is_empty()
            || crate::fold_for_search(name).contains(&query)
            || crate::fold_for_search(key).contains(&query))
            && (key_query.is_empty() || key == key_query)
    };
    let mut rows = Vec::new();
    for kind in Category::ALL {
        let category = kind.index();
        let only = app.get_shortcut_only();
        let changeable = (0..count())
            .filter(|i| category_at(*i) == kind)
            .filter(|i| only < 0 || *i as i32 == only)
            .map(|i| {
                let fixed = actions::all()[i].fixed;
                (
                    if fixed { -1 } else { i as i32 },
                    shown(name_at(i)),
                    keys[i].as_str(),
                    fixed,
                )
            });
        let fixed = FIXED
            .iter()
            .filter(|_| only < 0)
            .filter(|(owner, _, _)| *owner == kind)
            .map(|(_, name, key)| (-1, shown(*name), *key, true));
        let items: Vec<ShortcutItem> = changeable
            .chain(fixed)
            .filter(|(_, name, key, _)| matches(name, key))
            .map(|(id, name, key, fixed)| ShortcutItem {
                id,
                name: name.into(),
                key: key.into(),
                header: false,
                category,
                open: true,
                fixed,
                note: Default::default(),
                count: 0,
            })
            .collect();
        if searching && items.is_empty() {
            continue;
        }
        let open = searching || folded & (1 << category) == 0;
        rows.push(ShortcutItem {
            id: -1,
            name: shown(kind.title()).into(),
            key: Default::default(),
            header: true,
            category,
            open,
            fixed: false,
            note: shown(kind.note()).into(),
            count: items.len() as i32,
        });
        if open {
            rows.extend(items);
        }
    }
    app.set_shortcut_rows(ModelRc::new(VecModel::from(rows)));
    app.set_shortcut_keys(ModelRc::new(VecModel::from(
        keys.into_iter()
            .map(Into::into)
            .collect::<Vec<slint::SharedString>>(),
    )));
    // RFN01-114: ⋮・右クリックの行に併記するキーも読み直させる。
    let actions = app.global::<crate::MenuActions>();
    actions.set_keys_generation(actions.get_keys_generation() + 1);
    // キーのプリセット（RFN01-31）がいまの割り当てに合うか。
    crate::settings_transfer::publish(app);
}
pub fn wire(app: &AppWindow, live: &Live) {
    publish(app);
    app.on_shortcut_is_tree(|id| usize::try_from(id).is_ok_and(|id| id < count() && scope_at(id) == TREE_SCOPE));
    let weak = app.as_weak();
    app.on_shortcut_filter(move || {
        if let Some(app) = weak.upgrade() {
            publish(&app);
        }
    });
    // 書き手の求め 2026-09-15: 分類を畳む・開く。
    let weak = app.as_weak();
    app.on_shortcut_fold(move |category| {
        if let Some(app) = weak.upgrade() {
            app.set_shortcut_folded(app.get_shortcut_folded() ^ (1 << category.clamp(0, 30)));
            publish(&app);
        }
    });
    // 書き手の求め 2026-09-15: **キーを押して探す。**「CTRL+O」と打つと綴りを
    // 間違えるので、割り当てるときと同じように押したキーをそのまま取り込む。
    let weak = app.as_weak();
    app.on_shortcut_key_search(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        if modifier_only(&text) {
            return;
        }
        let Some(key) = chord_for(&text, control, alt, shift, TREE_SCOPE) else {
            return;
        };
        app.set_shortcut_key_query(key.into());
        publish(&app);
    });
    let weak = app.as_weak();
    app.on_shortcut_capture(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        // Modifier-only events do not overwrite the displayed combination.
        if modifier_only(&text) {
            return;
        }
        let i = app.get_shortcut_selected() as usize;
        if i >= count() {
            return;
        }
        let scope = scope_at(i);
        let key = chord_for(&text, control, alt, shift, scope);
        if key.is_none() && text.chars().any(|c| c as u32 >= 0xE000) {
            return;
        }
        let key = key.unwrap_or_default();
        app.set_shortcut_edit(if key.is_empty() {
            shown(("使用不可", "Not available")).into()
        } else {
            key.clone().into()
        });
        app.set_shortcut_status(
            if !key.is_empty() && normalize_for(&key, scope).is_some() {
                shown((
                    "キーを受け付けました。「適用して保存」で確定します。",
                    "Key accepted. Click \"Apply and Save\" to confirm.",
                ))
            } else {
                shown((
                    "修飾キー付き（Ctrl／Alt／Shift）かF1〜F12を押してください",
                    "Press a key with Ctrl, Alt or Shift, or one of F1–F12",
                ))
            }
            .into(),
        );
    });
    let weak = app.as_weak();
    let saved_live = live.clone();
    app.on_shortcut_save(move |reset| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let i = app.get_shortcut_selected() as usize;
        if i >= count() {
            return;
        }
        // **既定へ戻すときは綴りを確かめない**——既定には、書き手が自分では選べない
        // キー（`Ctrl+Shift+C`、`TAKEN`）もある。
        let typed = if reset {
            Some(default_at(i).to_owned())
        } else {
            normalize_for(&app.get_shortcut_edit(), scope_at(i))
        };
        let Some(key) = typed else {
            app.set_shortcut_status(
                shown((
                    "修飾キー付き（Ctrl／Alt／Shift）かF1〜F12を指定してください",
                    "Use a key with Ctrl, Alt or Shift, or one of F1–F12",
                ))
                .into(),
            );
            return;
        };
        let mut keys = bindings(&app.get_shortcut_bindings());
        if let Some(other) = conflict(&keys, i, &key) {
            let name = shown(name_at(other));
            let told = crate::say!("{name}に割り当て済みです", "Already assigned to {name}");
            app.set_shortcut_status(told.into());
            return;
        }
        keys[i] = key.clone();
        app.set_shortcut_bindings(encode(&keys).into());
        app.set_shortcut_edit(key.into());
        publish(&app);
        crate::save_settings(&app, &saved_live.cache);
        let name = shown(name_at(i));
        let told = crate::say!("{name}の設定を保存しました", "Saved the key for {name}");
        app.set_shortcut_status(told.into());
    });
    // 書き手の求め 2026-09-15: Keys の面の Reset。割り当てを全部既定へ。
    let weak = app.as_weak();
    let reset_live = live.clone();
    app.on_shortcut_reset_all(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        app.set_shortcut_bindings("".into());
        app.set_shortcut_selected(-1);
        app.set_shortcut_edit("".into());
        publish(&app);
        crate::save_settings(&app, &reset_live.cache);
        app.set_shortcut_status(
            shown((
                "すべてのキーを既定に戻しました",
                "Restored all keys to their defaults",
            ))
            .into(),
        );
    });
    // RFN01-75/76: from a field of the left pane, only the left pane's own
    // operations — as assigned, so a key the writer moved works there too.
    let weak = app.as_weak();
    let field_live = live.clone();
    app.on_left_field_shortcut(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let command = resolve(&app.get_shortcut_bindings(), 0, &text, control, alt, shift);
        let Some(found) = usize::try_from(command).ok().and_then(command_at) else {
            return false;
        };
        match found {
            // RFN01-66 第3弾: キーやメニューと同じ1つの入口。
            Command::FolderFind => crate::open_folder_search(&app, &field_live, false, None),
            Command::FolderReplace => crate::open_folder_search(&app, &field_live, true, None),
            Command::ExplorePane => app.invoke_focus_explore(),
            Command::ToggleLeftPane => app.invoke_toggle_left_pane(),
            Command::ShowLeftTab(tab) => app.invoke_show_left_tab(*tab),
            _ => return false,
        }
        true
    });
    // RFN01-98（書き手の求め 2026-11-04）: **a shell gets every key but the
    // window's few** (as in VS Code). Whatever those are bound to now, so a
    // writer who moved one takes it with them.
    //
    // RFN01-102（同日）: the palette, New File, and Next/Previous Tab — the way
    // out of a shell by keys when the window is not split. Everything else
    // stays the shell's (Ctrl+W deletes a word in bash); the palette reaches
    // the rest.
    let weak = app.as_weak();
    let palette_live = live.clone();
    app.on_terminal_shortcut_key(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let command = resolve(&app.get_shortcut_bindings(), 0, &text, control, alt, shift);
        if !passes_terminal(command) {
            return false;
        }
        let pane = crate::focused_pane(&app);
        let weak = weak.clone();
        let live = palette_live.clone();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(app) = weak.upgrade() {
                run_command(&app, &live, pane, command);
            }
        });
        true
    });
    let weak = app.as_weak();
    let tree_live = live.clone();
    app.on_tree_shortcut(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let bindings = app.get_shortcut_bindings();
        let id = resolve(&bindings, TREE_SCOPE, &text, control, alt, shift);
        if id == -2 {
            return true;
        }
        if let Some(Command::Tree(action)) = usize::try_from(id).ok().and_then(command_at) {
            crate::tree_actions::queue(&app, &tree_live, crate::menu_commands::Target::for_tree(&app, &tree_live), *action);
            return true;
        }
        app.invoke_shortcut_key(text, control, alt, shift)
    });
    let weak = app.as_weak();
    let overlay_live = live.clone();
    app.on_panel_shortcut(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else { return false };
        let id = resolve(&app.get_shortcut_bindings(), 0, &text, control, alt, shift);
        if let Some(Command::PanelAction(action)) = usize::try_from(id).ok().and_then(command_at) {
            crate::panel_actions::queue(&app, &overlay_live, *action);
            return true;
        }
        false
    });
    let weak = app.as_weak();
    let keyed_live = live.clone();
    app.on_shortcut_key(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let command = resolve(&app.get_shortcut_bindings(), 0, &text, control, alt, shift);
        if command == -1 {
            return false;
        }
        if command == -2 {
            return true;
        }
        if let Some(Command::PanelAction(action)) = usize::try_from(command).ok().and_then(command_at) {
            crate::panel_actions::queue(&app, &keyed_live, *action);
            return true;
        }
        // RFN01-67（書き手の求め 2026-10-03）: Git Repository の画面では「戻る」で画面を閉じる。
        // ほかのショートカットは、下に隠れているペインへは届けない。
        if app.get_git_repo_active() {
            if usize::try_from(command).ok().and_then(command_at) == Some(&Command::Navigate(false))
            {
                let weak = weak.clone();
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    if let Some(app) = weak.upgrade() {
                        crate::git_repo_ui::close(&app);
                    }
                });
                return true;
            }
            return false;
        }
        let pane = crate::focused_pane(&app);
        // RFN01-82: **縦書きのCtrl+↑↓は矢印のまま**——画面の上下へ動かす先が無い。
        if usize::try_from(command)
            .ok()
            .and_then(command_at)
            .is_some_and(|command| matches!(command, Command::ScrollLine(_)))
            && pane.vertical(&app)
        {
            return false;
        }
        let on_settings = keyed_live
            .tabs
            .borrow()
            .of(pane)
            .current()
            .is_some_and(|tab| tab.settings);
        if on_settings && !passes_settings(command) {
            return true;
        }
        let weak = weak.clone();
        let live = keyed_live.clone();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(app) = weak.upgrade() {
                run_command(&app, &live, pane, command);
            }
        });
        true
    });
}
/// RFN01-98・102: the commands a shell does not get first — the palette, New
/// File, Next Tab and Previous Tab.
fn passes_terminal(command: i32) -> bool {
    usize::try_from(command)
        .ok()
        .and_then(command_at)
        .is_some_and(|command| {
            matches!(
                command,
                Command::Step(_) | Command::CommandPalette(_) | Command::NewFile
            )
        })
}

/// 割り当ての一覧のid（アクションの表の位置）で、その操作を走らせる。キーからも、コマンド
/// パレットからも（RFN01-87）——**同じ道**（`menu_commands::run`）で、前提もそこで確かめる。
pub(crate) fn run_command(app: &AppWindow, live: &Live, pane: crate::PaneId, command: i32) {
    let Ok(id) = usize::try_from(command) else {
        return;
    };
    crate::menu_commands::run_key(app, live, pane, id, name(id));
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu_commands::InsertShape;
    /// 表の位置（IDから）。
    fn at(id: &str) -> usize {
        actions::index_of(id).unwrap_or_else(|| panic!("{id} is not listed"))
    }
    fn hit(id: &str) -> i32 {
        at(id) as i32
    }

    /// RFN01-114: **保存はIDで書き、読み戻せる。**並びを変えても割り当ては動かない。
    #[test]
    fn bindings_are_saved_by_id() {
        let mut keys = bindings("");
        keys[at("file.save")] = "Ctrl+Alt+S".into();
        let written = encode(&keys);
        assert!(written.contains("file.save=Ctrl+Alt+S"));
        assert!(written.ends_with(&format!("v={BINDINGS_VERSION}")));
        assert_eq!(bindings(&written)[at("file.save")], "Ctrl+Alt+S");
        // 知らないIDは読み飛ばす（ほかの割り当ては残る）。
        let with_unknown = format!("gone.action=Ctrl+Alt+G;{written}");
        assert_eq!(bindings(&with_unknown)[at("file.save")], "Ctrl+Alt+S");
    }

    #[test]
    fn numeric_bindings_are_ignored() {
        assert_eq!(bindings("1=Ctrl+Alt+S;v=4"), bindings(""));
    }

    #[test]
    fn invalid_keys_are_rejected_and_colliding_bindings_restore_defaults() {
        assert_eq!(normalize("ctrl+alt+s").as_deref(), Some("Ctrl+Alt+S"));
        assert!(normalize("S").is_none());
        assert!(normalize("Ctrl+A").is_none());
        // A collision with another action's default invalidates the whole saved map.
        assert_eq!(
            bindings("file.save=Ctrl+O;view.back=Ctrl+Alt+B;v=5"),
            bindings("")
        );
    }

    #[test]
    fn scopes_allow_reuse_but_body_categories_conflict() {
        let raw = "file.save=Ctrl+Alt+S;draft.copy-all=Ctrl+Alt+S;v=5";
        let keys = bindings(raw);
        assert_eq!(keys[at("file.save")], "Ctrl+Alt+S");
        assert_eq!(keys[at("draft.copy-all")], "Ctrl+Alt+S");
        assert_eq!(resolve(raw, 0, "s", true, true, false), hit("file.save"));
        assert_eq!(
            resolve(raw, 1, "s", true, true, false),
            hit("draft.copy-all")
        );
        assert_eq!(
            conflict(&keys, at("edit.kill-line"), "Ctrl+Alt+S"),
            Some(at("file.save"))
        );
        assert!(conflict(&keys, at("view.quick-draft"), "").is_none());
    }

    #[test]
    fn modified_emacs_and_tab_keys_consume_the_old_binding() {
        let raw = "edit.kill-line=Ctrl+Alt+K;draft.copy-all=Ctrl+Alt+C;v=5";
        assert_eq!(resolve(raw, 0, "k", true, false, false), -2);
        assert_eq!(
            resolve(raw, 0, "k", true, true, false),
            hit("edit.kill-line")
        );
        assert_eq!(resolve(raw, 1, "C", true, false, true), -2);
        assert_eq!(
            resolve(raw, 1, "c", true, true, false),
            hit("draft.copy-all")
        );
        let tab: slint::SharedString = slint::platform::Key::Tab.into();
        assert_eq!(
            resolve("", 0, &tab, true, false, true),
            hit("view.previous-tab")
        );
        // RFN01-63: F8／Shift+F8は既定で次／前のスペルミス。
        let f8: slint::SharedString = slint::platform::Key::F8.into();
        assert_eq!(
            resolve("", 0, &f8, false, false, false),
            hit("run.next-spelling")
        );
        assert_eq!(
            resolve("", 0, &f8, false, false, true),
            hit("run.previous-spelling")
        );
        assert_eq!(
            chord(" ", true, false, true).as_deref(),
            Some("Ctrl+Shift+Space")
        );
        for fixed in ["Ctrl+C", "Ctrl+X", "Ctrl+V", "Ctrl+A"] {
            assert!(normalize(fixed).is_none());
        }
        assert_eq!(resolve("", 1, "k", true, false, false), -1);
        // キーが固定の行（コピーなど）は、割り当ての引き当てに出てこない。
        assert_eq!(resolve("", 0, "c", true, false, false), -1);
    }

    #[test]
    fn modifiers_do_not_become_letters() {
        use slint::platform::Key;
        let settings = "layout.toggle-direction=Ctrl+Alt+R;view.toggle-preview=Ctrl+Alt+P;v=5";
        for key in [
            Key::Shift,
            Key::ShiftR,
            Key::Control,
            Key::ControlR,
            Key::Alt,
            Key::AltGr,
            Key::Meta,
            Key::MetaR,
            Key::CapsLock,
        ] {
            let text: slint::SharedString = key.into();
            for scope in [0, 1] {
                assert_eq!(resolve(settings, scope, &text, true, true, false), -2);
                assert!(chord(&text, true, true, false).is_none());
            }
        }
        assert_eq!(
            resolve(settings, 0, "r", true, true, false),
            hit("layout.toggle-direction")
        );
        assert_eq!(
            resolve(settings, 0, "p", true, true, false),
            hit("view.toggle-preview")
        );
        // RFN01-87: Ctrl+Rは縦書き・横書きの既定。書き手が別のキーへ移したあとは、
        // 古い処理へ回さずに飲み込む（`-2`、既定のキーの扱いと同じ）。
        assert_eq!(resolve(settings, 0, "r", true, false, false), -2);
        assert_eq!(
            resolve("", 0, "r", true, false, false),
            hit("layout.toggle-direction")
        );
    }

    /// RFN01-114: **表の操作はすべて割り当てられる**（キーが固定の行と、既定の空の行を含め）。
    /// 割り当てれば効き、割り当てていなければ効かない。
    #[test]
    fn every_action_can_take_a_key() {
        for (id, action) in actions::all().iter().enumerate() {
            if action.fixed {
                continue;
            }
            let scope = scope_at(id);
            let mut keys = bindings("");
            for key in keys.iter_mut() {
                if key == "Ctrl+Alt+Shift+F12" {
                    key.clear();
                }
            }
            keys[id] = "Ctrl+Alt+Shift+F12".into();
            let raw = encode(&keys);
            let f12: slint::SharedString = slint::platform::Key::F12.into();
            assert_eq!(
                resolve(&raw, scope, &f12, true, true, true),
                id as i32,
                "{}",
                action.id
            );
        }
        assert_eq!(resolve("", 0, "u", true, false, true), -1);
        assert_eq!(resolve("", 0, "n", true, false, false), hit("file.new"));
    }

    /// RFN01-82: VS Codeの既定に合わせたキー（書き手と合意 2026-11-03）。
    #[test]
    fn vs_code_keys_are_the_defaults() {
        assert_eq!(resolve("", 0, "b", true, false, false), hit("left.toggle"));
        assert_eq!(
            resolve("", 0, "V", true, false, true),
            hit("view.toggle-preview")
        );
        assert_eq!(
            resolve("", 0, "O", true, false, true),
            hit("view.sidebar.outline")
        );
        assert_eq!(
            resolve("", 0, "G", true, false, true),
            hit("view.sidebar.git")
        );
        assert_eq!(
            resolve("", 0, ",", true, false, false),
            hit("left.settings")
        );
        assert_eq!(
            resolve("", 0, "\\", true, false, false),
            hit("view.split-right")
        );
        assert_eq!(
            resolve("", 0, "l", true, false, false),
            hit("text.select-line")
        );
        let up: slint::SharedString = slint::platform::Key::UpArrow.into();
        let down: slint::SharedString = slint::platform::Key::DownArrow.into();
        assert_eq!(
            resolve("", 0, &up, true, false, false),
            hit("text.scroll-up")
        );
        assert_eq!(
            resolve("", 0, &down, true, false, false),
            hit("text.scroll-down")
        );
        // Ctrl+Shift+Cは場面で分かれる：本文ではTerminal起動、Quick Draftでは全文コピー。
        assert_eq!(
            resolve("", 0, "C", true, false, true),
            hit("file.terminal-in-folder")
        );
        assert_eq!(
            resolve("", 1, "C", true, false, true),
            hit("draft.copy-all")
        );
        // Ctrl+Alt+Lはもう何にも当たっていない。
        assert_eq!(resolve("", 0, "l", true, true, false), -1);
    }

    /// RFN01-82: **前の版が保存した割り当てでも、新しい既定が効く。**保存はどの枠も
    /// 明示して書くので、古い既定（空やCtrl+Alt+L）がそのまま残っている。移すのは
    /// 古い既定のままの枠だけで、書き手が替えたキーは残す。印を付けて保存したあとは
    /// 移さない——後で外した枠は外れたまま。
    #[test]
    fn ctrl_p_goes_to_the_palette_and_print_preview_lets_it_go() {
        assert_eq!(
            resolve("", 0, "p", true, false, false),
            hit("view.go-to-file")
        );
        assert_eq!(
            resolve("", 0, "P", true, false, true),
            hit("view.command-palette")
        );
        let mut keys = bindings("");
        keys[at("view.go-to-file")].clear();
        keys[at("file.print")] = "Ctrl+P".into();
        assert_eq!(bindings(&encode(&keys))[at("file.print")], "Ctrl+P");
    }

    #[test]
    fn tree_navigation_keys_are_local_and_can_be_reassigned() {
        use slint::platform::Key;
        for (key, action) in [
            (Key::F2, "tree.rename"),
            (Key::Delete, "tree.delete"),
            (Key::Return, "tree.open"),
            (Key::UpArrow, "tree.previous"),
            (Key::DownArrow, "tree.next"),
            (Key::LeftArrow, "tree.left"),
            (Key::RightArrow, "tree.right"),
        ] {
            let text: slint::SharedString = key.into();
            let id = actions::index_of(action).unwrap() as i32;
            assert_eq!(resolve("", TREE_SCOPE, &text, false, false, false), id);
            assert_ne!(resolve("", 0, &text, false, false, false), id);
            assert_ne!(resolve("", 1, &text, false, false, false), id);
        }
        let raw = "tree.open=Delete;tree.delete=;v=5";
        let key: slint::SharedString = Key::Delete.into();
        let id = actions::index_of("tree.open").unwrap() as i32;
        assert_eq!(resolve(raw, TREE_SCOPE, &key, false, false, false), id);
        assert_eq!(resolve(raw, 0, &key, false, false, false), -1);
        let key: slint::SharedString = Key::Return.into();
        assert_eq!(resolve(raw, TREE_SCOPE, &key, false, false, false), -2);
    }

    /// RFN01-82: 開閉しても、出ていた面はそのまま。
    #[test]
    fn toggling_the_left_pane_keeps_the_panel_it_showed() {
        use slint::platform::software_renderer::MinimalSoftwareWindow;
        struct Offscreen(std::rc::Rc<MinimalSoftwareWindow>);
        impl slint::platform::Platform for Offscreen {
            fn create_window_adapter(
                &self,
            ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError>
            {
                Ok(self.0.clone())
            }
        }
        let surface = MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(Offscreen(surface))).unwrap();
        let window = AppWindow::new().unwrap();
        window.set_tree_open(true);
        window.set_left_tab(2);
        window.invoke_toggle_left_pane();
        assert!(!window.get_tree_open());
        assert_eq!(window.get_left_tab(), 2);
        window.invoke_toggle_left_pane();
        assert!(window.get_tree_open());
        assert_eq!(window.get_left_tab(), 2);
    }

    /// 挿入の操作は、`document::INSERT_EDITS`と`LINE_NOTE_EDITS`の**形を1つずつ
    /// 重複なく指す**。形の名前からIDを作るので、挿入の並びが動いても割り当ては動かない。
    #[test]
    fn insert_actions_cover_every_shape_once() {
        let used: Vec<InsertShape> = actions::all()
            .iter()
            .filter_map(|action| match action.command {
                Some(Command::Insert(shape)) => Some(shape),
                _ => None,
            })
            .collect();
        for what in crate::document::INSERT_EDITS {
            let count = used
                .iter()
                .filter(|shape| **shape == InsertShape::Edit(what))
                .count();
            assert_eq!(count, 1, "{what:?}");
        }
        for what in crate::document::LINE_NOTE_EDITS {
            let count = used
                .iter()
                .filter(|shape| **shape == InsertShape::Line(what))
                .count();
            assert_eq!(count, 1, "{what:?}");
        }
        for shape in [
            InsertShape::Table,
            InsertShape::PageBreak,
            InsertShape::Template,
        ] {
            let count = used.iter().filter(|used| **used == shape).count();
            assert_eq!(count, 1, "{shape:?}");
        }
        assert_eq!(
            used.len(),
            crate::document::INSERT_EDITS.len() + crate::document::LINE_NOTE_EDITS.len() + 3
        );
        // 4までの番号の並びも、挿入の形を1つずつ持っていた。
    }

    /// 指定できるキーは**修飾キー付きかF1〜F12**。素の打鍵と、窓・アプリが先に
    /// 使う鍵は断る。
    #[test]
    fn only_modifier_keys_and_function_keys_are_accepted() {
        for key in [
            "Ctrl+Shift+A",
            "Ctrl+Alt+5",
            "Alt+Shift+F",
            "F1",
            "F12",
            "Shift+F5",
            "Ctrl+Alt+Left",
        ] {
            assert!(normalize(key).is_some(), "{key} was refused");
        }
        for key in [
            "A",
            "Shift+A",
            "5",
            "Ctrl+C",
            "Ctrl+Shift+C",
            "F3",
            "Shift+F3",
            "Alt+Tab",
            "Alt+F4",
            "Ctrl+Alt+Del",
            "Ctrl+Shift+@",
        ] {
            assert!(normalize(key).is_none(), "{key} was accepted");
        }
    }

    /// F1〜F12は**鍵そのもの**から読む（Slintは私用面の文字で渡す）。
    #[test]
    fn function_keys_are_read_from_the_key_itself() {
        let f5: slint::SharedString = slint::platform::Key::F5.into();
        assert_eq!(chord(&f5, false, false, false).as_deref(), Some("F5"));
        assert_eq!(chord(&f5, true, false, false).as_deref(), Some("Ctrl+F5"));
        let f12: slint::SharedString = slint::platform::Key::F12.into();
        assert_eq!(
            chord(&f12, true, true, false).as_deref(),
            Some("Ctrl+Alt+F12")
        );
        // **素の字は取らない。**
        assert!(chord("a", false, false, false).is_none());
    }

    /// 設定のTABでは、**新しいTABを作るもの・移るもの**を通し、本文に効くものは通さない。
    #[test]
    fn only_new_panes_pass_the_settings_tab() {
        assert!(passes_settings(hit("file.open")), "ファイルを開く");
        assert!(passes_settings(hit("file.new")), "新規文書");
        assert!(passes_settings(hit("file.new-terminal")), "新しいTerminal");
        assert!(passes_settings(hit("edit.find")), "設定を探す");
        assert!(
            !passes_settings(hit("file.save")),
            "代役の空文書を保存してしまう"
        );
        let insert = hit(&actions::insert_id(InsertShape::Table));
        assert!(!passes_settings(insert), "挿入は代役の空文書へ入ってしまう");
    }
}
