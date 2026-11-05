//! Main menu: immutable invocation target, native popup lifetime, existing commands.
use super::*;
use std::cell::OnceCell;
use windows::{
    Win32::{
        Foundation::{LPARAM, POINT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        UI::WindowsAndMessaging::*,
    },
    core::PCWSTR,
};

#[derive(Clone)]
pub(crate) enum Command {
    NewFile,
    /// RFN01-88: 新規文書をテンプレートから（名前で）。
    NewFromTemplate(String),
    NewTerminal(i32),
    Open,
    Folder,
    Recent(std::path::PathBuf),
    Workspaces,
    Save(bool),
    SaveAll,
    Close,
    CloseOthers(bool),
    CloseClean,
    Reopen,
    Duplicate,
    CopyPath,
    Reveal,
    Encoding(i32),
    Print,
    Exit,
    /// RFN01-66: ヘルプ。0 キーの一覧、1 サンプル文書を開く、2 RFN Editについて。
    Help(i32),
    FolderTerminal,
    Undo(bool),
    Copy(bool),
    Paste,
    Rename,
    Paper(i32),
    Display(i32),
    CopyBody,
    SelectAll,
    Find(bool),
    Goto,
    FolderFind,
    Line(i32),
    Mark(bool),
    Kill(i32),
    Word(u32),
    List(i32, i32),
    Insert(InsertShape),
    Direction(bool),
    Appearance(i32),
    Sidebar(i32),
    Preview,
    Viewer,
    Split(bool),
    OtherPanes,
    Swap(i32),
    Step(bool),
    Focus(i32),
    Below,
    Above,
    Navigate(bool),
    Draft,
    Zoom(i32),
    ZoomReset,
    Compare(i32),
    /// RFN01-67: Git Repository の画面を開く。
    GitRepository,
    /// RFN01-63: スペルチェックを始める／終える（アクティブな文書）。
    Spelling,
    /// RFN01-63: 次（`false`）／前（`true`）のスペルミスへ。
    SpellingStep(bool),
    /// RFN01-62: 読み上げを始める／止める（キャレットの位置から）。
    ReadAloud,
    Terminal(i32),
    Panel(i32),
    Shell(i32),
    ZoomSet(i32),
    /// 書式のツールバー・パレットを出す／しまう（2026-09-23）。
    Toolbar,
    Palette,
    /// RFN01-82: 横書きで本文を1行ぶん動かす（`true`で下へ）。
    ScrollLine(bool),
    /// RFN01-82: キャレットの行を選ぶ。
    SelectLine,
}

/// 設定から割り当てたキーで走らせる、メニューの操作（RFN01-18）。
///
/// **同じ[`Command`]へ落として実行する**——入口が2つになっても、効く先は1つに
/// 保つ。名前と並びは`shortcuts::MENU_ACTIONS`が持ち、ここは意味だけを言う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    NewFile,
    NewTerminal,
    FolderTerminal,
    /// 挿入メニューのひな形そのもの（RFN01-48）。
    Insert(InsertShape),
    /// 箇条書きの記号（`document::BULLET_MARKS`の位置）。
    Bullets(i32),
    NumberedList,
    Renumber,
    /// RFN01-63: 次（`false`）／前（`true`）のスペルミスへ。
    SpellingStep(bool),
    /// RFN01-62: 読み上げを始める／止める。
    ReadAloud,
    /// RFN01-75/76: フォルダ内を検索する（木を見ていればその場所で）。
    SearchInFolder,
    /// RFN01-76: キーボードをExplore Paneの絞り込み欄へ。
    ExplorePane,
    /// RFN01-75: フォルダ内を置換する（検索に置換欄を出して開く）。
    ReplaceInFolder,
    /// RFN01-82: 左ペインを開く・閉じる（縦列で、いま見ている面をもう一度押すのと同じ）。
    ToggleLeftPane,
    /// RFN01-82: 左ペインにこの面を出す（閉じていれば開く。出ていればそのまま）。
    ShowLeftTab(i32),
    /// RFN01-82: Settings を開く。
    OpenSettings,
    /// RFN01-82: いまのペインを右に分割する。
    SplitRight,
    /// RFN01-82: 横書きで本文を1行ぶん動かす（`true`で下へ）。キャレットは動かさない。
    ScrollLine(bool),
    /// RFN01-82: キャレットの行を選ぶ。行を選んでいれば次の行まで広げる。
    SelectLine,
    /// RFN01-87: コマンドパレットを開く。`true`は操作を探す（「>」入り）、`false`はファイル名。
    Palette(bool),
    /// RFN01-107: 下段（Editor Panel・Terminal Panel）の前に出ているTABを閉じる。
    CloseStripTab,
}

/// 挿入メニューのひな形（RFN01-48）。**番号ではなく形で持つ**——番号は挿入の並びを
/// 変えるたびに動くので、番号で持つと数え違えて別の形を走らせてしまう。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertShape {
    /// `document::INSERT_EDITS`の形（字を包むもの）。
    Edit(document::InsertEdit),
    /// `document::LINE_NOTE_EDITS`の形（見出し・字下げ・地付き）。
    Line(document::LineNoteEdit),
    /// 表（RFN01-49）。押すとマス目が開き、選んだ大きさの表を置く。
    Table,
    /// 改ページ。
    PageBreak,
    /// RFN01-88: テンプレート。押すとパレットがテンプレートの一覧で開き、選んだものを
    /// キャレットの所へ入れる。並びの最後の1つである。**書式のボタンには出さない**。
    Template,
}

impl InsertShape {
    /// 挿入の番号——右クリックメニュー（Slint）が押された行を言い返すときの言い方。
    /// **並びを知っているのはここだけ**（[`INSERT_MENU`]を上から数えた位置）。
    /// 番号は開くたびに組み直す行にしか書かないので、並びを変えても古い番号は残らない。
    pub fn number(self) -> i32 {
        insert_items()
            .position(|(shape, _)| *shape == self)
            .map_or(-1, |at| at as i32)
    }

    /// [`Self::number`]の逆。並びの外の番号は`None`。
    pub fn from_number(number: i32) -> Option<Self> {
        let at = usize::try_from(number).ok()?;
        insert_items().nth(at).map(|(shape, _)| *shape)
    }

    /// メニューの文言（日本語・英語）。**タイトルバー・右クリック・キーの設定が
    /// 同じ言葉を読む**——3か所に書くと、1か所だけ直して食い違う。
    pub fn title(self) -> (&'static str, &'static str) {
        insert_items()
            .find(|(shape, _)| *shape == self)
            .map_or(("", ""), |(_, title)| *title)
    }
}

/// 日本語と英語の文言の対。
type Title = (&'static str, &'static str);

/// 挿入メニューの並び（RFN01-38・RFN01-47）。
///
/// **タイトルバーの挿入メニューも右クリックメニューも、この1つの表から組む**——
/// [`InsertEntry::Row`]はそのまま入る行、[`InsertEntry::Group`]は右へ開く組である。
/// タイトルバーだけは、段落注記の中を字下げと地付きへもう1段分ける
/// （[`title_submenu`]）。
enum InsertEntry {
    Row((InsertShape, Title)),
    Group(Title, &'static [(InsertShape, Title)]),
}

/// 表の行を、上から順に1つずつ（組の中身も含めて）。
fn insert_items() -> impl Iterator<Item = &'static (InsertShape, Title)> {
    INSERT_MENU.iter().flat_map(|entry| match entry {
        InsertEntry::Row(item) => std::slice::from_ref(item),
        InsertEntry::Group(_, items) => items,
    })
}

const INSERT_MENU: &[InsertEntry] = &[
    InsertEntry::Group(
        ("リンク", "Link"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::MarkdownLink),
                ("Markdownリンク", "Markdown Link"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::WikiLink),
                ("Wikiリンク", "Wiki Link"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::WikiLinkAlias),
                ("別名付きWikiリンク", "Wiki Link with Alias"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::MarkdownImage),
                ("画像リンク(Markdown)", "Image Link (Markdown)"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::WikiImage),
                ("画像リンク(Wiki)", "Image Link (Wiki)"),
            ),
        ],
    ),
    InsertEntry::Row((
        InsertShape::Edit(document::InsertEdit::Ruby),
        ("ルビ", "Ruby"),
    )),
    InsertEntry::Group(
        ("注記", "Annotation"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::SideNote),
                ("左側の注記", "Left-side Annotation"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::RubyWithSideNote),
                ("ルビと左側の注記", "Ruby and Left-side Annotation"),
            ),
        ],
    ),
    InsertEntry::Group(
        ("傍点", "Emphasis Marks"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::EmphasisDots),
                ("傍点", "Emphasis Dots"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::SesameDotsNote),
                ("ゴマ傍点", "Sesame Dots"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::RoundDotsNote),
                ("丸傍点", "Round Dots"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::WhiteRoundDotsNote),
                ("白丸傍点", "White Round Dots"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::DoubleRoundDotsNote),
                ("二重丸傍点", "Double Round Dots"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::CrossDotsNote),
                ("×傍点", "Cross Dots"),
            ),
        ],
    ),
    InsertEntry::Group(
        ("傍線", "Emphasis Lines"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::LineNote),
                ("傍線", "Single Line"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::DoubleLineNote),
                ("二重傍線", "Double Line"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::WaveLineNote),
                ("波線", "Wavy Line"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::ChainLineNote),
                ("鎖線", "Chain Line"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::DashLineNote),
                ("破線", "Dashed Line"),
            ),
        ],
    ),
    InsertEntry::Group(
        ("文字注記", "Text Annotation"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::Upright),
                ("縦中横", "Tate-chu-yoko"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::Warichu),
                ("割り注", "Warichu"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::SmallText),
                ("小さな文字", "Small Text"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::LargeText),
                ("大きな文字", "Large Text"),
            ),
        ],
    ),
    // 書き手の求め 2026-09-22: Obsidianの記法。
    InsertEntry::Row((
        InsertShape::Edit(document::InsertEdit::Highlight),
        ("ハイライト", "Highlight"),
    )),
    InsertEntry::Row((
        InsertShape::Edit(document::InsertEdit::Comment),
        ("コメント", "Comment"),
    )),
    InsertEntry::Row((
        InsertShape::Edit(document::InsertEdit::Footnote),
        ("脚注", "Footnote"),
    )),
    InsertEntry::Group(
        ("Callout", "Callout"),
        &[
            (
                InsertShape::Edit(document::InsertEdit::Callout(document::CalloutType::Note)),
                ("Note", "Note"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::Callout(document::CalloutType::Tip)),
                ("Tip", "Tip"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::Callout(
                    document::CalloutType::Important,
                )),
                ("Important", "Important"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::Callout(
                    document::CalloutType::Warning,
                )),
                ("Warning", "Warning"),
            ),
            (
                InsertShape::Edit(document::InsertEdit::Callout(
                    document::CalloutType::Caution,
                )),
                ("Caution", "Caution"),
            ),
        ],
    ),
    InsertEntry::Group(
        ("見出し", "Heading"),
        &[
            (
                InsertShape::Line(document::LineNoteEdit::Heading(1)),
                ("見出し 1", "Heading 1"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Heading(2)),
                ("見出し 2", "Heading 2"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Heading(3)),
                ("見出し 3", "Heading 3"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Heading(4)),
                ("見出し 4", "Heading 4"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Heading(5)),
                ("見出し 5", "Heading 5"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Heading(6)),
                ("見出し 6", "Heading 6"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::NoHeading),
                ("見出しを解除", "Remove Heading"),
            ),
        ],
    ),
    InsertEntry::Group(
        ("段落注記", "Paragraph Annotation"),
        &[
            (
                InsertShape::Line(document::LineNoteEdit::Indent(1)),
                ("1字下げ", "Indent 1 Characters"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Indent(2)),
                ("2字下げ", "Indent 2 Characters"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Indent(3)),
                ("3字下げ", "Indent 3 Characters"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::Indent(4)),
                ("4字下げ", "Indent 4 Characters"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::NoIndent),
                ("字下げを解除", "Remove Indent"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::AlignToEnd(0)),
                ("地付き", "Align to End"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::AlignToEnd(1)),
                ("地から1字上げ", "1 Characters from End"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::AlignToEnd(2)),
                ("地から2字上げ", "2 Characters from End"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::AlignToEnd(3)),
                ("地から3字上げ", "3 Characters from End"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::AlignToEnd(4)),
                ("地から4字上げ", "4 Characters from End"),
            ),
            (
                InsertShape::Line(document::LineNoteEdit::NoAlignToEnd),
                ("地付きを解除", "Remove End Alignment"),
            ),
        ],
    ),
    InsertEntry::Row((InsertShape::Table, ("表", "Table"))),
    InsertEntry::Row((InsertShape::PageBreak, ("改ページ", "Page Break"))),
    InsertEntry::Row((InsertShape::Template, ("テンプレート…", "Template…"))),
];

/// その形が押せるか、いま効いているか（RFN01-47）。
///
/// **タイトルバーの挿入メニューと右クリックメニューが、同じ規則で読む。**
/// `editable`には矩形選択の断りまで含めて渡す。
fn context_insert_state(
    shape: InsertShape,
    editable: bool,
    picked: bool,
    notes: &document::LineNoteState,
    places: Places,
) -> (bool, bool) {
    match shape {
        InsertShape::Edit(what) => {
            let allowed = !what.needs_a_picked_word() || picked;
            (editable && allowed, false)
        }
        InsertShape::Line(document::LineNoteEdit::Heading(level)) => {
            (editable && notes.can_heading, notes.level == Some(level))
        }
        InsertShape::Line(document::LineNoteEdit::NoHeading) => (
            editable && notes.can_heading && notes.level.is_some(),
            false,
        ),
        InsertShape::Line(document::LineNoteEdit::Indent(count)) => {
            (editable && notes.can_indent, notes.indent == Some(count))
        }
        InsertShape::Line(document::LineNoteEdit::NoIndent) => (
            editable && notes.can_indent && notes.indent.is_some(),
            false,
        ),
        InsertShape::Line(document::LineNoteEdit::AlignToEnd(cells)) => {
            (editable && notes.can_tail, notes.tail == Some(cells))
        }
        InsertShape::Line(document::LineNoteEdit::NoAlignToEnd) => {
            (editable && notes.can_tail && notes.tail.is_some(), false)
        }
        InsertShape::PageBreak => (editable && places.page_break, false),
        InsertShape::Table => (editable && places.table, false),
        // RFN01-88: Templates Folder が無ければ押せない（書き手の決定 2026-11-04）。
        InsertShape::Template => (editable && places.template, false),
    }
}

/// 右クリックメニューへ出す挿入の行（RFN01-47）。返すのは1段目と、組の中身。
fn context_insert_rows(
    editable: bool,
    picked: bool,
    notes: document::LineNoteState,
    places: Places,
) -> (Vec<crate::InsertRow>, Vec<crate::InsertRow>) {
    let entry = |shape: InsertShape, title: (&str, &str), group: i32| {
        let (enabled, checked) = context_insert_state(shape, editable, picked, &notes, places);
        crate::InsertRow {
            title: pick(title.0, title.1).into(),
            flyout: false,
            group,
            number: shape.number(),
            enabled,
            checked,
            picker: shape == InsertShape::Table,
        }
    };
    let mut top = Vec::new();
    let mut children = Vec::new();
    let mut group = 0;
    for what in INSERT_MENU {
        match what {
            InsertEntry::Row((shape, title)) => top.push(entry(*shape, *title, -1)),
            InsertEntry::Group((ja, en), rows) => {
                let opened = group;
                group += 1;
                let mut openable = false;
                for (shape, title) in *rows {
                    let row = entry(*shape, *title, opened);
                    openable |= row.enabled;
                    children.push(row);
                }
                // **押せる行が1つも無い組も出す**——開けば空である（書き手の言葉
                // 2026-09-11：「何も選ばれていなければ、空」）。箇条書きと同じ。
                top.push(crate::InsertRow {
                    title: pick(ja, en).into(),
                    flyout: true,
                    group: opened,
                    number: -1,
                    enabled: openable,
                    checked: false,
                    picker: false,
                });
            }
        }
    }
    (top, children)
}

/// いまの選択で、行の体裁がどうなっているかと、改ページを入れられるか
/// （RFN01-38）。**押せるのに何も起きない行を作らない**ため、メニューを組む側は
/// 押したときと同じ答えを読む。
fn insert_inputs(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    document: &OpenDocument,
) -> (document::LineNoteState, Places) {
    let source = document.text.borrow();
    let (from, to) = chosen_source_range(&live.states.of(id).borrow(), source.len());
    (
        document::line_note_state(&source, from, to, reading_of(window)),
        Places {
            page_break: document::can_break_page_here(&source, from, to),
            table: {
                let mut counts = document.counts.borrow_mut();
                let styles = counts.get(&source, reading_of(window)).line_styles();
                crate::table_edit::can_place_table(&source, styles, from, to)
            },
            template: !crate::templates_in_use(live).is_empty(),
        },
    )
}

/// いまの位置に置けるもの（改ページ・表）。**押せるのに何も起きない行を作らない**ため、
/// メニューを組む側は押したときと同じ答えを読む。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Places {
    page_break: bool,
    table: bool,
    /// RFN01-88: 選べるテンプレートがあるか。
    template: bool,
}

/// 本文の右クリックメニューの挿入の行を組み直す（RFN01-47）。
///
/// **開いた時点の状態で組む**——選んだ字や、いまの行の体裁で押せる行が変わる。
pub fn publish_context_insert(window: &AppWindow, live: &Live, id: PaneId) {
    if id.is_panel() {
        return;
    }
    let screen = id.screen(window);
    let document = live.states.document(id);
    let empty = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .is_some_and(|tab| tab.empty);
    let text = !screen.settings && !screen.terminal && !empty;
    let editable = text && !screen.viewer && !document.read_only();
    let picked = !selected_runs(&live.cache, id).is_empty();
    let rectangular = live.states.of(id).borrow().rectangular;
    let (notes, places) = insert_inputs(window, live, id, &document);
    let (top, children) = context_insert_rows(editable && !rectangular, picked, notes, places);
    window.set_insert_rows(slint::ModelRc::new(slint::VecModel::from(top)));
    window.set_insert_children(slint::ModelRc::new(slint::VecModel::from(children)));
    let table = if editable && !rectangular {
        table_rows(window, live, id, &document)
    } else {
        Vec::new()
    };
    window.set_table_rows(slint::ModelRc::new(slint::VecModel::from(table)));
}

/// 右クリックの「Edit Table ▸」の中身（RFN01-49 ②）。キャレットが表の中になければ空で、
/// そのときは行そのものを出さない。押せる条件は押したときと同じ答え
/// （`table_edit::table_edit_state`）。
fn table_rows(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    document: &OpenDocument,
) -> Vec<crate::InsertRow> {
    let source = document.text.borrow();
    let (from, to) = chosen_source_range(&live.states.of(id).borrow(), source.len());
    let mut counts = document.counts.borrow_mut();
    let styles = counts.get(&source, reading_of(window)).line_styles();
    let Some(state) = crate::table_edit::table_edit_state(&source, styles, from.min(to)) else {
        return Vec::new();
    };
    crate::table_edit::TableEdit::ALL
        .iter()
        .zip(state)
        .map(|(what, enabled)| {
            let (ja, en) = what.title(id.vertical(window));
            crate::InsertRow {
                title: pick(ja, en).into(),
                flyout: false,
                group: -1,
                number: what.number(),
                enabled,
                checked: false,
                picker: false,
            }
        })
        .collect()
}

/// 書式のボタンの絵（2026-09-23）。16×16の線画で、`ui/controls.slint`の`Icons`と
/// 同じ描き方（線幅1.5・丸い端）。**絵はこの表が持つ**——ボタンの並びも言葉も
/// 挿入メニューの表から来るので、絵だけをSlintに置くと並びを2か所で数えることになる。
mod face {
    pub const LINK: &str =
        "M6.5 9.5l3-3M7 4.5l1-1a2.5 2.5 0 0 1 3.5 3.5l-1 1M9 11.5l-1 1a2.5 2.5 0 0 1-3.5-3.5l1-1";
    /// 親字（田の形）の上に、小さな読み。
    pub const RUBY: &str = "M4.5 2.5h2.5M9 2.5h2.5M3 5.5h10v8H3zM8 5.5v8M3 9.5h10";
    /// 縦に並ぶ親字の左に、小さな注記。
    pub const SIDE_NOTE: &str = "M6.5 2.5h7v11h-7zM6.5 8h7M10 2.5v11M3.5 3.5v2.5M3.5 9v2.5";
    /// 字の上に点。点は長さの無い線を丸い端で打つ（`Icons.more`と同じ）。
    pub const DOTS: &str = "M4.5 3h.2M11.5 3h.2M2 6.5h5v6.5H2zM9 6.5h5v6.5H9z";
    /// 字の下に線。
    pub const LINES: &str = "M2 2.5h5v6.5H2zM9 2.5h5v6.5H9zM2 12.5h12";
    /// 大きさの違う2つのA。
    pub const TEXT: &str =
        "M1.5 13.5 5 3l3.5 10.5M2.8 9.5h4.4M9.5 13.5l2.25-6 2.25 6M10.3 11.5h2.9";
    pub const HIGHLIGHT: &str = "M10 2.5l3.5 3.5-6.5 6.5-3.5.5.5-3.5zM8.5 4l3.5 3.5M2.5 14.5h11";
    pub const COMMENT: &str = "M2.5 3h11v8h-6l-3 2.5V11h-2z";
    pub const FOOTNOTE: &str = "M2.5 3.5h7M2.5 6.5h9M12.5 2.5v3M2.5 10.5h4M2.5 13.5h9";
    pub const CALLOUT: &str = "M2.5 2.5h11v11h-11zM8 5.5v3.5M8 11.3v.1";
    pub const HEADING: &str = "M2.5 3v10M2.5 8h6M8.5 3v10M11.5 7.5 13 6.5V13";
    pub const PARAGRAPH: &str = "M2.5 3h11M6.5 6.5h7M6.5 10h7M2.5 13.5h11M2.5 6.5l2 1.75-2 1.75";
    /// 3行3列のます目。
    pub const TABLE: &str = "M2.5 2.5h11v11h-11zM2.5 6h11M2.5 9.8h11M6.2 2.5v11M9.8 2.5v11";
    pub const PAGE_BREAK: &str =
        "M3.5 1.5v4h9v-4M3.5 14.5v-4h9v4M1.5 8h1.5M5.5 8h2M9.5 8h2M13.5 8h1";
    pub const BULLETS: &str = "M3 4h.2M3 8h.2M3 12h.2M6 4h7.5M6 8h7.5M6 12h7.5";
    pub const NUMBERED: &str = "M2.5 3.2 3.7 2.5V6.5M2.3 9.7c.3-1.2 2.4-1.1 2.4.2 0 1-2.4 1.8-2.4 3.1h2.5M7 4.5h6.5M7 11.5h6.5";
    pub const RENUMBER: &str = "M2.5 3.2 3.7 2.5V6.5M2.3 9.7c.3-1.2 2.4-1.1 2.4.2 0 1-2.4 1.8-2.4 3.1h2.5M7 4.5h3M13.8 10.5a2.8 2.8 0 1 1-.9-2.1M13.3 6.2v2.4h-2.4";
}

/// 挿入メニューの1段目の絵と束（ツールバーの縦線・パレットの行で区切る単位）。
///
/// **組は先頭の形で見分ける**——表の並びを変えても、絵は項目に付いてくる。
/// 知らない項目は`None`（試験が捕まえる）。
fn format_face(first: InsertShape) -> Option<(&'static str, i32)> {
    use document::{CalloutType, InsertEdit as E, LineNoteEdit as L};
    Some(match first {
        InsertShape::Edit(E::MarkdownLink) => (face::LINK, 0),
        InsertShape::Edit(E::Ruby) => (face::RUBY, 0),
        InsertShape::Edit(E::SideNote) => (face::SIDE_NOTE, 0),
        InsertShape::Edit(E::EmphasisDots) => (face::DOTS, 1),
        InsertShape::Edit(E::LineNote) => (face::LINES, 1),
        InsertShape::Edit(E::Upright) => (face::TEXT, 1),
        InsertShape::Edit(E::Highlight) => (face::HIGHLIGHT, 2),
        InsertShape::Edit(E::Comment) => (face::COMMENT, 2),
        InsertShape::Edit(E::Footnote) => (face::FOOTNOTE, 2),
        InsertShape::Edit(E::Callout(CalloutType::Note)) => (face::CALLOUT, 2),
        InsertShape::Line(L::Heading(1)) => (face::HEADING, 3),
        InsertShape::Line(L::Indent(1)) => (face::PARAGRAPH, 3),
        InsertShape::Table => (face::TABLE, 3),
        InsertShape::PageBreak => (face::PAGE_BREAK, 3),
        _ => return None,
    })
}

/// 箇条書きの束（挿入の束の次）。
const LIST_BAND: i32 = 4;

/// 箇条書きの操作を、書式のボタンの番号にしたときの始まり。**挿入の番号
/// （[`InsertShape::number`]、数十まで）と重ならない所に置く。**
const FORMAT_LIST: i32 = 1_000;

/// `Command::List(what, mark)`を書式のボタンの番号へ。`mark`は-1（記号を選ばない）から。
fn list_action(what: i32, mark: i32) -> i32 {
    FORMAT_LIST + what * 10 + mark + 1
}

/// 書式のボタンの番号を、メニューと同じ操作へ戻す。並びの外なら`None`。
fn format_command(action: i32) -> Option<Command> {
    if action >= FORMAT_LIST {
        let at = action - FORMAT_LIST;
        return Some(Command::List(at / 10, at % 10 - 1));
    }
    InsertShape::from_number(action).map(Command::Insert)
}

/// 書式のボタンが読む、いまの行の体裁（[`insert_inputs`]の答え）を覚えておく所。
///
/// **本文も選択も動いていなければ読み直さない**——行の体裁は文書の頭から
/// 読むので、長い原稿を刻みごとに読み直すと重い。本文の書き換えは必ず
/// `changed_at`を進める（`SharedText::borrow_mut`）ので、それを目印にする。
#[derive(Default)]
pub struct FormatCache {
    key: Option<FormatKey>,
    inputs: (document::LineNoteState, Places),
    shown: Option<(Vec<crate::FormatButton>, Vec<crate::InsertRow>)>,
}

type FormatKey = (
    *const OpenDocument,
    std::time::Instant,
    usize,
    (usize, usize),
    document::Reading,
);

/// 書式のボタンと組の中身を、いま字を入れているPaneについて組み直す（2026-09-23）。
///
/// **押せる条件はタイトルバーの挿入メニューと同じ**（[`text_state`]・
/// [`context_insert_state`]、箇条書きは記号を読むか）。出していない間は何もしない。
/// 変わっていなければ画面へ渡さない——渡すたびにボタンが作り直され、指の下の
/// 明るさや開いた行が消える。
pub fn publish_format(window: &AppWindow, live: &Live, cache: &RefCell<FormatCache>) {
    if !window.get_format_toolbar_open() && !window.get_format_palette_open() {
        return;
    }
    let t = Target::for_shortcut(window, live);
    window.set_format_vertical(t.id.vertical(window));
    let (_, editable) = text_state(window, live, &t);
    let picked = !selected_runs(&live.cache, t.id).is_empty();
    let rectangular = live.states.of(t.id).borrow().rectangular;
    let inputs = {
        let source = t.document.text.borrow();
        let range = chosen_source_range(&live.states.of(t.id).borrow(), source.len());
        let key = (
            Rc::as_ptr(&t.document),
            t.document.text.changed_at(),
            source.len(),
            range,
            reading_of(window),
        );
        drop(source);
        let mut cache = cache.borrow_mut();
        if cache.key != Some(key) {
            cache.inputs = insert_inputs(window, live, t.id, &t.document);
            cache.key = Some(key);
        }
        cache.inputs
    };
    let rows = format_rows(
        editable,
        rectangular,
        picked,
        inputs,
        bullet_marks_of(window),
    );
    let mut cache = cache.borrow_mut();
    if cache.shown.as_ref() == Some(&rows) {
        return;
    }
    window.set_format_buttons(slint::ModelRc::new(slint::VecModel::from(rows.0.clone())));
    window.set_format_children(slint::ModelRc::new(slint::VecModel::from(rows.1.clone())));
    cache.shown = Some(rows);
}

/// ボタンと組の中身。1段目は挿入メニューの表の順、そのあとに箇条書きの束。
fn format_rows(
    editable: bool,
    rectangular: bool,
    picked: bool,
    (notes, places): (document::LineNoteState, Places),
    marks: document::BulletMarks,
) -> (Vec<crate::FormatButton>, Vec<crate::InsertRow>) {
    let (top, mut children) = context_insert_rows(editable && !rectangular, picked, notes, places);
    let mut buttons: Vec<crate::FormatButton> = INSERT_MENU
        .iter()
        .zip(top)
        // RFN01-88: テンプレートはメニューとパレットから（書式のボタンは持たない）。
        .filter(|(entry, _)| !matches!(entry, InsertEntry::Row((InsertShape::Template, _))))
        .map(|(entry, row)| {
            let first = match entry {
                InsertEntry::Row((shape, _)) => *shape,
                InsertEntry::Group(_, items) => items[0].0,
            };
            let (icon, band) = format_face(first).unwrap_or((face::CALLOUT, LIST_BAND - 1));
            crate::FormatButton {
                icon: icon.into(),
                tip: row.title,
                flyout: row.flyout,
                group: if row.flyout { row.group } else { -1 },
                action: row.number,
                band,
                offset: 0.0,
                divided: false,
                enabled: row.enabled,
                picker: row.picker,
            }
        })
        .collect();
    // **箇条書きは1つにまとめて、記号を組の中身にする**（書き手の合意 2026-09-23）
    // ——右クリックメニューと同じ形。押せる条件はタイトルバーの行と同じ。
    let group = children
        .iter()
        .map(|row| row.group)
        .max()
        .map_or(0, |at| at + 1);
    let mut any = false;
    for (i, mark) in document::BULLET_MARKS.iter().enumerate() {
        let enabled = editable && marks.reads(*mark);
        any |= enabled;
        children.push(crate::InsertRow {
            title: mark.to_string().into(),
            flyout: false,
            group,
            number: list_action(0, i as i32),
            enabled,
            checked: false,
            picker: false,
        });
    }
    let numbered = editable && marks.first().is_some();
    for (icon, title, action, flyout, enabled) in [
        (face::BULLETS, ("箇条書き", "Bullet List"), -1, true, any),
        (
            face::NUMBERED,
            ("番号付きリスト", "Numbered List"),
            list_action(1, -1),
            false,
            numbered,
        ),
        (
            face::RENUMBER,
            ("番号を振り直す", "Renumber"),
            list_action(2, -1),
            false,
            numbered,
        ),
    ] {
        buttons.push(crate::FormatButton {
            icon: icon.into(),
            tip: pick(title.0, title.1).into(),
            flyout,
            group: if flyout { group } else { -1 },
            action,
            band: LIST_BAND,
            offset: 0.0,
            divided: false,
            enabled,
            picker: false,
        });
    }
    // ツールバーでの左端。ボタンは26pxに間2px、束の切れ目は縦線の分11px足す。
    let mut x = 0.0;
    for at in 0..buttons.len() {
        let divided = at > 0 && buttons[at - 1].band != buttons[at].band;
        if divided {
            x += 11.0;
        }
        buttons[at].divided = divided;
        buttons[at].offset = x;
        x += 28.0;
    }
    (buttons, children)
}

/// 書式のボタン（か、その組の行）が押された。**メニューと同じ操作を、同じ対象へ**
/// ——いま字を入れているPane（[`Target::for_shortcut`]、キーで走らせるのと同じ）。
/// 押せない条件は実行先が同じ判定を持っているので、ここでは断らない。
pub fn run_format(window: &AppWindow, live: &Live, action: i32) {
    let Some(command) = format_command(action) else {
        return;
    };
    let target = Target::for_shortcut(window, live);
    if !target.valid(window, live) {
        return;
    }
    execute(window, live, &target, command);
}

/// 書式のボタンの表のマス目で選んだ（RFN01-49 ①）。**ボタンと同じ対象へ**置く。
pub fn run_format_table(window: &AppWindow, live: &Live, rows: i32, columns: i32) {
    let target = Target::for_shortcut(window, live);
    if !target.valid(window, live) {
        return;
    }
    crate::insert_table_in_pane(window, live, target.id, rows, columns);
}

struct Popup {
    handle: HMENU,
}
impl Popup {
    fn new() -> windows::core::Result<Self> {
        Ok(Self {
            handle: unsafe { CreatePopupMenu()? },
        })
    }
    fn row(
        &self,
        title: &str,
        id: usize,
        enabled: bool,
        checked: bool,
    ) -> windows::core::Result<()> {
        let text: Vec<u16> = title
            .replace('&', "&&")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let flags = MF_STRING
            | if enabled { MF_ENABLED } else { MF_GRAYED }
            | if checked { MF_CHECKED } else { MF_UNCHECKED };
        unsafe { AppendMenuW(self.handle, flags, id, PCWSTR(text.as_ptr())) }
    }
    fn sep(&self) -> windows::core::Result<()> {
        unsafe { AppendMenuW(self.handle, MF_SEPARATOR, 0, PCWSTR::null()) }
    }
    fn child(&self, title: &str, child: Popup) -> windows::core::Result<()> {
        let text: Vec<u16> = title
            .replace('&', "&&")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            AppendMenuW(
                self.handle,
                MF_POPUP,
                child.handle.0 as usize,
                PCWSTR(text.as_ptr()),
            )?;
        }
        std::mem::forget(child); // Parent owns submenu after successful append.
        Ok(())
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.handle);
        }
    }
}

/// RFN01-87: コマンドパレットに並べる、タイトルバーのメニューの1行。
pub(crate) struct MenuEntry {
    /// 分類から行までの道（例：「表示 › 縦書き」）。
    pub title: String,
    /// 割り当て済みのキー。無ければ空。
    pub key: String,
    /// メニューで押せるか。押せない行はパレットに出さない。
    pub enabled: bool,
    /// 同じ道の、表示していない側の言葉。**日本語でも英語でも探せ、両方を見せる**ように。
    pub other: String,
    id: usize,
}

/// RFN01-87: **タイトルバーのメニューを、開いた時点のペインで組んで読み取ったもの。**
/// 行と操作の対応はメニューと同じ（`build`）なので、パレットで選んだ行はメニューで
/// 選んだのと同じに効く。対象のペインもメニューと同じく、組んだ時点で捕まえておく。
pub(crate) struct MenuSnapshot {
    target: Target,
    commands: Vec<Command>,
    pub entries: Vec<MenuEntry>,
}

const GROUP_TITLES: [Title; MENU_GROUPS as usize] = [
    ("ファイル", "File"),
    ("編集", "Edit"),
    ("挿入", "Insert"),
    ("レイアウト", "Layout"),
    ("表示", "View"),
    ("実行", "Run"),
    ("ヘルプ", "Help"),
];

pub(crate) fn menu_snapshot(window: &AppWindow, live: &Live) -> MenuSnapshot {
    let target = Target::for_shortcut(window, live);
    let mut commands = Vec::new();
    let mut entries = Vec::new();
    // RFN01-66 第2弾: Windowsのメニューは作らず、中身をそのまま読む。両方の言葉の道も中身が持つ。
    fn walk(
        window: &AppWindow,
        items: Vec<Item>,
        path: &[(String, String)],
        commands: &mut Vec<Command>,
        entries: &mut Vec<MenuEntry>,
    ) {
        for item in items {
            match item {
                Item::Row {
                    ja,
                    en,
                    command,
                    enabled,
                    ..
                } => {
                    let mut here = path.to_vec();
                    here.push((ja, en));
                    let (shown, other): (Vec<_>, Vec<_>) = here
                        .iter()
                        .map(|(ja, en)| {
                            let shown = pick(ja, en);
                            (shown, if shown == ja { en } else { ja })
                        })
                        .unzip();
                    entries.push(MenuEntry {
                        title: shown.join(" › "),
                        key: shortcut(window, &command, false),
                        enabled,
                        other: other
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(" › "),
                        id: commands.len() + 1,
                    });
                    commands.push(command);
                }
                Item::Sep => {}
                Item::Child { ja, en, items } => {
                    let mut here = path.to_vec();
                    here.push((ja, en));
                    walk(window, items, &here, commands, entries);
                }
            }
        }
    }
    for group in 0..MENU_GROUPS {
        let Some(items) = build(window, live, &target, group) else {
            continue;
        };
        let (ja, en) = GROUP_TITLES[group as usize];
        walk(
            window,
            items,
            &[(ja.to_owned(), en.to_owned())],
            &mut commands,
            &mut entries,
        );
    }
    MenuSnapshot {
        target,
        commands,
        entries,
    }
}

impl MenuSnapshot {
    /// RFN01-107: この行が倍率の刻み（倍率 › 50%〜240%）か。パレットには並べない。
    pub(crate) fn zoom_step(&self, entry: usize) -> bool {
        self.entries
            .get(entry)
            .and_then(|entry| entry.id.checked_sub(1))
            .and_then(|at| self.commands.get(at))
            .is_some_and(|command| matches!(command, Command::ZoomSet(_)))
    }

    /// パレットで選んだ行を、メニューで選んだのと同じに走らせる。
    pub(crate) fn run(&self, window: &AppWindow, live: &Live, entry: usize) {
        let Some(id) = self.entries.get(entry).map(|entry| entry.id) else {
            return;
        };
        let Some(command) = id.checked_sub(1).and_then(|at| self.commands.get(at)) else {
            return;
        };
        if !self.target.valid(window, live) {
            return;
        }
        execute(window, live, &self.target, command.clone());
    }
}

/// タイトルバーの分類の数（File / Edit / Insert / Layout / View / Run / Help）。
/// **画面の並びと1対1**である（`title-menu-bar.slint`の並び）。
pub const MENU_GROUPS: i32 = 7;

/// 開ける分類のうち、この向きでいちばん近いもの（RFN01-38）。**灰色の分類を
/// 飛ばす**——キーの左右で、押しても何も出ない分類に止まらないようにする。
fn step_openable(from: i32, delta: i32, openable: &[bool; MENU_GROUPS as usize]) -> Option<i32> {
    let mut at = from;
    for _ in 0..MENU_GROUPS {
        at = (at + delta).rem_euclid(MENU_GROUPS);
        if openable.get(at as usize) == Some(&true) {
            return Some(at);
        }
    }
    None
}

/// 押せる末端の行が1つでもあるか（RFN01-38）。
pub fn menu_opens(window: &AppWindow, live: &Live, group: i32) -> bool {
    let target = Target::capture(window, live);
    build(window, live, &target, group).is_some_and(|items| Item::any_enabled(&items))
}

/// タイトルバーで、段落注記の中をもう1段分ける組（A案の並び）。右クリックの
/// メニューは右へ開く1段しか持たないので、そちらは分けない。
fn title_submenu(shape: InsertShape) -> Option<Title> {
    match shape {
        InsertShape::Line(document::LineNoteEdit::Indent(_) | document::LineNoteEdit::NoIndent) => {
            Some(("字下げ", "Indent"))
        }
        InsertShape::Line(
            document::LineNoteEdit::AlignToEnd(_) | document::LineNoteEdit::NoAlignToEnd,
        ) => Some(("地付き", "End Alignment")),
        _ => None,
    }
}

/// 挿入メニューの**実行できる項目**（RFN01-38）。並び・文言・押せる条件は
/// [`INSERT_MENU`]と[`context_insert_state`]——右クリックメニューと同じものを読む。
fn insert_rows(scene: &Scene, root: &mut Vec<Item>) {
    insert_rows_by(root, |shape| scene.insert_state(shape));
}

/// 挿入の行を、形ごとの「押せるか・いまの形か」（`state`）から組む。
fn insert_rows_by(root: &mut Vec<Item>, state: impl Fn(InsertShape) -> (bool, bool)) {
    let row = |(shape, (ja, en)): (InsertShape, Title)| {
        let (enabled, checked) = state(shape);
        Item::Row {
            ja: ja.to_owned(),
            en: en.to_owned(),
            command: Command::Insert(shape),
            enabled,
            checked,
        }
    };
    for entry in INSERT_MENU {
        match entry {
            InsertEntry::Row(item) => root.push(row(*item)),
            InsertEntry::Group((ja, en), items) => {
                let mut group = Vec::new();
                let mut nested: Option<(Title, Vec<Item>)> = None;
                for item in *items {
                    let wanted = title_submenu(item.0);
                    if nested.as_ref().map(|(title, _)| *title) != wanted {
                        if let Some(((ja, en), rows)) = nested.take() {
                            group.push(Item::child(ja, en, rows));
                        }
                        if let Some(title) = wanted {
                            nested = Some((title, Vec::new()));
                        }
                    }
                    match nested.as_mut() {
                        Some((_, rows)) => rows.push(row(*item)),
                        None => group.push(row(*item)),
                    }
                }
                if let Some(((ja, en), rows)) = nested {
                    group.push(Item::child(ja, en, rows));
                }
                root.push(Item::child(ja, en, group));
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct Target {
    id: PaneId,
    parent: PaneId,
    document: Rc<OpenDocument>,
    identity: Option<Rc<()>>,
    session: Option<Rc<RefCell<TerminalSession>>>,
    shells: Vec<TerminalShell>,
    spot: TerminalSpot,
    field: i32,
}
impl Target {
    fn capture(window: &AppWindow, live: &Live) -> Self {
        let field = window.global::<MenuInput>().get_target();
        Self::with_field(window, live, field)
    }
    /// 設定から割り当てたキーで走らせるときの対象（RFN01-18）。
    ///
    /// **メニューから開いたのではない**ので、字を入れていた欄の持ち主はいない
    /// ——`field`は0（欄ではない）から始める。
    fn for_shortcut(window: &AppWindow, live: &Live) -> Self {
        Self::with_field(window, live, 0)
    }
    fn with_field(window: &AppWindow, live: &Live, field: i32) -> Self {
        Self::of(window, live, focused_pane(window), field)
    }
    fn of(window: &AppWindow, live: &Live, id: PaneId, field: i32) -> Self {
        let parent = if id.is_panel() {
            PaneId::from_index(id.screen(window).panel_owner)
        } else {
            id
        };
        let spot = if !id.is_panel() && field == -1 {
            TerminalSpot::Below
        } else {
            TerminalSpot::Front
        };
        let identity = if id.is_panel() {
            None
        } else {
            live.tabs
                .borrow()
                .of(id)
                .current()
                .map(|tab| tab.identity.clone())
        };
        let session = live
            .cache
            .borrow_mut()
            .pane(parent)
            .shell(spot)
            .map(|view| view.session.clone());
        Self {
            id,
            parent,
            document: live.states.document(id),
            identity,
            session,
            shells: configured_shells(window),
            spot,
            field,
        }
    }
    fn valid(&self, window: &AppWindow, live: &Live) -> bool {
        self.id.is_shown(window)
            && Rc::ptr_eq(&self.document, &live.states.document(self.id))
            && (self.id.is_panel()
                || live.tabs.borrow().of(self.id).current().is_some_and(|tab| {
                    self.identity
                        .as_ref()
                        .is_some_and(|identity| Rc::ptr_eq(identity, &tab.identity))
                }))
            && self.shells == configured_shells(window)
            && self.session.as_ref().is_none_or(|session| {
                live.cache
                    .borrow_mut()
                    .pane(self.parent)
                    .shell(self.spot)
                    .is_some_and(|view| Rc::ptr_eq(session, &view.session))
            })
    }
}

pub fn install(window: &AppWindow, live: &Live) {
    install_menu_actions(window, live);
    let weak = window.as_weak();
    window.global::<MenuInput>().on_open_menu(move || {
        if let Some(window) = weak.upgrade() {
            if window.get_question_open() || window.get_print_active() || window.get_diff_active() {
                return;
            }
            window.set_title_menu_visible(true);
            window.set_title_menu_active(0);
            window.set_title_menu_focus_generation(window.get_title_menu_focus_generation() + 1);
        }
    });
    let weak = window.as_weak();
    window.global::<MenuInput>().on_dismiss_menu(move || {
        let Some(window) = weak.upgrade() else {
            return false;
        };
        if !window.get_title_menu_visible() {
            return false;
        }
        window.invoke_title_menu_dismissed();
        true
    });
    let weak = window.as_weak();
    window.on_title_menu_dismissed(move || {
        if let Some(window) = weak.upgrade() {
            window.set_title_menu_visible(false);
            restore_input(
                &window,
                focused_pane(&window),
                window.global::<MenuInput>().get_target(),
            );
        }
    });
    let weak = window.as_weak();
    let live = live.clone();
    window.on_title_menu_requested(move |group| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if window.get_question_open() || window.get_print_active() || window.get_diff_active() {
            return;
        }
        if window.get_title_menu_open() {
            return;
        }
        // **押せる行が1つも無い分類は開かない**（書き手の決定 2026-09-21）。
        // タイトルバーも同じ判定で灰色にしているが、**開くのはここ**なので、
        // ここでも断る——見た目と実際が食い違わないように、同じ答えを使う。
        if !menu_opens(&window, &live, group) {
            return;
        }
        let target = Rc::new(Target::capture(&window, &live));
        queue_group(&window, live.clone(), target, group);
    });
}

/// RFN01-66 第2弾: ⋮・TABの右クリック・TAB帯・本文の右クリックの行（`MenuActions`）。
/// 開く直前の`refresh`で対象（前にあるペイン）を捕まえ、行の押せるかも実行も同じ対象で行う。
fn install_menu_actions(window: &AppWindow, live: &Live) {
    thread_local! {
        static TARGET: RefCell<Option<Rc<Target>>> = const { RefCell::new(None) };
    }
    let actions = window.global::<MenuActions>();
    let weak = window.as_weak();
    let refresh_live = live.clone();
    actions.on_refresh(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let target = Target::for_shortcut(&window, &refresh_live);
        TARGET.with(|held| *held.borrow_mut() = Some(Rc::new(target)));
        let actions = window.global::<MenuActions>();
        actions.set_generation(actions.get_generation() + 1);
    });
    let weak = window.as_weak();
    let allowed_live = live.clone();
    actions.on_allowed(move |action, _| {
        let Some(window) = weak.upgrade() else {
            return false;
        };
        let Some(target) = TARGET.with(|held| held.borrow().clone()) else {
            return false;
        };
        let scene = Scene::read(&window, &allowed_live, &target);
        scene.enabled(&menu_action_command(&window, &scene, action))
    });
    let weak = window.as_weak();
    let run_live = live.clone();
    actions.on_run(move |action| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(target) = TARGET.with(|held| held.borrow().clone()) else {
            return;
        };
        if !target.valid(&window, &run_live) {
            return;
        }
        let scene = Scene::read(&window, &run_live, &target);
        let command = menu_action_command(&window, &scene, action);
        if scene.enabled(&command) {
            execute(&window, &run_live, &target, command);
        }
    });
}

/// 画面の配置だけを確かめる試験（`Live`を持たない窓）のための`MenuActions`。行はどれも押せ、
/// 押せば試験が見張る窓のコールバックへ渡す。
#[cfg(test)]
pub(crate) fn wire_bare_menu_actions(window: &AppWindow) {
    let actions = window.global::<MenuActions>();
    actions.on_allowed(|_, _| true);
    let weak = window.as_weak();
    actions.on_run(move |action| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let p = window.get_focused_pane();
        match action {
            MenuAction::CopyBody => window.invoke_pane_copy_body(p),
            MenuAction::Back => window.invoke_pane_navigate(p, false),
            MenuAction::Forward => window.invoke_pane_navigate(p, true),
            MenuAction::Below => window.invoke_pane_below_toggled(p),
            MenuAction::FolderTerminal => window.invoke_shell_profile_action(8, 0),
            _ => {}
        }
    });
}

/// 行の操作を、タイトルバーのメニューと同じ[`Command`]へ。状態で行き先が変わる行（ログの開始・停止）
/// は、いまの状態で決める。
fn menu_action_command(window: &AppWindow, scene: &Scene, action: MenuAction) -> Command {
    match action {
        MenuAction::Save => Command::Save(false),
        MenuAction::SaveAs => Command::Save(true),
        MenuAction::FolderTerminal => Command::FolderTerminal,
        MenuAction::Reveal => Command::Reveal,
        MenuAction::CloseTab => Command::Close,
        MenuAction::CloseOthers => Command::CloseOthers(true),
        MenuAction::CloseAll => Command::CloseOthers(false),
        MenuAction::CloseClean => Command::CloseClean,
        MenuAction::Reopen => Command::Reopen,
        MenuAction::SplitRight => Command::Split(true),
        MenuAction::SplitDown => Command::Split(false),
        MenuAction::SwapLeft => Command::Swap(0),
        MenuAction::SwapRight => Command::Swap(1),
        MenuAction::SwapUp => Command::Swap(2),
        MenuAction::SwapDown => Command::Swap(3),
        MenuAction::OtherPanes => Command::OtherPanes,
        MenuAction::Below => Command::Below,
        MenuAction::Goto => Command::Goto,
        MenuAction::CompareSaved => Command::Compare(0),
        MenuAction::CompareHead => Command::Compare(1),
        MenuAction::CompareFile => Command::Compare(2),
        MenuAction::Print => Command::Print,
        MenuAction::ZoomIn => Command::Zoom(1),
        MenuAction::ZoomOut => Command::Zoom(-1),
        MenuAction::ZoomReset => Command::ZoomReset,
        MenuAction::Duplicate => Command::Duplicate,
        MenuAction::CopyPath => Command::CopyPath,
        MenuAction::NewFile => Command::NewFile,
        MenuAction::NewTerminal => Command::NewTerminal(window.get_default_shell()),
        MenuAction::OpenFile => Command::Open,
        MenuAction::Undo => Command::Undo(false),
        MenuAction::Redo => Command::Undo(true),
        MenuAction::Cut => Command::Copy(true),
        MenuAction::Copy => Command::Copy(false),
        MenuAction::Paste => Command::Paste,
        MenuAction::CopyBody => Command::CopyBody,
        MenuAction::Mark => Command::Mark(false),
        MenuAction::MarkRect => Command::Mark(true),
        MenuAction::Draft => Command::Draft,
        MenuAction::Back => Command::Navigate(false),
        MenuAction::Forward => Command::Navigate(true),
        MenuAction::PaneBackground => Command::Paper(1),
        MenuAction::TabColor => Command::Paper(2),
        MenuAction::TextBackground => Command::Paper(0),
        MenuAction::Appearance => Command::Appearance(1),
        MenuAction::FindTerminal => Command::Find(false),
        MenuAction::Latest => Command::Terminal(4),
        MenuAction::SaveOutput => Command::Terminal(7),
        MenuAction::ClearScreen => Command::Terminal(5),
        MenuAction::ClearHistory => Command::Terminal(6),
        MenuAction::SelectionToPanel => Command::Panel(9),
        MenuAction::HistoryToPanel => Command::Panel(8),
        MenuAction::LastOutputToPanel => Command::Panel(15),
        MenuAction::CopyLastOutput => Command::Terminal(22),
        MenuAction::FileLog => Command::Panel(if scene.screen.terminal_file_logging {
            14
        } else {
            10
        }),
        MenuAction::PanelLog => Command::Panel(if scene.screen.terminal_capturing {
            13
        } else {
            7
        }),
    }
}

fn queue_group(window: &AppWindow, live: Live, target: Rc<Target>, group: i32) {
    // **押せる行が1つも無い分類は開かない**（書き手の決定 2026-09-21）。開く道は
    // ここ1つに集まっているので、断るのもここに置く——見た目（灰色）と実際が
    // 食い違わないように、同じ問いを、答えを使う場所で聞く。
    if !menu_opens(window, &live, group) {
        return;
    }
    window.set_title_menu_open(true);
    window.set_title_menu_active(group);
    let weak = window.as_weak();
    // Give the title row one frame to paint the new active category before
    // Windows enters its modal menu loop. No document/input target is recaptured.
    Timer::single_shot(Duration::from_millis(16), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !window.get_title_menu_visible()
            || window.get_question_open()
            || window.get_print_active()
            || window.get_diff_active()
        {
            window.set_title_menu_open(false);
            return;
        }
        if !target.valid(&window, &live) {
            window.set_title_menu_open(false);
            window.tell(
                pick(
                    "操作対象が変わりました。選び直してください",
                    "The target changed. Please choose again",
                )
                .into(),
            );
            return;
        }
        match show(&window, &live, &target, group) {
            Ok(Some(next)) => queue_group(&window, live, target, next),
            result => {
                window.set_title_menu_open(false);
                if let Err(error) = result {
                    window.tell(
                        format!(
                            "{}: {error}",
                            pick("メニューを開けません", "Cannot open menu")
                        )
                        .into(),
                    );
                }
            }
        }
    });
}

/// 本文を出しているか（1つめ）、書き換えてよいか（2つめ）。
///
/// **タイトルバーのメニューと書式のボタンが同じ答えを使う**——片方だけが押せて、
/// もう片方が灰色、ということが起きないように。
fn text_state(window: &AppWindow, live: &Live, t: &Target) -> (bool, bool) {
    let screen = t.id.screen(window);
    let terminal = screen.terminal || matches!(t.spot, TerminalSpot::Below);
    let empty = live
        .tabs
        .borrow()
        .of(t.id)
        .current()
        .is_some_and(|tab| tab.empty);
    let text = !screen.settings && !terminal && !empty;
    let editable = text && !screen.viewer && !t.document.read_only() && t.field <= 0;
    (text, editable)
}

/// RFN01-66 第2弾（書き手と合意 2026-11-05）: メニューの中身。**Windowsのメニューにする前の形**
/// で、タイトルバーのメニュー（[`native`]）、コマンドパレット（[`menu_snapshot`]）、分類が
/// 開けるかの判定（[`menu_opens`]）が同じものを読む。押せるかは組むときに[`Scene::enabled`]で決める。
pub(crate) enum Item {
    Row {
        ja: String,
        en: String,
        command: Command,
        enabled: bool,
        checked: bool,
    },
    Sep,
    Child {
        ja: String,
        en: String,
        items: Vec<Item>,
    },
}
impl Item {
    fn child(ja: &str, en: &str, items: Vec<Item>) -> Self {
        Item::Child {
            ja: ja.to_owned(),
            en: en.to_owned(),
            items,
        }
    }
    /// 押せる行が1つでもあるか（子の段の中も見る）。
    fn any_enabled(items: &[Item]) -> bool {
        items.iter().any(|item| match item {
            Item::Row { enabled, .. } => *enabled,
            Item::Sep => false,
            Item::Child { items, .. } => Item::any_enabled(items),
        })
    }
}

/// RFN01-66 第2弾（書き手と合意 2026-11-05）: **アクションが実行できる前提の答えは、ここ1つ。**
///
/// 場面（対象のペインとTABが何で、どんな状態か）を一度だけ読み、[`Scene::enabled`]が
/// 操作ごとに答える。タイトルバーのメニュー・コマンドパレット・キーが同じ答えを使う
/// ——メニューで灰色の操作はキーでも動かず、パレットにも出ない。
///
/// 読むのに手間のかかるもの（バックアップの有無、Gitの管理下か、いまの行に置けるもの）は、
/// 問われたときに一度だけ読む——キー1つの判定で全部を読まないように。
pub(crate) struct Scene<'a> {
    window: &'a AppWindow,
    live: &'a Live,
    t: &'a Target,
    screen: PaneScreen,
    /// 対象がTerminal（上段のTerminalか、下段のTerminal Panel）。
    terminal: bool,
    /// 本文を出している（設定のTAB・Terminal・中身の無い新しいTABではない）。
    text: bool,
    /// 書き換えられる（本文を出していて、Viewer・ReadOnlyでなく、読み取り専用でなく、欄にいない）。
    editable: bool,
    /// 上段の本文（下段のEditor Panelではない）。
    main: bool,
    /// 文書が保存先のファイルを持つ。
    path: bool,
    /// 入力欄にいる（検索欄など）。
    field: bool,
    selected: OnceCell<bool>,
    backups: OnceCell<bool>,
    git_history: OnceCell<bool>,
    inserts: OnceCell<(document::LineNoteState, Places)>,
}
impl<'a> Scene<'a> {
    pub(crate) fn read(window: &'a AppWindow, live: &'a Live, t: &'a Target) -> Self {
        let screen = t.id.screen(window);
        let (text, editable) = text_state(window, live, t);
        let terminal = screen.terminal || matches!(t.spot, TerminalSpot::Below);
        Self {
            window,
            live,
            t,
            terminal,
            text,
            editable,
            main: text && !t.id.is_panel(),
            path: t.document.file.borrow().path().is_some(),
            field: t.field > 0,
            screen,
            selected: OnceCell::new(),
            backups: OnceCell::new(),
            git_history: OnceCell::new(),
            inserts: OnceCell::new(),
        }
    }
    /// 本文またはTerminalで字を選んでいる。
    fn selected(&self) -> bool {
        *self.selected.get_or_init(|| {
            if self.terminal {
                self.live
                    .cache
                    .borrow_mut()
                    .pane(self.t.parent)
                    .shell(self.t.spot)
                    .is_some_and(|view| view.selection.is_some())
            } else {
                !selected_runs(&self.live.cache, self.t.id).is_empty()
            }
        })
    }
    /// 対象のシェルが終わっていない。
    fn running(&self) -> bool {
        self.t
            .session
            .as_ref()
            .is_some_and(|session| !session.borrow().finished())
    }
    fn front_terminal(&self) -> bool {
        self.terminal && matches!(self.t.spot, TerminalSpot::Front)
    }
    fn zoomable(&self) -> bool {
        (self.text || self.terminal) && !self.screen.settings
    }
    fn neighbour(&self, side: i32) -> bool {
        let parent = self.t.parent.screen(self.window);
        match side {
            0 => parent.swap_left,
            1 => parent.swap_right,
            2 => parent.swap_up,
            3 => parent.swap_down,
            _ => false,
        }
    }
    fn tab_count(&self) -> usize {
        self.live.tabs.borrow().of(self.t.parent).tabs.len()
    }
    fn can_navigate(&self, forward: bool) -> bool {
        let tabs = self.live.tabs.borrow();
        let strip = tabs.of(self.t.id);
        stepped_place(strip.history.len(), strip.at, forward).is_some()
    }
    fn panel_capturing(&self) -> bool {
        terminal_panels::current(self.live, self.t.parent)
            .is_some_and(|panel| panel.borrow().capture.is_some())
    }
    fn inserts(&self) -> &(document::LineNoteState, Places) {
        self.inserts
            .get_or_init(|| insert_inputs(self.window, self.live, self.t.id, &self.t.document))
    }
    /// 挿入のひな形が押せるか（1つめ）、いまの行がその形か（2つめ）。
    fn insert_state(&self, shape: InsertShape) -> (bool, bool) {
        // RFN01-38: **矩形選択のときは押せない**——矩形へまとめて入れるのは別の課題（RFN01-41）。
        let rectangular = self.live.states.of(self.t.id).borrow().rectangular;
        let (notes, places) = self.inserts();
        context_insert_state(
            shape,
            self.editable && !rectangular,
            self.selected(),
            notes,
            *places,
        )
    }

    /// その操作が、いまの場面で実行できるか。
    pub(crate) fn enabled(&self, command: &Command) -> bool {
        let t = self.t;
        let window = self.window;
        let (text, editable, main, path, field) =
            (self.text, self.editable, self.main, self.path, self.field);
        let panel = t.id.is_panel();
        match command {
            Command::NewFile
            | Command::NewFromTemplate(_)
            | Command::NewTerminal(_)
            | Command::Open
            | Command::Folder
            | Command::Recent(_)
            | Command::Workspaces
            | Command::GitRepository
            | Command::SaveAll
            | Command::Close
            | Command::Exit
            | Command::Help(_)
            | Command::FolderFind
            | Command::Sidebar(1..)
            | Command::Toolbar
            | Command::Palette
            | Command::Draft => true,
            // RFN01-111: Workspaceを使っている間、Explorerの面は押せない（アイコン列と同じ）。
            Command::Sidebar(_) => !window.get_workspace_active(),
            Command::FolderTerminal
            | Command::CopyPath
            | Command::Reveal
            | Command::Encoding(_) => main && path,
            Command::Save(false) => text && !t.id.reads_only(window) && !t.document.read_only(),
            Command::Save(true) => text && !t.document.read_only(),
            Command::CloseOthers(_) | Command::CloseClean | Command::Reopen => !panel,
            Command::Rename => {
                text && path
                    && (!panel
                        || terminal_panels::current(self.live, t.parent)
                            .is_some_and(|panel| panel.borrow().capture.is_none()))
            }
            Command::Duplicate | Command::Print => main,
            Command::Undo(redo) => {
                field
                    || (editable
                        && if *redo {
                            self.screen.can_redo
                        } else {
                            self.screen.can_undo
                        })
            }
            Command::Copy(true) => field || (editable && self.selected()),
            Command::Copy(false) => field || ((text || self.terminal) && self.selected()),
            Command::Paste => field || editable || (self.terminal && self.running()),
            Command::SelectAll => field || text,
            Command::CopyBody => text && !field,
            Command::Find(false) => text || self.terminal || self.screen.settings,
            Command::Find(true) => editable,
            Command::Goto => text,
            Command::Line(_) => editable,
            Command::Mark(_) => text && !field,
            Command::Kill(i) => match i {
                1 => text && self.selected() && !field,
                2 => editable && self.selected(),
                3 => editable && !self.live.kills.borrow().ring.is_empty(),
                4 => {
                    editable
                        && standing_yank(t.id, &self.live.states, &t.document, &self.live.kills)
                            .is_some()
                }
                _ => editable,
            },
            Command::Word(_) => main,
            Command::Insert(shape) => self.insert_state(*shape).0,
            Command::List(0, i) => {
                editable
                    && document::BULLET_MARKS
                        .get(*i as usize)
                        .is_some_and(|mark| bullet_marks_of(window).reads(*mark))
            }
            Command::List(_, _) => editable && bullet_marks_of(window).first().is_some(),
            Command::Direction(true) | Command::Preview => main && !t.id.reads_only(window),
            Command::Direction(false) => main,
            Command::Paper(0) => main,
            Command::Paper(1) => !panel,
            Command::Paper(_) => !panel && matches!(t.spot, TerminalSpot::Front),
            Command::Appearance(_) => self.terminal || panel,
            Command::Viewer => text && !t.document.read_only(),
            Command::Display(_) => main,
            Command::Split(_) => PaneId::count(window) < MAX_PANES,
            Command::OtherPanes => PaneId::count(window) > 1,
            Command::Swap(side) => self.neighbour(*side),
            Command::Step(_) => !panel && self.tab_count() > 1,
            Command::Focus(side) => {
                self.neighbour(*side) || (*side == 3 && t.parent.screen(window).below_kind != 0)
            }
            Command::Navigate(forward) => text && self.can_navigate(*forward),
            Command::Below => !self.screen.settings,
            Command::Above => panel || matches!(t.spot, TerminalSpot::Below),
            Command::Zoom(step) => {
                let now = t.parent.zoom(window);
                self.zoomable()
                    && if *step > 0 {
                        now < ZOOM_MAX
                    } else {
                        now > ZOOM_MIN
                    }
            }
            Command::ZoomReset | Command::ZoomSet(_) => self.zoomable(),
            Command::Compare(0) | Command::Compare(1) => main && path,
            Command::Compare(3) => {
                main && *self
                    .backups
                    .get_or_init(|| path && crate::backup_ui::has_backups(window, &t.document))
            }
            Command::Compare(4) => {
                main && path
                    && *self
                        .git_history
                        .get_or_init(|| crate::git_file_history::available(&t.document))
            }
            Command::Compare(_) => main,
            Command::Spelling => {
                if crate::spelling_ui::checking(&t.document) {
                    main
                } else {
                    main && editable
                }
            }
            Command::SpellingStep(_) => main && crate::spelling_ui::checking(&t.document),
            Command::ReadAloud => {
                crate::read_aloud::reading() || (main && window.get_speech_available())
            }
            // Terminalの面の操作。メニューにはTerminalが対象のときだけ出る。
            Command::Terminal(_) | Command::Shell(_) => self.terminal,
            Command::Panel(10) => {
                self.front_terminal() && self.running() && !self.screen.terminal_file_logging
            }
            Command::Panel(14) => self.front_terminal() && self.screen.terminal_file_logging,
            Command::Panel(7) => {
                self.front_terminal() && self.running() && !self.screen.terminal_capturing
            }
            Command::Panel(13) => self.front_terminal() && self.screen.terminal_capturing,
            Command::Panel(9) => self.front_terminal() && self.selected(),
            Command::Panel(8) | Command::Panel(15) => self.front_terminal(),
            Command::Panel(5) => panel && self.panel_capturing(),
            Command::Panel(_) => self.terminal || panel,
            Command::ScrollLine(_) | Command::SelectLine => text,
        }
    }
    /// メニューの1行。押せるかは[`Scene::enabled`]が決める。
    fn row(&self, ja: &str, en: &str, command: Command, checked: bool) -> Item {
        let enabled = match &command {
            // 挿入の形は、押せるかと一緒に「いまの行がその形か」も同じ答えから読む。
            Command::Insert(shape) => self.insert_state(*shape).0,
            command => self.enabled(command),
        };
        Item::Row {
            ja: ja.to_owned(),
            en: en.to_owned(),
            command,
            enabled,
            checked,
        }
    }
}

/// 分類のメニューの中身を組む（RFN01-38）。
///
/// **出す前に、押せる行があるかを見るのにも使う**——子が全部無効なら親も無効に
/// するためである（書き手の決定 2026-09-21）。RFN01-66 第2弾: 押せるかは行ごとに
/// [`Scene::enabled`]が答える（ここには書かない）。
fn build(window: &AppWindow, live: &Live, t: &Target, group: i32) -> Option<Vec<Item>> {
    let scene = Scene::read(window, live, t);
    let mut root: Vec<Item> = Vec::new();
    let terminal = scene.terminal;
    let zoom_now = t.parent.zoom(window);
    macro_rules! add {
        ($ja:expr,$en:expr,$cmd:expr) => {
            root.push(scene.row($ja, $en, $cmd, false))
        };
    }
    match group {
        0 => {
            // RFN01-88（書き手の決定 2026-11-04）: テンプレートがあれば「新規文書 ▸」の
            // 中に空の文書と各テンプレートを並べる。Ctrl+Nは空の文書のまま。
            let templates = crate::templates_in_use(live);
            if templates.is_empty() {
                add!("新規文書", "New File", Command::NewFile);
            } else {
                let mut documents = Vec::new();
                documents.push(scene.row("空の文書", "Blank Document", Command::NewFile, false));
                documents.push(Item::Sep);
                for template in templates {
                    documents.push(scene.row(
                        &template.name,
                        &template.name,
                        Command::NewFromTemplate(template.name.clone()),
                        false,
                    ));
                }
                root.push(Item::child("新規文書", "New File", documents));
            }
            let mut shells = Vec::new();
            for (i, shell) in configured_shells(window).iter().enumerate() {
                shells.push(scene.row(
                    &shell.name,
                    &shell.name,
                    Command::NewTerminal(i as i32),
                    false,
                ));
            }
            root.push(Item::child("新しいTerminal", "New Terminal", shells));
            add!(
                "文書のフォルダでTerminal起動",
                "Terminal in Document Folder",
                Command::FolderTerminal
            );
            add!("ファイルを開く…", "Open File…", Command::Open);
            add!("フォルダを開く…", "Open Folder…", Command::Folder);
            let mut recent = Vec::new();
            for path in offered_folders(live) {
                let name = path.display().to_string();
                recent.push(scene.row(&name, &name, Command::Recent(path), false));
            }
            root.push(Item::child("最近のフォルダ", "Recent Folders", recent));
            add!("Workspaces…", "Workspaces…", Command::Workspaces);
            // RFN01-67: 全ブランチのグラフ（Git Changes で選んでいるリポジトリ）。
            add!("Git Repository…", "Git Repository…", Command::GitRepository);
            root.push(Item::Sep);
            add!("保存", "Save", Command::Save(false));
            add!("名前を付けて保存…", "Save As…", Command::Save(true));
            add!(
                "すべて保存（Editor Panelを除く）",
                "Save All (except Editor Panels)",
                Command::SaveAll
            );
            root.push(Item::Sep);
            add!("TABを閉じる", "Close Tab", Command::Close);
            add!(
                "他のTABを閉じる",
                "Close Other Tabs",
                Command::CloseOthers(true)
            );
            add!(
                "すべてのTABを閉じる",
                "Close All Tabs",
                Command::CloseOthers(false)
            );
            add!(
                "未編集のTABを閉じる",
                "Close Unedited Tabs",
                Command::CloseClean
            );
            add!("閉じたTABを開き直す", "Reopen Closed Tab", Command::Reopen);
            root.push(Item::Sep);
            add!("名前を変更…", "Rename…", Command::Rename);
            add!("別TABで開く", "Open in Another Tab", Command::Duplicate);
            add!("パスをコピー", "Copy Path", Command::CopyPath);
            add!("Explorerで表示", "Reveal in Explorer", Command::Reveal);
            let mut enc = Vec::new();
            for (i, name) in ["UTF-8", "UTF-16 LE", "UTF-16 BE", "CP932"]
                .iter()
                .enumerate()
            {
                enc.push(scene.row(name, name, Command::Encoding(i as i32), false));
            }
            root.push(Item::child(
                "文字コードを指定して開き直す",
                "Reopen with Encoding",
                enc,
            ));
            // RFN01-61（書き手の求め 2026-09-27）: バックアップの履歴は File から（今のTABのファイル）。
            add!(
                "バックアップの履歴…",
                "Backup History…",
                Command::Compare(3)
            );
            // RFN01-67 PR 3: Git の履歴も File から（今のTABのファイル、管理下になければ淡く）。
            add!("Gitの履歴…", "Git History…", Command::Compare(4));
            add!("印刷…", "Print…", Command::Print);
            root.push(Item::Sep);
            add!("終了", "Exit", Command::Exit);
        }
        1 => {
            add!("元に戻す", "Undo", Command::Undo(false));
            add!("やり直し", "Redo", Command::Undo(true));
            root.push(Item::Sep);
            add!("切り取り", "Cut", Command::Copy(true));
            add!("コピー", "Copy", Command::Copy(false));
            add!("貼り付け", "Paste", Command::Paste);
            add!("全選択", "Select All", Command::SelectAll);
            add!("本文だけをコピー", "Copy Text Only", Command::CopyBody);
            root.push(Item::Sep);
            add!("検索…", "Find…", Command::Find(false));
            add!("置換…", "Replace…", Command::Find(true));
            add!("行へ移動…", "Go to Line…", Command::Goto);
            add!("フォルダ内を検索…", "Find in Folder…", Command::FolderFind);
            if terminal {
                root.push(Item::Sep);
                add!("画面をクリア", "Clear Screen", Command::Terminal(5));
                add!("履歴をクリア", "Clear History", Command::Terminal(6));
            }
            let mut lines = Vec::new();
            for (ja, en, n) in [
                ("行を削除", "Delete Line", 4),
                ("行を上へ移動", "Move Line Up", 0),
                ("行を下へ移動", "Move Line Down", 1),
                ("行を上へ複製", "Duplicate Line Up", 2),
                ("行を下へ複製", "Duplicate Line Down", 3),
            ] {
                lines.push(scene.row(ja, en, Command::Line(n), false));
            }
            root.push(Item::child("行操作", "Lines", lines));
            add!(
                "選択を開始・解除",
                "Start / Cancel Selection",
                Command::Mark(false)
            );
            add!(
                "矩形選択を開始・解除",
                "Start / Cancel Rectangle Selection",
                Command::Mark(true)
            );
            let mut kill_menu = Vec::new();
            for (i, (ja, en)) in [
                ("行末まで切り取り", "Cut to End of Line"),
                ("Kill Ringへコピー", "Copy to Kill Ring"),
                ("Kill Ringへ切り取り", "Cut to Kill Ring"),
                ("最新のKillを貼り付け", "Paste Latest Kill"),
                ("前のKillへ置換", "Replace with Previous Kill"),
            ]
            .iter()
            .enumerate()
            {
                kill_menu.push(scene.row(ja, en, Command::Kill(i as i32), false));
            }
            root.push(Item::child("Kill Ring", "Kill Ring", kill_menu));
            let mut words = Vec::new();
            words.push(scene.row(
                "なし",
                "None",
                Command::Word(0),
                pane_word_mode(live, t.id) == 0,
            ));
            for mode in word_modes_now() {
                words.push(scene.row(
                    &mode.name,
                    &mode.name,
                    Command::Word(mode.id),
                    pane_word_mode(live, t.id) == mode.id,
                ));
            }
            root.push(Item::child("単語チェック", "Word Check", words));
        }
        2 => {
            // **行の体裁は、いまの行が何かを読んでから出す**——押せるのに何も
            // 起きない行を作らないため、見る側と押す側が同じ答え（`Scene::insert_state`）を使う。
            insert_rows(&scene, &mut root);
            root.push(Item::Sep);
            for (i, mark) in document::BULLET_MARKS.iter().enumerate() {
                let name = format!("{} {mark}", pick("箇条書き", "Bullet List"));
                root.push(scene.row(&name, &name, Command::List(0, i as i32), false));
            }
            add!("番号付きリスト", "Numbered List", Command::List(1, -1));
            add!("番号を振り直す", "Renumber", Command::List(2, -1));
        }
        3 => {
            root.push(scene.row(
                "横書き",
                "Horizontal",
                Command::Direction(false),
                !t.id.vertical(window),
            ));
            root.push(scene.row(
                "縦書き",
                "Vertical",
                Command::Direction(true),
                t.id.vertical(window),
            ));
            root.push(Item::Sep);
            add!(
                "このPaneの背景…（個別色を解除）",
                "Pane Background… (clears individual colors)",
                Command::Paper(1)
            );
            add!("このTABの本文背景…", "Tab Background…", Command::Paper(0));
            add!("このTAB見出しの色…", "Tab Header Color…", Command::Paper(2));
            if terminal || t.id.is_panel() {
                add!(
                    "このTABの外観…",
                    "Appearance of This Tab…",
                    Command::Appearance(
                        if t.id.is_panel() || matches!(t.spot, TerminalSpot::Below) {
                            2
                        } else {
                            1
                        }
                    )
                );
            }
        }
        4 => {
            let mut side = Vec::new();
            for (ja, en, n) in [
                ("Workspace", "Workspace", 4),
                ("Explorer", "Explorer", 0),
                ("検索", "Search", 1),
                ("履歴", "History", 2),
                ("Outline", "Outline", 3),
                // RFN01-66 第2弾: アイコン列と同じ面を同じ順で（BookmarksとTagsが無かった）。
                ("ブックマーク", "Bookmarks", 5),
                ("タグ", "Tags", 6),
                ("Git Changes", "Git Changes", 8),
                ("バックアップ", "Backups", 7),
            ] {
                side.push(scene.row(
                    ja,
                    en,
                    Command::Sidebar(n),
                    window.get_tree_open() && window.get_left_tab() == n,
                ));
            }
            root.push(Item::child("サイドバー", "Sidebar", side));
            // 書き手の求め 2026-09-23: 挿入メニューの全部をアイコンで。**どちらか
            // 片方でも、両方でもよい**ので、2つは別の行である。
            root.push(scene.row(
                "ツールバー",
                "Toolbar",
                Command::Toolbar,
                window.get_format_toolbar_open(),
            ));
            root.push(scene.row(
                "書式パレット",
                "Format Palette",
                Command::Palette,
                window.get_format_palette_open(),
            ));
            add!(
                "Source・Preview切替",
                "Toggle Source / Preview",
                Command::Preview
            );
            add!(
                "Viewer・ReadOnly開始／終了",
                "Start / Exit Viewer or ReadOnly",
                Command::Viewer
            );
            for (setting, ja, en) in [
                (12, "行番号", "Line Numbers"),
                (54, "空白・改行記号", "Spaces and Line Breaks"),
            ] {
                // RFN01-66（書き手の判断 2026-11-05）: 今のペインの向きの設定を切り替える入口。
                // 「（横書き共通）」の注記は外した——設定のLayoutと同じ値を、ここからも切り替えるだけ。
                let name = pick(ja, en).to_owned();
                let on = Setting::from_index(setting)
                    .is_some_and(|s| s.read(window, usize::from(t.id.vertical(window))) != 0);
                root.push(scene.row(&name, &name, Command::Display(setting), on));
            }
            root.push(Item::Sep);
            add!("右へ分割", "Split Right", Command::Split(true));
            add!("下へ分割", "Split Down", Command::Split(false));
            add!("他のPaneを閉じる", "Close Other Panes", Command::OtherPanes);
            for (n, (ja, en)) in [
                ("左と入替", "Swap Left"),
                ("右と入替", "Swap Right"),
                ("上と入替", "Swap Up"),
                ("下と入替", "Swap Down"),
            ]
            .iter()
            .enumerate()
            {
                root.push(scene.row(ja, en, Command::Swap(n as i32), false));
            }
            let mut go = Vec::new();
            go.push(scene.row("次のTAB", "Next Tab", Command::Step(false), false));
            go.push(scene.row("前のTAB", "Previous Tab", Command::Step(true), false));
            for (n, (ja, en)) in [
                ("左のPane", "Left Pane"),
                ("右のPane", "Right Pane"),
                ("上のPane", "Upper Pane"),
                ("下段／下のPane", "Panel Below / Lower Pane"),
            ]
            .iter()
            .enumerate()
            {
                go.push(scene.row(ja, en, Command::Focus(n as i32), false));
            }
            go.push(scene.row("戻る", "Back", Command::Navigate(false), false));
            go.push(scene.row("進む", "Forward", Command::Navigate(true), false));
            root.push(Item::child("移動", "Go To", go));
            add!("下段を表示・閉じる", "Show / Hide Panel", Command::Below);
            if terminal {
                add!(
                    "最新の出力へ",
                    "Latest Terminal Output",
                    Command::Terminal(4)
                );
            }
            if t.id.is_panel() || matches!(t.spot, TerminalSpot::Below) {
                add!("上の領域へ", "Focus Above", Command::Above);
            }
            add!("Quick Draft…", "Quick Draft…", Command::Draft);
            root.push(Item::Sep);
            add!("拡大", "Zoom In", Command::Zoom(1));
            add!("縮小", "Zoom Out", Command::Zoom(-1));
            add!("既定倍率に戻す", "Reset Zoom", Command::ZoomReset);
            let mut zoom = Vec::new();
            for percent in (ZOOM_MIN..=ZOOM_MAX).step_by(ZOOM_STEP as usize) {
                let name = format!("{percent}%");
                zoom.push(scene.row(&name, &name, Command::ZoomSet(percent), zoom_now == percent));
            }
            root.push(Item::child("倍率", "Zoom", zoom));
        }
        5 => {
            add!("保存版と比較", "Compare with Saved", Command::Compare(0));
            add!(
                "Gitの最終Commitと比較",
                "Compare with Git Last Commit",
                Command::Compare(1)
            );
            add!(
                "別ファイルと比較…",
                "Compare with Another File…",
                Command::Compare(2)
            );
            // RFN01-63: **アクティブな文書に対して実行する**（書き手と合意 2026-09-28）。
            // 確認しているあいだは、同じ場所が終える行になる。
            root.push(Item::Sep);
            if crate::spelling_ui::checking(&t.document) {
                add!(
                    "スペルチェックを終了",
                    "Stop Spell Check",
                    Command::Spelling
                );
            } else {
                add!(
                    "スペルチェック（英語）",
                    "Check Spelling (English)",
                    Command::Spelling
                );
            }
            // 書き手と合意 2026-09-28: 誤りから誤りへ。スペルチェック中だけ押せる。
            add!(
                "次のスペルミス",
                "Next Spelling Error",
                Command::SpellingStep(false)
            );
            add!(
                "前のスペルミス",
                "Previous Spelling Error",
                Command::SpellingStep(true)
            );
            // RFN01-62（書き手と合意 2026-09-28）: 読み上げ。読んでいるあいだは同じ場所が
            // 止める行になる。**日本語の声が無ければ押せない。**
            root.push(Item::Sep);
            if crate::read_aloud::reading() {
                add!("読み上げを停止", "Stop Reading Aloud", Command::ReadAloud);
            } else {
                add!("読み上げ", "Read Aloud", Command::ReadAloud);
            }
            if terminal {
                root.push(Item::Sep);
                add!("出力を保存…", "Save Output…", Command::Terminal(7));
                if matches!(t.spot, TerminalSpot::Front) {
                    add!(
                        "ファイルへログ開始…",
                        "Capture Log to File…",
                        Command::Panel(10)
                    );
                    add!("ファイルログ停止", "Stop File Log", Command::Panel(14));
                    add!(
                        "新しいEditor Panelへログ開始",
                        "Capture Log to New Editor Panel",
                        Command::Panel(7)
                    );
                    add!("Panelログ停止", "Stop Panel Log", Command::Panel(13));
                    add!(
                        "選択をEditor Panelへ送る",
                        "Selection to Editor Panel",
                        Command::Panel(9)
                    );
                    add!(
                        "履歴を新しいEditor Panelへ送る",
                        "History to New Editor Panel",
                        Command::Panel(8)
                    );
                }
                let mut shells = Vec::new();
                for (i, shell) in configured_shells(window).iter().enumerate() {
                    shells.push(scene.row(
                        &shell.name,
                        &shell.name,
                        Command::Shell(i as i32),
                        false,
                    ));
                }
                root.push(Item::child("シェル切替", "Switch Shell", shells));
            }
            if t.id.is_panel() {
                add!(
                    "このPanelの取り込みを停止",
                    "Stop This Panel Log",
                    Command::Panel(5)
                );
            }
        }
        // RFN01-66（書き手の判断 2026-11-05）: ヘルプを簡単に作る。使い方はキーの一覧と
        // 紹介用のサンプル文書、RFN Editについては名前・版・著作権。
        6 => {
            add!("キーの一覧…", "Keyboard Shortcuts…", Command::Help(0));
            add!(
                "サンプル文書を開く",
                "Open Sample Document",
                Command::Help(1)
            );
            root.push(Item::Sep);
            add!("RFN Editについて…", "About RFN Edit…", Command::Help(2));
        }
        _ => return None,
    }
    Some(root)
}

/// 中身から、Windowsのメニューを作る。**項目のidは`commands`の並び（1から）**。
fn native(
    window: &AppWindow,
    items: Vec<Item>,
    field: bool,
) -> windows::core::Result<(Popup, Vec<Command>)> {
    fn fill(
        window: &AppWindow,
        menu: &Popup,
        items: Vec<Item>,
        field: bool,
        commands: &mut Vec<Command>,
    ) -> windows::core::Result<()> {
        for item in items {
            match item {
                Item::Row {
                    ja,
                    en,
                    command,
                    enabled,
                    checked,
                } => {
                    let key = shortcut(window, &command, field);
                    let name = pick(&ja, &en);
                    let title = if key.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{name}\t{key}")
                    };
                    commands.push(command);
                    menu.row(&title, commands.len(), enabled, checked)?;
                }
                Item::Sep => menu.sep()?,
                Item::Child { ja, en, items } => {
                    let child = Popup::new()?;
                    fill(window, &child, items, field, commands)?;
                    menu.child(pick(&ja, &en), child)?;
                }
            }
        }
        Ok(())
    }
    let root = Popup::new()?;
    let mut commands = Vec::new();
    fill(window, &root, items, field, &mut commands)?;
    Ok((root, commands))
}

/// 分類のメニューを出す（RFN01-38）。
fn show(
    window: &AppWindow,
    live: &Live,
    t: &Target,
    group: i32,
) -> windows::core::Result<Option<i32>> {
    let Some(items) = build(window, live, t, group) else {
        return Ok(None);
    };
    let (root, commands) = native(window, items, t.field > 0)?;
    let Some(hwnd) = window_chrome::window_handle(window) else {
        window.tell(
            pick(
                "メニューのウィンドウを取得できません",
                "Cannot obtain the menu window",
            )
            .into(),
        );
        return Ok(None);
    };
    let scale = window.window().scale_factor();
    let mut point = POINT {
        x: ((36. + group as f32 * 48.) * scale) as i32,
        y: (36. * scale) as i32,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut point);
    }
    // **開ける分類を、メニューの輪へも渡す**（RFN01-38）——開いている間の横移動は
    // Slintを通らないので、ここで渡さないと灰色の分類へも動いてしまう。
    let openable = std::array::from_fn(|group| menu_opens(window, live, group as i32));
    let navigation = Navigation::begin(hwnd, root.handle, group, scale, openable)?;
    let navigation_guard = NavigationGuard;
    let picked = unsafe {
        windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
        TrackPopupMenuEx(
            root.handle,
            (TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON).0,
            point.x,
            point.y,
            hwnd,
            None,
        )
        .0
    };
    if picked == 0 {
        let error = unsafe { windows::Win32::Foundation::GetLastError() };
        if error.0 != 0 {
            return Err(windows::core::Error::from_thread());
        }
    }
    drop(navigation_guard);
    if picked == 0 && navigation.next.get().is_none() {
        if navigation.escaped.get() {
            window.set_title_menu_focus_generation(window.get_title_menu_focus_generation() + 1);
        } else {
            window.set_title_menu_visible(false);
            restore_input(window, t.id, t.field);
        }
    }
    if picked > 0
        && let Some(command) = commands.get(picked as usize - 1)
    {
        if t.valid(window, live) {
            window.set_title_menu_visible(false);
            execute(window, live, t, command.clone());
        } else {
            window.tell(
                pick(
                    "操作対象が変わりました。選び直してください",
                    "The target changed. Please choose again",
                )
                .into(),
            );
        }
    }
    Ok(navigation.next.get())
}

// Windows' menu loop handles item/submenu navigation. At the root only,
// route left/right and pointer movement to the adjacent title menu.
struct Navigation {
    hwnd: windows::Win32::Foundation::HWND,
    root: HMENU,
    group: i32,
    scale: f32,
    in_root: Cell<bool>,
    submenu: Cell<bool>,
    next: Cell<Option<i32>>,
    hook: Cell<HHOOK>,
    pointer: Cell<(i32, i32)>,
    escaped: Cell<bool>,
    /// 各分類が開けるか（RFN01-38）。**メニューが開いている間の横移動はSlintを
    /// 通らない**——Windowsのメニューの輪の中でここが分類を切り替えるので、その
    /// 切り替えにも同じ答えが要る（書き手の確認 2026-09-21）。
    openable: [bool; MENU_GROUPS as usize],
}
thread_local! { static NAVIGATION: RefCell<Option<Rc<Navigation>>> = const { RefCell::new(None) }; }
impl Navigation {
    fn begin(
        hwnd: windows::Win32::Foundation::HWND,
        root: HMENU,
        group: i32,
        scale: f32,
        openable: [bool; MENU_GROUPS as usize],
    ) -> windows::core::Result<Rc<Self>> {
        let mut pointer = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pointer);
        }
        let nav = Rc::new(Self {
            hwnd,
            root,
            group,
            scale,
            in_root: Cell::new(true),
            submenu: Cell::new(false),
            next: Cell::new(None),
            hook: Cell::new(HHOOK::default()),
            pointer: Cell::new((pointer.x, pointer.y)),
            escaped: Cell::new(false),
            openable,
        });
        let hook = unsafe {
            SetWindowsHookExW(
                WH_MSGFILTER,
                Some(menu_filter),
                None,
                windows::Win32::System::Threading::GetCurrentThreadId(),
            )?
        };
        nav.hook.set(hook);
        NAVIGATION.with(|held| *held.borrow_mut() = Some(nav.clone()));
        Ok(nav)
    }
}
// Explicit guard cleanup: TLS owns a reference during the native callback.
struct NavigationGuard;
impl Drop for NavigationGuard {
    fn drop(&mut self) {
        NAVIGATION.with(|held| {
            if let Some(nav) = held.borrow_mut().take() {
                unsafe {
                    let _ = UnhookWindowsHookEx(nav.hook.get());
                }
            }
        });
    }
}
pub(crate) fn native_selection(w: WPARAM, l: LPARAM) {
    NAVIGATION.with(|held| {
        if let Some(nav) = held.borrow().as_ref() {
            nav.in_root.set(l.0 == nav.root.0 as isize);
            nav.submenu.set((w.0 as u32 >> 16) & MF_POPUP.0 != 0);
        }
    });
}

unsafe extern "system" fn menu_filter(
    code: i32,
    w: WPARAM,
    l: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::{Foundation::LRESULT, Graphics::Gdi::ScreenToClient};
    if code == MSGF_MENU as i32 {
        let message = unsafe { &*(l.0 as *const MSG) };
        let consumed = NAVIGATION.with(|held| {
            let held = held.borrow();
            let Some(nav) = held.as_ref() else {
                return false;
            };
            let mut next = None;
            if message.message == WM_KEYDOWN && nav.in_root.get() {
                if message.wParam.0 == 0x1b {
                    nav.escaped.set(true);
                }
                // **開ける分類まで送る**（RFN01-38）——灰色の分類へは動かない。
                if message.wParam.0 == 0x25 {
                    next = step_openable(nav.group, -1, &nav.openable);
                }
                if message.wParam.0 == 0x27 && !nav.submenu.get() {
                    next = step_openable(nav.group, 1, &nav.openable);
                }
            } else if matches!(message.message, WM_MOUSEMOVE | WM_LBUTTONDOWN) {
                let mut point = message.pt;
                let previous = nav.pointer.replace((point.x, point.y));
                if message.message == WM_MOUSEMOVE && previous == (point.x, point.y) {
                    return false;
                }
                if unsafe { ScreenToClient(nav.hwnd, &mut point) }.as_bool() {
                    let x = point.x as f32 / nav.scale;
                    let y = point.y as f32 / nav.scale;
                    if message.message == WM_LBUTTONDOWN
                        && (0. ..36.).contains(&y)
                        && (0. ..32.).contains(&x)
                    {
                        unsafe {
                            let _ = EndMenu();
                        }
                        return true;
                    }
                    if (0. ..36.).contains(&y) && (36. ..372.).contains(&x) {
                        let group = ((x - 36.) / 48.) as i32;
                        // **開ける分類のときだけ動く**（RFN01-38）。開けないところへ
                        // ポインタが入っても、いま開いているメニューはそのまま。
                        if group != nav.group && nav.openable.get(group as usize) == Some(&true) {
                            next = Some(group);
                        }
                    }
                }
            }
            if let Some(next) = next {
                nav.next.set(Some(next));
                unsafe {
                    let _ = EndMenu();
                }
                true
            } else {
                false
            }
        });
        if consumed {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, w, l) }
}

/// 設定から割り当てたキーで、メニューの操作を実行する（RFN01-18）。
///
/// **対象は、いまフォーカスしているPane・TAB**——メニューは開いた時点で対象を
/// 捕まえるが、キーには捕まえる瞬間が無い。押せない条件（選んだ字が要る形など）は
/// それぞれの実行先が同じ判定を持っているので、ここでは断らない。
/// RFN01-66 第2弾: 割り当ての一覧のid（0〜45、メニューに無いキーの操作）が、`pane`を対象に
/// いま実行できるか。**メニューと同じ答え**（[`Scene::enabled`]）を読む。
pub(crate) fn key_allowed(window: &AppWindow, live: &Live, pane: PaneId, id: i32) -> Allowed {
    let target = Target::of(window, live, pane, 0);
    let scene = Scene::read(window, live, &target);
    let command = match id {
        0 => Command::Open,
        1 => Command::Save(false),
        2 => Command::Save(true),
        3 => Command::Close,
        4 => Command::Find(false),
        5 => Command::Find(true),
        6 => Command::Goto,
        7 => Command::Reopen,
        8 => Command::Undo(false),
        9 => Command::Undo(true),
        10 => Command::Draft,
        11 => Command::Step(false),
        12 => Command::Step(true),
        13..=16 => Command::Focus(id - 13),
        17 => Command::Below,
        19 => Command::Navigate(false),
        20 => Command::Navigate(true),
        // Emacs風の削除・移動・選択（メニューに無い）：消すのは書き換えられるとき、
        // 動く・選ぶのは本文を出しているとき。
        21 => return Allowed::from(scene.editable, false),
        22 | 23 | 31 | 32 => return Allowed::from(scene.text, false),
        24 => Command::Kill(0),
        25 => Command::Mark(false),
        26 => Command::Mark(true),
        27..=30 => Command::Kill(id - 26),
        33 => Command::Line(4),
        34..=37 => Command::Line(id - 34),
        42 => Command::Direction(!pane.vertical(window)),
        43 => Command::Preview,
        44 => Command::Viewer,
        45 => Command::Print,
        _ => match crate::shortcuts::menu_action_at(id as usize)
            .and_then(|action| shortcut_command(window, action))
        {
            Some(command) => command,
            None => return Allowed::Yes,
        },
    };
    Allowed::from(scene.enabled(&command), quiet(&command))
}

/// キーで押した操作を実行できるか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Allowed {
    Yes,
    /// この場面では使えない。知らせる（Terminalで保存を押した、など）。
    Tell,
    /// いまの状態では何も起きない。知らせない（戻す履歴が無い、隣のPaneが無い、など）。
    Quiet,
}
impl Allowed {
    fn from(enabled: bool, quiet: bool) -> Self {
        match (enabled, quiet) {
            (true, _) => Allowed::Yes,
            (false, true) => Allowed::Quiet,
            (false, false) => Allowed::Tell,
        }
    }
}

/// RFN01-66 第2弾: 押せない理由が**場所ではなく、その時の状態**の操作。キーを押すたびに
/// 知らせると煩いので、黙って何もしない（以前と同じ）。
fn quiet(command: &Command) -> bool {
    matches!(
        command,
        Command::Undo(_)
            | Command::Step(_)
            | Command::Focus(_)
            | Command::Swap(_)
            | Command::Navigate(_)
            | Command::Zoom(_)
            | Command::Kill(3 | 4)
            | Command::SpellingStep(_)
    )
}

/// RFN01-66 第2弾: キーで押した操作が、いまの場面では使えないことを知らせる
/// （以前は何も起きず、知らせも無かった）。
pub(crate) fn tell_unavailable(window: &AppWindow, name: &str) {
    window.tell(
        format!(
            "{}: {name}",
            pick("この場面では使えません", "Not available here")
        )
        .into(),
    );
}

pub fn run_shortcut(window: &AppWindow, live: &Live, action: ShortcutAction) {
    // RFN01-75/76: **the left pane's, not a document's** — no Pane or TAB is
    // acted on, so none has to be valid.
    match action {
        ShortcutAction::SearchInFolder => return window.invoke_open_folder_search(),
        ShortcutAction::ExplorePane => return window.invoke_focus_explore(),
        ShortcutAction::ReplaceInFolder => return window.invoke_open_folder_replace(),
        ShortcutAction::ToggleLeftPane => return window.invoke_toggle_left_pane(),
        ShortcutAction::ShowLeftTab(tab) => return window.invoke_show_left_tab(tab),
        ShortcutAction::OpenSettings => return window.invoke_settings_requested(),
        ShortcutAction::Palette(commands) => {
            return crate::command_palette::open(window, live, commands);
        }
        _ => {}
    }
    let target = Target::for_shortcut(window, live);
    if !target.valid(window, live) {
        return;
    }
    // RFN01-107: 下段のTABを閉じる——下段で「TABを閉じる」を選んだのと同じ。上段にいても効く。
    if action == ShortcutAction::CloseStripTab {
        if crate::has_strip_tab(live, target.parent) {
            terminal_panels::action(window, live, target.parent, 2, 0);
        }
        return;
    }
    let Some(command) = shortcut_command(window, action) else {
        return;
    };
    // RFN01-66 第2弾: **メニューで押せない場面では動かさず、知らせる。**
    if !Scene::read(window, live, &target).enabled(&command) {
        if !quiet(&command) {
            tell_unavailable(window, &crate::shortcuts::action_label(window, action));
        }
        return;
    }
    execute(window, live, &target, command);
}

/// 割り当てたキーの操作（メニューの操作）を、メニューと同じ[`Command`]へ落とす。
/// 左ペイン・設定・パレット・下段のTABのように、文書の対象を持たない操作は`None`。
fn shortcut_command(window: &AppWindow, action: ShortcutAction) -> Option<Command> {
    Some(match action {
        ShortcutAction::NewFile => Command::NewFile,
        // **既定のシェルで開く。**シェルの一覧は設定で変わるので、キーには
        // 特定のシェルを結び付けない（書き手の合意 2026-09-21）。
        ShortcutAction::NewTerminal => Command::NewTerminal(window.get_default_shell()),
        ShortcutAction::FolderTerminal => Command::FolderTerminal,
        // **設定が持つのは形**なので、挿入の並びが動いても指す先は動かない。
        ShortcutAction::Insert(shape) => Command::Insert(shape),
        ShortcutAction::Bullets(index) => Command::List(0, index),
        ShortcutAction::NumberedList => Command::List(1, -1),
        ShortcutAction::Renumber => Command::List(2, -1),
        ShortcutAction::SpellingStep(back) => Command::SpellingStep(back),
        ShortcutAction::ReadAloud => Command::ReadAloud,
        ShortcutAction::SplitRight => Command::Split(true),
        ShortcutAction::ScrollLine(down) => Command::ScrollLine(down),
        ShortcutAction::SelectLine => Command::SelectLine,
        ShortcutAction::SearchInFolder
        | ShortcutAction::ExplorePane
        | ShortcutAction::ReplaceInFolder
        | ShortcutAction::ToggleLeftPane
        | ShortcutAction::ShowLeftTab(_)
        | ShortcutAction::OpenSettings
        | ShortcutAction::Palette(_)
        | ShortcutAction::CloseStripTab => return None,
    })
}

fn execute(window: &AppWindow, live: &Live, t: &Target, command: Command) {
    restore_input(window, t.id, t.field);
    if t.field > 0 {
        let kind = match command {
            Command::Undo(false) => Some(0),
            Command::Undo(true) => Some(1),
            Command::Copy(true) => Some(2),
            Command::Copy(false) => Some(3),
            Command::Paste => Some(4),
            Command::SelectAll => Some(5),
            _ => None,
        };
        if let Some(kind) = kind {
            let input = window.global::<MenuInput>();
            input.set_target(t.field);
            input.set_kind(kind);
            input.set_generation(input.get_generation() + 1);
            return;
        }
    }
    window.set_focused_pane(t.id.index());
    let p = t.id.index();
    let parent = t.parent.index();
    match command {
        Command::NewFile => window.invoke_pane_new_file(p),
        Command::NewFromTemplate(name) => crate::new_from_template(window, live, t.id, &name),
        Command::NewTerminal(i) => window.invoke_pane_new_terminal(parent, i),
        Command::Open => {
            if t.id.is_panel() {
                terminal_panels::action(window, live, t.parent, 11, 0)
            } else {
                window.invoke_open_file_requested()
            }
        }
        Command::Folder => window.invoke_work_folder_requested(),
        Command::Recent(path) => open_work_folder(window, live, &path),
        Command::Workspaces => window.invoke_workspace_manager_requested(),
        Command::GitRepository => window.invoke_git_repository_requested(),
        Command::Save(false) => window.invoke_save_requested(),
        Command::Save(true) => window.invoke_save_as_requested(),
        Command::SaveAll => window.invoke_save_all_requested(),
        Command::Close => {
            if t.id.is_panel() || matches!(t.spot, TerminalSpot::Below) {
                terminal_panels::action(window, live, t.parent, 2, 0)
            } else {
                let at = live.tabs.borrow().of(t.id).active;
                close_tab(window, live, t.id, at);
            }
        }
        Command::CloseOthers(keep) => start_close_run(window, live, t.parent, keep),
        Command::CloseClean => close_clean_tabs(window, live, t.parent),
        Command::Reopen => reopen_closed_tab(window, live),
        Command::Duplicate => {
            let at = live.tabs.borrow().of(t.id).active;
            duplicate_tab(window, live, t.id, at);
        }
        Command::CopyPath => {
            let at = live.tabs.borrow().of(t.id).active;
            copy_tab_path(window, live, t.id, at);
        }
        Command::Reveal => window.invoke_reveal_requested(),
        Command::Encoding(i) => window.invoke_reopen_encoding(i),
        Command::Print => window.invoke_print_requested(),
        Command::Help(0) => crate::open_settings_page(window, live, 6),
        Command::Help(1) => crate::open_sample_document(window, live, t.id),
        Command::Help(_) => crate::show_about(window, live),
        Command::Exit => {
            if let Some(hwnd) = window_chrome::window_handle(window) {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        Command::FolderTerminal => terminal_shells::action(window, live, 8, 0),
        Command::Undo(redo) => window.invoke_pane_undo(p, redo),
        Command::Paste => ui_action(
            window,
            t.id,
            if matches!(t.spot, TerminalSpot::Below) {
                1
            } else {
                0
            },
        ),
        Command::Rename => ui_action(
            window,
            if t.id.is_panel() { t.parent } else { t.id },
            if t.id.is_panel() { 8 } else { 2 },
        ),
        Command::Paper(which) => ui_action(window, t.parent, 3 + which),
        Command::Display(setting) => {
            let sheet = i32::from(t.id.vertical(window));
            if let Some(s) = Setting::from_index(setting) {
                let value = s.read(window, sheet as usize);
                let before = window.get_sheet();
                window.set_sheet(shared_sheet(window, sheet as usize, s.page()) as i32);
                window.invoke_sheet_chose(setting, if value == 0 { 1 } else { 0 });
                window.set_sheet(before);
            }
        }
        Command::Copy(cut) => {
            if matches!(t.spot, TerminalSpot::Below) {
                window.invoke_pane_below_copy(parent)
            } else {
                window.invoke_pane_copy(p, cut)
            }
        }
        Command::CopyBody => window.invoke_pane_copy_body(p),
        Command::SelectAll => window.invoke_pane_select_all(p),
        Command::SelectLine => window.invoke_pane_select_line(p),
        Command::ScrollLine(down) => crate::scroll_by_line(window, &live.cache, t.id, down),
        Command::Find(replace) => {
            if t.id.screen(window).terminal || matches!(t.spot, TerminalSpot::Below) {
                terminal_workflow::action(window, live, t.parent, t.spot, 0)
            } else {
                window.invoke_toggle_find(p, replace)
            }
        }
        Command::Goto => window.invoke_goto_requested(true),
        Command::FolderFind => {
            window.set_tree_open(true);
            window.set_left_tab(1);
            window.invoke_left_tab_chosen(1);
        }
        Command::Line(n) => window.invoke_pane_line_edit(p, n),
        Command::Mark(rect) => window.invoke_pane_mark_toggled(p, rect),
        Command::Kill(n) => window.invoke_pane_kill(p, n),
        Command::Word(n) => set_word_mode_of(window, live, t.id, n),
        Command::List(n, mark) => window.invoke_pane_list_edit(p, n, mark),
        Command::Insert(shape) => insert_in_pane(window, live, t.id, shape),
        Command::Direction(vertical) => {
            if t.id.vertical(window) != vertical {
                window.invoke_pane_direction_toggled(p)
            }
        }
        Command::Appearance(scope) => window.invoke_appearance_requested(parent, scope),
        Command::Toolbar => window.set_format_toolbar_open(!window.get_format_toolbar_open()),
        Command::Palette => window.set_format_palette_open(!window.get_format_palette_open()),
        Command::Sidebar(n) => {
            window.set_tree_open(!(window.get_tree_open() && window.get_left_tab() == n));
            window.set_left_tab(n);
            window.invoke_left_tab_chosen(n);
        }
        Command::Preview => {
            if t.id.screen(window).viewer {
                window.invoke_pane_viewer_toggled(p)
            }
            window.invoke_pane_preview_toggled(p);
        }
        Command::Viewer => {
            if t.id.is_panel() {
                terminal_panels::action(window, live, t.parent, 4, 0)
            } else {
                window.invoke_pane_viewer_toggled(p)
            }
        }
        Command::Split(side) => {
            window.set_focused_pane(parent);
            window.invoke_divide_requested(side);
        }
        Command::OtherPanes => close_other_panes(window, live, t.parent),
        Command::Swap(n) => {
            window.set_focused_pane(parent);
            window.invoke_swap_requested(n);
        }
        Command::Step(back) => window.invoke_pane_tab_stepped(parent, back),
        Command::Focus(3) if below_kind(live.cache.borrow_mut().pane(t.parent)) != 0 => {
            window.invoke_pane_below_focus(parent, true)
        }
        Command::Focus(n) => window.invoke_pane_focus_moved(parent, n),
        Command::Below => window.invoke_pane_below_toggled(parent),
        Command::Above => window.invoke_pane_below_focus(parent, false),
        // RFN01-67: Git Repository の画面では「戻る」で画面を閉じる（書き手の求め 2026-10-03）。
        Command::Navigate(false) if window.get_git_repo_active() => {
            crate::git_repo_ui::close(window)
        }
        Command::Navigate(forward) => window.invoke_pane_navigate(p, forward),
        Command::Draft => window.invoke_quick_draft_requested(),
        Command::Zoom(n) => window.invoke_pane_zoom(p, n),
        Command::ZoomReset => window.invoke_pane_zoom_reset(p),
        Command::ZoomSet(percent) => window.invoke_pane_zoom_set(p, percent),
        Command::Compare(0) => window.invoke_compare_saved_requested(),
        Command::Compare(1) => window.invoke_compare_head_requested(),
        Command::Compare(3) => window.invoke_backup_history_requested(),
        Command::Compare(4) => window.invoke_git_history_requested(),
        Command::Compare(_) => window.invoke_compare_files_requested(),
        Command::Spelling => crate::spelling_ui::toggle(window, live, t.id),
        Command::SpellingStep(back) => crate::spelling_ui::step(window, live, t.id, back),
        Command::ReadAloud => crate::read_aloud::toggle(window, live, t.id),
        Command::Terminal(n) => terminal_workflow::action(window, live, t.parent, t.spot, n),
        Command::Panel(n) => terminal_panels::action(window, live, t.parent, n, 0),
        Command::Shell(n) => {
            if matches!(t.spot, TerminalSpot::Below) {
                terminal_panels::action(window, live, t.parent, 12, n)
            } else {
                window.invoke_pane_switch_shell(parent, n)
            }
        }
    }
}

fn ui_action(window: &AppWindow, pane: PaneId, kind: i32) {
    window.set_menu_pane(pane.index());
    window.set_menu_action(kind);
    window.set_menu_generation(window.get_menu_generation() + 1);
}

fn restore_input(window: &AppWindow, pane: PaneId, field: i32) {
    if field > 0 {
        let input = window.global::<MenuInput>();
        input.set_target(field);
        input.set_kind(6);
        input.set_generation(input.get_generation() + 1);
    } else {
        ui_action(window, pane, if field == -1 { 7 } else { 6 });
    }
}

fn shortcut(window: &AppWindow, command: &Command, field: bool) -> String {
    match command {
        Command::Copy(false) => return "Ctrl+C".into(),
        Command::Copy(true) => return "Ctrl+X".into(),
        Command::Paste => return "Ctrl+V".into(),
        Command::SelectAll => return "Ctrl+A".into(),
        Command::Undo(false) if field => return "Ctrl+Z".into(),
        Command::Undo(true) if field => return "Ctrl+Y".into(),
        _ => {}
    }
    let id = match command {
        Command::Open => 0,
        Command::Save(false) => 1,
        Command::Save(true) => 2,
        Command::Close => 3,
        Command::Find(false) => 4,
        Command::Find(true) => 5,
        Command::Goto => 6,
        Command::Reopen => 7,
        Command::Undo(false) => 8,
        Command::Undo(true) => 9,
        Command::Draft => 10,
        Command::Step(false) => 11,
        Command::Step(true) => 12,
        Command::Focus(n) => 13 + *n as usize,
        Command::Below => 17,
        Command::Navigate(false) => 19,
        Command::Navigate(true) => 20,
        Command::Mark(false) => 25,
        Command::Mark(true) => 26,
        Command::Line(4) => 33,
        Command::Line(n) => 34 + *n as usize,
        Command::Kill(0) => 24,
        Command::Kill(n) => 26 + *n as usize,
        Command::Preview => 43,
        Command::Viewer => 44,
        Command::Print => 45,
        // RFN01-63: メニューから来た操作のキー（既定はF8／Shift+F8）。
        Command::SpellingStep(back) => {
            return shortcuts::action_label(window, ShortcutAction::SpellingStep(*back));
        }
        Command::ReadAloud => return shortcuts::action_label(window, ShortcutAction::ReadAloud),
        _ => return String::new(),
    };
    shortcuts::label(window, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ui_tests::Harness;

    /// 末端の項目を、**押せるか・印が付いているかと番号つきで**並べる（RFN01-38）。
    fn leaves(menu: HMENU) -> Vec<(u32, bool, bool)> {
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        assert!(count >= 0);
        let mut found = Vec::new();
        for index in 0..count {
            let mut item = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STATE | MIIM_SUBMENU,
                ..Default::default()
            };
            unsafe {
                GetMenuItemInfoW(menu, index as u32, true, &mut item).unwrap();
            }
            if item.hSubMenu.0.is_null() {
                found.push((
                    item.wID,
                    item.fState.0 & MFS_DISABLED.0 == 0,
                    item.fState.0 & MFS_CHECKED.0 != 0,
                ));
            } else {
                found.extend(leaves(item.hSubMenu));
            }
        }
        found
    }

    /// 末端の項目を、**文言つきで**並べる（RFN01-47）。道順は[`leaves`]と同じで、
    /// 右クリックメニューと突き合わせるために言葉も取る。
    fn titled_leaves(menu: HMENU) -> Vec<(String, bool, bool)> {
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        assert!(count >= 0);
        let mut found = Vec::new();
        for index in 0..count {
            let mut item = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STATE | MIIM_SUBMENU | MIIM_STRING,
                ..Default::default()
            };
            unsafe {
                GetMenuItemInfoW(menu, index as u32, true, &mut item).unwrap();
            }
            if !item.hSubMenu.0.is_null() {
                found.extend(titled_leaves(item.hSubMenu));
                continue;
            }
            // 文言は2度問い合わせる——1度目で長さ、2度目で本文。
            let mut text = vec![0u16; item.cch as usize + 1];
            item.dwTypeData = windows::core::PWSTR(text.as_mut_ptr());
            // **渡すときは、終わりの0まで含めた長さ**——含めないと1字足りない。
            item.cch = text.len() as u32;
            unsafe {
                GetMenuItemInfoW(menu, index as u32, true, &mut item).unwrap();
            }
            let title = String::from_utf16_lossy(&text[..item.cch as usize]);
            found.push((
                title,
                item.fState.0 & MFS_DISABLED.0 == 0,
                item.fState.0 & MFS_CHECKED.0 != 0,
            ));
        }
        found
    }

    /// 行の体裁を何も持たない文書の答え（試験の初期値）。
    /// 改ページも表も置ける（`true`）か、どちらも置けない（`false`）。
    fn places(open: bool) -> Places {
        Places {
            page_break: open,
            table: open,
            template: open,
        }
    }

    fn bare_notes() -> document::LineNoteState {
        document::LineNoteState {
            can_heading: true,
            can_indent: true,
            can_tail: true,
            level: None,
            indent: None,
            tail: None,
        }
    }

    /// 挿入の形を全部、メニューの並びで——`document::INSERT_EDITS`の後ろに
    /// `LINE_NOTE_EDITS`、表、最後に改ページ。
    fn every_shape() -> Vec<InsertShape> {
        document::INSERT_EDITS
            .iter()
            .map(|what| InsertShape::Edit(*what))
            .chain(
                document::LINE_NOTE_EDITS
                    .iter()
                    .map(|what| InsertShape::Line(*what)),
            )
            .chain([
                InsertShape::Table,
                InsertShape::PageBreak,
                InsertShape::Template,
            ])
            .collect()
    }

    /// **表には形が1つずつ、漏れなく並ぶ。**番号は行き来して同じ形に戻り、並びの
    /// 外の番号はどの形も指さない。文言は空でない。
    #[test]
    fn the_insert_table_holds_every_shape_once() {
        let listed: Vec<InsertShape> = insert_items().map(|(shape, _)| *shape).collect();
        assert_eq!(listed, every_shape());
        for (at, shape) in listed.iter().enumerate() {
            assert_eq!(shape.number(), at as i32, "{shape:?}");
            assert_eq!(InsertShape::from_number(at as i32), Some(*shape));
            let (ja, en) = shape.title();
            assert!(!ja.is_empty() && !en.is_empty(), "{shape:?}");
        }
        assert_eq!(InsertShape::from_number(-1), None);
        assert_eq!(InsertShape::from_number(listed.len() as i32), None);
    }

    /// 以前の`insert_commands`と同じ形で、挿入の行をWindowsのメニューへ組む（試験用）。
    #[allow(clippy::too_many_arguments)]
    fn insert_commands(
        window: &AppWindow,
        menu: &Popup,
        commands: &mut Vec<Command>,
        field: bool,
        enabled: bool,
        picked: bool,
        notes: document::LineNoteState,
        places: Places,
    ) -> windows::core::Result<()> {
        let mut items = Vec::new();
        insert_rows_by(&mut items, |shape| {
            context_insert_state(shape, enabled, picked, &notes, places)
        });
        let (built, built_commands) = native(window, items, field)?;
        // 組んだ段を、渡された段の下へそのまま移す。
        let count = unsafe { GetMenuItemCount(Some(built.handle)) };
        for _ in 0..count.max(0) {
            let mut text = vec![0u16; 512];
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STRING | MIIM_ID | MIIM_STATE | MIIM_SUBMENU | MIIM_FTYPE,
                dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
                cch: text.len() as u32 - 1,
                ..Default::default()
            };
            unsafe {
                GetMenuItemInfoW(built.handle, 0, true, &mut info)?;
                RemoveMenu(built.handle, 0, MF_BYPOSITION)?;
                info.dwTypeData = windows::core::PWSTR(text.as_mut_ptr());
                InsertMenuItemW(menu.handle, u32::MAX, true, &info)?;
            }
        }
        commands.extend(built_commands);
        Ok(())
    }

    /// RFN01-38: **本文へ置ける項目は押せる。**並びは表（`INSERT_MENU`）のまま。
    #[test]
    fn the_insert_menu_offers_what_it_can_write_into_the_body() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let menu = Popup::new().unwrap();
        let mut commands = Vec::new();
        insert_commands(
            &h.window,
            &menu,
            &mut commands,
            false,
            true,
            true,
            bare_notes(),
            places(true),
        )
        .unwrap();

        let shapes = every_shape();
        assert_eq!(commands.len(), shapes.len());
        for (command, shape) in commands.iter().zip(&shapes) {
            assert!(
                matches!(command, Command::Insert(given) if given == shape),
                "{shape:?}"
            );
        }
        let rows = leaves(menu.handle);
        assert_eq!(rows.len(), commands.len());
        // **解除だけは、その指定がある行でしか押せない**（この文書には何も無い）。
        let closed: Vec<document::LineNoteEdit> = rows
            .iter()
            .enumerate()
            .skip(document::INSERT_EDITS.len())
            .take(document::LINE_NOTE_EDITS.len())
            .filter(|(_, (_, pickable, _))| !*pickable)
            .map(|(at, _)| document::LINE_NOTE_EDITS[at - document::INSERT_EDITS.len()])
            .collect();
        assert_eq!(
            closed,
            vec![
                document::LineNoteEdit::NoHeading,
                document::LineNoteEdit::NoIndent,
                document::LineNoteEdit::NoAlignToEnd,
            ]
        );
        // **入れられる位置の改ページは押せる。**
        assert!(
            rows.last().is_some_and(|(_, pickable, _)| *pickable),
            "改ページ"
        );
    }

    /// **選んだ字を指す注記は、字を選んでいなければ押せない**（書き手の合意
    /// 2026-09-21）——同じ語を本文と注記の2か所へ書く形は、指す先が無いと書けない。
    /// どの形がそうかは本文を持つ側（`document::InsertEdit`）が答える。**解除の行は、
    /// その指定がある行でだけ押せる。**
    #[test]
    fn a_note_that_points_at_a_word_needs_the_word_picked() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let menu = Popup::new().unwrap();
        let mut commands = Vec::new();
        insert_commands(
            &h.window,
            &menu,
            &mut commands,
            false,
            true,
            false,
            bare_notes(),
            places(false),
        )
        .unwrap();

        let rows = leaves(menu.handle);
        for (index, (_, pickable, _)) in rows.iter().take(document::INSERT_EDITS.len()).enumerate()
        {
            let wanted = document::INSERT_EDITS[index].needs_a_picked_word();
            assert_eq!(*pickable, !wanted, "選んだ字が要る形だけが押せない");
        }
        // 何も指定の無い行では、値の行だけが押せて、解除の行は押せない。
        for (at, (_, pickable, _)) in rows
            .iter()
            .enumerate()
            .skip(document::INSERT_EDITS.len())
            .take(document::LINE_NOTE_EDITS.len())
        {
            let edit = document::LINE_NOTE_EDITS[at - document::INSERT_EDITS.len()];
            let allowed = !matches!(
                edit,
                document::LineNoteEdit::NoHeading
                    | document::LineNoteEdit::NoIndent
                    | document::LineNoteEdit::NoAlignToEnd
            );
            assert_eq!(*pickable, allowed, "{edit:?}");
        }
        // **入れられない位置の改ページも押せない。**
        assert!(rows.last().is_some_and(|(_, pickable, _)| !*pickable));
    }

    /// RFN01-47: **右クリックメニューの挿入は、タイトルバーの挿入メニューと同じ。**
    /// 言葉も、押せるかも、いま効いている印も一致する——見る側が2つに分かれると
    /// 必ず食い違うので、ここで突き合わせる。
    #[test]
    fn the_right_click_rows_are_the_insert_menu() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let notes = bare_notes();
        for picked in [false, true] {
            let menu = Popup::new().unwrap();
            let mut commands = Vec::new();
            insert_commands(
                &h.window,
                &menu,
                &mut commands,
                false,
                true,
                picked,
                notes,
                places(false),
            )
            .unwrap();
            let native = titled_leaves(menu.handle);

            let (top, children) = context_insert_rows(true, picked, notes, places(false));
            let mut ours: Vec<(&str, bool, bool)> = Vec::new();
            let (mut at_top, mut at_child) = (0, 0);
            for what in INSERT_MENU {
                match what {
                    InsertEntry::Row(_) => {
                        let row = &top[at_top];
                        ours.push((row.title.as_str(), row.enabled, row.checked));
                        at_top += 1;
                    }
                    InsertEntry::Group(_, rows) => {
                        for row in &children[at_child..at_child + rows.len()] {
                            ours.push((row.title.as_str(), row.enabled, row.checked));
                        }
                        at_child += rows.len();
                        at_top += 1;
                    }
                }
            }
            assert_eq!(ours.len(), native.len(), "picked={picked}");
            for (at, (ours, native)) in ours.iter().zip(native.iter()).enumerate() {
                assert_eq!(ours.0, native.0, "row {at}, picked={picked}");
                assert_eq!(ours.1, native.1, "enabled, row {at}: {}", native.0);
                assert_eq!(ours.2, native.2, "checked, row {at}: {}", native.0);
            }
            // 組の行は、中に押せる行があるかどうかで押せる（**空でも出す**）。
            let mut group = 0;
            for what in INSERT_MENU {
                let InsertEntry::Group(_, rows) = what else {
                    continue;
                };
                let inside = children.iter().filter(|row| row.group == group).count();
                let openable = children.iter().any(|row| row.group == group && row.enabled);
                let row = top
                    .iter()
                    .find(|row| row.flyout && row.group == group)
                    .unwrap();
                assert_eq!(inside, rows.len(), "組 {group}");
                assert_eq!(row.enabled, openable, "組 {group}");
                group += 1;
            }
        }
    }

    /// **いま効いている指定には印が付く**（書き手の合意 2026-09-21）。
    #[test]
    fn the_line_note_rows_mark_the_value_that_is_in_force() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let menu = Popup::new().unwrap();
        let mut commands = Vec::new();
        let notes = document::LineNoteState {
            level: Some(3),
            indent: Some(2),
            tail: Some(1),
            ..bare_notes()
        };
        insert_commands(
            &h.window,
            &menu,
            &mut commands,
            false,
            true,
            true,
            notes,
            places(true),
        )
        .unwrap();

        let marked: Vec<document::LineNoteEdit> = leaves(menu.handle)
            .iter()
            .enumerate()
            .skip(document::INSERT_EDITS.len())
            .take(document::LINE_NOTE_EDITS.len())
            .filter(|(_, (_, _, checked))| *checked)
            .map(|(at, _)| document::LINE_NOTE_EDITS[at - document::INSERT_EDITS.len()])
            .collect();
        assert_eq!(
            marked,
            vec![
                document::LineNoteEdit::Heading(3),
                document::LineNoteEdit::Indent(2),
                document::LineNoteEdit::AlignToEnd(1),
            ]
        );
    }

    /// 本文へ何も書けないとき（Viewer・ReadOnly・矩形選択のとき）は、**同じ行が
    /// どれも無効になる**。
    #[test]
    fn no_insert_row_is_pickable_when_the_body_cannot_be_written() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let menu = Popup::new().unwrap();
        insert_commands(
            &h.window,
            &menu,
            &mut Vec::new(),
            false,
            false,
            true,
            bare_notes(),
            places(true),
        )
        .unwrap();

        assert!(
            leaves(menu.handle)
                .iter()
                .all(|(_, pickable, _)| !*pickable),
            "書けないときは押せない"
        );
    }

    #[test]
    fn display_toggle_writes_effective_shared_sheet_and_restores_settings_tab() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        PaneId::FIRST.update_screen(&h.window, |s| s.vertical = true);
        h.window.set_text_shared(true);
        h.window.set_layout_shared(true);
        h.window.set_paper_shared(true);
        h.window.set_sheet(1);
        let seen = Rc::new(Cell::new((-1, -1)));
        let capture = seen.clone();
        let weak = h.window.as_weak();
        h.window.on_sheet_chose(move |setting, _| {
            capture.set((setting, weak.upgrade().unwrap().get_sheet()));
        });
        let target = Target::capture(&h.window, &h.live);
        execute(&h.window, &h.live, &target, Command::Display(12));
        assert_eq!(seen.get(), (12, 0));
        assert_eq!(h.window.get_sheet(), 1);
    }

    #[test]
    fn target_rejects_a_different_tab_even_when_it_shares_the_document() {
        let (h, doc) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let target = Target::capture(&h.window, &h.live);
        assert!(target.valid(&h.window, &h.live));
        h.live.tabs.borrow_mut().of_mut(PaneId::FIRST).unwrap().tabs[0] =
            PaneTab::showing(&h.window, PaneId::FIRST, doc);
        assert!(!target.valid(&h.window, &h.live));
    }

    #[test]
    fn panel_target_does_not_fall_back_to_parent_when_panel_closes() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let owner = PaneId::FIRST;
        owner.update_screen(&h.window, |s| {
            s.terminal = true;
            s.below_kind = 2;
            s.below_height = 240.;
        });
        terminal_panels::ensure(&h.window, &h.live, owner);
        panel_source::sync(&h.window, &h.live);
        panel_source::focus(&h.window, &h.live, owner, true);
        let target = Target::capture(&h.window, &h.live);
        assert!(target.id.is_panel());
        assert!(target.valid(&h.window, &h.live));
        h.live.tabs.borrow_mut().of_mut(owner).unwrap().tabs[0]
            .below
            .entries
            .clear();
        panel_source::sync(&h.window, &h.live);
        assert!(!target.valid(&h.window, &h.live));
    }

    /// RFN01-38: **子が全部無効なら親も無効**（書き手の決定 2026-09-21）。
    /// 本文に書けるTabでは「挿入」が開き、まだ何も実行しないHelpは開かない。
    #[test]
    fn a_category_with_nothing_to_pick_does_not_open() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));

        assert!(menu_opens(&h.window, &h.live, 2), "挿入は開く");
        // RFN01-66（2026-11-05）: ヘルプに行ができた（以前は未実装の行だけで開かなかった）。
        assert!(menu_opens(&h.window, &h.live, 6), "Helpは開く");
    }

    /// RFN01-66 第2弾: ⋮・右クリックの行（`MenuActions`）も**メニューと同じ答え**で灰色になり、
    /// 押せば同じ操作が走る。
    #[test]
    fn the_pane_menus_read_the_same_premises() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let (window, live) = (&h.window, &h.live);
        install(window, live);
        let actions = window.global::<MenuActions>();
        actions.invoke_refresh();
        let generation = actions.get_generation();
        assert!(actions.invoke_allowed(MenuAction::Save, generation));
        // 無題の文書にはフォルダが無い（タイトルバーのメニューでも灰色）。
        assert!(!actions.invoke_allowed(MenuAction::FolderTerminal, generation));
        assert!(!actions.invoke_allowed(MenuAction::Reveal, generation));
        assert!(
            !actions.invoke_allowed(MenuAction::SwapLeft, generation),
            "隣のPaneが無い"
        );
        let saves = Rc::new(Cell::new(0));
        let seen = saves.clone();
        window.on_save_requested(move || seen.set(seen.get() + 1));
        actions.invoke_run(MenuAction::Save);
        assert_eq!(saves.get(), 1);
        // 押せない行は、押されても走らせない。
        let reveals = Rc::new(Cell::new(0));
        let seen = reveals.clone();
        window.on_reveal_requested(move || seen.set(seen.get() + 1));
        actions.invoke_run(MenuAction::Reveal);
        assert_eq!(reveals.get(), 0);
    }

    /// RFN01-66 第2弾: キーは**メニューと同じ前提**を読む。使えない場所では動かさずに知らせ、
    /// その時の状態で何も起きないだけの操作（戻す履歴が無い等）は黙る。パレットも同じ答え。
    #[test]
    fn keys_and_the_palette_read_the_menus_premises() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let (window, live) = (&h.window, &h.live);
        let pane = PaneId::FIRST;
        assert_eq!(key_allowed(window, live, pane, 1), Allowed::Yes, "保存");
        assert_eq!(
            key_allowed(window, live, pane, 8),
            Allowed::Quiet,
            "戻す履歴が無い"
        );
        // メニューの行とキーが同じ答え。
        let snapshot = menu_snapshot(window, live);
        let save = snapshot
            .entries
            .iter()
            .position(|entry| entry.title.ends_with("› 保存") || entry.title.ends_with("› Save"))
            .unwrap();
        assert!(snapshot.entries[save].enabled);

        crate::open_settings_page(window, live, 0);
        let saves = Rc::new(Cell::new(0));
        let seen = saves.clone();
        window.on_save_requested(move || seen.set(seen.get() + 1));
        assert_eq!(
            key_allowed(window, live, pane, 1),
            Allowed::Tell,
            "設定のTABは保存できない"
        );
        crate::shortcuts::run_command(window, live, pane, 1);
        assert_eq!(saves.get(), 0);
        assert!(
            window.get_render_status().contains("使えません"),
            "知らせる"
        );
        let snapshot = menu_snapshot(window, live);
        let save = snapshot
            .entries
            .iter()
            .position(|entry| entry.title.ends_with("› 保存") || entry.title.ends_with("› Save"))
            .unwrap();
        assert!(!snapshot.entries[save].enabled, "メニューでも灰色");
    }

    /// 書式のボタン（2026-09-23）：**挿入メニューの1段目の全部に絵があり**、束は
    /// 挿入の並びのまま、箇条書きが最後の束である。
    #[test]
    fn every_insert_entry_has_a_format_button() {
        for entry in INSERT_MENU {
            let first = match entry {
                InsertEntry::Row((shape, _)) => *shape,
                InsertEntry::Group(_, items) => items[0].0,
            };
            if first == InsertShape::Template {
                continue;
            }
            assert!(format_face(first).is_some(), "{first:?}");
        }
        let (buttons, _) = format_rows(
            true,
            false,
            true,
            (bare_notes(), places(true)),
            document::BulletMarks::all(),
        );
        // テンプレートだけは書式のボタンを持たない（RFN01-88）。
        assert_eq!(buttons.len(), INSERT_MENU.len() - 1 + 3);
        assert!(
            buttons.windows(2).all(|pair| pair[0].band <= pair[1].band),
            "束は前から順に"
        );
        assert_eq!(buttons.last().map(|button| button.band), Some(LIST_BAND));
        let tips: Vec<_> = buttons
            .iter()
            .map(|button| button.tip.to_string())
            .collect();
        assert_eq!(
            tips.first().map(String::as_str),
            Some(pick("リンク", "Link"))
        );
        assert_eq!(
            tips.last().map(String::as_str),
            Some(pick("番号を振り直す", "Renumber"))
        );
    }

    /// **押した番号は、メニューと同じ操作へ戻る**——組の中身も、箇条書きも。
    #[test]
    fn a_format_button_runs_the_menu_command() {
        let (buttons, children) = format_rows(
            true,
            false,
            true,
            (bare_notes(), places(true)),
            document::BulletMarks::all(),
        );
        for button in &buttons {
            assert_eq!(button.flyout, button.action == -1, "{}", button.tip);
            assert_eq!(button.flyout, button.group >= 0, "{}", button.tip);
            if button.flyout {
                assert!(
                    children.iter().any(|row| row.group == button.group),
                    "{}の組は空ではない",
                    button.tip
                );
            }
        }
        let inserts: Vec<_> = buttons
            .iter()
            .map(|button| button.action)
            .chain(children.iter().map(|row| row.number))
            .filter(|&action| (0..FORMAT_LIST).contains(&action))
            .collect();
        // テンプレートだけは書式のボタンを持たない（RFN01-88）。
        assert_eq!(
            inserts.len(),
            insert_items().count() - 1,
            "挿入の形はどれも1回ずつ"
        );
        for action in inserts {
            let shape = InsertShape::from_number(action).unwrap();
            assert!(
                matches!(format_command(action), Some(Command::Insert(given)) if given == shape)
            );
        }
        let bullets: Vec<_> = children
            .iter()
            .filter(|row| row.number >= FORMAT_LIST)
            .map(|row| (row.title.to_string(), row.number))
            .collect();
        assert_eq!(
            bullets,
            [("-", 0), ("*", 1), ("+", 2)].map(|(mark, at)| (mark.to_owned(), list_action(0, at)))
        );
        for (at, _) in [0, 1, 2].iter().enumerate() {
            let at = at as i32;
            assert!(
                matches!(format_command(list_action(0, at)), Some(Command::List(0, mark)) if mark == at)
            );
        }
        assert!(matches!(
            format_command(list_action(1, -1)),
            Some(Command::List(1, -1))
        ));
        assert!(matches!(
            format_command(list_action(2, -1)),
            Some(Command::List(2, -1))
        ));
        assert!(format_command(FORMAT_LIST - 1).is_none());
    }

    /// 書けないとき、**ボタンも組の中身も押せない**（メニューと同じ）。
    #[test]
    fn no_format_button_is_pickable_when_the_body_cannot_be_written() {
        let (buttons, children) = format_rows(
            false,
            false,
            true,
            (bare_notes(), places(true)),
            document::BulletMarks::all(),
        );
        assert!(buttons.iter().all(|button| !button.enabled));
        assert!(children.iter().all(|row| !row.enabled));
    }

    /// 出していなければ組まない。出せば組み、**押せば本文へ入る**。
    #[test]
    fn the_palette_publishes_and_inserts_into_the_pane() {
        let (h, document) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let cache = RefCell::new(FormatCache::default());
        publish_format(&h.window, &h.live, &cache);
        assert_eq!(h.window.get_format_buttons().row_count(), 0, "しまってある");

        h.window.set_format_palette_open(true);
        publish_format(&h.window, &h.live, &cache);
        assert_eq!(
            h.window.get_format_buttons().row_count(),
            INSERT_MENU.len() - 1 + 3
        );

        let ruby = InsertShape::Edit(document::InsertEdit::Ruby).number();
        run_format(&h.window, &h.live, ruby);
        assert!(
            document.text.borrow().contains('《'),
            "{}",
            document.text.borrow()
        );
    }
}
