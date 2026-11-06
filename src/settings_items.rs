use std::sync::OnceLock;

use crate::{AppWindow, Live, Setting, colour_name, fold_for_search, font_name, i18n};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    App,
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Inherits {
    Never,
    Text,
    Layout,
    Paper,
}

pub(crate) struct Item {
    pub(crate) id: String,
    ja: String,
    en: String,
    search_terms: String,
    group: &'static str,
    pub(crate) toggle: Option<Toggle>,
    pub(crate) tab: i32,
    scope: Scope,
    inherits: Inherits,
}

#[derive(Clone, Copy)]
pub(crate) enum Toggle {
    RubyMarkup,
    CountRuby,
    TextShared,
    LayoutShared,
    PaperShared,
    TerminalPaste,
    TerminalClose,
    TerminalRestore,
    Sheet { setting: Setting, scope: usize },
}

impl Toggle {
    fn for_id(id: &str) -> Option<Self> {
        Some(match id {
            "ruby.marks" => Self::RubyMarkup,
            "count.ruby" => Self::CountRuby,
            "text.shared" => Self::TextShared,
            "layout.shared" => Self::LayoutShared,
            "paper.shared" => Self::PaperShared,
            "terminal.confirm_paste" => Self::TerminalPaste,
            "terminal.confirm_close" => Self::TerminalClose,
            "terminal.restore_output" => Self::TerminalRestore,
            _ => {
                let (scope, name) = id.split_once('.')?;
                let scope = match scope {
                    "h" => 0,
                    "v" => 1,
                    _ => return None,
                };
                let setting = Setting::from_name(name)?;
                match setting {
                    Setting::LineNumbers
                    | Setting::Whitespace
                    | Setting::UprightDigits
                    | Setting::UprightMarks => Self::Sheet { setting, scope },
                    _ => return None,
                }
            }
        })
    }

    fn sheet(window: &AppWindow, setting: Setting, scope: usize) -> usize {
        match setting {
            Setting::UprightDigits | Setting::UprightMarks => scope,
            _ => crate::shared_sheet(window, scope, setting.page()),
        }
    }

    pub(crate) fn current(self, window: &AppWindow) -> bool {
        match self {
            Self::RubyMarkup => window.get_ruby_marks(),
            Self::CountRuby => window.get_count_ruby(),
            Self::TextShared => window.get_text_shared(),
            Self::LayoutShared => window.get_layout_shared(),
            Self::PaperShared => window.get_paper_shared(),
            Self::TerminalPaste => window.get_terminal_confirm_paste(),
            Self::TerminalClose => window.get_terminal_confirm_close(),
            Self::TerminalRestore => window.get_terminal_restore_output(),
            Self::Sheet { setting, scope } => {
                setting.read_own(window, Self::sheet(window, setting, scope)) != 0
            }
        }
    }

    pub(crate) fn flip(self, window: &AppWindow) {
        let wanted = !self.current(window);
        match self {
            Self::RubyMarkup => window.invoke_ruby_marks_toggled(wanted),
            Self::CountRuby => window.invoke_count_ruby_toggled(wanted),
            Self::TextShared => window.invoke_text_shared_toggled(wanted),
            Self::LayoutShared => window.invoke_layout_shared_toggled(wanted),
            Self::PaperShared => window.invoke_paper_shared_toggled(wanted),
            Self::TerminalPaste => window.set_terminal_confirm_paste(wanted),
            Self::TerminalClose => window.set_terminal_confirm_close(wanted),
            Self::TerminalRestore => window.set_terminal_restore_output(wanted),
            Self::Sheet { setting, scope } => {
                let original_sheet = window.get_sheet();
                window.set_sheet(Self::sheet(window, setting, scope) as i32);
                window.invoke_sheet_chose(setting.row_in_sheet() as i32, i32::from(wanted));
                window.set_sheet(original_sheet);
            }
        }
    }
}

#[derive(Clone)]
enum GroupId {
    Shell(u64),
    WordMode(u32),
}

#[derive(Clone)]
pub(crate) struct Group {
    id: GroupId,
    pub(crate) titles: (String, String),
    members: &'static [(&'static str, &'static str)],
}

impl Group {
    pub(crate) fn all(window: &AppWindow) -> Vec<Self> {
        let shells = crate::configured_shells(window).into_iter().map(|shell| {
            let name = &shell.name;
            Self {
                titles: (
                    format!("設定 › Terminal › {name} のプロファイル設定"),
                    format!("Settings › Terminal › {name} Profile"),
                ),
                members: &[
                    ("名前", "Name"),
                    ("実行ファイル", "Executable"),
                    ("引数", "Arguments"),
                    ("開始フォルダ", "Start Folder"),
                    ("既定", "Default"),
                ],
                id: GroupId::Shell(shell.id),
            }
        });
        let modes = crate::word_modes()
            .iter()
            .map(|marks| {
                let name = &marks.mode.name;
                Self {
                    titles: (
                        format!("設定 › Editor › {name} の単語チェック設定"),
                        format!("Settings › Editor › {name} Word Check Settings"),
                    ),
                    members: &[
                        ("モード名", "Mode Name"),
                        ("語群", "Group"),
                        ("色", "Color"),
                        ("登録語", "Words"),
                        ("覚え書き", "Notes"),
                    ],
                    id: GroupId::WordMode(marks.mode.id),
                }
            })
            .collect::<Vec<_>>();
        shells.chain(modes).collect()
    }

    pub(crate) fn matches(&self, folded_query: &str) -> bool {
        let (ja, en) = &self.titles;
        let title = fold_for_search(&format!("{ja} {en}"));
        folded_query.split_whitespace().all(|word| {
            title.contains(word)
                || self.members.iter().any(|(ja, en)| {
                    fold_for_search(ja).contains(word) || fold_for_search(en).contains(word)
                })
        })
    }

    pub(crate) fn open(self, window: &AppWindow, live: &Live) {
        let title = i18n::pick(&self.titles.0, &self.titles.1);
        match self.id {
            GroupId::Shell(id) => {
                if let Some(at) = crate::configured_shells(window)
                    .iter()
                    .position(|shell| shell.id == id)
                {
                    crate::open_setting_target(window, live, title, "terminal.profiles", 1);
                    crate::terminal_shells::action(window, live, 0, at as i32);
                }
            }
            GroupId::WordMode(id) => {
                if let Some(at) = crate::word_modes()
                    .iter()
                    .position(|marks| marks.mode.id == id)
                {
                    crate::open_setting_target(window, live, title, "words.manage", 3);
                    window.invoke_word_mode_opened(at as i32);
                }
            }
        }
    }
}

impl Item {
    pub(crate) fn all() -> impl Iterator<Item = &'static Self> {
        static ITEMS: OnceLock<Vec<Item>> = OnceLock::new();
        ITEMS.get_or_init(items).iter()
    }

    pub(crate) fn titles(&self) -> (String, String) {
        (
            format!("設定 › {}", self.ja),
            format!("Settings › {}", self.en),
        )
    }

    pub(crate) fn title(&self) -> String {
        let (ja, en) = self.titles();
        i18n::pick(&ja, &en).to_owned()
    }

    pub(crate) fn matches(&self, folded_query: &str) -> bool {
        folded_query
            .split_whitespace()
            .all(|word| self.search_terms.contains(word))
    }

    fn effective_sheet(&self, text: bool, layout: bool, paper: bool) -> i32 {
        match self.scope {
            Scope::App => -1,
            Scope::Horizontal => 0,
            Scope::Vertical => i32::from(!match self.inherits {
                Inherits::Never => false,
                Inherits::Text => text,
                Inherits::Layout => layout,
                Inherits::Paper => paper,
            }),
        }
    }
}

pub(crate) fn shows(
    route: &str,
    sheet: i32,
    query: &str,
    selected: &str,
    shared: [bool; 3],
) -> bool {
    if selected.is_empty() && query.is_empty() {
        return true;
    }
    let [text, layout, paper] = shared;
    let route_matches = |candidate: &str| {
        route
            .strip_suffix('*')
            .map_or(candidate == route, |prefix| candidate.starts_with(prefix))
    };
    let query = fold_for_search(query.trim());
    Item::all().any(|item| {
        if !selected.is_empty() && item.id != selected {
            return false;
        }
        let effective = item.effective_sheet(text, layout, paper);
        let dependency = match item.inherits {
            Inherits::Never => "",
            Inherits::Text => "text.shared",
            Inherits::Layout => "layout.shared",
            Inherits::Paper => "paper.shared",
        };
        let inherited = item.scope == Scope::Vertical && effective == 0;
        let visible = (route_matches(item.group) && effective == sheet)
            || (inherited && sheet == 1 && route_matches(dependency))
            || (route == "layout.wrap-mode"
                && item.group == "layout.wrap-length"
                && sheet == effective);
        visible && (!selected.is_empty() || item.matches(&query))
    })
}

pub(crate) fn wire(window: &crate::AppWindow) {
    window.on_settings_row_visible(|route, sheet, query, selected, text, layout, paper| {
        shows(&route, sheet, &query, &selected, [text, layout, paper])
    });
}

fn items() -> Vec<Item> {
    let mut items = Vec::new();
    let mut add = |id: &str, ja: &str, en: &str, aliases, route, tab, scope, inherits| {
        let (scope_ja, scope_en) = match scope {
            Scope::App => ("", ""),
            Scope::Horizontal => ("横書き › ", "Horizontal › "),
            Scope::Vertical => ("縦書き › ", "Vertical › "),
        };
        items.push(Item {
            id: id.into(),
            ja: format!("{scope_ja}{ja}"),
            en: format!("{scope_en}{en}"),
            search_terms: fold_for_search(&format!(
                "設定 Settings {scope_ja}{ja} {scope_en}{en} {aliases}"
            )),
            group: route,
            toggle: Toggle::for_id(id),
            tab,
            scope,
            inherits,
        });
    };
    use Inherits::{Layout, Never, Paper, Text};
    use Scope::{App, Horizontal, Vertical};
    macro_rules! app {
        ($tab:expr, $route:literal, $id:literal, $ja:literal, $en:literal, $aliases:literal) => {
            add($id, $ja, $en, $aliases, $route, $tab, App, Never)
        };
    }
    app!(
        0,
        "general.language",
        "language",
        "表示言語",
        "Display Language",
        "言語 Language"
    );
    app!(
        0,
        "general.zoom",
        "zoom.default",
        "既定の倍率",
        "Default Zoom",
        "拡大 縮小 倍率"
    );
    app!(
        0,
        "general.speech.voice",
        "speech.voice",
        "読み上げの声",
        "Read Aloud Voice",
        "音声"
    );
    app!(
        0,
        "general.speech.speed",
        "speech.speed",
        "読み上げの速さ",
        "Read Aloud Speed",
        "速度"
    );
    app!(
        0,
        "left.size",
        "left.size",
        "左ペインの文字サイズ",
        "Left Pane Text Size",
        "字 大きさ"
    );
    app!(
        0,
        "left.paper",
        "left.paper",
        "左ペインの背景色",
        "Left Pane Background Color",
        "紙"
    );
    app!(
        0,
        "left.ink",
        "left.ink",
        "左ペインの文字色",
        "Left Pane Text Color",
        "色"
    );
    app!(
        0,
        "left.folder",
        "left.folder",
        "左ペインのフォルダ色",
        "Left Pane Folder Color",
        "色"
    );
    app!(
        0,
        "left.file",
        "left.file",
        "左ペインのファイル色",
        "Left Pane File Color",
        "色"
    );
    app!(
        3,
        "markup.count",
        "count.ruby",
        "ルビを字数に数える",
        "Count Ruby in Word Count",
        "Markup 記法"
    );
    app!(
        3,
        "markup.ruby",
        "ruby.marks",
        "ルビの記法を読む",
        "Ruby Markup",
        "ルビ 記法"
    );
    app!(
        3,
        "markup.list",
        "list.marks",
        "原稿の箇条書き記号",
        "Markup Bullet Marks",
        "リスト 記法"
    );
    app!(
        3,
        "page.random",
        "paper.random",
        "新規TABのランダム背景",
        "New Tabs Random Background",
        "紙 色"
    );
    app!(
        3,
        "page.transparency",
        "background.transparency",
        "背景の透過",
        "Background Transparency",
        "不透明"
    );
    app!(
        3,
        "page.wallpaper",
        "wallpaper.kind",
        "壁紙の種類",
        "Background Image Type",
        "Windows Wallpaper"
    );
    app!(
        3,
        "page.wallpaper",
        "wallpaper.path",
        "壁紙の画像ファイル",
        "Background Image File",
        "壁紙 パス"
    );
    app!(
        3,
        "page.wallpaper",
        "wallpaper.fit",
        "壁紙の配置",
        "Background Image Fit",
        "Tile Fit Height Fit Width"
    );
    app!(
        3,
        "page.wallpaper",
        "wallpaper.strength",
        "壁紙の濃さ",
        "Background Image Strength",
        "壁紙"
    );
    app!(
        3,
        "words",
        "words.manage",
        "単語チェックのモードと語群",
        "Word Check Modes and Groups",
        "単語チェック Words 辞書 Import Export New Mode New Group Edit"
    );
    app!(
        1,
        "terminal.profiles",
        "terminal.profiles",
        "シェルプロファイルの管理",
        "Manage Shell Profiles",
        "Terminal Profile name Executable Arguments Start folder シェル"
    );
    app!(
        1,
        "terminal.profiles",
        "terminal.default",
        "既定のシェル",
        "Default Shell",
        "Terminal プロファイル"
    );
    app!(
        1,
        "terminal.paste",
        "terminal.confirm_paste",
        "複数行の貼り付けを確認",
        "Confirm Multiline Paste",
        "Terminal 複数行 送信"
    );
    app!(
        1,
        "terminal.close",
        "terminal.confirm_close",
        "シェルの終了を確認",
        "Confirm Shell Termination",
        "Terminal 終了"
    );
    app!(
        1,
        "terminal.restore",
        "terminal.restore_output",
        "再起動時に出力を復元",
        "Restore Output After Restart",
        "Terminal 復元"
    );
    app!(
        1,
        "terminal.history",
        "terminal.history_limit",
        "Terminalの履歴行数",
        "Terminal History Lines",
        "出力"
    );
    app!(
        1,
        "terminal.timestamp",
        "terminal.timestamp_format",
        "タイムスタンプの書式",
        "Terminal Timestamp Format",
        "ログ 時刻"
    );
    app!(
        8,
        "backup.recovery",
        "work.autosave",
        "作業コピーの自動退避",
        "Auto Recovery",
        "Backup 自動退避"
    );
    app!(
        8,
        "backup.keep",
        "backup.keep",
        "バックアップの世代数",
        "Backup Generations",
        "Auto Backup 世代"
    );
    app!(
        8,
        "backup.folder",
        "backup.folder",
        "バックアップの保存先",
        "Backup Folder",
        "Auto Backup 置き場 フォルダ"
    );
    app!(
        8,
        "backup.name",
        "backup.name",
        "バックアップ名の形式",
        "Backup File Name Format",
        "Auto Backup 名前"
    );

    for (owner, ja, route, tab, editor_panel) in [
        ("Terminal", "Terminal", "style.terminal", 1, false),
        (
            "Editor Panel",
            "Editor Panel",
            "style.editor-panel",
            1,
            true,
        ),
        (
            "Terminal Panel",
            "Terminal Panel",
            "style.terminal-panel",
            3,
            false,
        ),
    ] {
        for (field, field_ja, field_en) in [
            ("paper", "背景色", "Background Color"),
            ("ink", "文字色", "Text Color"),
            ("font", "フォント", "Font"),
            ("size", "文字サイズ", "Size"),
            ("transparency", "透過", "Transparency"),
            ("bold", "太字", "Bold"),
            ("italic", "斜体", "Italic"),
            ("underline", "下線", "Underline"),
            ("random", "ランダム背景", "Random Background"),
        ] {
            if editor_panel || !matches!(field, "ink" | "underline") {
                add(
                    &format!("{route}.{field}"),
                    &format!("{ja}の{field_ja}"),
                    &format!("{owner} {field_en}"),
                    "外観 Appearance",
                    route,
                    tab,
                    App,
                    Never,
                );
            }
        }
    }

    for (scope, prefix) in [(Horizontal, "h"), (Vertical, "v")] {
        macro_rules! sheet {
            ($route:expr, $key:expr, $ja:expr, $en:expr, $aliases:literal, $inherits:expr) => {
                add(
                    &format!("{prefix}.{}", $key),
                    $ja,
                    $en,
                    $aliases,
                    $route,
                    3,
                    scope,
                    $inherits,
                )
            };
        }
        for (slot, route, ja, en) in [
            (0, "text.body", "本文", "Body"),
            (1, "text.h1", "見出し1", "H1"),
            (2, "text.h2", "見出し2", "H2"),
            (3, "text.h3", "見出し3", "H3"),
            (4, "text.h4", "見出し4", "H4"),
            (5, "text.h5", "見出し5", "H5"),
            (6, "text.h6", "見出し6", "H6"),
        ] {
            let size = if slot == 0 {
                Setting::BodySize
            } else {
                Setting::Heading(slot - 1)
            };
            sheet!(
                route,
                size.name(),
                &format!("{ja}の大きさ"),
                &format!("{en} Size"),
                "文字サイズ Text",
                Text
            );
            sheet!(
                route,
                font_name(slot),
                &format!("{ja}のフォント"),
                &format!("{en} Font"),
                "書体 Font Family Text",
                Text
            );
            sheet!(
                route,
                colour_name(slot),
                &format!("{ja}の文字色"),
                &format!("{en} Text Color"),
                "色 Default Color Ink Colour",
                Text
            );
            sheet!(
                route,
                colour_name(8 + slot),
                &format!("{ja}の背景色"),
                &format!("{en} Background Color"),
                "色 Paper",
                Text
            );
            for (kind, field_ja, field_en) in [
                (0, "太字", "Bold"),
                (1, "斜体", "Italic"),
                (2, "取消線", "Strikethrough"),
                (3, "背景の表示", "Background Enabled"),
                (4, "下線", "Underline"),
            ] {
                sheet!(
                    route,
                    Setting::Decoration(slot, kind).name(),
                    &format!("{ja}の{field_ja}"),
                    &format!("{en} {field_en}"),
                    "装飾 Decoration Text",
                    Text
                );
            }
        }
        sheet!(
            "text.code",
            font_name(crate::CODE_SLOT),
            "コードのフォント",
            "Code Font",
            "書体 Text",
            Text
        );
        sheet!(
            "text.highlight",
            colour_name(crate::HIGHLIGHT_SLOT),
            "ハイライト色",
            "Highlight Color",
            "色 Text",
            Text
        );
        sheet!(
            "page.paper",
            colour_name(crate::PAPER_SLOT),
            "紙の色",
            "Paper Color",
            "用紙 背景 Page",
            Paper
        );
        for (setting, route, ja, en, aliases) in [
            (
                Setting::LineAdvance,
                "layout.line",
                "行送り・列の間隔",
                "Line Height / Column Gap",
                "行間 Layout",
            ),
            (
                Setting::CharAdvance,
                "layout.char",
                "字間",
                "Character Advance",
                "Char advance Layout",
            ),
            (
                Setting::PageMargin,
                "layout.margin",
                "紙の余白",
                "Page Margin",
                "Layout",
            ),
            (
                Setting::WrapMode,
                "layout.wrap-mode",
                "折り返し方式",
                "Wrap Mode",
                "Width Pane Layout",
            ),
            (
                Setting::WrapChars,
                "layout.wrap-length",
                "折り返しの行長",
                "Wrap Line Length",
                "文字数 Layout",
            ),
            (
                Setting::LineNumbers,
                "layout.numbers",
                "行番号",
                "Line Numbers",
                "Layout",
            ),
            (
                Setting::Whitespace,
                "layout.whitespace",
                "空白・改行記号",
                "Show Spaces and Line Breaks",
                "Whitespace Layout",
            ),
            (
                Setting::RubySize,
                "layout.ruby-size",
                "ルビの大きさ",
                "Ruby Size",
                "傍点 Layout",
            ),
            (
                Setting::RubyOffset,
                "layout.ruby-position",
                "ルビの位置",
                "Ruby Position",
                "傍点 Layout",
            ),
            (
                Setting::BulletMark(0),
                "layout.bullet",
                "箇条書きの表示記号（-）",
                "Bullet Mark for Hyphen",
                "Bullet marks Layout",
            ),
            (
                Setting::BulletMark(1),
                "layout.bullet",
                "箇条書きの表示記号（*）",
                "Bullet Mark for Asterisk",
                "Bullet marks Layout",
            ),
            (
                Setting::BulletMark(2),
                "layout.bullet",
                "箇条書きの表示記号（+）",
                "Bullet Mark for Plus",
                "Bullet marks Layout",
            ),
        ] {
            add(
                &format!("{prefix}.{}", setting.name()),
                ja,
                en,
                aliases,
                route,
                3,
                scope,
                Layout,
            );
        }
    }
    for (id, ja, en) in [
        (
            "text.shared",
            "文字の設定を横書きと同じにする",
            "Text: Same as Horizontal",
        ),
        (
            "layout.shared",
            "組み方を横書きと同じにする",
            "Layout: Same as Horizontal",
        ),
        (
            "paper.shared",
            "紙の色を横書きと同じにする",
            "Page: Same as Horizontal",
        ),
    ] {
        add(id, ja, en, "継承 共有", id, 3, Vertical, Never);
    }
    add(
        &format!("v.{}", Setting::UprightDigits.name()),
        "数字の縦中横",
        "Tate-chu-yoko for Digits",
        "Layout",
        "layout.upright-digits",
        3,
        Vertical,
        Never,
    );
    add(
        &format!("v.{}", Setting::UprightMarks.name()),
        "記号の縦中横",
        "Tate-chu-yoko for Symbols",
        "Layout",
        "layout.upright-marks",
        3,
        Vertical,
        Never,
    );
    items
}

#[cfg(test)]
mod terminal_routes {
    use super::*;

    #[test]
    fn every_terminal_setting_has_a_row_and_search_can_reveal_it() {
        let ui = include_str!("../ui/app-window.slint");
        let component = include_str!("../ui/terminal-settings.slint");
        let section = ui
            .split_once("if root.shows-group(1): TerminalSettings {")
            .unwrap()
            .1
            .split_once("if root.shows-group(7)")
            .unwrap()
            .0;
        for item in Item::all().filter(|item| item.tab == 1) {
            let row = section
                .lines()
                .find(|line| line.contains(&format!("root.shows-setting(\"{}\", -1)", item.group)))
                .unwrap_or_else(|| panic!("Missing Terminal row for {}", item.id));
            let (visibility, _) = row.trim().split_once(':').unwrap();
            assert!(component.contains(&format!("if root.{visibility}:")));
            assert!(shows(item.group, -1, "", &item.id, [false; 3]));
        }
    }
}
