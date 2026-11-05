use crate::document::{CalloutType, InsertEdit, LineNoteEdit};
use crate::menu_commands::{InsertShape, ShortcutAction};
use crate::{AppWindow, Live, ShortcutItem};
use slint::{ComponentHandle, ModelRc, VecModel};
/// 操作名：日本語と英語（追加要件 2026-09-15、表示の国際化）。
const NAMES: &[(&str, &str)] = &[
    ("ファイルを開く", "Open File"),
    ("保存", "Save"),
    ("名前を付けて保存", "Save As"),
    ("TABを閉じる", "Close Tab"),
    ("本文を検索", "Find in Text"),
    ("本文を置換", "Replace in Text"),
    ("行へ移動", "Go to Line"),
    ("閉じたTABを開き直す", "Reopen Closed Tab"),
    ("元に戻す", "Undo"),
    ("やり直し", "Redo"),
    ("Quick Draftを開く（本文）", "Open Quick Draft (from Text)"),
    ("次のTAB", "Next Tab"),
    ("前のTAB", "Previous Tab"),
    ("左のPaneへ", "To Left Pane"),
    ("右のPaneへ", "To Right Pane"),
    ("上のPaneへ", "To Upper Pane"),
    ("下段または下のPaneへ", "To Strip Below or Lower Pane"),
    ("下段を表示・閉じる", "Show or Hide Strip Below"),
    ("新しいTAB", "New Tab"),
    ("戻る", "Back"),
    ("進む", "Forward"),
    ("次の1文字を削除", "Delete Next Character"),
    ("1単語後へ", "Forward One Word"),
    ("1単語前へ", "Back One Word"),
    ("行末まで切り取り", "Cut to End of Line"),
    ("選択を開始・解除", "Start or Cancel Selection"),
    (
        "矩形選択を開始・解除",
        "Start or Cancel Rectangle Selection",
    ),
    ("Kill Ringへコピー", "Copy to Kill Ring"),
    ("Kill Ringへ切り取り", "Cut to Kill Ring"),
    ("最新のKillを貼り付け", "Paste Latest Kill"),
    ("前のKillへ置換", "Replace with Previous Kill"),
    ("1単語後まで選択", "Select Forward One Word"),
    ("1単語前まで選択", "Select Back One Word"),
    ("行を削除", "Delete Line"),
    ("行を上へ移動", "Move Line Up"),
    ("行を下へ移動", "Move Line Down"),
    ("行を上へ複製", "Duplicate Line Up"),
    ("行を下へ複製", "Duplicate Line Down"),
    ("全文をコピー（Quick Draft内）", "Copy All (in Quick Draft)"),
    (
        "コピーして閉じる（Quick Draft内）",
        "Copy and Close (in Quick Draft)",
    ),
    ("クリア（Quick Draft内）", "Clear (in Quick Draft)"),
    (
        "TABへ貼り付け（Quick Draft内）",
        "Paste to Tab (in Quick Draft)",
    ),
    ("縦書き・横書きを切り替え", "Toggle Vertical / Horizontal"),
    // RFN01-66（2026-11-05）: 名前はタイトルバーのメニューにそろえる。
    ("Source・Preview切替", "Toggle Source / Preview"),
    (
        "Viewer・ReadOnly開始／終了",
        "Start / Exit Viewer or ReadOnly",
    ),
    ("印刷プレビューを開く", "Open Print Preview"),
];
const DEFAULTS: &[&str] = &[
    "Ctrl+O",
    "Ctrl+S",
    "Ctrl+Shift+S",
    "Ctrl+W",
    "Ctrl+F",
    "Ctrl+H",
    "Ctrl+G",
    "Ctrl+Shift+T",
    "Ctrl+Z",
    "Ctrl+Y",
    "",
    "Ctrl+Tab",
    "Ctrl+Shift+Tab",
    "Ctrl+Alt+Left",
    "Ctrl+Alt+Right",
    "Ctrl+Alt+Up",
    "Ctrl+Alt+Down",
    "Ctrl+`",
    "",
    "Alt+Left",
    "Alt+Right",
    "Ctrl+D",
    "Alt+F",
    "Alt+B",
    "Ctrl+K",
    "Ctrl+Space",
    "Ctrl+Shift+Space",
    "Alt+W",
    "Alt+X",
    "Alt+Y",
    "Alt+Shift+Y",
    "Alt+Shift+F",
    "Alt+Shift+B",
    "Ctrl+Shift+K",
    "Alt+Up",
    "Alt+Down",
    "Alt+Shift+Up",
    "Alt+Shift+Down",
    "Ctrl+Shift+C",
    "",
    "",
    "",
    // RFN01-87（書き手の求め 2026-11-03）: 縦書き・横書きの切り替え。
    "Ctrl+R",
    // RFN01-82: VS Codeの「Toggle Markdown Preview」と同じ。
    "Ctrl+Shift+V",
    "",
    // RFN01-87（書き手の決定 2026-11-03）: Ctrl+Pはファイル名で開くへ。印刷プレビューはキーなし。
    "",
];
const CATEGORIES: &[i32] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 0, 0, 0, 0, 0, 6, 6, 6, 6, 4, 4, 4, 0,
];
/// Keys の面の分類（書き手の求め 2026-09-15：タブで切り替えず、全部を並べて
/// 分類ごとに畳めるように）。番号は[`CATEGORIES`]の値。
const CATEGORY_NAMES: [(&str, &str); 10] = [
    ("本文の操作", "Text Actions"),
    ("基本編集", "Basic Editing"),
    ("Terminal", "Terminal"),
    ("入力欄", "Input Fields"),
    ("Pane・TAB", "Panes & Tabs"),
    ("Emacs", "Emacs"),
    ("Quick Draft", "Quick Draft"),
    ("ファイル", "File"),
    ("挿入", "Insert"),
    ("実行", "Run"),
];
const CATEGORY_NOTES: [(&str, &str); 10] = [
    (
        "本文にフォーカスがあり、IME変換中でないときに有効です。",
        "Active when the text has focus and no IME conversion is in progress.",
    ),
    (
        "固定 · 本文の編集時に使います。Viewerでは編集できません。",
        "Fixed · Used while editing the text. Nothing can be edited in Viewer.",
    ),
    (
        "固定 · Terminalにフォーカスがあるときに有効です。その他のキーは実行中のシェル・プログラムへ渡します。",
        "Fixed · Active when a Terminal has focus. Other keys go to the running shell or program.",
    ),
    (
        "固定 · 検索欄などの文字入力欄にフォーカスがあるときに有効です。Enter・Escの動作は入力欄ごとの用途に従います。",
        "Fixed · Active when an input field such as Find has focus. Enter and Esc work as each field needs.",
    ),
    (
        "本文からのPane・TAB操作です。Terminal内のキーは固定です。",
        "Pane and tab actions from the text. Keys inside a Terminal are fixed.",
    ),
    (
        "本文で常時使えるEmacs風キーです。専用モードの切り替えはありません。",
        "Emacs-style keys always available in the text. There is no separate mode to switch to.",
    ),
    (
        "本文から開く操作と、Quick Draft内の操作を区別します。",
        "Tells opening from the text apart from actions inside Quick Draft.",
    ),
    (
        "メニューの「ファイル」と同じ操作です。既定では割り当てていません。",
        "The same actions as the File menu. Nothing is assigned to most of them by default.",
    ),
    (
        "メニューの「挿入」と同じ操作です。選んだ字が要る形は、選んでいないときは何もしません。",
        "The same actions as the Insert menu. A form that needs a picked word does nothing when nothing is picked.",
    ),
    (
        "メニューの「実行」と同じ操作です。スペルミスへ移るのは、スペルチェック中だけです。",
        "The same actions as the Run menu. Moving to a spelling error works only during a spell check.",
    ),
];
/// 変えられないキー。**同じ一覧に並べる**——「このキーは何か」を探す書き手に
/// とって、変えられるかどうかは探したあとの話である。
const FIXED: &[(i32, (&str, &str), &str)] = &[
    (1, ("コピー", "Copy"), "Ctrl+C"),
    (1, ("切り取り", "Cut"), "Ctrl+X"),
    (1, ("貼り付け", "Paste"), "Ctrl+V"),
    (1, ("全選択", "Select All"), "Ctrl+A"),
    (
        2,
        ("選択した文字をコピー", "Copy Selected Text"),
        "Ctrl+Shift+C",
    ),
    (2, ("貼り付け", "Paste"), "Ctrl+V"),
    (
        2,
        ("下段を表示・閉じる", "Show or Hide Strip Below"),
        "Ctrl+`",
    ),
    (
        2,
        ("下段から本文へ戻る", "Back to Text from Strip Below"),
        "Ctrl+Alt+Up",
    ),
    (3, ("コピー", "Copy"), "Ctrl+C"),
    (3, ("切り取り", "Cut"), "Ctrl+X"),
    (3, ("貼り付け", "Paste"), "Ctrl+V"),
    (3, ("全選択", "Select All"), "Ctrl+A"),
];

/// 日本語と英語の対から、いまの言語のほうを。
fn shown((japanese, english): (&'static str, &'static str)) -> &'static str {
    crate::i18n::pick(japanese, english)
}
/// 追加要件 2026-09-14: 設定のTABが前にあるときに通す操作。**TABとペインを
/// 移る・閉じる・開く**だけで、本文に効くもの（保存・置換・Undo・字の編集）は
/// 通さない——設定のTABの下にあるのは代役の空文書で、保存すれば空の無題が
/// ディスクへ出ていく。
///
/// **検索（4）は通す**（書き手の報告 2026-09-15：設定TABでCtrl+Fが効かない）。設定のTABでは
/// 検索欄が「設定を探す」欄になり、代役の文書は探さない。
const SETTINGS_PASS: &[i32] = &[0, 3, 4, 7, 10, 11, 12, 13, 14, 15, 16, 18, 19, 20];
/// 設定のTABが前にあるときに通す操作か（追加要件 2026-09-14、RFN01-18で拡張）。
///
/// 既存の46枠は[`SETTINGS_PASS`]のまま。**メニューから来た分は、新しいTABを
/// 作るものだけ通す**——挿入は代役の空文書へ入ってしまうので通さない。
fn passes_settings(command: i32) -> bool {
    let Ok(id) = usize::try_from(command) else {
        return false;
    };
    if id < NAMES.len() {
        return SETTINGS_PASS.contains(&command);
    }
    matches!(
        menu_at(id).map(|(action, _)| action.action),
        Some(
            ShortcutAction::NewFile | ShortcutAction::NewTerminal | ShortcutAction::FolderTerminal
        )
    )
}
const SCOPES: &[i32] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0,
];
/// メニューから来た操作（RFN01-18）。**`NAMES`の後ろへ続く**——並びの位置が
/// そのままidなので、既存の46枠を動かさないよう末尾へだけ足す。
///
/// **名前はメニューの文言と同じ**にする。設定画面で探す書き手が見るのは、どちらも
/// メニューの言葉である。**既定は空（未設定）**で、キーは書き手が決める。
struct MenuAction {
    /// 文言（日本語・英語）。**挿入の形は`None`**——挿入メニューの表
    /// （`InsertShape::title`）から引き、メニューと同じ言葉を2か所に書かない。
    name: Option<(&'static str, &'static str)>,
    /// 既定のキー。**空は「まだ決めていない」**（`DEFAULTS`の空と同じ意味）。
    default: &'static str,
    action: ShortcutAction,
}
/// 既定を持たない操作を並べるための短い書き方。
macro_rules! menu_action {
    ($ja:expr, $en:expr, $action:expr) => {
        MenuAction {
            name: Some(($ja, $en)),
            default: "",
            action: $action,
        }
    };
    // 既定を持つもの（いまは新規文書の`Ctrl+N`だけ）。
    ($ja:expr, $en:expr, $default:expr, $action:expr) => {
        MenuAction {
            name: Some(($ja, $en)),
            default: $default,
            action: $action,
        }
    };
}
/// メニューから来た操作の分類。`CATEGORY_NAMES`の番号。
const FILE_CATEGORY: i32 = 7;
const INSERT_CATEGORY: i32 = 8;
const RUN_CATEGORY: i32 = 9;
/// 実行の分類に並べる、メニューから来た操作（RFN01-63）。
///
/// **スペルミスへ移る操作は既定を持つ**（F8／Shift+F8、書き手と合意 2026-09-28）。
/// F3（検索の次の一致）と共用すると、どちらに効くかの決まりが要るので分けた。
const RUN_ACTIONS: &[MenuAction] = &[
    menu_action!(
        "次のスペルミス",
        "Next Spelling Error",
        "F8",
        ShortcutAction::SpellingStep(false)
    ),
    menu_action!(
        "前のスペルミス",
        "Previous Spelling Error",
        "Shift+F8",
        ShortcutAction::SpellingStep(true)
    ),
    // RFN01-62: 読み上げ。**既定なし**で、書き手が割り当てる（書き手と合意 2026-09-28）。
    // idは末尾へ足す。
    menu_action!(
        "読み上げ・停止",
        "Read Aloud / Stop",
        ShortcutAction::ReadAloud
    ),
];
/// Pane・TABの分類に足す、左のPaneの操作（RFN01-75/76、書き手と合意 2026-11-02）。
/// **idは末尾へ足す**——設定は番号で持っているので、途中に入れると後ろの割り当てがずれる。
const PANE_CATEGORY: i32 = 4;
const PANE_ACTIONS: &[MenuAction] = &[
    menu_action!(
        "フォルダ内を検索",
        "Search in Folder",
        "Ctrl+Shift+F",
        ShortcutAction::SearchInFolder
    ),
    menu_action!(
        "Explore Paneへ",
        "To Explore Pane",
        "Ctrl+Shift+E",
        ShortcutAction::ExplorePane
    ),
    // 書き手の求め 2026-11-02: 置換の入口をキーにも。末尾へ足す。
    menu_action!(
        "フォルダ内を置換",
        "Replace in Folder",
        "Ctrl+Shift+H",
        ShortcutAction::ReplaceInFolder
    ),
    // RFN01-82（書き手の求め 2026-11-03）: 左ペインの開閉をキーにも。末尾へ足す。
    // **キーはVS Codeの「Toggle Sidebar Visibility」**（最初はCtrl+Alt+L、同じ日に替えた）。
    menu_action!(
        "左ペインを開く・閉じる",
        "Show or Hide Left Pane",
        "Ctrl+B",
        ShortcutAction::ToggleLeftPane
    ),
    // RFN01-82: VS Codeの既定に合わせて足す（書き手と合意 2026-11-03）。末尾へ。
    menu_action!(
        "Outlineを開く",
        "Show Outline",
        "Ctrl+Shift+O",
        ShortcutAction::ShowLeftTab(3)
    ),
    menu_action!(
        "Git Changesを開く",
        "Show Git Changes",
        "Ctrl+Shift+G",
        ShortcutAction::ShowLeftTab(8)
    ),
    menu_action!(
        "設定を開く",
        "Open Settings",
        "Ctrl+,",
        ShortcutAction::OpenSettings
    ),
    menu_action!(
        "右へ分割",
        "Split Right",
        "Ctrl+\\",
        ShortcutAction::SplitRight
    ),
];
/// 本文の操作の分類に足す、VS Codeに合わせた操作（RFN01-82、書き手と合意 2026-11-03）。
/// **idは末尾へ足す。**
const TEXT_CATEGORY: i32 = 0;
const TEXT_ACTIONS: &[MenuAction] = &[
    // 横書きで本文を1行ぶん動かす。キャレットは動かさない。縦書きでは画面の上下へ
    // 動かす先が無いので、これまでどおり矢印として効く。
    menu_action!(
        "1行上へスクロール",
        "Scroll Line Up",
        "Ctrl+Up",
        ShortcutAction::ScrollLine(false)
    ),
    menu_action!(
        "1行下へスクロール",
        "Scroll Line Down",
        "Ctrl+Down",
        ShortcutAction::ScrollLine(true)
    ),
    menu_action!(
        "行を選択",
        "Select Line",
        "Ctrl+L",
        ShortcutAction::SelectLine
    ),
    // RFN01-87: コマンドパレット。VS Codeの「Go to File」と「Show All Commands」。
    menu_action!(
        "ファイル名で開く",
        "Go to File",
        "Ctrl+P",
        ShortcutAction::Palette(false)
    ),
    menu_action!(
        "コマンドパレット",
        "Command Palette",
        "Ctrl+Shift+P",
        ShortcutAction::Palette(true)
    ),
];
/// RFN01-107（書き手の求め 2026-11-05）: 下段の操作。Pane・TABの分類に並べるが、**idは
/// いちばん後ろ**——`PANE_ACTIONS`に足すと、後ろの`TEXT_ACTIONS`の割り当てがずれる。
const STRIP_ACTIONS: &[MenuAction] = &[menu_action!(
    "下段のTABを閉じる",
    "Close Strip Tab",
    "",
    ShortcutAction::CloseStripTab
)];

/// RFN01-107: この操作が「下段のTABを閉じる」か（パレットは下段が無ければ出さない）。
pub(crate) fn closes_strip(id: i32) -> bool {
    usize::try_from(id)
        .ok()
        .and_then(menu_at)
        .is_some_and(|(action, _)| action.action == ShortcutAction::CloseStripTab)
}

/// RFN01-107: この名前（いまの言語の表示）の操作がキーに割り当てられるか。パレットの行に
/// 鍵の印を出すかを決める。メニューの行は道の最後の名前で訊く。
pub(crate) fn assignable(name: &str) -> bool {
    (0..count()).any(|id| scope_at(id) == 0 && shown(name_at(id)) == name)
}

/// ファイルの分類に並べる、メニューから来た操作。
///
/// **新規文書だけは既定を持つ**（`Ctrl+N`、書き手の合意 2026-09-21）——Windowsの
/// 慣例どおりのキーで、どこも使っていない。
const FILE_ACTIONS: &[MenuAction] = &[
    menu_action!("新規文書", "New File", "Ctrl+N", ShortcutAction::NewFile),
    menu_action!(
        "新しいTerminal",
        "New Terminal",
        ShortcutAction::NewTerminal
    ),
    // RFN01-82: VS Codeの「Open New Command Prompt」（Ctrl+Shift+C）に当たる。
    // Quick Draftの全文コピーとTerminalのコピーも同じキーだが、場面が違う。
    menu_action!(
        "文書のフォルダでTerminal起動",
        "Terminal in Document Folder",
        "Ctrl+Shift+C",
        ShortcutAction::FolderTerminal
    ),
];
impl MenuAction {
    fn name(&self) -> (&'static str, &'static str) {
        match (self.name, self.action) {
            (Some(name), _) => name,
            (None, ShortcutAction::Insert(shape)) => shape.title(),
            (None, _) => ("", ""),
        }
    }
}
/// 挿入の形を並べるための短い書き方。**番号は書かない**——形で指す（RFN01-48）。
/// 文言は挿入メニューの表から引く。
const fn insert_shape(shape: InsertShape) -> MenuAction {
    MenuAction {
        name: None,
        default: "",
        action: ShortcutAction::Insert(shape),
    }
}
const fn insert(what: InsertEdit) -> MenuAction {
    insert_shape(InsertShape::Edit(what))
}
const fn line_note(what: LineNoteEdit) -> MenuAction {
    insert_shape(InsertShape::Line(what))
}
/// 挿入の分類に並べる、メニューから来た操作。
///
/// **持つのは形であって番号ではない。**番号はメニューの並びが変わるたびに動くので、
/// 番号で持つと数え違えて別の形を走らせてしまう。**idは末尾へ足す**——既に割り当てた
/// キーを動かさないためで、だから画面の並びとこの一覧の並びは同じとは限らない
/// （画像はメニューの途中、一覧では末尾にある）。
const INSERT_ACTIONS: &[MenuAction] = &[
    // リンク・ルビ・注記
    insert(InsertEdit::MarkdownLink),
    insert(InsertEdit::WikiLink),
    insert(InsertEdit::WikiLinkAlias),
    insert(InsertEdit::Ruby),
    insert(InsertEdit::SideNote),
    insert(InsertEdit::RubyWithSideNote),
    // 傍点
    insert(InsertEdit::EmphasisDots),
    insert(InsertEdit::SesameDotsNote),
    insert(InsertEdit::RoundDotsNote),
    insert(InsertEdit::WhiteRoundDotsNote),
    insert(InsertEdit::DoubleRoundDotsNote),
    insert(InsertEdit::CrossDotsNote),
    // 傍線
    insert(InsertEdit::LineNote),
    insert(InsertEdit::DoubleLineNote),
    insert(InsertEdit::WaveLineNote),
    insert(InsertEdit::ChainLineNote),
    insert(InsertEdit::DashLineNote),
    // 文字注記
    insert(InsertEdit::Upright),
    insert(InsertEdit::Warichu),
    insert(InsertEdit::SmallText),
    insert(InsertEdit::LargeText),
    // 見出し
    line_note(LineNoteEdit::Heading(1)),
    line_note(LineNoteEdit::Heading(2)),
    line_note(LineNoteEdit::Heading(3)),
    line_note(LineNoteEdit::Heading(4)),
    line_note(LineNoteEdit::Heading(5)),
    line_note(LineNoteEdit::Heading(6)),
    line_note(LineNoteEdit::NoHeading),
    // 字下げ
    line_note(LineNoteEdit::Indent(1)),
    line_note(LineNoteEdit::Indent(2)),
    line_note(LineNoteEdit::Indent(3)),
    line_note(LineNoteEdit::Indent(4)),
    line_note(LineNoteEdit::NoIndent),
    // 地付き
    line_note(LineNoteEdit::AlignToEnd(0)),
    line_note(LineNoteEdit::AlignToEnd(1)),
    line_note(LineNoteEdit::AlignToEnd(2)),
    line_note(LineNoteEdit::AlignToEnd(3)),
    line_note(LineNoteEdit::AlignToEnd(4)),
    line_note(LineNoteEdit::NoAlignToEnd),
    // 改ページ
    insert_shape(InsertShape::PageBreak),
    // 箇条書き（`document::BULLET_MARKS`の並び）
    menu_action!("箇条書き -", "Bullet List -", ShortcutAction::Bullets(0)),
    menu_action!("箇条書き *", "Bullet List *", ShortcutAction::Bullets(1)),
    menu_action!("箇条書き +", "Bullet List +", ShortcutAction::Bullets(2)),
    menu_action!(
        "番号付きリスト",
        "Numbered List",
        ShortcutAction::NumberedList
    ),
    menu_action!("番号を振り直す", "Renumber", ShortcutAction::Renumber),
    // 画像（RFN01-48）。**idは末尾へ足す**——既に割り当てたキーを動かさないため。
    insert(InsertEdit::MarkdownImage),
    insert(InsertEdit::WikiImage),
    // Obsidianの記法（書き手の求め 2026-09-22）。同じく末尾へ。
    insert(InsertEdit::Highlight),
    insert(InsertEdit::Comment),
    insert(InsertEdit::Footnote),
    insert(InsertEdit::Callout(CalloutType::Note)),
    insert(InsertEdit::Callout(CalloutType::Tip)),
    insert(InsertEdit::Callout(CalloutType::Important)),
    insert(InsertEdit::Callout(CalloutType::Warning)),
    insert(InsertEdit::Callout(CalloutType::Caution)),
    // 表（RFN01-49）。キーではマス目がキャレットの所に開く。同じく末尾へ。
    insert_shape(InsertShape::Table),
    // RFN01-88: テンプレート。パレットがテンプレートの一覧で開く。同じく末尾へ。
    insert_shape(InsertShape::Template),
];
/// 変更できる操作の数——既存の46枠と、メニューから来た分。
fn count() -> usize {
    NAMES.len()
        + FILE_ACTIONS.len()
        + INSERT_ACTIONS.len()
        + RUN_ACTIONS.len()
        + PANE_ACTIONS.len()
        + TEXT_ACTIONS.len()
        + STRIP_ACTIONS.len()
}
/// idがメニューから来たものなら、その1つと分類の番号。
fn menu_at(id: usize) -> Option<(&'static MenuAction, i32)> {
    let index = id.checked_sub(NAMES.len())?;
    FILE_ACTIONS
        .get(index)
        .map(|action| (action, FILE_CATEGORY))
        .or_else(|| {
            INSERT_ACTIONS
                .get(index - FILE_ACTIONS.len())
                .map(|action| (action, INSERT_CATEGORY))
        })
        .or_else(|| {
            RUN_ACTIONS
                .get(index - FILE_ACTIONS.len() - INSERT_ACTIONS.len())
                .map(|action| (action, RUN_CATEGORY))
        })
        .or_else(|| {
            let before = FILE_ACTIONS.len() + INSERT_ACTIONS.len() + RUN_ACTIONS.len();
            PANE_ACTIONS
                .get(index - before)
                .map(|action| (action, PANE_CATEGORY))
        })
        .or_else(|| {
            let before =
                FILE_ACTIONS.len() + INSERT_ACTIONS.len() + RUN_ACTIONS.len() + PANE_ACTIONS.len();
            TEXT_ACTIONS
                .get(index - before)
                .map(|action| (action, TEXT_CATEGORY))
        })
        .or_else(|| {
            let before = FILE_ACTIONS.len()
                + INSERT_ACTIONS.len()
                + RUN_ACTIONS.len()
                + PANE_ACTIONS.len()
                + TEXT_ACTIONS.len();
            STRIP_ACTIONS
                .get(index - before)
                .map(|action| (action, PANE_CATEGORY))
        })
}
fn name_at(id: usize) -> (&'static str, &'static str) {
    match menu_at(id) {
        Some((action, _)) => action.name(),
        None => NAMES[id],
    }
}
fn default_at(id: usize) -> &'static str {
    match menu_at(id) {
        Some((action, _)) => action.default,
        None => DEFAULTS[id],
    }
}
fn category_at(id: usize) -> i32 {
    match menu_at(id) {
        Some((_, category)) => category,
        None => CATEGORIES[id],
    }
}
fn scope_at(id: usize) -> i32 {
    match menu_at(id) {
        // **メニューの操作は本文の場面**——メニューも本文から開く。
        Some(_) => 0,
        None => SCOPES[id],
    }
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
/// 押された組み合わせが、保存する綴りになるか。
///
/// 取るのは**修飾キー付き**か、**F1〜F12だけ**（書き手の合意 2026-09-21）。素の
/// 打鍵を取ると本文が打てなくなるので、`Shift`だけの`Shift+A`も同じ理由で断る。
/// 固定の基本編集と、窓が先に取る鍵（[`TAKEN`]）も断る。
fn normalize(value: &str) -> Option<String> {
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
    if !control && !alt && !is_function(&key) {
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
fn chord(text: &str, control: bool, alt: bool, shift: bool) -> Option<String> {
    use slint::platform::Key;
    let special = [
        (Key::Tab, "Tab"),
        (Key::LeftArrow, "Left"),
        (Key::RightArrow, "Right"),
        (Key::UpArrow, "Up"),
        (Key::DownArrow, "Down"),
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
    // **修飾キー付きか、F1〜F12だけ**（書き手の合意 2026-09-21）。
    if !control && !alt && !is_function(&key) {
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
/// RFN01-82: 割り当ての書き方の版。**保存はどの枠も明示して書く**ので（`i=`は
/// 「キーなし」）、既定を替えても、前に保存した書き手には古い既定が残る。この印の
/// 無い割り当ては、[`migrations`]の古い既定のままの枠だけを新しい既定へ移す。
/// 印は保存のたびに書くので、移すのは一度きり——後で書き手が外した枠は外れたまま。
const BINDINGS_VERSION: u32 = 4;
/// 既定を替えた枠と、その前の既定、替えた版。
///
/// 2：VS Codeに合わせた（RFN01-82、2026-11-03）。3：Ctrl+Pをファイル名で開くへ
/// 譲り、印刷プレビューはキーなし（RFN01-87、2026-11-03）。4：縦書き・横書きの
/// 切り替えにCtrl+R（同）。
fn migrations() -> Vec<(u32, usize, &'static str)> {
    let mut moved = vec![(2, 43, ""), (3, 45, "Ctrl+P"), (4, 42, "")];
    for (version, action, before) in [
        (2, ShortcutAction::FolderTerminal, ""),
        (2, ShortcutAction::ToggleLeftPane, "Ctrl+Alt+L"),
    ] {
        if let Some(id) = id_of(action) {
            moved.push((version, id, before));
        }
    }
    moved
}
/// 保存した割り当ての版。印が無ければ1（RFN01-82より前）。
fn saved_version(raw: &str) -> u32 {
    raw.split(';')
        .find_map(|entry| entry.strip_prefix("v=")?.parse().ok())
        .unwrap_or(1)
}
/// メニューから来た操作のid。
fn id_of(action: ShortcutAction) -> Option<usize> {
    (NAMES.len()..count()).find(|id| menu_at(*id).is_some_and(|(held, _)| held.action == action))
}
/// 保存する形。[`BINDINGS_VERSION`]を添える。
fn encode(keys: &[String]) -> String {
    keys.iter()
        .enumerate()
        .map(|(i, k)| format!("{i}={k}"))
        .chain(std::iter::once(format!("v={BINDINGS_VERSION}")))
        .collect::<Vec<_>>()
        .join(";")
}
fn bindings(raw: &str) -> Vec<String> {
    let mut keys: Vec<_> = (0..count()).map(|i| default_at(i).to_string()).collect();
    for entry in raw.split(';') {
        if let Some((i, value)) = entry.split_once('=') {
            if let (Ok(i), Some(key)) = (i.parse::<usize>(), normalize(value)) {
                if i < keys.len() {
                    keys[i] = key;
                }
            }
        }
    }
    if !raw.trim().is_empty() {
        let saved = saved_version(raw);
        for (version, id, before) in migrations() {
            if version > saved && keys.get(id).is_some_and(|key| key == before) {
                keys[id] = default_at(id).to_owned();
            }
        }
    }
    // ID 18 stays reserved for saved bindings; New Tab is now the + button only.
    keys[18].clear();
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
/// メニューから来た操作に、いま割り当てているキー（メニューの行に併記する）。
pub(crate) fn action_label(app: &AppWindow, action: ShortcutAction) -> String {
    (NAMES.len()..count())
        .find(|id| menu_at(*id).is_some_and(|(held, _)| held.action == action))
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
    let Some(key) = chord(text, control, alt, shift) else {
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
    for (category, title) in CATEGORY_NAMES.iter().enumerate() {
        let category = category as i32;
        let changeable = (0..count())
            .filter(|i| *i != 18 && category_at(*i) == category)
            .map(|i| (i as i32, shown(name_at(i)), keys[i].as_str(), false));
        let fixed = FIXED
            .iter()
            .filter(|(owner, _, _)| *owner == category)
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
            name: shown(*title).into(),
            key: Default::default(),
            header: true,
            category,
            open,
            fixed: false,
            note: shown(CATEGORY_NOTES[category as usize]).into(),
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
    // キーのプリセット（RFN01-31）がいまの割り当てに合うか。
    crate::settings_transfer::publish(app);
}
pub fn wire(app: &AppWindow, live: &Live) {
    publish(app);
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
        let Some(key) = chord(&text, control, alt, shift) else {
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
        if chord(&text, control, alt, shift).is_none() && text.chars().any(|c| c as u32 >= 0xE000) {
            return;
        }
        let key = chord(&text, control, alt, shift).unwrap_or_default();
        app.set_shortcut_edit(if key.is_empty() {
            shown(("使用不可", "Not available")).into()
        } else {
            key.clone().into()
        });
        app.set_shortcut_status(
            if !key.is_empty() && normalize(&key).is_some() {
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
            normalize(&app.get_shortcut_edit())
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
    app.on_left_field_shortcut(move |text, control, alt, shift| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        let command = resolve(&app.get_shortcut_bindings(), 0, &text, control, alt, shift);
        let Some((action, _)) = usize::try_from(command).ok().and_then(menu_at) else {
            return false;
        };
        match action.action {
            ShortcutAction::SearchInFolder => app.invoke_open_folder_search(),
            ShortcutAction::ReplaceInFolder => app.invoke_open_folder_replace(),
            ShortcutAction::ExplorePane => app.invoke_focus_explore(),
            ShortcutAction::ToggleLeftPane => app.invoke_toggle_left_pane(),
            ShortcutAction::ShowLeftTab(tab) => app.invoke_show_left_tab(tab),
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
        // RFN01-67（書き手の求め 2026-10-03）: Git Repository の画面では「戻る」で画面を閉じる。
        // ほかのショートカットは、下に隠れているペインへは届けない。
        if app.get_git_repo_active() {
            if command == 19 {
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
            .and_then(menu_at)
            .is_some_and(|(action, _)| matches!(action.action, ShortcutAction::ScrollLine(_)))
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
/// File, Next Tab (11) and Previous Tab (12).
fn passes_terminal(command: i32) -> bool {
    command == 11
        || command == 12
        || usize::try_from(command)
            .ok()
            .and_then(menu_at)
            .is_some_and(|(action, _)| {
                matches!(
                    action.action,
                    ShortcutAction::Palette(_) | ShortcutAction::NewFile
                )
            })
}

/// RFN01-87: コマンドパレットに並べる、キーの一覧の操作。
pub(crate) struct PaletteAction {
    pub title: String,
    /// 表示していない側の言葉。
    pub other: String,
    pub key: String,
    pub id: i32,
}

/// RFN01-87: 本文の場面の操作を、いまの割り当てと一緒に。**パレットを開く操作自身と、
/// 枠だけの18番は除く。**メニューにもある操作は、パレットの側で名前を見て重ねない。
pub(crate) fn palette_actions(app: &AppWindow) -> Vec<PaletteAction> {
    let keys = bindings(&app.get_shortcut_bindings());
    (0..count())
        .filter(|id| *id != 18 && scope_at(*id) == 0)
        .filter(|id| {
            !menu_at(*id)
                .is_some_and(|(action, _)| matches!(action.action, ShortcutAction::Palette(_)))
        })
        .map(|id| {
            let (ja, en) = name_at(id);
            let title = shown((ja, en));
            PaletteAction {
                title: title.to_owned(),
                other: if title == ja { en } else { ja }.to_owned(),
                key: keys[id].clone(),
                id: id as i32,
            }
        })
        .collect()
}

/// 割り当ての一覧のidで、その操作を走らせる。キーからも、コマンドパレットからも
/// （RFN01-87）——**同じ道**なので、パレットで選んだ操作はキーと同じに効く。
pub(crate) fn run_command(app: &AppWindow, live: &Live, pane: crate::PaneId, command: i32) {
    match command {
        0 => app.invoke_open_file_requested(),
        1 => app.invoke_save_requested(),
        2 => app.invoke_save_as_requested(),
        3 => {
            let at = live.tabs.borrow().of(pane).active;
            crate::close_tab(&app, &live, pane, at);
        }
        4 => app.invoke_toggle_find(pane.index(), false),
        5 => app.invoke_toggle_find(pane.index(), true),
        6 => app.invoke_goto_requested(false),
        7 => crate::reopen_closed_tab(&app, &live),
        8 => app.invoke_pane_undo(pane.index(), false),
        9 => app.invoke_pane_undo(pane.index(), true),
        10 => app.invoke_quick_draft_requested(),
        11 => app.invoke_pane_tab_stepped(pane.index(), false),
        12 => app.invoke_pane_tab_stepped(pane.index(), true),
        13 => app.invoke_pane_focus_moved(pane.index(), 0),
        14 => app.invoke_pane_focus_moved(pane.index(), 1),
        15 => app.invoke_pane_focus_moved(pane.index(), 2),
        16 => {
            if crate::below_kind(live.cache.borrow_mut().pane(pane)) != 0 {
                app.invoke_pane_below_focus(pane.index(), true);
            } else {
                app.invoke_pane_focus_moved(pane.index(), 3);
            }
        }
        17 => app.invoke_pane_below_toggled(pane.index()),
        18 => {} // Reserved: New Tab is the + button only.
        19 => app.invoke_pane_navigate(pane.index(), false),
        20 => app.invoke_pane_navigate(pane.index(), true),
        21 => app.invoke_pane_delete(pane.index()),
        22 => app.invoke_pane_move(pane.index(), 3, false),
        23 => app.invoke_pane_move(pane.index(), -3, false),
        24 => app.invoke_pane_kill(pane.index(), 0),
        25 => app.invoke_pane_mark_toggled(pane.index(), false),
        26 => app.invoke_pane_mark_toggled(pane.index(), true),
        27 => app.invoke_pane_kill(pane.index(), 1),
        28 => app.invoke_pane_kill(pane.index(), 2),
        29 => app.invoke_pane_kill(pane.index(), 3),
        30 => app.invoke_pane_kill(pane.index(), 4),
        31 => app.invoke_pane_move(pane.index(), 3, true),
        32 => app.invoke_pane_move(pane.index(), -3, true),
        33 => app.invoke_pane_line_edit(pane.index(), 4),
        34 => app.invoke_pane_line_edit(pane.index(), 0),
        35 => app.invoke_pane_line_edit(pane.index(), 1),
        36 => app.invoke_pane_line_edit(pane.index(), 2),
        37 => app.invoke_pane_line_edit(pane.index(), 3),
        42 => app.invoke_pane_direction_toggled(pane.index()),
        43 => {
            // ReadOnlyはViewerを抜けずに断る（追加要件 2026-09-15）——抜けて
            // プレビューへ回すと、読むだけのつもりの面が書ける面になる。
            let viewer = live.states.of(pane).borrow().viewer;
            if viewer && !pane.reads_only(&app) {
                app.invoke_pane_viewer_toggled(pane.index());
            }
            app.invoke_pane_preview_toggled(pane.index());
        }
        44 => app.invoke_pane_viewer_toggled(pane.index()),
        // 要件 7.10: 紙の形で見る。**入口は窓に一つ**なので、どの面から
        // 押しても同じ——刷るのは前に出ている文書である。
        45 => app.invoke_print_requested(),
        // RFN01-18: メニューから来た操作は、メニューと同じ道で実行する
        // （`menu_at`が`None`を返す18・38〜41番は、これまでどおり何もしない）。
        _ => {
            if let Some(action) = menu_at(command as usize).map(|(action, _)| action.action) {
                crate::menu_commands::run_shortcut(&app, &live, action);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_new_tab_binding_does_not_shift_following_ids() {
        assert!(bindings("18=Ctrl+Alt+N;19=Ctrl+Alt+B")[18].is_empty());
        assert_eq!(
            resolve("18=Ctrl+Alt+N;19=Ctrl+Alt+B", 0, "n", true, true, false),
            -1
        );
        assert_eq!(
            resolve("18=Ctrl+Alt+N;19=Ctrl+Alt+B", 0, "b", true, true, false),
            19
        );
    }
    #[test]
    fn settings_validate_and_roundtrip() {
        assert_eq!(normalize("ctrl+alt+s").as_deref(), Some("Ctrl+Alt+S"));
        assert!(normalize("S").is_none());
        assert!(normalize("Ctrl+A").is_none());
        assert_eq!(bindings("1=Ctrl+Alt+S")[1], "Ctrl+Alt+S");
        assert_eq!(bindings("1=Ctrl+O")[1], "Ctrl+S");
    }
    #[test]
    fn scopes_allow_reuse_but_body_categories_conflict() {
        let keys = bindings("1=Ctrl+Alt+S;38=Ctrl+Alt+S");
        assert_eq!(keys[1], "Ctrl+Alt+S");
        assert_eq!(keys[38], "Ctrl+Alt+S");
        assert_eq!(
            resolve("1=Ctrl+Alt+S;38=Ctrl+Alt+S", 0, "s", true, true, false),
            1
        );
        assert_eq!(
            resolve("1=Ctrl+Alt+S;38=Ctrl+Alt+S", 1, "s", true, true, false),
            38
        );
        assert_eq!(conflict(&keys, 24, "Ctrl+Alt+S"), Some(1));
        assert!(conflict(&keys, 10, "").is_none());
    }
    #[test]
    fn modified_emacs_and_tab_keys_consume_the_old_binding() {
        assert_eq!(resolve("24=Ctrl+Alt+K", 0, "k", true, false, false), -2);
        assert_eq!(resolve("24=Ctrl+Alt+K", 0, "k", true, true, false), 24);
        assert_eq!(resolve("38=Ctrl+Alt+C", 1, "C", true, false, true), -2);
        assert_eq!(resolve("38=Ctrl+Alt+C", 1, "c", true, true, false), 38);
        let tab: slint::SharedString = slint::platform::Key::Tab.into();
        assert_eq!(resolve("", 0, &tab, true, false, true), 12);
        // RFN01-63: F8／Shift+F8は既定で次／前のスペルミス。
        let f8: slint::SharedString = slint::platform::Key::F8.into();
        let next = resolve("", 0, &f8, false, false, false);
        let back = resolve("", 0, &f8, false, false, true);
        assert_eq!(
            menu_at(next as usize).map(|(action, _)| action.action),
            Some(ShortcutAction::SpellingStep(false))
        );
        assert_eq!(
            menu_at(back as usize).map(|(action, _)| action.action),
            Some(ShortcutAction::SpellingStep(true))
        );
        assert_eq!(
            chord(" ", true, false, true).as_deref(),
            Some("Ctrl+Shift+Space")
        );
        for fixed in ["Ctrl+C", "Ctrl+X", "Ctrl+V", "Ctrl+A"] {
            assert!(normalize(fixed).is_none());
        }
        assert_eq!(resolve("", 1, "k", true, false, false), -1);
    }
    #[test]
    fn modifiers_do_not_become_letters() {
        use slint::platform::Key;
        let settings = "42=Ctrl+Alt+R;43=Ctrl+Alt+P";
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
        assert_eq!(resolve(settings, 0, "r", true, true, false), 42);
        assert_eq!(resolve(settings, 0, "p", true, true, false), 43);
        // RFN01-87: Ctrl+Rは縦書き・横書きの既定。書き手が別のキーへ移したあとは、
        // 古い処理へ回さずに飲み込む（`-2`、既定のキーの扱いと同じ）。
        assert_eq!(resolve(settings, 0, "r", true, false, false), -2);
        assert_eq!(resolve("", 0, "r", true, false, false), 42);
    }

    /// RFN01-18: **メニューから来た操作は、既定では何も決まっていない。**
    /// 例外は新規文書の`Ctrl+N`だけ（書き手の合意 2026-09-21）。
    #[test]
    fn menu_actions_start_unassigned_except_new_file() {
        let keys = bindings("");
        assert_eq!(keys[NAMES.len()], "Ctrl+N");
        assert_eq!(keys[NAMES.len() + 1], "");
        for (id, key) in keys.iter().enumerate().skip(NAMES.len()) {
            let expected = default_at(id);
            assert_eq!(key, expected, "{id}");
        }
    }

    /// 割り当てれば効き、割り当てていなければ効かない。
    #[test]
    fn an_assigned_menu_action_runs_and_an_unassigned_one_does_not() {
        let new_file = NAMES.len() as i32;
        let insert = new_file + FILE_ACTIONS.len() as i32;
        let raw = format!("{insert}=Ctrl+Shift+U");
        assert_eq!(resolve(&raw, 0, "u", true, false, true), insert);
        assert_eq!(resolve("", 0, "u", true, false, true), -1);
        // 新規文書の`Ctrl+N`は既定のまま効く。
        assert_eq!(resolve("", 0, "n", true, false, false), new_file);
    }

    /// RFN01-82: VS Codeの既定に合わせたキー（書き手と合意 2026-11-03）。
    #[test]
    fn vs_code_keys_are_the_defaults() {
        let id = |action| id_of(action).expect("the action is listed") as i32;
        assert_eq!(
            resolve("", 0, "b", true, false, false),
            id(ShortcutAction::ToggleLeftPane)
        );
        assert_eq!(resolve("", 0, "V", true, false, true), 43);
        assert_eq!(
            resolve("", 0, "O", true, false, true),
            id(ShortcutAction::ShowLeftTab(3))
        );
        assert_eq!(
            resolve("", 0, "G", true, false, true),
            id(ShortcutAction::ShowLeftTab(8))
        );
        assert_eq!(
            resolve("", 0, ",", true, false, false),
            id(ShortcutAction::OpenSettings)
        );
        assert_eq!(
            resolve("", 0, "\\", true, false, false),
            id(ShortcutAction::SplitRight)
        );
        assert_eq!(
            resolve("", 0, "l", true, false, false),
            id(ShortcutAction::SelectLine)
        );
        let up: slint::SharedString = slint::platform::Key::UpArrow.into();
        let down: slint::SharedString = slint::platform::Key::DownArrow.into();
        assert_eq!(
            resolve("", 0, &up, true, false, false),
            id(ShortcutAction::ScrollLine(false))
        );
        assert_eq!(
            resolve("", 0, &down, true, false, false),
            id(ShortcutAction::ScrollLine(true))
        );
        // Ctrl+Shift+Cは場面で分かれる：本文ではTerminal起動、Quick Draftでは全文コピー。
        assert_eq!(
            resolve("", 0, "C", true, false, true),
            id(ShortcutAction::FolderTerminal)
        );
        assert_eq!(resolve("", 1, "C", true, false, true), 38);
        // Ctrl+Alt+Lはもう何にも当たっていない。
        assert_eq!(resolve("", 0, "l", true, true, false), -1);
    }

    /// RFN01-82: **前の版が保存した割り当てでも、新しい既定が効く。**保存はどの枠も
    /// 明示して書くので、古い既定（空やCtrl+Alt+L）がそのまま残っている。移すのは
    /// 古い既定のままの枠だけで、書き手が替えたキーは残す。印を付けて保存したあとは
    /// 移さない——後で外した枠は外れたまま。
    #[test]
    fn bindings_saved_before_the_change_take_the_new_defaults_once() {
        let toggle = id_of(ShortcutAction::ToggleLeftPane).unwrap();
        let folder = id_of(ShortcutAction::FolderTerminal).unwrap();
        // 書き手のセッションと同じ形：どの枠も書いてあり、ふたつは自分で決めたキー。
        let mut saved: Vec<String> = (0..count())
            .map(|i| format!("{i}={}", default_at(i)))
            .collect();
        saved[43] = "43=".into();
        saved[folder] = format!("{folder}=");
        saved[toggle] = format!("{toggle}=Ctrl+Alt+L");
        saved[10] = "10=F5".into();
        let saved = saved.join(";");
        let keys = bindings(&saved);
        assert_eq!(keys[43], "Ctrl+Shift+V");
        assert_eq!(keys[folder], "Ctrl+Shift+C");
        assert_eq!(keys[toggle], "Ctrl+B");
        assert_eq!(keys[10], "F5", "the writer's own key stays");
        // 印のある保存では、空は「外した」の意味で、そのまま。
        let mut after = keys.clone();
        after[43].clear();
        let encoded = encode(&after);
        assert!(encoded.ends_with(&format!("v={BINDINGS_VERSION}")));
        assert_eq!(bindings(&encoded)[43], "");
        assert_eq!(bindings(&encoded)[toggle], "Ctrl+B");
    }

    /// RFN01-87: Ctrl+Pはファイル名で開く、Ctrl+Shift+Pはコマンドパレット。**前に
    /// 保存した割り当ての印刷プレビューのCtrl+Pは外す**——残すとぶつかって、
    /// 割り当てが全部既定に戻ってしまう。v=2で保存した書き手も同じ。
    #[test]
    fn ctrl_p_goes_to_the_palette_and_print_preview_lets_it_go() {
        let quick = id_of(ShortcutAction::Palette(false)).unwrap();
        let commands = id_of(ShortcutAction::Palette(true)).unwrap();
        assert_eq!(resolve("", 0, "p", true, false, false), quick as i32);
        assert_eq!(resolve("", 0, "P", true, false, true), commands as i32);
        for saved in [
            "0=Ctrl+O;42=;45=Ctrl+P;94=F5",
            "0=Ctrl+O;42=;45=Ctrl+P;94=F5;v=2",
        ] {
            let keys = bindings(saved);
            assert_eq!(keys[45], "", "{saved}");
            // 版4：縦書き・横書きの切り替えにCtrl+R。空のまま保存していた書き手にも付く。
            assert_eq!(keys[42], "Ctrl+R", "{saved}");
            assert_eq!(keys[quick], "Ctrl+P", "{saved}");
            assert_eq!(keys[94], "F5", "{saved}");
        }
        // 版3のあとで書き手が印刷プレビューに戻したなら、それは書き手の決めたこと。
        let mut keys = bindings("");
        keys[quick].clear();
        keys[45] = "Ctrl+P".into();
        assert_eq!(bindings(&encode(&keys))[45], "Ctrl+P");
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
    /// 重複なく指す**。番号ではなく形で持つので、挿入の並びが動いても追随する。
    ///
    /// **画面の並びと一覧の並びは同じとは限らない**——idは末尾へ足すので、画像の
    /// ようにメニューの途中へ入る項目は、一覧では最後に来る（RFN01-48）。
    #[test]
    fn insert_actions_cover_every_shape_once() {
        let mut used = Vec::new();
        for action in INSERT_ACTIONS.iter().map(|action| action.action) {
            match action {
                ShortcutAction::Insert(shape) => used.push(shape),
                ShortcutAction::Bullets(_)
                | ShortcutAction::NumberedList
                | ShortcutAction::Renumber => {}
                other => panic!("挿入の操作ではない: {other:?}"),
            }
        }
        // 文言は挿入メニューの表から来る。**どの行も名前を持つ。**
        for action in INSERT_ACTIONS {
            let (ja, en) = action.name();
            assert!(!ja.is_empty() && !en.is_empty(), "{:?}", action.action);
        }
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

    /// 設定のTABでは、**新しいTABを作るものだけ**通す。
    #[test]
    fn only_new_panes_pass_the_settings_tab() {
        assert!(passes_settings(0), "ファイルを開く");
        assert!(passes_settings(NAMES.len() as i32), "新規文書");
        assert!(passes_settings(NAMES.len() as i32 + 1), "新しいTerminal");
        let insert = (NAMES.len() + FILE_ACTIONS.len()) as i32;
        assert!(!passes_settings(insert), "挿入は代役の空文書へ入ってしまう");
    }
}
