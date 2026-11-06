//! RFN01-114（書き手と合意 2026-11-05）: **アクションの表。**
//!
//! 操作ごとに、変わらないID・Title・Description（日本語・英語）・既定のキー・分類・操作
//! （[`Command`]）を1行で持つ。タイトルバーのメニューの行の名前、コマンドパレット、設定の
//! Keys、キーの割り当ての保存（IDで書く）が、みなこの表を読む。仕組みは
//! `design/アクションの仕組み.md`。
//!
//! **IDは保存に使うので変えない。**名前や並びは変えてよい（割り当てはIDで残る）。
use crate::menu_commands::{Command, InsertShape};
use std::sync::OnceLock;

/// Keysの面の分類。並びは画面の並び。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    File,
    Edit,
    Insert,
    Layout,
    View,
    Run,
    Help,
    /// 本文の操作（メニューに無い、Emacs風の移動など）。
    Text,
    Terminal,
    LeftPane,
    QuickDraft,
    Surface,
    Git,
}
impl Category {
    pub(crate) const ALL: [Category; 13] = [
        Category::File,
        Category::Edit,
        Category::Insert,
        Category::Layout,
        Category::View,
        Category::Run,
        Category::Help,
        Category::Text,
        Category::Terminal,
        Category::LeftPane,
        Category::QuickDraft,
        Category::Surface,
        Category::Git,
    ];
    pub(crate) fn index(self) -> i32 {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0) as i32
    }
    pub(crate) fn title(self) -> (&'static str, &'static str) {
        match self {
            Category::File => ("ファイル", "File"),
            Category::Edit => ("編集", "Edit"),
            Category::Insert => ("挿入", "Insert"),
            Category::Layout => ("レイアウト", "Layout"),
            Category::View => ("表示", "View"),
            Category::Run => ("実行", "Run"),
            Category::Help => ("ヘルプ", "Help"),
            Category::Text => ("本文の操作", "Text Actions"),
            Category::Terminal => ("Terminal", "Terminal"),
            Category::LeftPane => ("左ペイン", "Left Pane"),
            Category::QuickDraft => ("Quick Draft", "Quick Draft"),
            Category::Surface => ("画面内の操作", "Panel Actions"),
            Category::Git => ("Git", "Git"),
        }
    }
    pub(crate) fn note(self) -> (&'static str, &'static str) {
        match self {
            Category::File => (
                "メニューの「ファイル」と同じ操作です。",
                "The same actions as the File menu.",
            ),
            Category::Edit => (
                "メニューの「編集」と同じ操作です。コピー・切り取り・貼り付け・全選択のキーは固定です。",
                "The same actions as the Edit menu. Copy, Cut, Paste and Select All keys are fixed.",
            ),
            Category::Insert => (
                "メニューの「挿入」と同じ操作です。選んだ字が要る形は、選んでいないときは使えません。",
                "The same actions as the Insert menu. A form that needs a picked word is unavailable when nothing is picked.",
            ),
            Category::Layout => (
                "メニューの「レイアウト」と同じ操作です。",
                "The same actions as the Layout menu.",
            ),
            Category::View => (
                "メニューの「表示」と同じ操作です。拡大・縮小・既定倍率に戻すのキーは固定です。",
                "The same actions as the View menu. Zoom In, Zoom Out and Reset Zoom keys are fixed.",
            ),
            Category::Run => (
                "メニューの「実行」と同じ操作です。スペルミスへ移るのは、スペルチェック中だけです。",
                "The same actions as the Run menu. Moving to a spelling error works only during a spell check.",
            ),
            Category::Help => (
                "メニューの「ヘルプ」と同じ操作です。",
                "The same actions as the Help menu.",
            ),
            Category::Text => (
                "メニューに無い、本文の操作です。本文にフォーカスがあり、IME変換中でないときに有効です。",
                "Text actions not in the menus. Active when the text has focus and no IME conversion is in progress.",
            ),
            Category::Terminal => (
                "Terminalの操作です。Terminalの中のキーは固定で、ほかのキーは実行中のシェル・プログラムへ渡します。",
                "Terminal actions. Keys inside a Terminal are fixed; other keys go to the running shell or program.",
            ),
            Category::LeftPane => (
                "左ペイン（Explorer・検索・Outlineなど）の操作です。",
                "Actions of the left pane (Explorer, Search, Outline and so on).",
            ),
            Category::Surface => (
                "対象の画面を表示しているときに使えます。",
                "Available when the relevant panel is showing.",
            ),
            Category::Git => (
                "Gitリポジトリの操作や履歴の確認に使います。",
                "Actions for Git repositories and their history.",
            ),
            Category::QuickDraft => (
                "Quick Draftの中で効く操作です。",
                "Actions that work inside Quick Draft.",
            ),
        }
    }
}

/// どこでキーを受けるか。**同じキーでも場面が違えば別の操作**にできる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// 窓（本文・Terminalの外・左ペイン）。メニューの操作はみなここ。
    Main,
    /// Quick Draftの窓の中。
    QuickDraft,
    /// Explorer・Workspaceのファイルツリーにフォーカスがあるとき。
    Tree,
}

/// アクションの1行。
pub(crate) struct ActionDef {
    /// 変わらないID（保存に使う）。
    pub id: String,
    pub title: (String, String),
    pub description: (String, String),
    /// 既定のキー。空は「既定では割り当てない」。
    pub key: &'static str,
    /// キーが固定（Ctrl+Cなど）。Keysには変えられない行として出し、割り当ての対象にしない。
    pub fixed: bool,
    pub category: Category,
    pub scope: Scope,
    /// 走らせる操作。Quick Draftの中の操作は`None`（Quick Draftの窓が自分で走らせる）。
    pub command: Option<Command>,
}

fn def(
    id: &str,
    title: (&str, &str),
    description: (&str, &str),
    key: &'static str,
    category: Category,
    command: Command,
) -> ActionDef {
    ActionDef {
        id: id.to_owned(),
        title: (title.0.to_owned(), title.1.to_owned()),
        description: (description.0.to_owned(), description.1.to_owned()),
        key,
        fixed: false,
        category,
        scope: Scope::Main,
        command: Some(command),
    }
}

/// キーが固定の行（Ctrl+C・Ctrl+=など）。
fn fixed(
    id: &str,
    title: (&str, &str),
    description: (&str, &str),
    key: &'static str,
    category: Category,
    command: Command,
) -> ActionDef {
    ActionDef {
        fixed: true,
        ..def(id, title, description, key, category, command)
    }
}

/// 表の全部。**一度だけ組む**（挿入の形は挿入メニューの表から組む）。
pub(crate) fn all() -> &'static [ActionDef] {
    static ACTIONS: OnceLock<Vec<ActionDef>> = OnceLock::new();
    ACTIONS.get_or_init(build)
}

/// 表の中の位置（画面の行や割り当ての並びに使う。保存には使わない）。
pub(crate) fn index_of(id: &str) -> Option<usize> {
    all().iter().position(|action| action.id == id)
}

/// その操作の行（同じ操作を名指す行があれば最初の1つ）。
pub(crate) fn of_command(command: &Command) -> Option<&'static ActionDef> {
    all()
        .iter()
        .find(|action| action.command.as_ref() == Some(command))
}

fn build() -> Vec<ActionDef> {
    use Category::*;
    use Command as C;
    let mut v = vec![
        // ファイル
        def(
            "file.new",
            ("新規文書", "New File"),
            (
                "空の無題の文書を新しいTABで開きます。",
                "Opens an empty untitled document in a new tab.",
            ),
            "Ctrl+N",
            File,
            C::NewFile,
        ),
        def(
            "file.new-from-template",
            ("テンプレートから新規…", "New from Template…"),
            (
                "Templates Folderのテンプレートを選び、その中身で無題の文書を開きます。",
                "Picks a template from the Templates Folder and opens an untitled document with its contents.",
            ),
            "",
            File,
            C::TemplatePicker,
        ),
        def(
            "file.new-terminal",
            ("新しいTerminal", "New Terminal"),
            (
                "既定のシェルで新しいTerminalのTABを開きます。",
                "Opens a new Terminal tab with the default shell.",
            ),
            "",
            File,
            C::NewTerminal(-1),
        ),
        def(
            "file.new-terminal-pick",
            (
                "新しいTerminal（シェルを選ぶ）…",
                "New Terminal (Choose Shell)…",
            ),
            (
                "登録したシェルを選んで、新しいTerminalのTABを開きます。",
                "Chooses one of the registered shells and opens a new Terminal tab with it.",
            ),
            "",
            File,
            C::ShellPicker,
        ),
        def(
            "file.terminal-in-folder",
            (
                "文書のフォルダでTerminal起動",
                "Terminal in Document Folder",
            ),
            (
                "いまの文書があるフォルダを開始フォルダにして、Terminalを開きます。",
                "Opens a Terminal that starts in the folder of the current document.",
            ),
            "Ctrl+Shift+C",
            File,
            C::FolderTerminal,
        ),
        def(
            "file.open",
            ("ファイルを開く…", "Open File…"),
            ("ファイルを選んで開きます。", "Chooses a file and opens it."),
            "Ctrl+O",
            File,
            C::Open,
        ),
        def(
            "file.open-folder",
            ("フォルダを開く…", "Open Folder…"),
            (
                "フォルダを選び、Explorerの作業フォルダにします。",
                "Chooses a folder and makes it the Explorer's work folder.",
            ),
            "",
            File,
            C::Folder,
        ),
        def(
            "file.recent-folder",
            ("最近のフォルダを開く…", "Open Recent Folder…"),
            (
                "最近開いたフォルダを選び、作業フォルダにします。",
                "Chooses a recently opened folder and makes it the work folder.",
            ),
            "",
            File,
            C::RecentPicker,
        ),
        def(
            "file.workspaces",
            ("Workspaces…", "Workspaces…"),
            (
                "Workspaceの管理画面を開きます。",
                "Opens the Workspace manager.",
            ),
            "",
            File,
            C::Workspaces,
        ),
        def(
            "file.git-repository",
            ("Git Repository…", "Git Repository…"),
            (
                "Git Changesで選んでいるリポジトリの、全ブランチのグラフを開きます。",
                "Opens the graph of all branches of the repository chosen in Git Changes.",
            ),
            "",
            Git,
            C::GitRepository,
        ),
        def(
            "file.save",
            ("保存", "Save"),
            (
                "いまの文書を上書き保存します。無題なら保存先を訊きます。",
                "Saves the current document. Asks where to save an untitled one.",
            ),
            "Ctrl+S",
            File,
            C::Save(false),
        ),
        def(
            "file.save-as",
            ("名前を付けて保存…", "Save As…"),
            (
                "いまの文書を、名前と場所を指定して保存します。",
                "Saves the current document under a name and place you choose.",
            ),
            "Ctrl+Shift+S",
            File,
            C::Save(true),
        ),
        def(
            "file.save-all",
            (
                "すべて保存（Editor Panelを除く）",
                "Save All (except Editor Panels)",
            ),
            (
                "編集したまま保存していない文書を、すべて保存します。",
                "Saves every document with unsaved edits.",
            ),
            "",
            File,
            C::SaveAll,
        ),
        def(
            "file.close-tab",
            ("TABを閉じる", "Close Tab"),
            ("いまのTABを閉じます。", "Closes the current tab."),
            "Ctrl+W",
            File,
            C::Close,
        ),
        def(
            "file.close-others",
            ("他のTABを閉じる", "Close Other Tabs"),
            (
                "このPaneの、いまのTAB以外を閉じます。",
                "Closes every tab in this pane except the current one.",
            ),
            "",
            File,
            C::CloseOthers(true),
        ),
        def(
            "file.close-all",
            ("すべてのTABを閉じる", "Close All Tabs"),
            (
                "このPaneのTABをすべて閉じます。",
                "Closes every tab in this pane.",
            ),
            "",
            File,
            C::CloseOthers(false),
        ),
        def(
            "file.close-clean",
            ("未編集のTABを閉じる", "Close Unedited Tabs"),
            (
                "このPaneの、編集していないTABを閉じます。",
                "Closes the tabs in this pane that have no unsaved edits.",
            ),
            "",
            File,
            C::CloseClean,
        ),
        def(
            "file.reopen",
            ("閉じたTABを開き直す", "Reopen Closed Tab"),
            (
                "最後に閉じたTABを開き直します。",
                "Reopens the tab closed last.",
            ),
            "Ctrl+Shift+T",
            File,
            C::Reopen,
        ),
        def(
            "file.rename",
            ("名前を変更…", "Rename…"),
            (
                "いまの文書のファイル名を変えます。",
                "Renames the file of the current document.",
            ),
            "",
            File,
            C::Rename,
        ),
        def(
            "file.duplicate",
            ("別TABで開く", "Open in Another Tab"),
            (
                "いまの文書を、もう1つのTABでも開きます。",
                "Opens the current document in one more tab.",
            ),
            "",
            File,
            C::Duplicate,
        ),
        def(
            "file.copy-path",
            ("パスをコピー", "Copy Path"),
            (
                "いまの文書のファイルの場所をクリップボードへ写します。",
                "Copies the path of the current document's file to the clipboard.",
            ),
            "",
            File,
            C::CopyPath,
        ),
        def(
            "file.reveal",
            ("Explorerで表示", "Reveal in Explorer"),
            (
                "いまの文書のファイルを、WindowsのExplorerで見せます。",
                "Shows the current document's file in Windows Explorer.",
            ),
            "",
            File,
            C::Reveal,
        ),
        def(
            "file.reopen-utf8",
            (
                "文字コードを指定して開き直す：UTF-8",
                "Reopen with Encoding: UTF-8",
            ),
            (
                "いまの文書を、UTF-8として読み直します。",
                "Reads the current document again as UTF-8.",
            ),
            "",
            File,
            C::Encoding(0),
        ),
        def(
            "file.reopen-utf16le",
            (
                "文字コードを指定して開き直す：UTF-16 LE",
                "Reopen with Encoding: UTF-16 LE",
            ),
            (
                "いまの文書を、UTF-16 LEとして読み直します。",
                "Reads the current document again as UTF-16 LE.",
            ),
            "",
            File,
            C::Encoding(1),
        ),
        def(
            "file.reopen-utf16be",
            (
                "文字コードを指定して開き直す：UTF-16 BE",
                "Reopen with Encoding: UTF-16 BE",
            ),
            (
                "いまの文書を、UTF-16 BEとして読み直します。",
                "Reads the current document again as UTF-16 BE.",
            ),
            "",
            File,
            C::Encoding(2),
        ),
        def(
            "file.reopen-cp932",
            (
                "文字コードを指定して開き直す：CP932",
                "Reopen with Encoding: CP932",
            ),
            (
                "いまの文書を、CP932（Shift_JIS）として読み直します。",
                "Reads the current document again as CP932 (Shift_JIS).",
            ),
            "",
            File,
            C::Encoding(3),
        ),
        def(
            "file.backup-history",
            ("バックアップの履歴…", "Backup History…"),
            (
                "いまの文書のバックアップを並べ、選んだものと比べます。",
                "Lists the backups of the current document and compares the one you choose.",
            ),
            "",
            File,
            C::Compare(3),
        ),
        def(
            "file.git-history",
            ("Gitの履歴…", "Git History…"),
            (
                "いまの文書が入っているCommitを並べ、選んだものと比べます。",
                "Lists the commits that contain the current document and compares the one you choose.",
            ),
            "",
            Git,
            C::Compare(4),
        ),
        def(
            "file.print",
            ("印刷…", "Print…"),
            (
                "いまの文書を、紙の形で見て印刷します（印刷プレビュー）。",
                "Shows the current document on paper and prints it (print preview).",
            ),
            "",
            File,
            C::Print,
        ),
        def(
            "file.exit",
            ("終了", "Exit"),
            ("RFN Editを終えます。", "Quits RFN Edit."),
            "",
            File,
            C::Exit,
        ),
        // 編集
        def(
            "edit.undo",
            ("元に戻す", "Undo"),
            ("直前の編集を取り消します。", "Undoes the last edit."),
            "Ctrl+Z",
            Edit,
            C::Undo(false),
        ),
        def(
            "edit.redo",
            ("やり直し", "Redo"),
            (
                "取り消した編集をやり直します。",
                "Redoes the edit you undid.",
            ),
            "Ctrl+Y",
            Edit,
            C::Undo(true),
        ),
        fixed(
            "edit.cut",
            ("切り取り", "Cut"),
            (
                "選んだ範囲を切り取り、クリップボードへ入れます。",
                "Cuts the selection to the clipboard.",
            ),
            "Ctrl+X",
            Edit,
            C::Copy(true),
        ),
        fixed(
            "edit.copy",
            ("コピー", "Copy"),
            (
                "選んだ範囲をクリップボードへ写します。",
                "Copies the selection to the clipboard.",
            ),
            "Ctrl+C",
            Edit,
            C::Copy(false),
        ),
        fixed(
            "edit.paste",
            ("貼り付け", "Paste"),
            (
                "クリップボードの中身を貼り付けます。",
                "Pastes what is on the clipboard.",
            ),
            "Ctrl+V",
            Edit,
            C::Paste,
        ),
        fixed(
            "edit.select-all",
            ("全選択", "Select All"),
            ("本文をすべて選びます。", "Selects all the text."),
            "Ctrl+A",
            Edit,
            C::SelectAll,
        ),
        def(
            "edit.copy-body",
            ("本文だけをコピー", "Copy Text Only"),
            (
                "選んだ範囲を、記法の記号を外した本文だけにして写します。",
                "Copies the selection as plain text, without markup.",
            ),
            "",
            Edit,
            C::CopyBody,
        ),
        def(
            "edit.find",
            ("検索…", "Find…"),
            (
                "いまの文書の中を探します（Terminalでは出力の中、設定のTABでは設定の項目）。",
                "Searches the current document (the output in a Terminal, the settings in the settings tab).",
            ),
            "Ctrl+F",
            Edit,
            C::Find(false),
        ),
        def(
            "edit.replace",
            ("置換…", "Replace…"),
            (
                "いまの文書の中を探して置き換えます。",
                "Finds and replaces in the current document.",
            ),
            "Ctrl+H",
            Edit,
            C::Find(true),
        ),
        def(
            "edit.goto",
            ("行へ移動…", "Go to Line…"),
            (
                "行の番号を入れて、その行へ移ります。",
                "Moves to the line whose number you enter.",
            ),
            "Ctrl+G",
            Edit,
            C::Goto(true),
        ),
        def(
            "edit.find-in-folder",
            ("フォルダ内を検索…", "Find in Folder…"),
            (
                "作業フォルダのファイルを検索します。ファイルツリーが表示されていれば、その上に検索欄を開きます。",
                "Searches the files of the work folder, over the tree when it is showing.",
            ),
            "Ctrl+Shift+F",
            Edit,
            C::FolderFind,
        ),
        def(
            "edit.replace-in-folder",
            ("フォルダ内を置換…", "Replace in Folder…"),
            (
                "作業フォルダのファイルの中を探して置き換えます。",
                "Finds and replaces in the files of the work folder.",
            ),
            "Ctrl+Shift+H",
            Edit,
            C::FolderReplace,
        ),
        def(
            "edit.delete-line",
            ("行を削除", "Delete Line"),
            (
                "キャレットのある行（選んでいれば、その行すべて）を消します。",
                "Deletes the line with the caret (or every selected line).",
            ),
            "Ctrl+Shift+K",
            Edit,
            C::Line(4),
        ),
        def(
            "edit.move-line-up",
            ("行を上へ移動", "Move Line Up"),
            (
                "いまの行を1行上へ動かします。",
                "Moves the current line up by one.",
            ),
            "Alt+Up",
            Edit,
            C::Line(0),
        ),
        def(
            "edit.move-line-down",
            ("行を下へ移動", "Move Line Down"),
            (
                "いまの行を1行下へ動かします。",
                "Moves the current line down by one.",
            ),
            "Alt+Down",
            Edit,
            C::Line(1),
        ),
        def(
            "edit.copy-line-up",
            ("行を上へ複製", "Duplicate Line Up"),
            (
                "いまの行を写して、上に置きます。",
                "Duplicates the current line above itself.",
            ),
            "Alt+Shift+Up",
            Edit,
            C::Line(2),
        ),
        def(
            "edit.copy-line-down",
            ("行を下へ複製", "Duplicate Line Down"),
            (
                "いまの行を写して、下に置きます。",
                "Duplicates the current line below itself.",
            ),
            "Alt+Shift+Down",
            Edit,
            C::Line(3),
        ),
        def(
            "edit.mark",
            ("選択を開始・解除", "Start / Cancel Selection"),
            (
                "キャレットを動かすと選ぶ状態を始めます。もう一度で解きます（Emacs風）。",
                "Starts selecting as the caret moves; again to stop (Emacs style).",
            ),
            "Ctrl+Space",
            Edit,
            C::Mark(false),
        ),
        def(
            "edit.mark-rect",
            ("矩形選択を開始・解除", "Start / Cancel Rectangle Selection"),
            (
                "キャレットを動かすと矩形に選ぶ状態を始めます。もう一度で解きます。",
                "Starts a rectangular selection as the caret moves; again to stop.",
            ),
            "Ctrl+Shift+Space",
            Edit,
            C::Mark(true),
        ),
        def(
            "edit.kill-line",
            ("行末まで切り取り", "Cut to End of Line"),
            (
                "キャレットから行の終わりまでを切り取り、Kill Ringへ入れます。",
                "Cuts from the caret to the end of the line into the Kill Ring.",
            ),
            "Ctrl+K",
            Edit,
            C::Kill(0),
        ),
        def(
            "edit.kill-copy",
            ("Kill Ringへコピー", "Copy to Kill Ring"),
            (
                "選んだ範囲を、Kill Ringへ写します。",
                "Copies the selection into the Kill Ring.",
            ),
            "Alt+W",
            Edit,
            C::Kill(1),
        ),
        def(
            "edit.kill-cut",
            ("Kill Ringへ切り取り", "Cut to Kill Ring"),
            (
                "選んだ範囲を切り取り、Kill Ringへ入れます。",
                "Cuts the selection into the Kill Ring.",
            ),
            "Alt+X",
            Edit,
            C::Kill(2),
        ),
        def(
            "edit.yank",
            ("最新のKillを貼り付け", "Paste Latest Kill"),
            (
                "Kill Ringのいちばん新しいものを貼り付けます。",
                "Pastes the newest entry of the Kill Ring.",
            ),
            "Alt+Y",
            Edit,
            C::Kill(3),
        ),
        def(
            "edit.yank-pop",
            ("前のKillへ置換", "Replace with Previous Kill"),
            (
                "貼り付けたばかりのものを、Kill Ringの1つ前のものに替えます。",
                "Replaces what was just pasted with the previous Kill Ring entry.",
            ),
            "Alt+Shift+Y",
            Edit,
            C::Kill(4),
        ),
        def(
            "edit.word-mode",
            ("単語チェックのモードを選ぶ…", "Choose Word Check Mode…"),
            (
                "いまの文書に使う単語チェックのモードを選びます。",
                "Chooses the word check mode for the current document.",
            ),
            "",
            Edit,
            C::WordModePicker,
        ),
        def(
            "edit.word-mode-none",
            ("単語チェック：なし", "Word Check: None"),
            (
                "いまの文書の単語チェックを止めます。",
                "Turns word check off for the current document.",
            ),
            "",
            Edit,
            C::Word(0),
        ),
        // 挿入（箇条書きと番号。挿入の形は後ろで足す）
        def(
            "insert.bullet-dash",
            ("箇条書き -", "Bullet List -"),
            (
                "いまの行（選んだ行）を「-」の箇条書きにします。もう一度で外します。",
                "Makes the current (or selected) lines a \"-\" bullet list; again to remove.",
            ),
            "",
            Insert,
            C::List(0, 0),
        ),
        def(
            "insert.bullet-star",
            ("箇条書き *", "Bullet List *"),
            (
                "いまの行（選んだ行）を「*」の箇条書きにします。もう一度で外します。",
                "Makes the current (or selected) lines a \"*\" bullet list; again to remove.",
            ),
            "",
            Insert,
            C::List(0, 1),
        ),
        def(
            "insert.bullet-plus",
            ("箇条書き +", "Bullet List +"),
            (
                "いまの行（選んだ行）を「+」の箇条書きにします。もう一度で外します。",
                "Makes the current (or selected) lines a \"+\" bullet list; again to remove.",
            ),
            "",
            Insert,
            C::List(0, 2),
        ),
        def(
            "insert.numbered",
            ("番号付きリスト", "Numbered List"),
            (
                "いまの行（選んだ行）を番号付きリストにします。",
                "Makes the current (or selected) lines a numbered list.",
            ),
            "",
            Insert,
            C::List(1, -1),
        ),
        def(
            "insert.renumber",
            ("番号を振り直す", "Renumber"),
            (
                "番号付きリストの番号を、上から振り直します。",
                "Renumbers a numbered list from the top.",
            ),
            "",
            Insert,
            C::List(2, -1),
        ),
        // レイアウト
        def(
            "layout.toggle-direction",
            ("縦書き・横書きを切り替え", "Toggle Vertical / Horizontal"),
            (
                "いまのTABを、縦書きと横書きで切り替えます。",
                "Switches the current tab between vertical and horizontal writing.",
            ),
            "Ctrl+R",
            Layout,
            C::DirectionToggle,
        ),
        def(
            "layout.horizontal",
            ("横書き", "Horizontal"),
            (
                "いまのTABを横書きにします。",
                "Writes the current tab horizontally.",
            ),
            "",
            Layout,
            C::Direction(false),
        ),
        def(
            "layout.vertical",
            ("縦書き", "Vertical"),
            (
                "いまのTABを縦書きにします。",
                "Writes the current tab vertically.",
            ),
            "",
            Layout,
            C::Direction(true),
        ),
        def(
            "layout.pane-background",
            ("このPaneの背景…", "Pane Background…"),
            (
                "このPaneの紙の色を選びます（TABごとの色は解除します）。",
                "Chooses the paper colour of this pane (clears per-tab colours).",
            ),
            "",
            Layout,
            C::Paper(1),
        ),
        def(
            "layout.tab-background",
            ("このTABの本文背景…", "Tab Background…"),
            (
                "このTABの紙の色を選びます。",
                "Chooses the paper colour of this tab.",
            ),
            "",
            Layout,
            C::Paper(0),
        ),
        def(
            "layout.tab-color",
            ("このTAB見出しの色…", "Tab Header Color…"),
            (
                "このTABの見出しの色を選びます。",
                "Chooses the colour of this tab's header.",
            ),
            "",
            Layout,
            C::Paper(2),
        ),
        // 表示
        def(
            "view.sidebar.workspace",
            ("サイドバー：Workspace", "Sidebar: Workspace"),
            (
                "左ペインにWorkspaceを出します。出ていれば閉じます。",
                "Shows the Workspace in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(4),
        ),
        def(
            "view.sidebar.explorer",
            ("サイドバー：Explorer", "Sidebar: Explorer"),
            (
                "左ペインにExplorerを出します。出ていれば閉じます。",
                "Shows the Explorer in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(0),
        ),
        def(
            "view.sidebar.search",
            ("サイドバー：検索", "Sidebar: Search"),
            (
                "左ペインに検索の面を出します。出ていれば閉じます。",
                "Shows the Search panel in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(1),
        ),
        def(
            "view.sidebar.history",
            ("サイドバー：履歴", "Sidebar: History"),
            (
                "左ペインに最近開いたファイルを出します。出ていれば閉じます。",
                "Shows recently opened files in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(2),
        ),
        def(
            "view.sidebar.outline",
            ("サイドバー：Outline", "Sidebar: Outline"),
            (
                "左ペインに見出しの一覧を出します。",
                "Shows the list of headings in the left pane.",
            ),
            "Ctrl+Shift+O",
            View,
            C::ShowLeftTab(3),
        ),
        def(
            "view.sidebar.bookmarks",
            ("サイドバー：ブックマーク", "Sidebar: Bookmarks"),
            (
                "左ペインにブックマークを出します。出ていれば閉じます。",
                "Shows the Bookmarks in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(5),
        ),
        def(
            "view.sidebar.tags",
            ("サイドバー：タグ", "Sidebar: Tags"),
            (
                "左ペインにタグの一覧を出します。出ていれば閉じます。",
                "Shows the Tags in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(6),
        ),
        def(
            "view.sidebar.git",
            ("サイドバー：Git Changes", "Sidebar: Git Changes"),
            (
                "左ペインにGitの変更を出します。",
                "Shows Git changes in the left pane.",
            ),
            "Ctrl+Shift+G",
            Git,
            C::ShowLeftTab(8),
        ),
        def(
            "view.sidebar.backups",
            ("サイドバー：バックアップ", "Sidebar: Backups"),
            (
                "左ペインにバックアップを出します。出ていれば閉じます。",
                "Shows the Backups in the left pane, or closes it if showing.",
            ),
            "",
            View,
            C::Sidebar(7),
        ),
        def(
            "view.toolbar",
            ("ツールバー", "Toolbar"),
            (
                "書式のツールバーを出す・しまう。",
                "Shows or hides the format toolbar.",
            ),
            "",
            View,
            C::Toolbar,
        ),
        def(
            "view.format-palette",
            ("書式パレット", "Format Palette"),
            (
                "書式パレットを出す・しまう。",
                "Shows or hides the format palette.",
            ),
            "",
            View,
            C::Palette,
        ),
        def(
            "view.toggle-preview",
            ("Source・Preview切替", "Toggle Source / Preview"),
            (
                "記法をそのまま見せる表示と、整えた表示を切り替えます。",
                "Switches between the markup as written and the formatted view.",
            ),
            "Ctrl+Shift+V",
            View,
            C::Preview,
        ),
        def(
            "view.toggle-viewer",
            (
                "Viewer・ReadOnly開始／終了",
                "Start / Exit Viewer or ReadOnly",
            ),
            (
                "書き換えない表示（Viewer、原文ならReadOnly）を始める・終えます。",
                "Starts or ends the read-only view (Viewer, or ReadOnly for the source).",
            ),
            "",
            View,
            C::Viewer,
        ),
        def(
            "view.line-numbers",
            ("行番号", "Line Numbers"),
            (
                "いまの向き（横書き・縦書き）の行番号を出す・しまう。",
                "Shows or hides line numbers for the current direction.",
            ),
            "",
            View,
            C::Display(12),
        ),
        def(
            "view.whitespace",
            ("空白・改行記号", "Spaces and Line Breaks"),
            (
                "いまの向きの空白・改行の記号を出す・しまう。",
                "Shows or hides space and line-break marks for the current direction.",
            ),
            "",
            View,
            C::Display(54),
        ),
        def(
            "view.split-right",
            ("右へ分割", "Split Right"),
            (
                "いまのPaneを左右に分けます。",
                "Splits the current pane into left and right.",
            ),
            "Ctrl+\\",
            View,
            C::Split(true),
        ),
        def(
            "view.split-down",
            ("下へ分割", "Split Down"),
            (
                "いまのPaneを上下に分けます。",
                "Splits the current pane into top and bottom.",
            ),
            "",
            View,
            C::Split(false),
        ),
        def(
            "view.close-other-panes",
            ("他のPaneを閉じる", "Close Other Panes"),
            (
                "いまのPaneだけにします。ほかのPaneのTABはこのPaneへ移ります。",
                "Leaves only the current pane; the other panes' tabs move here.",
            ),
            "",
            View,
            C::OtherPanes,
        ),
        def(
            "view.swap-left",
            ("左と入替", "Swap Left"),
            (
                "いまのPaneを、左のPaneと入れ替えます。",
                "Swaps the current pane with the one on its left.",
            ),
            "",
            View,
            C::Swap(0),
        ),
        def(
            "view.swap-right",
            ("右と入替", "Swap Right"),
            (
                "いまのPaneを、右のPaneと入れ替えます。",
                "Swaps the current pane with the one on its right.",
            ),
            "",
            View,
            C::Swap(1),
        ),
        def(
            "view.swap-up",
            ("上と入替", "Swap Up"),
            (
                "いまのPaneを、上のPaneと入れ替えます。",
                "Swaps the current pane with the one above.",
            ),
            "",
            View,
            C::Swap(2),
        ),
        def(
            "view.swap-down",
            ("下と入替", "Swap Down"),
            (
                "いまのPaneを、下のPaneと入れ替えます。",
                "Swaps the current pane with the one below.",
            ),
            "",
            View,
            C::Swap(3),
        ),
        def(
            "view.next-tab",
            ("次のTAB", "Next Tab"),
            (
                "このPaneの次のTABへ移ります。",
                "Moves to the next tab in this pane.",
            ),
            "Ctrl+Tab",
            View,
            C::Step(false),
        ),
        def(
            "view.previous-tab",
            ("前のTAB", "Previous Tab"),
            (
                "このPaneの前のTABへ移ります。",
                "Moves to the previous tab in this pane.",
            ),
            "Ctrl+Shift+Tab",
            View,
            C::Step(true),
        ),
        def(
            "view.focus-left",
            ("左のPaneへ", "To Left Pane"),
            ("左のPaneへ移ります。", "Moves to the pane on the left."),
            "Ctrl+Alt+Left",
            View,
            C::Focus(0),
        ),
        def(
            "view.focus-right",
            ("右のPaneへ", "To Right Pane"),
            ("右のPaneへ移ります。", "Moves to the pane on the right."),
            "Ctrl+Alt+Right",
            View,
            C::Focus(1),
        ),
        def(
            "view.focus-up",
            ("上のPaneへ", "To Upper Pane"),
            ("上のPaneへ移ります。", "Moves to the pane above."),
            "Ctrl+Alt+Up",
            View,
            C::Focus(2),
        ),
        def(
            "view.focus-down",
            ("下段または下のPaneへ", "To Strip Below or Lower Pane"),
            (
                "下段が出ていれば下段へ、無ければ下のPaneへ移ります。",
                "Moves to the strip below if it is showing, otherwise to the pane below.",
            ),
            "Ctrl+Alt+Down",
            View,
            C::Focus(3),
        ),
        def(
            "view.back",
            ("戻る", "Back"),
            (
                "このPaneで、前に見ていた所へ戻ります。",
                "Goes back to where you were before in this pane.",
            ),
            "Alt+Left",
            View,
            C::Navigate(false),
        ),
        def(
            "view.forward",
            ("進む", "Forward"),
            (
                "戻る前の所へ進みます。",
                "Goes forward again after going back.",
            ),
            "Alt+Right",
            View,
            C::Navigate(true),
        ),
        def(
            "view.below",
            ("下段を表示・閉じる", "Show / Hide Panel"),
            (
                "このPaneの下段（Editor Panel・Terminal Panel）を出す・閉じる。",
                "Shows or hides this pane's strip below (Editor Panel or Terminal Panel).",
            ),
            "Ctrl+`",
            View,
            C::Below,
        ),
        def(
            "view.close-strip-tab",
            ("下段のTABを閉じる", "Close Strip Tab"),
            (
                "下段の前に出ているTABを閉じます。",
                "Closes the tab in front in the strip below.",
            ),
            "",
            View,
            C::CloseStripTab,
        ),
        def(
            "view.quick-draft",
            ("Quick Draft…", "Quick Draft…"),
            (
                "思いついたことをすぐ書き留める、小さな窓を開きます。",
                "Opens a small window to jot something down at once.",
            ),
            "",
            View,
            C::Draft,
        ),
        fixed(
            "view.zoom-in",
            ("拡大", "Zoom In"),
            (
                "いまのTABの倍率を上げます。",
                "Raises the zoom of the current tab.",
            ),
            "Ctrl+=",
            View,
            C::Zoom(1),
        ),
        fixed(
            "view.zoom-out",
            ("縮小", "Zoom Out"),
            (
                "いまのTABの倍率を下げます。",
                "Lowers the zoom of the current tab.",
            ),
            "Ctrl+-",
            View,
            C::Zoom(-1),
        ),
        fixed(
            "view.zoom-reset",
            ("既定倍率に戻す", "Reset Zoom"),
            (
                "いまのTABの倍率を、既定の倍率に戻します。",
                "Puts the current tab's zoom back to the default.",
            ),
            "Ctrl+0",
            View,
            C::ZoomReset,
        ),
        def(
            "view.command-palette",
            ("コマンドパレット", "Command Palette"),
            (
                "操作を名前で探して実行します。",
                "Finds an action by name and runs it.",
            ),
            "Ctrl+Shift+P",
            View,
            C::CommandPalette(true),
        ),
        def(
            "view.go-to-file",
            ("ファイル名で開く", "Go to File"),
            (
                "ファイルを名前で探して開きます。",
                "Finds a file by name and opens it.",
            ),
            "Ctrl+P",
            View,
            C::CommandPalette(false),
        ),
        // 実行
        def(
            "run.compare-saved",
            ("保存版と比較", "Compare with Saved"),
            (
                "いまの文書を、保存してある内容と比べます。",
                "Compares the current document with what is saved.",
            ),
            "",
            Run,
            C::Compare(0),
        ),
        def(
            "run.compare-head",
            ("Gitの最終Commitと比較", "Compare with Git Last Commit"),
            (
                "いまの文書を、Gitの最後のCommitと比べます。",
                "Compares the current document with Git's last commit.",
            ),
            "",
            Git,
            C::Compare(1),
        ),
        def(
            "run.compare-file",
            ("別ファイルと比較…", "Compare with Another File…"),
            (
                "いまの文書を、選んだ別のファイルと比べます。",
                "Compares the current document with another file you choose.",
            ),
            "",
            Run,
            C::Compare(2),
        ),
        def(
            "run.spelling",
            (
                "スペルチェック（英語）開始・終了",
                "Start / Stop Spell Check (English)",
            ),
            (
                "いまの文書の英語の綴りを確かめる・確かめるのを終えます。",
                "Starts or stops checking English spelling in the current document.",
            ),
            "",
            Run,
            C::Spelling,
        ),
        def(
            "run.next-spelling",
            ("次のスペルミス", "Next Spelling Error"),
            (
                "次の綴りの誤りへ移ります（スペルチェック中）。",
                "Moves to the next spelling error (during a spell check).",
            ),
            "F8",
            Run,
            C::SpellingStep(false),
        ),
        def(
            "run.previous-spelling",
            ("前のスペルミス", "Previous Spelling Error"),
            (
                "前の綴りの誤りへ移ります（スペルチェック中）。",
                "Moves to the previous spelling error (during a spell check).",
            ),
            "Shift+F8",
            Run,
            C::SpellingStep(true),
        ),
        def(
            "run.read-aloud",
            ("読み上げ・停止", "Read Aloud / Stop"),
            (
                "キャレットの位置から読み上げます。読んでいれば止めます。",
                "Reads aloud from the caret, or stops if reading.",
            ),
            "",
            Run,
            C::ReadAloud,
        ),
        // ヘルプ
        def(
            "help.keys",
            ("キーの一覧…", "Keyboard Shortcuts…"),
            (
                "設定のKeysを開き、キーの割り当てを見せます。",
                "Opens Keys in the settings to show the key assignments.",
            ),
            "",
            Help,
            C::Help(0),
        ),
        def(
            "help.sample",
            ("サンプル文書を開く", "Open Sample Document"),
            (
                "紹介用のサンプル文書を、無題で開きます。",
                "Opens the sample document as an untitled one.",
            ),
            "",
            Help,
            C::Help(1),
        ),
        def(
            "help.about",
            ("RFN Editについて…", "About RFN Edit…"),
            (
                "名前・版・著作権を見せます。",
                "Shows the name, version and copyright.",
            ),
            "",
            Help,
            C::Help(2),
        ),
        // 本文の操作（メニューに無い）
        def(
            "text.delete-next",
            ("次の1文字を削除", "Delete Next Character"),
            (
                "キャレットの後ろの1文字を消します（Emacs風）。",
                "Deletes the character after the caret (Emacs style).",
            ),
            "Ctrl+D",
            Text,
            C::DeleteNext,
        ),
        def(
            "text.word-forward",
            ("1単語後へ", "Forward One Word"),
            (
                "キャレットを1単語後ろへ動かします（Emacs風）。",
                "Moves the caret forward one word (Emacs style).",
            ),
            "Alt+F",
            Text,
            C::WordMove(true, false),
        ),
        def(
            "text.word-back",
            ("1単語前へ", "Back One Word"),
            (
                "キャレットを1単語前へ動かします（Emacs風）。",
                "Moves the caret back one word (Emacs style).",
            ),
            "Alt+B",
            Text,
            C::WordMove(false, false),
        ),
        def(
            "text.select-word-forward",
            ("1単語後まで選択", "Select Forward One Word"),
            ("1単語後ろまで選びます。", "Selects forward one word."),
            "Alt+Shift+F",
            Text,
            C::WordMove(true, true),
        ),
        def(
            "text.select-word-back",
            ("1単語前まで選択", "Select Back One Word"),
            ("1単語前まで選びます。", "Selects back one word."),
            "Alt+Shift+B",
            Text,
            C::WordMove(false, true),
        ),
        def(
            "text.scroll-up",
            ("1行上へスクロール", "Scroll Line Up"),
            (
                "キャレットを動かさずに、本文を1行ぶん動かします（横書き）。",
                "Scrolls the text by one line without moving the caret (horizontal writing).",
            ),
            "Ctrl+Up",
            Text,
            C::ScrollLine(false),
        ),
        def(
            "text.scroll-down",
            ("1行下へスクロール", "Scroll Line Down"),
            (
                "キャレットを動かさずに、本文を1行ぶん動かします（横書き）。",
                "Scrolls the text by one line without moving the caret (horizontal writing).",
            ),
            "Ctrl+Down",
            Text,
            C::ScrollLine(true),
        ),
        def(
            "text.select-line",
            ("行を選択", "Select Line"),
            (
                "キャレットの行を選びます。もう一度で次の行まで広げます。",
                "Selects the caret's line; again to extend to the next line.",
            ),
            "Ctrl+L",
            Text,
            C::SelectLine,
        ),
        // Terminal
        def(
            "terminal.latest",
            ("最新の出力へ", "Latest Terminal Output"),
            (
                "Terminalの出力のいちばん下へ移ります。",
                "Moves to the bottom of the Terminal output.",
            ),
            "",
            Terminal,
            C::Terminal(4),
        ),
        def(
            "terminal.clear-screen",
            ("画面をクリア", "Clear Screen"),
            ("Terminalの画面を消します。", "Clears the Terminal screen."),
            "",
            Terminal,
            C::Terminal(5),
        ),
        def(
            "terminal.clear-history",
            ("履歴をクリア", "Clear History"),
            (
                "Terminalの出力の履歴を消します。",
                "Clears the Terminal output history.",
            ),
            "",
            Terminal,
            C::Terminal(6),
        ),
        def(
            "terminal.save-output",
            ("出力を保存…", "Save Output…"),
            (
                "Terminalの出力をファイルへ保存します。",
                "Saves the Terminal output to a file.",
            ),
            "",
            Terminal,
            C::Terminal(7),
        ),
        def(
            "terminal.copy-last-output",
            ("直前の出力をコピー", "Copy Last Output"),
            (
                "直前のコマンドの出力を写します。",
                "Copies the output of the last command.",
            ),
            "",
            Terminal,
            C::Terminal(22),
        ),
        def(
            "terminal.log-file",
            ("ファイルへログ開始…", "Capture Log to File…"),
            (
                "Terminalの出力をファイルへ書き続けます。",
                "Keeps writing the Terminal output to a file.",
            ),
            "",
            Terminal,
            C::Panel(10),
        ),
        def(
            "terminal.log-file-stop",
            ("ファイルログ停止", "Stop File Log"),
            ("ファイルへのログを止めます。", "Stops the log to a file."),
            "",
            Terminal,
            C::Panel(14),
        ),
        def(
            "terminal.log-panel",
            (
                "新しいEditor Panelへログ開始",
                "Capture Log to New Editor Panel",
            ),
            (
                "Terminalの出力を、新しいEditor Panelへ写し続けます。",
                "Keeps copying the Terminal output into a new Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(7),
        ),
        def(
            "terminal.log-panel-stop",
            ("Panelログ停止", "Stop Panel Log"),
            (
                "Editor Panelへのログを止めます。",
                "Stops the log to an Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(13),
        ),
        def(
            "terminal.selection-to-panel",
            ("選択をEditor Panelへ送る", "Selection to Editor Panel"),
            (
                "Terminalで選んだ字を、Editor Panelへ写します。",
                "Copies the text selected in the Terminal into the Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(9),
        ),
        def(
            "terminal.history-to-panel",
            (
                "履歴を新しいEditor Panelへ送る",
                "History to New Editor Panel",
            ),
            (
                "Terminalの出力の履歴を、新しいEditor Panelへ写します。",
                "Copies the Terminal output history into a new Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(8),
        ),
        def(
            "terminal.last-output-to-panel",
            ("直前の出力をEditor Panelへ", "Last Output to Editor Panel"),
            (
                "直前のコマンドの出力を、Editor Panelへ写します。",
                "Copies the last command's output into the Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(15),
        ),
        def(
            "terminal.stop-panel-capture",
            ("このPanelの取り込みを停止", "Stop This Panel Log"),
            (
                "このEditor Panelへの取り込みを止めます。",
                "Stops capturing into this Editor Panel.",
            ),
            "",
            Terminal,
            C::Panel(5),
        ),
        def(
            "terminal.appearance",
            ("このTABの外観…", "Appearance of This Tab…"),
            (
                "このTerminal・Editor Panelの色とフォントを選びます。",
                "Chooses the colours and font of this Terminal or Editor Panel.",
            ),
            "",
            Terminal,
            C::Appearance(-1),
        ),
        def(
            "terminal.focus-above",
            ("上の領域へ", "Focus Above"),
            (
                "下段から、上の本文へ戻ります。",
                "Goes back from the strip below to the text above.",
            ),
            "",
            Terminal,
            C::Above,
        ),
        // 左ペイン
        def(
            "left.toggle",
            ("左ペインを開く・閉じる", "Show or Hide Left Pane"),
            ("左ペインを開く・閉じる。", "Shows or hides the left pane."),
            "Ctrl+B",
            LeftPane,
            C::ToggleLeftPane,
        ),
        def(
            "left.explore",
            ("Explore Paneへ", "To Explore Pane"),
            (
                "キーボードを左ペインの絞り込み欄へ移します。",
                "Moves the keyboard to the left pane's filter field.",
            ),
            "Ctrl+Shift+E",
            LeftPane,
            C::ExplorePane,
        ),
        def(
            "left.settings",
            ("設定を開く", "Open Settings"),
            ("設定のTABを開きます。", "Opens the settings tab."),
            "Ctrl+,",
            LeftPane,
            C::OpenSettings,
        ),
    ];
    // 挿入の形（挿入メニューの表から）。
    for (shape, (ja, en)) in crate::menu_commands::insert_shapes() {
        let (title, description) = insert_words(shape, (ja, en));
        v.push(ActionDef {
            id: insert_id(shape),
            title,
            description,
            key: "",
            fixed: false,
            category: Insert,
            scope: Scope::Main,
            command: Some(C::Insert(shape)),
        });
    }
    // Quick Draftの中の操作（窓が自分で走らせる）。
    for (id, title, description, key) in [
        (
            "draft.copy-all",
            ("全文をコピー（Quick Draft内）", "Copy All (in Quick Draft)"),
            (
                "Quick Draftの全文を写します。",
                "Copies all the text in Quick Draft.",
            ),
            "Ctrl+Shift+C",
        ),
        (
            "draft.copy-and-close",
            (
                "コピーして閉じる（Quick Draft内）",
                "Copy and Close (in Quick Draft)",
            ),
            (
                "全文を写して、Quick Draftを閉じます。",
                "Copies all the text and closes Quick Draft.",
            ),
            "",
        ),
        (
            "draft.clear",
            ("クリア（Quick Draft内）", "Clear (in Quick Draft)"),
            (
                "Quick Draftの中身を消します（履歴には残ります）。",
                "Clears Quick Draft (it stays in the history).",
            ),
            "",
        ),
        (
            "draft.paste-to-tab",
            (
                "TABへ貼り付け（Quick Draft内）",
                "Paste to Tab (in Quick Draft)",
            ),
            (
                "Quick Draftの全文を、TABの本文へ貼り付けます。",
                "Pastes all the text of Quick Draft into a tab.",
            ),
            "",
        ),
    ] {
        v.push(ActionDef {
            id: id.to_owned(),
            title: (title.0.to_owned(), title.1.to_owned()),
            description: (description.0.to_owned(), description.1.to_owned()),
            key,
            fixed: false,
            category: QuickDraft,
            scope: Scope::QuickDraft,
            command: None,
        });
    }
    {
        use crate::panel_actions::Action as A;
        use crate::git_repo_ui::RepoAction as G;
        v.extend([
        ("outline.collapse-all", ("アウトラインをすべて畳む", "Collapse All Outline Headings"), A::OutlineCollapse),
        ("outline.expand-all", ("アウトラインをすべて展開", "Expand All Outline Headings"), A::OutlineExpand),
        ("tags.collapse-all", ("タグをすべて畳む", "Collapse All Tags"), A::TagsCollapse),
        ("tags.expand-all", ("タグをすべて展開", "Expand All Tags"), A::TagsExpand),
        ("bookmarks.add-current", ("現在のTABをブックマークに追加", "Bookmark Current Tab"), A::BookmarkAdd),
        ("bookmarks.create-group", ("ブックマークのグループを作成…", "Create Bookmark Group…"), A::BookmarkGroup),
        ("workspace.attach-folder", ("Workspaceにフォルダを追加…", "Add Folder to Workspace…"), A::WorkspaceAttach),
        ("workspace.clone-repository", ("WorkspaceにリポジトリをClone…", "Clone Repository into Workspace…"), A::WorkspaceClone),
        ("workspace.reset-view", ("Workspaceの表示をリセット…", "Reset Workspace View…"), A::WorkspaceReset),
        ("workspace.leave", ("Workspaceを閉じる", "Close Workspace"), A::WorkspaceLeave),
        ("git.fetch", ("Git Fetch", "Git Fetch"), A::GitFetch),
        ("git.pull", ("Git Pull", "Git Pull"), A::GitPull),
        ("git.push", ("Git Push／Publish", "Git Push or Publish"), A::GitPush),
        ("git.sync", ("Git Sync", "Git Sync"), A::GitSync),
        ("git.cancel", ("実行中のGit操作を中止", "Cancel Git Operation"), A::GitCancel),
        ("git.commit-changes", ("入力したメッセージでCommit", "Commit with Entered Message"), A::GitCommit),
        ("git.open-comparison", ("比較画面で開く", "Open in Comparison"), A::GitRepo(G::OpenComparison)),
        ("git.review-blocks", ("Gitの変更ブロックを確認…", "Review Git Blocks…"), A::GitRepo(G::ReviewBlocks)),
        ("git.stage-block", ("ブロックをStage", "Stage Block"), A::GitRepo(G::StageBlock)),
        ("git.unstage-block", ("ブロックをUnstage", "Unstage Block"), A::GitRepo(G::UnstageBlock)),
        ("git.open-history-search", ("履歴検索", "History Search"), A::GitRepo(G::OpenSearch)),
        ("git.close-history-search", ("コミットグラフに戻る", "Return to Commit Graph"), A::GitRepo(G::CloseSearch)),
        ("git.set-compare-base", ("比較元に設定", "Set as Base"), A::GitRepo(G::SetBase)),
        ("git.compare-with-base", ("比較元との差分", "Compare with Base"), A::GitRepo(G::CompareWithBase)),
        ("git.merge-resolution", ("Mergeの解消を開く", "Open Merge Resolution"), A::GitMergeOpen),
        ("git.merge-edit-result", ("解消結果を編集", "Edit Merge Result"), A::GitMergeEdit),
        ("git.merge-stage-result", ("解消結果をStage", "Stage Merge Result"), A::GitMergeStage),
        ("git.merge-stage-deletion", ("削除を採用してStage…", "Accept Deletion and Stage…"), A::GitMergeDelete),
        ("git.merge-continue", ("Mergeを続行", "Continue Merge"), A::GitMergeContinue),
        ("git.merge-abort", ("Mergeを中止…", "Abort Merge…"), A::GitMergeAbort),
        ("comparison.previous", ("前の差分", "Previous Difference"), A::DiffPrevious),
        ("comparison.next", ("次の差分", "Next Difference"), A::DiffNext),
        ("comparison.close", ("比較を終える", "End Comparison"), A::DiffClose),
        ("comparison.keep-left", ("選んだ差分の左側を残す", "Keep Left Side of Selected Difference"), A::DiffKeepLeft),
        ("comparison.take-right", ("選んだ差分の右側を取る", "Take Right Side of Selected Difference"), A::DiffTakeRight),
        ("comparison.apply", ("選んだ差分を本文に反映", "Apply Chosen Differences"), A::DiffApply),
        ("comparison.copy-left", ("比較の左側を全文コピー", "Copy Left Side of Comparison"), A::DiffCopyLeft),
        ("comparison.copy-right", ("比較の右側を全文コピー", "Copy Right Side of Comparison"), A::DiffCopyRight),
        ("backups.toggle-all", ("バックアップ履歴をすべて選択／解除", "Toggle All Backup History Checks"), A::BackupSelectAll),
        ("backups.delete-checked", ("選択したバックアップを削除…", "Delete Checked Backups…"), A::BackupDelete),
        ("print-preview.previous", ("印刷プレビューの前のページ", "Previous Print Preview Page"), A::PrintPrevious),
        ("print-preview.next", ("印刷プレビューの次のページ", "Next Print Preview Page"), A::PrintNext),
        ("print-preview.close", ("印刷プレビューを閉じる", "Close Print Preview"), A::PrintClose),
        ("print-preview.print", ("プレビューを印刷…", "Print from Preview…"), A::PrintSubmit),
        ("settings.export-all", ("すべての設定を書き出す…", "Export All Settings…"), A::SettingsExport),
        ("settings.import-all", ("すべての設定を取り込む…", "Import All Settings…"), A::SettingsImport),
        ("settings.reset-all", ("すべての設定を既定に戻す…", "Reset All Settings…"), A::SettingsReset),
        ].map(|(id, title, action)| {
            let category = match action {
                A::WorkspaceClone | A::GitFetch | A::GitPull | A::GitPush | A::GitSync
                | A::GitCancel | A::GitCommit => Git,
                action if action.available_in_git_overlay() => Git,
                _ => Surface,
            };
            def(id, title, action.description(), "", category, C::PanelAction(action))
        }));
    }
    use crate::tree_actions::Action as A;
    use crate::TreeCommand as T;
    v.extend([
        ("tree.new-file", ("フォルダに新規ファイル…", "New File in Folder…"), ("選んだフォルダ内、または選んだファイルと同じフォルダにファイルを作成します。", "Creates a file inside the selected folder or alongside the selected file."), "", A::Entry(T::NewFile)),
        ("tree.new-folder", ("サブフォルダを作成…", "Create Subfolder…"), ("選んだフォルダ内、または選んだファイルと同じフォルダにサブフォルダを作成します。", "Creates a subfolder inside the selected folder or alongside the selected file."), "", A::Entry(T::NewFolder)),
        ("tree.rename", ("名前を変更…", "Rename…"), ("選択した項目の名前を変更します。", "Renames the selected file or folder."), "F2", A::Entry(T::Rename)),
        ("tree.duplicate", ("ファイル・フォルダを複製", "Duplicate File or Folder"), ("選択した項目を複製します。", "Duplicates the selected file or folder."), "", A::Entry(T::Duplicate)),
        ("tree.delete", ("ファイル・フォルダを削除…", "Delete File or Folder…"), ("選択した項目の削除を確認します。", "Asks before deleting the selected item."), "Delete", A::Entry(T::Delete)),
        ("tree.reveal", ("Explorerで表示", "Show in Windows Explorer"), ("選択した項目をWindowsのExplorerで表示します。", "Shows the selected item in Windows Explorer."), "", A::Entry(T::Reveal)),
        ("tree.search-in", ("このフォルダを検索", "Search in Selected Folder"), ("選択した項目の場所を検索します。", "Searches the folder named by the selected item."), "", A::Entry(T::SearchIn)),
        ("tree.terminal-in", ("このフォルダでTerminal", "Terminal in Selected Folder"), ("選択した項目の場所でTerminalを開きます。", "Opens a terminal in the selected folder."), "", A::Entry(T::TerminalIn)),
        ("tree.reveal-document", ("現在の文書をファイルツリーで表示", "Show Current Document in Tree"), ("編集中のファイルをファイルツリーで表示します。", "Shows the current document in the tree."), "", A::RevealDocument),
        ("tree.collapse-all", ("ファイルツリーをすべて畳む", "Collapse All Tree Folders"), ("ファイルツリーのフォルダを畳みます。", "Collapses the folders in the file tree."), "", A::CollapseAll),
        ("tree.refresh", ("ファイルツリーを更新", "Refresh File Tree"), ("ファイルツリーを読み直します。", "Refreshes the file tree."), "", A::Refresh),
        ("tree.search", ("ファイルツリーの検索欄を開閉", "Toggle File Tree Search"), ("ファイルツリーの検索欄を開閉します。", "Opens or closes search in the file tree."), "", A::Search),
        ("tree.previous", ("ファイルツリーで前の行を選択", "Select Previous Tree Row"), ("ファイルツリーで前の行を選びます。", "Selects the previous row in the file tree."), "Up", A::Previous),
        ("tree.next", ("ファイルツリーで次の行を選択", "Select Next Tree Row"), ("ファイルツリーで次の行を選びます。", "Selects the next row in the file tree."), "Down", A::Next),
        ("tree.open", ("ファイル・フォルダを開く", "Open Tree Item"), ("ファイルを開くか、フォルダを展開・折り畳みます。", "Opens the file or toggles the folder."), "Enter", A::Activate),
        ("tree.left", ("フォルダを畳む／親フォルダを選択", "Collapse Tree Folder or Select Parent"), ("選択したフォルダを畳むか、親を選びます。", "Collapses the selected folder or selects its parent."), "Left", A::Left),
        ("tree.right", ("フォルダを展開／中の項目を選択", "Expand Tree Folder or Select Child"), ("選択したフォルダを開くか、子を選びます。", "Expands the selected folder or selects its first child."), "Right", A::Right),
    ].map(|(id, title, description, key, action)| ActionDef {
        scope: Scope::Tree,
        ..def(id, title, description, key, LeftPane, C::Tree(action))
    }));
    v
}

/// 挿入の形のID。**形の名前から作る**（並びの番号ではない）——並びを変えても割り当てが動かない。
pub(crate) fn insert_id(shape: InsertShape) -> String {
    let mut id = String::from("insert.");
    let mut last_dash = false;
    for c in format!("{shape:?}").chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            id.push('-');
            last_dash = true;
        }
    }
    id.trim_end_matches('-').to_owned()
}

/// 挿入の形のTitleとDescription。**組の中の短い名前は、組の名前を前に付けて**それだけで分かる
/// ように（「Note」→「Callout：Note」）。
fn insert_words(
    shape: InsertShape,
    (ja, en): (&str, &str),
) -> ((String, String), (String, String)) {
    let (title_ja, title_en) = match crate::menu_commands::insert_group(shape) {
        Some((group_ja, group_en)) if !ja.contains(group_ja) => {
            (format!("{group_ja}：{ja}"), format!("{group_en}: {en}"))
        }
        _ => (ja.to_owned(), en.to_owned()),
    };
    let description = match shape {
        InsertShape::Edit(edit) if edit.needs_a_picked_word() => (
            format!("選んだ字に「{title_ja}」を付けます。"),
            format!("Adds \u{201c}{title_en}\u{201d} to the picked text."),
        ),
        InsertShape::Edit(_) => (
            format!(
                "キャレットの位置に「{title_ja}」を入れます。選んだ字があれば、それを使います。"
            ),
            format!(
                "Inserts \u{201c}{title_en}\u{201d} at the caret, using the picked text if any."
            ),
        ),
        InsertShape::Line(_) => (
            format!("いまの行を「{title_ja}」にします。"),
            format!("Makes the current line \u{201c}{title_en}\u{201d}."),
        ),
        InsertShape::PageBreak => (
            "キャレットの行の前で、ページを改めます。".to_owned(),
            "Starts a new page before the caret's line.".to_owned(),
        ),
        InsertShape::Table => (
            "マス目で大きさを選び、表を入れます。".to_owned(),
            "Inserts a table whose size you pick on a grid.".to_owned(),
        ),
        InsertShape::Template => (
            "Templates Folderのテンプレートを選び、キャレットの位置に入れます。".to_owned(),
            "Picks a template from the Templates Folder and inserts it at the caret.".to_owned(),
        ),
    };
    ((title_ja, title_en), description)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IDは重ならず、どの行も2つの言葉でTitleとDescriptionを持つ。
    #[test]
    fn every_action_has_one_id_and_words_in_both_languages() {
        let mut seen = std::collections::HashSet::new();
        for action in all() {
            assert!(seen.insert(action.id.as_str()), "{} twice", action.id);
            assert!(
                !action.id.contains(['=', ';', ' ']),
                "{} can be written",
                action.id
            );
            for text in [
                &action.title.0,
                &action.title.1,
                &action.description.0,
                &action.description.1,
            ] {
                assert!(!text.is_empty(), "{}", action.id);
            }
        }
    }

    /// 同じ操作を2つの行が名指さない（キーの割り当てが2つに分かれないように）。
    #[test]
    fn no_two_actions_run_the_same_command() {
        let commands: Vec<_> = all().iter().filter_map(|a| a.command.as_ref()).collect();
        for (at, command) in commands.iter().enumerate() {
            assert!(!commands[at + 1..].contains(command), "{command:?} twice");
        }
    }

    /// 既定のキーは、同じ場面で重ならない。
    #[test]
    fn default_keys_do_not_collide() {
        let mut seen = std::collections::HashSet::new();
        for action in all().iter().filter(|a| !a.key.is_empty() && !a.fixed) {
            assert!(
                seen.insert((action.scope, action.key)),
                "{} {}",
                action.id,
                action.key
            );
        }
    }
}
