//! Main menu: immutable invocation target, native popup lifetime, existing commands.
use super::*;
use windows::{
    Win32::{
        Foundation::{LPARAM, POINT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        UI::WindowsAndMessaging::*,
    },
    core::PCWSTR,
};

#[derive(Clone)]
enum Command {
    NewFile,
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
    Insert(i32),
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
    Terminal(i32),
    Panel(i32),
    Shell(i32),
    ZoomSet(i32),
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

// Planned commands deliberately have no executable ID. Keep the complete
// menu structure discoverable without introducing new document mutations.
fn pending(menu: &Popup, ja: &str, en: &str) -> windows::core::Result<()> {
    menu.row(
        &format!(
            "{} {}",
            pick(ja, en),
            pick("（未実装）", "(not implemented)")
        ),
        0,
        false,
        false,
    )
}

fn pending_group(
    menu: &Popup,
    ja: &str,
    en: &str,
    rows: &[(&str, &str)],
) -> windows::core::Result<()> {
    let child = Popup::new()?;
    for (ja, en) in rows {
        pending(&child, ja, en)?;
    }
    menu.child(pick(ja, en), child)
}

/// メニューの1行を足す。**番号は押されたときの言い方**——`commands`の並びが
/// そのままIDになり、`execute`が同じ並びで読み返す。既定のキーがあれば行に併記する。
#[allow(clippy::too_many_arguments)]
fn menu_row(
    window: &AppWindow,
    menu: &Popup,
    commands: &mut Vec<Command>,
    field: bool,
    ja: &str,
    en: &str,
    command: Command,
    enabled: bool,
    checked: bool,
) -> windows::core::Result<()> {
    let key = shortcut(window, &command, field);
    let title = if key.is_empty() {
        pick(ja, en).to_owned()
    } else {
        format!("{}\t{key}", pick(ja, en))
    };
    commands.push(command);
    menu.row(&title, commands.len(), enabled, checked)
}

/// 実行できる挿入項目（RFN01-38の単位1）。**並びが`Command::Insert(n)`の番号**で、
/// `insert_in_pane`の言い方と1対1である。
const INSERT_LINKS: [(&str, &str); 3] = [
    ("Markdownリンク", "Markdown Link"),
    ("Wikiリンク", "Wiki Link"),
    ("別名付きWikiリンク", "Wiki Link with Alias"),
];

/// 挿入メニューの**実行できる項目**（RFN01-38）。リンク3つは子メニュー、ルビは
/// 親メニューの1行——画面の並びはA案のままである。
fn insert_commands(
    window: &AppWindow,
    menu: &Popup,
    commands: &mut Vec<Command>,
    field: bool,
    enabled: bool,
) -> windows::core::Result<()> {
    let link = Popup::new()?;
    for (index, (ja, en)) in INSERT_LINKS.iter().copied().enumerate() {
        menu_row(
            window,
            &link,
            commands,
            field,
            ja,
            en,
            Command::Insert(index as i32),
            enabled,
            false,
        )?;
    }
    menu.child(pick("リンク", "Link"), link)?;
    menu_row(
        window,
        menu,
        commands,
        field,
        "ルビ",
        "Ruby",
        Command::Insert(INSERT_LINKS.len() as i32),
        enabled,
        false,
    )
}

/// 挿入メニューのうち、まだ実行しない項目（RFN01-38の単位2以降）。**「メニューに
/// 無い」と「まだ実行しない」を区別する**（要件 6.7）——末端は「（未実装）」の
/// 無効表示のままにする。
fn insert_pending(menu: &Popup) -> windows::core::Result<()> {
    pending_group(
        menu,
        "注記",
        "Annotation",
        &[
            ("左側の注記", "Left-side Annotation"),
            ("ルビと左側の注記", "Ruby and Left-side Annotation"),
        ],
    )?;
    pending_group(
        menu,
        "傍点",
        "Emphasis Marks",
        &[
            ("傍点", "Emphasis Dots"),
            ("ゴマ傍点", "Sesame Dots"),
            ("丸傍点", "Round Dots"),
            ("白丸傍点", "White Round Dots"),
            ("二重丸傍点", "Double Round Dots"),
            ("×傍点", "Cross Dots"),
        ],
    )?;
    pending_group(
        menu,
        "傍線",
        "Emphasis Lines",
        &[
            ("傍線", "Single Line"),
            ("二重傍線", "Double Line"),
            ("波線", "Wavy Line"),
            ("鎖線", "Chain Line"),
            ("破線", "Dashed Line"),
        ],
    )?;
    pending_group(
        menu,
        "文字注記",
        "Text Annotation",
        &[
            ("縦中横", "Tate-chu-yoko"),
            ("割り注", "Warichu"),
            ("小さな文字", "Small Text"),
            ("大きな文字", "Large Text"),
        ],
    )?;
    let heading = Popup::new()?;
    for level in 1..=6 {
        let name = say!("見出し {level}", "Heading {level}");
        pending(&heading, &name, &name)?;
    }
    pending(&heading, "見出しを解除", "Remove Heading")?;
    menu.child(pick("見出し", "Heading"), heading)?;
    let paragraph = Popup::new()?;
    let indent = Popup::new()?;
    for count in 1..=4 {
        let name = say!("{count}字下げ", "Indent {count} Characters");
        pending(&indent, &name, &name)?;
    }
    pending(&indent, "字下げを解除", "Remove Indent")?;
    paragraph.child(pick("字下げ", "Indent"), indent)?;
    let tail = Popup::new()?;
    pending(&tail, "地付き", "Align to End")?;
    for count in 1..=4 {
        let name = say!("地から{count}字上げ", "{count} Characters from End");
        pending(&tail, &name, &name)?;
    }
    pending(&tail, "地付きを解除", "Remove End Alignment")?;
    paragraph.child(pick("地付き", "End Alignment"), tail)?;
    menu.child(pick("段落注記", "Paragraph Annotation"), paragraph)?;
    pending(menu, "改ページ", "Page Break")
}

struct Target {
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
        let id = focused_pane(window);
        let parent = if id.is_panel() {
            PaneId::from_index(id.screen(window).panel_owner)
        } else {
            id
        };
        let field = window.global::<MenuInput>().get_target();
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

pub fn install(window: &AppWindow, live: &Live, kills: &Rc<RefCell<Kills>>) {
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
    let kills = kills.clone();
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
        let target = Rc::new(Target::capture(&window, &live));
        queue_group(&window, live.clone(), kills.clone(), target, group);
    });
}

fn queue_group(
    window: &AppWindow,
    live: Live,
    kills: Rc<RefCell<Kills>>,
    target: Rc<Target>,
    group: i32,
) {
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
        match show(&window, &live, &kills, &target, group) {
            Ok(Some(next)) => queue_group(&window, live, kills, target, next),
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

fn show(
    window: &AppWindow,
    live: &Live,
    kills: &Rc<RefCell<Kills>>,
    t: &Target,
    group: i32,
) -> windows::core::Result<Option<i32>> {
    let root = Popup::new()?;
    let mut commands = Vec::new();
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
    let main = text && !t.id.is_panel();
    let parent_screen = t.parent.screen(window);
    let neighbours = [
        parent_screen.swap_left,
        parent_screen.swap_right,
        parent_screen.swap_up,
        parent_screen.swap_down,
    ];
    let tab_count = live.tabs.borrow().of(t.parent).tabs.len();
    let can_back = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(t.id);
        stepped_place(strip.history.len(), strip.at, false).is_some()
    };
    let can_forward = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(t.id);
        stepped_place(strip.history.len(), strip.at, true).is_some()
    };
    let path = t.document.file.borrow().path().is_some();
    let selected = if terminal {
        live.cache
            .borrow_mut()
            .pane(t.parent)
            .shell(t.spot)
            .is_some_and(|view| view.selection.is_some())
    } else {
        !selected_runs(&live.cache, t.id).is_empty()
    };
    let running = t
        .session
        .as_ref()
        .is_some_and(|session| !session.borrow().finished());
    let row = |menu: &Popup,
               commands: &mut Vec<Command>,
               ja: &str,
               en: &str,
               command,
               enabled,
               checked| {
        menu_row(
            window,
            menu,
            commands,
            t.field > 0,
            ja,
            en,
            command,
            enabled,
            checked,
        )
    };
    macro_rules! add {
        ($ja:expr,$en:expr,$cmd:expr,$enabled:expr) => {
            row(&root, &mut commands, $ja, $en, $cmd, $enabled, false)?
        };
    }
    match group {
        0 => {
            add!("新規文書", "New File", Command::NewFile, true);
            let shells = Popup::new()?;
            for (i, shell) in configured_shells(window).iter().enumerate() {
                row(
                    &shells,
                    &mut commands,
                    &shell.name,
                    &shell.name,
                    Command::NewTerminal(i as i32),
                    true,
                    false,
                )?;
            }
            root.child(pick("新しいTerminal", "New Terminal"), shells)?;
            add!(
                "文書のフォルダでTerminal起動",
                "Terminal in Document Folder",
                Command::FolderTerminal,
                main && path
            );
            add!("ファイルを開く…", "Open File…", Command::Open, true);
            add!("フォルダを開く…", "Open Folder…", Command::Folder, true);
            let recent = Popup::new()?;
            for path in offered_folders(live) {
                let name = path.display().to_string();
                row(
                    &recent,
                    &mut commands,
                    &name,
                    &name,
                    Command::Recent(path),
                    true,
                    false,
                )?;
            }
            root.child(pick("最近のフォルダ", "Recent Folders"), recent)?;
            add!("Workspaces…", "Workspaces…", Command::Workspaces, true);
            root.sep()?;
            add!(
                "保存",
                "Save",
                Command::Save(false),
                text && !t.id.reads_only(window) && !t.document.read_only()
            );
            add!(
                "名前を付けて保存…",
                "Save As…",
                Command::Save(true),
                text && !t.document.read_only()
            );
            add!(
                "すべて保存（Editor Panelを除く）",
                "Save All (except Editor Panels)",
                Command::SaveAll,
                true
            );
            root.sep()?;
            add!("TABを閉じる", "Close Tab", Command::Close, true);
            add!(
                "他のTABを閉じる",
                "Close Other Tabs",
                Command::CloseOthers(true),
                !t.id.is_panel()
            );
            add!(
                "すべてのTABを閉じる",
                "Close All Tabs",
                Command::CloseOthers(false),
                !t.id.is_panel()
            );
            add!(
                "未編集のTABを閉じる",
                "Close Unedited Tabs",
                Command::CloseClean,
                !t.id.is_panel()
            );
            add!(
                "閉じたTABを開き直す",
                "Reopen Closed Tab",
                Command::Reopen,
                !t.id.is_panel()
            );
            root.sep()?;
            let renamable = text
                && path
                && (!t.id.is_panel()
                    || terminal_panels::current(live, t.parent)
                        .is_some_and(|p| p.borrow().capture.is_none()));
            add!("名前を変更…", "Rename…", Command::Rename, renamable);
            add!(
                "別TABで開く",
                "Open in Another Tab",
                Command::Duplicate,
                main
            );
            add!("パスをコピー", "Copy Path", Command::CopyPath, main && path);
            add!(
                "Explorerで表示",
                "Reveal in Explorer",
                Command::Reveal,
                main && path
            );
            let enc = Popup::new()?;
            for (i, name) in ["UTF-8", "UTF-16 LE", "UTF-16 BE", "CP932"]
                .iter()
                .enumerate()
            {
                row(
                    &enc,
                    &mut commands,
                    name,
                    name,
                    Command::Encoding(i as i32),
                    main && path,
                    false,
                )?;
            }
            root.child(
                pick("文字コードを指定して開き直す", "Reopen with Encoding"),
                enc,
            )?;
            add!("印刷…", "Print…", Command::Print, main);
            root.sep()?;
            add!("終了", "Exit", Command::Exit, true);
        }
        1 => {
            add!(
                "元に戻す",
                "Undo",
                Command::Undo(false),
                (editable && screen.can_undo) || t.field > 0
            );
            add!(
                "やり直し",
                "Redo",
                Command::Undo(true),
                (editable && screen.can_redo) || t.field > 0
            );
            root.sep()?;
            add!(
                "切り取り",
                "Cut",
                Command::Copy(true),
                (editable && selected) || t.field > 0
            );
            add!(
                "コピー",
                "Copy",
                Command::Copy(false),
                ((text || terminal) && selected) || t.field > 0
            );
            add!(
                "貼り付け",
                "Paste",
                Command::Paste,
                editable || (terminal && running) || t.field > 0
            );
            add!(
                "全選択",
                "Select All",
                Command::SelectAll,
                text || t.field > 0
            );
            add!(
                "本文だけをコピー",
                "Copy Text Only",
                Command::CopyBody,
                text && t.field <= 0
            );
            root.sep()?;
            add!(
                "検索…",
                "Find…",
                Command::Find(false),
                text || terminal || screen.settings
            );
            add!("置換…", "Replace…", Command::Find(true), editable);
            add!("行へ移動…", "Go to Line…", Command::Goto, text);
            add!(
                "フォルダ内を検索…",
                "Find in Folder…",
                Command::FolderFind,
                true
            );
            if terminal {
                root.sep()?;
                add!("画面をクリア", "Clear Screen", Command::Terminal(5), true);
                add!("履歴をクリア", "Clear History", Command::Terminal(6), true);
            }
            let lines = Popup::new()?;
            for (ja, en, n) in [
                ("行を削除", "Delete Line", 4),
                ("行を上へ移動", "Move Line Up", 0),
                ("行を下へ移動", "Move Line Down", 1),
                ("行を上へ複製", "Duplicate Line Up", 2),
                ("行を下へ複製", "Duplicate Line Down", 3),
            ] {
                row(
                    &lines,
                    &mut commands,
                    ja,
                    en,
                    Command::Line(n),
                    editable,
                    false,
                )?;
            }
            root.child(pick("行操作", "Lines"), lines)?;
            add!(
                "選択を開始・解除",
                "Start / Cancel Selection",
                Command::Mark(false),
                text && t.field <= 0
            );
            add!(
                "矩形選択を開始・解除",
                "Start / Cancel Rectangle Selection",
                Command::Mark(true),
                text && t.field <= 0
            );
            let kill_menu = Popup::new()?;
            let can_yank = !kills.borrow().ring.is_empty();
            let can_older = standing_yank(t.id, &live.states, &t.document, kills).is_some();
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
                let enabled = match i {
                    1 => text && selected && t.field <= 0,
                    2 => editable && selected,
                    3 => editable && can_yank,
                    4 => editable && can_older,
                    _ => editable,
                };
                row(
                    &kill_menu,
                    &mut commands,
                    ja,
                    en,
                    Command::Kill(i as i32),
                    enabled,
                    false,
                )?;
            }
            root.child("Kill Ring", kill_menu)?;
            let words = Popup::new()?;
            row(
                &words,
                &mut commands,
                "なし",
                "None",
                Command::Word(0),
                main,
                pane_word_mode(live, t.id) == 0,
            )?;
            for mode in word_modes_now() {
                row(
                    &words,
                    &mut commands,
                    &mode.name,
                    &mode.name,
                    Command::Word(mode.id),
                    main,
                    pane_word_mode(live, t.id) == mode.id,
                )?;
            }
            root.child(pick("単語チェック", "Word Check"), words)?;
        }
        2 => {
            // RFN01-38の単位1: リンクとルビは実行できる。**矩形選択のときは押せない**
            // ——矩形へまとめて入れるのは別の課題である（RFN01-41）。
            let rectangular = live.states.of(t.id).borrow().rectangular;
            insert_commands(
                window,
                &root,
                &mut commands,
                t.field > 0,
                editable && !rectangular,
            )?;
            insert_pending(&root)?;
            root.sep()?;
            let marks = bullet_marks_of(window);
            for (i, mark) in document::BULLET_MARKS.iter().enumerate() {
                let name = format!("{} {mark}", pick("箇条書き", "Bullet List"));
                row(
                    &root,
                    &mut commands,
                    &name,
                    &name,
                    Command::List(0, i as i32),
                    editable && marks.reads(*mark),
                    false,
                )?;
            }
            add!(
                "番号付きリスト",
                "Numbered List",
                Command::List(1, -1),
                editable && marks.first().is_some()
            );
            add!(
                "番号を振り直す",
                "Renumber",
                Command::List(2, -1),
                editable && marks.first().is_some()
            );
        }
        3 => {
            row(
                &root,
                &mut commands,
                "横書き",
                "Horizontal",
                Command::Direction(false),
                main,
                !t.id.vertical(window),
            )?;
            row(
                &root,
                &mut commands,
                "縦書き",
                "Vertical",
                Command::Direction(true),
                main && !t.id.reads_only(window),
                t.id.vertical(window),
            )?;
            root.sep()?;
            add!(
                "このPaneの背景…（個別色を解除）",
                "Pane Background… (clears individual colors)",
                Command::Paper(1),
                !t.id.is_panel()
            );
            add!(
                "このTABの本文背景…",
                "Tab Background…",
                Command::Paper(0),
                main
            );
            add!(
                "このTAB見出しの色…",
                "Tab Header Color…",
                Command::Paper(2),
                !t.id.is_panel() && matches!(t.spot, TerminalSpot::Front)
            );
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
                    ),
                    true
                );
            }
        }
        4 => {
            let side = Popup::new()?;
            for (ja, en, n) in [
                ("Workspace", "Workspace", 4),
                ("Explorer", "Explorer", 0),
                ("検索", "Search", 1),
                ("履歴", "History", 2),
                ("Outline", "Outline", 3),
            ] {
                row(
                    &side,
                    &mut commands,
                    ja,
                    en,
                    Command::Sidebar(n),
                    true,
                    window.get_tree_open() && window.get_left_tab() == n,
                )?;
            }
            root.child(pick("サイドバー", "Sidebar"), side)?;
            add!(
                "Source・Preview切替",
                "Toggle Source / Preview",
                Command::Preview,
                main && !t.id.reads_only(window)
            );
            add!(
                "Viewer・ReadOnly開始／終了",
                "Start / Exit Viewer or ReadOnly",
                Command::Viewer,
                text && !t.document.read_only()
            );
            for (setting, ja, en) in [
                (12, "行番号", "Line Numbers"),
                (54, "空白・改行記号", "Spaces and Line Breaks"),
            ] {
                let shared = Setting::from_index(setting)
                    .is_some_and(|s| shared_sheet(window, 1, s.page()) == 0);
                let suffix = if shared {
                    pick("縦横共通", "horizontal and vertical, shared")
                } else if t.id.vertical(window) {
                    pick("縦書き共通", "vertical, shared")
                } else {
                    pick("横書き共通", "horizontal, shared")
                };
                let name = format!("{} ({suffix})", pick(ja, en));
                let on = Setting::from_index(setting)
                    .is_some_and(|s| s.read(window, usize::from(t.id.vertical(window))) != 0);
                row(
                    &root,
                    &mut commands,
                    &name,
                    &name,
                    Command::Display(setting),
                    main,
                    on,
                )?;
            }
            root.sep()?;
            add!(
                "右へ分割",
                "Split Right",
                Command::Split(true),
                PaneId::count(window) < MAX_PANES
            );
            add!(
                "下へ分割",
                "Split Down",
                Command::Split(false),
                PaneId::count(window) < MAX_PANES
            );
            add!(
                "他のPaneを閉じる",
                "Close Other Panes",
                Command::OtherPanes,
                PaneId::count(window) > 1
            );
            for (n, (ja, en)) in [
                ("左と入替", "Swap Left"),
                ("右と入替", "Swap Right"),
                ("上と入替", "Swap Up"),
                ("下と入替", "Swap Down"),
            ]
            .iter()
            .enumerate()
            {
                row(
                    &root,
                    &mut commands,
                    ja,
                    en,
                    Command::Swap(n as i32),
                    neighbours[n],
                    false,
                )?;
            }
            let go = Popup::new()?;
            row(
                &go,
                &mut commands,
                "次のTAB",
                "Next Tab",
                Command::Step(false),
                !t.id.is_panel() && tab_count > 1,
                false,
            )?;
            row(
                &go,
                &mut commands,
                "前のTAB",
                "Previous Tab",
                Command::Step(true),
                !t.id.is_panel() && tab_count > 1,
                false,
            )?;
            for (n, (ja, en)) in [
                ("左のPane", "Left Pane"),
                ("右のPane", "Right Pane"),
                ("上のPane", "Upper Pane"),
                ("下段／下のPane", "Panel Below / Lower Pane"),
            ]
            .iter()
            .enumerate()
            {
                row(
                    &go,
                    &mut commands,
                    ja,
                    en,
                    Command::Focus(n as i32),
                    neighbours[n] || (n == 3 && parent_screen.below_kind != 0),
                    false,
                )?;
            }
            row(
                &go,
                &mut commands,
                "戻る",
                "Back",
                Command::Navigate(false),
                text && can_back,
                false,
            )?;
            row(
                &go,
                &mut commands,
                "進む",
                "Forward",
                Command::Navigate(true),
                text && can_forward,
                false,
            )?;
            root.child(pick("移動", "Go To"), go)?;
            add!(
                "下段を表示・閉じる",
                "Show / Hide Panel",
                Command::Below,
                !screen.settings
            );
            if terminal {
                add!(
                    "最新の出力へ",
                    "Latest Terminal Output",
                    Command::Terminal(4),
                    true
                );
            }
            if t.id.is_panel() || matches!(t.spot, TerminalSpot::Below) {
                add!("上の領域へ", "Focus Above", Command::Above, true);
            }
            add!("Quick Draft…", "Quick Draft…", Command::Draft, true);
            root.sep()?;
            add!(
                "拡大",
                "Zoom In",
                Command::Zoom(1),
                main && t.id.zoom(window) < ZOOM_MAX
            );
            add!(
                "縮小",
                "Zoom Out",
                Command::Zoom(-1),
                main && t.id.zoom(window) > ZOOM_MIN
            );
            add!("既定倍率に戻す", "Reset Zoom", Command::ZoomReset, main);
            let zoom = Popup::new()?;
            for percent in (ZOOM_MIN..=ZOOM_MAX).step_by(ZOOM_STEP as usize) {
                let name = format!("{percent}%");
                row(
                    &zoom,
                    &mut commands,
                    &name,
                    &name,
                    Command::ZoomSet(percent),
                    main,
                    t.id.zoom(window) == percent,
                )?;
            }
            root.child(pick("倍率", "Zoom"), zoom)?;
        }
        5 => {
            add!(
                "保存版と比較",
                "Compare with Saved",
                Command::Compare(0),
                main && path
            );
            add!(
                "Gitの最終Commitと比較",
                "Compare with Git Last Commit",
                Command::Compare(1),
                main && path
            );
            add!(
                "別ファイルと比較…",
                "Compare with Another File…",
                Command::Compare(2),
                main
            );
            if terminal {
                root.sep()?;
                add!("出力を保存…", "Save Output…", Command::Terminal(7), true);
                if matches!(t.spot, TerminalSpot::Front) {
                    add!(
                        "ファイルへログ開始…",
                        "Capture Log to File…",
                        Command::Panel(10),
                        running && !screen.terminal_file_logging
                    );
                    add!(
                        "ファイルログ停止",
                        "Stop File Log",
                        Command::Panel(14),
                        screen.terminal_file_logging
                    );
                    add!(
                        "新しいEditor Panelへログ開始",
                        "Capture Log to New Editor Panel",
                        Command::Panel(7),
                        running && !screen.terminal_capturing
                    );
                    add!(
                        "Panelログ停止",
                        "Stop Panel Log",
                        Command::Panel(13),
                        screen.terminal_capturing
                    );
                    add!(
                        "選択をEditor Panelへ送る",
                        "Selection to Editor Panel",
                        Command::Panel(9),
                        selected
                    );
                    add!(
                        "履歴を新しいEditor Panelへ送る",
                        "History to New Editor Panel",
                        Command::Panel(8),
                        true
                    );
                }
                let shells = Popup::new()?;
                for (i, shell) in configured_shells(window).iter().enumerate() {
                    row(
                        &shells,
                        &mut commands,
                        &shell.name,
                        &shell.name,
                        Command::Shell(i as i32),
                        true,
                        false,
                    )?;
                }
                root.child(pick("シェル切替", "Switch Shell"), shells)?;
            }
            if t.id.is_panel() {
                add!(
                    "このPanelの取り込みを停止",
                    "Stop This Panel Log",
                    Command::Panel(5),
                    terminal_panels::current(live, t.parent)
                        .is_some_and(|p| p.borrow().capture.is_some())
                );
            }
        }
        6 => {
            root.row(
                pick("使い方…（未実装）", "User Guide… (not implemented)"),
                0,
                false,
                false,
            )?;
            root.row(
                pick(
                    "RFN Editについて…（未実装）",
                    "About RFN Edit… (not implemented)",
                ),
                0,
                false,
                false,
            )?;
        }
        _ => return Ok(None),
    }
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
    let navigation = Navigation::begin(hwnd, root.handle, group, scale)?;
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
}
thread_local! { static NAVIGATION: RefCell<Option<Rc<Navigation>>> = const { RefCell::new(None) }; }
impl Navigation {
    fn begin(
        hwnd: windows::Win32::Foundation::HWND,
        root: HMENU,
        group: i32,
        scale: f32,
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
                if message.wParam.0 == 0x25 {
                    next = Some((nav.group + 6) % 7);
                }
                if message.wParam.0 == 0x27 && !nav.submenu.get() {
                    next = Some((nav.group + 1) % 7);
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
                        if group != nav.group {
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
        Command::Insert(n) => insert_in_pane(window, live, t.id, n),
        Command::Direction(vertical) => {
            if t.id.vertical(window) != vertical {
                window.invoke_pane_direction_toggled(p)
            }
        }
        Command::Appearance(scope) => window.invoke_appearance_requested(parent, scope),
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
        Command::Navigate(forward) => window.invoke_pane_navigate(p, forward),
        Command::Draft => window.invoke_quick_draft_requested(),
        Command::Zoom(n) => window.invoke_pane_zoom(p, n),
        Command::ZoomReset => window.invoke_pane_zoom_reset(p),
        Command::ZoomSet(percent) => window.invoke_pane_zoom_set(p, percent),
        Command::Compare(0) => window.invoke_compare_saved_requested(),
        Command::Compare(1) => window.invoke_compare_head_requested(),
        Command::Compare(_) => window.invoke_compare_files_requested(),
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
        _ => return String::new(),
    };
    shortcuts::label(window, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ui_tests::Harness;

    /// 末端の項目を、**押せるかどうかと番号つきで**並べる（RFN01-38）。
    fn leaves(menu: HMENU) -> Vec<(u32, bool)> {
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
                found.push((item.wID, item.fState.0 & MFS_DISABLED.0 == 0));
            } else {
                found.extend(leaves(item.hSubMenu));
            }
        }
        found
    }

    /// RFN01-38の単位1: **リンクとルビは押せる。**番号は画面の並びと同じで、
    /// `Command::Insert`の言い方と1対1になる。
    #[test]
    fn the_insert_menu_offers_the_links_and_ruby() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let menu = Popup::new().unwrap();
        let mut commands = Vec::new();
        insert_commands(&h.window, &menu, &mut commands, false, true).unwrap();

        assert_eq!(commands.len(), 4);
        for (index, command) in commands.iter().enumerate() {
            assert!(
                matches!(command, Command::Insert(n) if *n == index as i32),
                "並びがそのまま番号になる"
            );
        }
        assert!(
            leaves(menu.handle).iter().all(|(_, pickable)| *pickable),
            "I01〜I04は押せる"
        );

        // **押せない条件では、同じ行が無効になる**（Viewerや矩形選択のとき）。
        let frozen = Popup::new().unwrap();
        insert_commands(&h.window, &frozen, &mut Vec::new(), false, false).unwrap();
        assert!(leaves(frozen.handle).iter().all(|(_, pickable)| !*pickable));
    }

    /// RFN01-38: **まだ実行しない項目は、番号を持たず無効のままである。**「メニュー
    /// に無い」のではなく「まだ実行しない」ことを、行が言っている（要件 6.7）。
    #[test]
    fn the_rest_of_the_insert_menu_is_still_planned() {
        let menu = Popup::new().unwrap();
        insert_pending(&menu).unwrap();

        let rows = leaves(menu.handle);
        assert_eq!(rows.len(), 36);
        assert!(
            rows.iter().all(|(id, pickable)| *id == 0 && !*pickable),
            "未実装の行は押せない"
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
}
