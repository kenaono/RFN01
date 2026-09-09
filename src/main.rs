mod app_data;
mod buffer;
mod clipboard;
mod diag;
mod directwrite_probe;
mod directwrite_render;
mod document;
mod file_dialog;
mod file_io;
mod file_tree;
mod find;
mod ime;
mod kill_ring;
mod open_document;
mod pane_layout;
mod pty;
mod quick_draft;
mod saving;
mod searcher;
mod session;
mod shell;
mod terminal;
mod terminal_session;
mod text_blocks;
#[cfg(test)]
mod vertical_layout;
mod wiring;
mod word_marks;
mod writer;

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs::File,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use buffer::DocumentFile;
use diag::DiagLog;
use directwrite_render::{
    CaretGeometry, LineFit, SelectionRect, TextEngine, TileSink, WritingMode, cells,
};
use document::{PreviewDocument, caret_place};
use kill_ring::{KillAction, KillRing};
use open_document::{Change, OpenDocument, replace_source_range};
use pane_layout::{Layout, Rect, Split, Towards, neighbour};
use saving::{
    check_external_change, collect_write_results, discard_all_work_copies, discard_work_copy,
    flush_work_copies, keep_work_copies_again, overwrite_the_outside_change, reload_from_file,
    restore_tabs, save_all, save_document, work_identity, write_work_copy_if_due,
    write_work_copy_now, write_work_copy_of,
};
use searcher::{NeverSuperseded, SearchJob, SearchOutcome, Searcher};
use session::{open_session, restore_window_place, write_session};
use slint::{
    CloseRequestResponse, Color, ComponentHandle, Image, Model, ModelRc, RenderingState,
    Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel,
};
use std::collections::{BTreeSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use terminal::{Key as TerminalKey, Modifiers as TerminalModifiers};
use terminal_session::TerminalSession;
use text_blocks::{
    DEFAULT_BODY_FONT, DEFAULT_CODE_FONT, DEFAULT_HEADING_FONT, DEFAULT_INK, DEFAULT_PAPER,
    Emphasis, LineMarker, MAX_HEADING_LEVEL, StyledText, TileSpan, Typography, visible_flow_range,
};
use unicode_segmentation::UnicodeSegmentation;
use writer::FileWriter;

slint::include_modules!();

/// What a terminal tab can be running (追加要件 Terminal: WSL2とPowerShell、
/// 既定はWSL2).
///
/// 追加要件 2026-09-08: **一覧は設定ファイルにある。**三つの決め打ちだった
/// ものが、名前とコマンド行の組の並びになった——書き手の機械には`cmd.exe`も
/// `git-bash`も`wsl -d Ubuntu`もあり、そのどれを「接続先」と呼ぶかは編集器が
/// 決めることではない。**組み込みの三つは初期値**で、設定ファイルが無いとき
/// にそこへ書き出されるだけの並びになった。
///
/// **`&'static str`ではなく`String`。**書き手が名づけたものは、この実行より
/// 長生きしない。
#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalShell {
    /// What a tab of this shell is called. **The shell, not 無題** — the
    /// document behind it is a stand-in nobody is writing in.
    name: String,
    /// The command line, the program first. Everything after the first word is
    /// handed to the shell as it is written.
    command: String,
}

impl TerminalShell {
    /// What a settings file with no shell list means (追加要件 2026-09-08).
    ///
    /// **The default first.** The menu is built in this order, and 追加要件
    /// says the first is where a writer who does not choose ends up.
    ///
    /// - `wsl.exe` with nothing after it opens the default distribution in the
    ///   current directory, and **starts it if it is not running** — which is
    ///   what the requirement asks for, done by the thing that knows how.
    /// - **No logo** on either PowerShell: a terminal opened to run something
    ///   should not spend its first second printing a banner.
    /// - PowerShell 7 is offered **because of what it reads, not what it
    ///   runs**. Windows PowerShell decodes a file with no BOM in the ANSI code
    ///   page, so `Get-Content` of a UTF-8 document comes out as mojibake
    ///   before it reaches any terminal (技術検証 9.4 measured both in one
    ///   screen). PowerShell 7 reads UTF-8 by default.
    fn built_in() -> Vec<TerminalShell> {
        [
            ("WSL", "wsl.exe"),
            ("PowerShell", "powershell.exe -NoLogo"),
            ("PowerShell 7", "pwsh.exe -NoLogo"),
        ]
        .into_iter()
        .map(|(name, command)| TerminalShell {
            name: name.to_owned(),
            command: command.to_owned(),
        })
        .collect()
    }

    /// How the settings file writes one: the name, then the command line.
    ///
    /// **`|` between them**, because a name may hold spaces and a command line
    /// certainly does. A line without one is all command and takes its name
    /// from the program.
    fn written(&self) -> String {
        format!("{} | {}", self.name, self.command)
    }

    fn read(written: &str) -> Option<TerminalShell> {
        let (name, command) = match written.split_once('|') {
            Some((name, command)) => (name.trim().to_owned(), command.trim().to_owned()),
            None => {
                let command = written.trim().to_owned();
                let name = command.split_whitespace().next()?.to_owned();
                (name, command)
            }
        };
        if name.is_empty() || command.is_empty() {
            return None;
        }
        Some(TerminalShell { name, command })
    }

    /// The program this shell is, as a file to look for.
    fn program(&self) -> &str {
        self.command.split_whitespace().next().unwrap_or("")
    }
}

/// The shells this machine actually has, out of the ones it is set to offer.
///
/// **A row that opens nothing looks exactly like a row that is broken**, so a
/// shell that is not installed is not offered. If the search finds nothing at
/// all — a `PATH` this cannot read — the whole list is offered rather than none
/// of it, because a message the writer can read beats a menu with no rows.
fn offered_shells(window: &AppWindow) -> Vec<TerminalShell> {
    let configured = configured_shells(window);
    let found: Vec<TerminalShell> = configured
        .iter()
        .filter(|shell| on_path(shell.program()))
        .cloned()
        .collect();
    if found.is_empty() { configured } else { found }
}

/// The whole list the settings file names, whether or not this machine has it.
///
/// **Held in the window, like every other setting** (要件 9's numbers, the
/// palette, the families, the default shell). Nothing draws these lines — the
/// menus read `terminal-shells`, which is the offered subset by name — but this
/// is what `save_settings` writes back, and that function is reached from
/// places that carry the window and nothing else.
fn configured_shells(window: &AppWindow) -> Vec<TerminalShell> {
    let written = window.get_terminal_shell_lines();
    let read: Vec<TerminalShell> = written
        .iter()
        .filter_map(|line| TerminalShell::read(&line))
        .collect();
    if read.is_empty() {
        TerminalShell::built_in()
    } else {
        read
    }
}

/// Put the list the file named into the window, in the file's own words.
fn hold_shells(window: &AppWindow, shells: &[TerminalShell]) {
    window.set_terminal_shell_lines(ModelRc::new(VecModel::from(
        shells
            .iter()
            .map(|shell| SharedString::from(shell.written()))
            .collect::<Vec<_>>(),
    )));
}

/// The shell a number from a menu names. **Anything unexpected is the first
/// one**, for the reason every other number arriving from the window is bounded
/// rather than trusted.
fn shell_at(window: &AppWindow, index: i32) -> TerminalShell {
    let offered = offered_shells(window);
    let first = || TerminalShell::built_in().remove(0);
    match offered.get(index.max(0) as usize) {
        Some(shell) => shell.clone(),
        None => offered.first().cloned().unwrap_or_else(first),
    }
}

/// Put the shells the writer may open in front of them (追加要件 Terminal).
///
/// **Called wherever the list can have changed**: once at startup, and again
/// when a settings file has been read. The default is held inside the list, so
/// a file naming a shell this machine does not have opens the first one it has.
fn publish_shells(window: &AppWindow) {
    let offered = offered_shells(window);
    let most = offered.len().saturating_sub(1) as i32;
    window.set_terminal_shells(ModelRc::new(VecModel::from(
        offered
            .iter()
            .map(|shell| SharedString::from(shell.name.clone()))
            .collect::<Vec<_>>(),
    )));
    window.set_default_shell(window.get_default_shell().clamp(0, most));
}

/// 端末の見た目を、設定から一つ作る（追加要件 2026-09-08、要件 6.8）。
///
/// **`TerminalLook::default()`を呼んでいた4か所を、ここへ集めた。**升目の大きさは
/// この`look`から出る（`terminal_cell_size`）ので、**描くときと、シェルへ「画面は何桁
/// 何行だ」と伝えるときとで違う`look`を使うと、プロンプトが折り返す場所がずれる**。
/// 1か所で作れば、そのずれ方が起きない。
///
/// 16色は選ばせず、背景の明るさから決める（`palette_for`）。
fn terminal_look(window: &AppWindow) -> cells::TerminalLook {
    let paper = channels(window.get_terminal_paper());
    let (low, high) = TERMINAL_SIZE_RANGE;
    cells::TerminalLook {
        family: window.get_terminal_font().to_string(),
        font_size: window.get_terminal_size().clamp(low, high) as f32,
        paper,
        ink: channels(window.get_terminal_ink()),
        palette: cells::TerminalLook::palette_for(paper),
        ..cells::TerminalLook::default()
    }
}

/// Whether a program is somewhere on `PATH`.
///
/// **A few `stat` calls, and only when a terminal is opened or the menu is
/// built.** Asking Windows to run it would be the other way of finding out, and
/// that costs a process.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

/// How long the window waits before drawing what a shell wrote.
///
/// **One frame.** Long enough that a burst of output is drawn once, short
/// enough that nobody sees the wait.
const TERMINAL_FRAME_MS: u64 = 16;

const SAMPLE_MARKDOWN: &str = r#"# 縦書きライブ編集の技術検証

これは、RustとSlintで作る文章Editorの検証画面です。

**Markdownの原文**を左で編集し、右側にはDirectWriteの実描画を表示します。

句読点、括弧（かっこ）、全角英数字ＡＢＣ１２３、半角英数字ABC123、そして長い文章の折り返しを確認します。

> 右側をクリックするとキャレットを置き、日本語IMEでも直接入力できます。
"#;
const TAB_INDENT: &str = "    ";
const IME_CANDIDATE_GAP: f32 = 8.0;
const CARET_SCROLL_PADDING: f32 = 24.0;
/// Fallback column height, used before the pane reports its own size and by
/// tests. The live value comes from the vertical pane.
const PREVIEW_HEIGHT: u32 = 520;
/// Below this the column holds too few characters to be worth laying out.
const MIN_PREVIEW_HEIGHT: u32 = 120;
/// The same two for the horizontal pane, where the line axis is its width.
const HORIZONTAL_WIDTH: u32 = 560;
const MIN_HORIZONTAL_WIDTH: u32 = 160;
/// How long the window must stop changing size before the document is laid out
/// again. Every height change invalidates every block measurement, so following
/// a drag pixel by pixel would remeasure the whole document on each frame.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
/// How long a zoom or typography control must stop being pressed before the
/// document is laid out again. One press re-measures every block in both panes
/// and costs about 200ms on a 4万字 document (技術検証 6.8), so a run of presses
/// pays that over and over for results nobody sees. The value is applied to the
/// toolbar immediately either way; only the re-measuring waits.
const SPEC_SETTLE: Duration = Duration::from_millis(150);
/// What one notch of 要件 11.5's `Ctrl+ホイール` or `Ctrl+=` is worth, and how
/// far the zoom may be taken either way.
///
/// **The bounds are held here rather than at each caller** — the buttons, the
/// keys, the wheel and a restored session all arrive at the same two numbers,
/// and a session that was hand-edited must not be able to open at 4000%.
const ZOOM_STEP: i32 = 10;
const ZOOM_MIN: i32 = 50;
const ZOOM_MAX: i32 = 240;
/// What a pane magnifies by until anything says otherwise.
const ZOOM_DEFAULT: i32 = 100;
/// How long the caret must stop moving before the Markdown of its line is
/// revealed. Revealing rewrites that block's text, which costs a whole block's
/// worth of pixels; a held arrow key would pay that on every repeat.
const REVEAL_SETTLE: Duration = Duration::from_millis(120);
const BASE_FONT_SIZE: i32 = 22;
/// A paragraph past this many characters is called out in the status bar.
///
/// Not an engine limit and not enforced: the writer is told, and the document is
/// left alone. Forcing a break would put text in the document that the author
/// did not type, while they are typing (技術検証 7.4).
///
/// The number is chosen from what editing costs, not from what DirectWrite can
/// hold. Typing at the *start* of a paragraph re-wraps all of it (6.10), at
/// about 4.2µs per character across both panes, so 32,000 characters is roughly
/// 130ms — and a paragraph that long is already outside what anyone writes.
/// `MAX_PROBE_FLOW` is a separate, much larger number guarding a separate thing;
/// lowering it to meet this one would only make the range between them slower.
const PARAGRAPH_WARNING_CHARACTERS: usize = 32_000;
/// The largest document this editor accepts, in characters.
///
/// **A hard limit, and refusing is the point.** A document this size is far
/// outside what a manuscript is, and the risk being avoided is not slowness but
/// the state of half-working on something out of scope — better to decline it
/// than to open it and behave in ways nobody has looked at.
///
/// Unlike the engine's guards this is a rule about documents, so it is a number
/// of characters and it does not move with the window. What decides it is the
/// one cost that follows the whole document rather than the paragraph being
/// edited: `preview` and `stats` walk all of it on every keystroke. That was
/// 0.05µs per character and is now 0.006µs (技術検証 6.12), so a million
/// characters is about 6ms of scanning before anything else happens — which is
/// what let this rise from half a million.
///
/// **The number moved; the reason for having one did not.** Making the scans
/// incremental bought room, not permission to open anything.
const MAX_DOCUMENT_CHARACTERS: usize = 1_000_000;

/// How long the writer has to stop before the work copy is written (要件 8.1).
const WORK_COPY_IDLE: Duration = Duration::from_secs(2);
/// The longest a run of continuous typing may go without one (要件 8.1).
const WORK_COPY_LONGEST: Duration = Duration::from_secs(5);
/// How often the two rules are checked. Short enough that two seconds means
/// two seconds, long enough to cost nothing while nothing is happening.
const WORK_COPY_TICK: Duration = Duration::from_millis(500);
/// How long closing waits for the last work copy to reach the disk before it
/// stops asking (追加要件 2026-09-09、残り2).
///
/// **失敗を見るには、答えを窓が開いているうちに受け取らなければならない**
/// ——書けなかったことに気づけるのはそこだけである。1件2.4〜5.9msの世界なので、
/// 普段のここは一往復で終わる。**上限があるのは閉じられない窓を作らないため**で、
/// 時間切れは失敗として扱わない（`FileWriter::finish`が残りを待ち切る）。
const WORK_COPY_SETTLE: Duration = Duration::from_secs(3);

/// How often the open file is checked for an outside change (要件 8.3).
///
/// Polled rather than watched. The question is one `stat` per open document,
/// not a read, and a watcher is a thread, a queue and a set of platform events
/// for something that is being asked twice a second anyway.
const EXTERNAL_CHECK_TICK: Duration = Duration::from_secs(2);
/// How much one press of a typography control moves it, in percent. Character
/// spacing is a fraction of the size rather than a multiple, so it steps finer.
/// How large a heading is set at each level, as a percentage of body size
/// (要件 9). **Six numbers rather than one ramp**: the requirement asks for
/// them to be set individually, and the ramp could only ever produce evenly
/// spaced sizes — which is not what a document with H1 and H2 in it wants.
const HEADING_DEFAULTS: [i32; MAX_HEADING_LEVEL] = [200, 160, 130, 115, 105, 100];
/// Tiles rendered on each side of the viewport, so crossing a tile boundary does
/// not stall on a rasterization the scroll is already waiting for. Caret moves
/// scroll the pane as much as the scrollbar does, so both paths prefetch.
const TILE_PREFETCH_COUNT: u32 = 1;
/// Resident tiles. Enough that scrolling back over ground already covered is
/// free, small enough to stay a fixed cost on any document length.
const TILE_CACHE_LIMIT: usize = 6;

/// How many evicted tile buffers to keep for redrawing into.
///
/// **Enough for a screenful**, because that is what one refresh can retire and
/// draw again: a change of width or of spec gives every tile on screen a new
/// fingerprint at once. They are held only between the eviction and the drawing
/// of the same refresh; what is left over waits for the next one. Each is about
/// two megabytes, and allocating that costs more than drawing it (技術検証 7.8).
const SPARE_TILE_BUFFERS: usize = 16;
/// Every refresh writes one line here — beside the executable, like the trace
/// (see [`diag::beside_executable`]). The status bar is a single unwrapped line
/// in a half-width pane, so anything past the first few figures is clipped; this
/// keeps the full breakdown somewhere it can actually be read afterwards.
const PERF_LOG_PATH: &str = "perf_log.txt";
/// The run before this one. See [`PerfLog`].
const PERF_LOG_PREVIOUS_PATH: &str = "perf_log.prev.txt";
/// Lines one run may write before the log gives up. A keystroke in Split writes
/// two, so this is a long session; past it the file holds more than anyone reads
/// and keeping it open only costs.
const PERF_LOG_LINE_LIMIT: usize = 20_000;

/// One pane's caret, selection and pending IME text, all in source bytes.
///
/// Both panes keep one of these. Everything here is about the document, not
/// about how a pane draws it, so the same struct serves either writing
/// direction: `preferred_line` is the coordinate on the *line* axis to hold on
/// to when stepping between lines, which is a y in the vertical pane and an x in
/// the horizontal one. `active_line_start` is only read back by the vertical
/// pane: it decides which line shows its Markdown and lags the caret on
/// purpose, while the horizontal pane works the same answer out from its own
/// caret ([`PaneId::revealed_line`]).
#[derive(Clone, Debug, Default)]
struct EditorState {
    caret_source_byte: Option<usize>,
    selection_anchor_source_byte: Option<usize>,
    active_line_start: Option<usize>,
    preedit: String,
    preferred_line: Option<f32>,
    /// Whether the selection is a rectangle rather than a run (要件 7.1).
    ///
    /// **The two ends are the same two bytes either way** — what changes is
    /// what is read out of them: a rectangle takes the lines they sit on and
    /// the columns they sit at, and covers every line between (`selection_ranges`).
    rectangular: bool,
    /// Whether `Ctrl+Space` has been pressed and not yet answered (要件 11.4).
    ///
    /// **A held Shift that the writer does not have to hold.** While it is on,
    /// every move extends the selection the way Shift does; it goes off when
    /// the writer does anything but move — an edit, or a click — and when
    /// `Ctrl+Space` is pressed again.
    mark: bool,
    /// 検索が置いた選択（E1、書き手の指摘 2026-09-09）。
    ///
    /// **「その範囲は書き手が選んだものか」を答えるためだけにある。**範囲内検索
    /// （`[ ]`）は書き手が選んだ範囲を覚えているが、選び直したら新しい範囲に
    /// なってほしい——ところが検索そのものも選択を動かす（一致を選ぶ）ので、
    /// 「選択が変わったら取り直す」では**2回目の検索で範囲が一致そのものに
    /// 潰れる**。ここに置いた最後の一致と今の選択を比べれば、書き手の手が
    /// 入ったかどうかが分かる。
    search_selection: Option<(usize, usize)>,
    /// 行番号から始まった選択（E3）。**押した行の頭のバイト。**
    ///
    /// **これがあるあいだ、引くと行ごと選ばれる。**番号を押すことは行を指す
    /// ことなので、そのまま引いた書き手が指しているのも行である——1画素の
    /// ぶれで行の選択が字の選択へ変わってしまうと、押しただけのつもりが
    /// 選び直しになる。ボタンを離すと消える。
    line_drag: Option<usize>,
    /// ダブルクリックが選んだ語（E3、書き手の報告 2026-09-10）。
    ///
    /// **2回目を離した合図から、選んだ語を守る。**離した合図はカーソルを押した
    /// 点へ置くので、そのままでは語の途中までしか残らない——「不安定に感じました」
    /// 「英語では単語選択にならない感じ」の半分はこれである。**引けば語ごと
    /// 伸びる**のも同じ印で、押し直すまで残る。
    word_drag: Option<(usize, usize)>,
    /// 直前の押下——いつ、どこを（E3）。**2回目かどうかを数えるためだけにある。**
    ///
    /// 2回目を数えたら空に戻す：**3回目は普通の押下**である。窓の
    /// `double-clicked`に任せていたときは3回目・4回目にも来ていて、
    /// 押すたびに語が選び直されるので選択が外れなくなった（書き手の報告
    /// 2026-09-10：「契機がわからないのですが、選択がはずれなくなります」）。
    last_click: Option<(Instant, f32, f32)>,
}

impl EditorState {
    /// この押下は「2回目」か——ダブルクリックの判定（E3）。
    ///
    /// **速さはWindowsのもの**（`GetDoubleClickTime`）。この編集器が独自の秒数を
    /// 持てば、書き手が他のアプリで慣れた速さと違う反応をすることになる。
    ///
    /// **場所も見る。**離れたところを2回押したのは、同じものを2回押したのではない。
    fn double_click(&mut self, x: f32, y: f32) -> bool {
        let now = Instant::now();
        let doubled = self.last_click.is_some_and(|(when, at_x, at_y)| {
            now.duration_since(when) <= double_click_time()
                && (x - at_x).abs() <= DOUBLE_CLICK_SLACK
                && (y - at_y).abs() <= DOUBLE_CLICK_SLACK
        });
        // 2回目で区切る。3回目は、次の1回目である。
        self.last_click = (!doubled).then_some((now, x, y));
        doubled
    }
}

/// 2回目とみなす、押した場所のずれ（画素、E3）。**手は完全には止まらない。**
const DOUBLE_CLICK_SLACK: f32 = 4.0;

/// Windowsで決められたダブルクリックの間隔（E3）。
fn double_click_time() -> Duration {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

    // SAFETY: 引数の無い呼び出しで、返るのはミリ秒の数である。失敗しない。
    let ms = unsafe { GetDoubleClickTime() };
    Duration::from_millis(u64::from(ms.max(1)))
}

/// Both panes' states, so that a callback carrying a pane number can reach the
/// one it names.
///
/// It hands out **the named pane's state and the other's**, because an edit
/// needs both: one to move the caret in, one to keep in step over the shared
/// document (要件 7.6). Cloned into every editing callback, which is why the
/// panes each hold an `Rc` rather than the state itself.
#[derive(Clone, Default)]
struct PaneStates {
    /// One slot per pane, indexed by [`PaneId::index`].
    ///
    /// **Shared rather than copied**, so that the clone every callback holds is
    /// the same list: a pane added by a split has to be visible to callbacks
    /// that were registered before it existed (要件 6.4).
    slots: Rc<RefCell<Vec<PaneSlot>>>,
}

/// One pane's own state, and the document it has in front of it.
#[derive(Clone)]
struct PaneSlot {
    state: Rc<RefCell<EditorState>>,
    /// **Asked at the moment it is needed, never held**: a pane can be showing
    /// a different document by the time a timer fires.
    showing: Rc<RefCell<Rc<OpenDocument>>>,
}

impl PaneStates {
    /// The editor's first pane, on the document it opened with (要件 6.3).
    fn new(document: &Rc<OpenDocument>) -> Self {
        let states = Self::default();
        states.add(document);
        states
    }

    /// Make room for a pane. **The new pane is the last**, which is the number
    /// a split hands out.
    fn add(&self, document: &Rc<OpenDocument>) {
        self.slots.borrow_mut().push(PaneSlot {
            state: Rc::new(RefCell::new(EditorState::default())),
            showing: Rc::new(RefCell::new(document.clone())),
        });
    }

    /// Take a pane out, and **close the numbering behind it**.
    ///
    /// Every pane after it moves down one, which is what keeps a pane's number
    /// and its row of the window's model the same thing (ペイン分割設計 5). The
    /// callers renumber the layout tree and the tab strips in the same breath.
    fn remove(&self, id: PaneId) {
        let mut slots = self.slots.borrow_mut();
        let at = id.index() as usize;
        if at < slots.len() {
            slots.remove(at);
        }
    }

    fn count(&self) -> usize {
        self.slots.borrow().len()
    }

    fn of(&self, id: PaneId) -> Rc<RefCell<EditorState>> {
        let slots = self.slots.borrow();
        match slots.get(id.index() as usize) {
            Some(slot) => slot.state.clone(),
            // A pane number that names nothing arrives with a keystroke, and a
            // keystroke must not be able to stop the editor. An unshared state
            // takes the edit nowhere, which is what a pane that is not there
            // should do with one.
            None => Rc::new(RefCell::new(EditorState::default())),
        }
    }

    /// What this pane is showing.
    fn document(&self, id: PaneId) -> Rc<OpenDocument> {
        let slots = self.slots.borrow();
        let slot = slots.get(id.index() as usize).or(slots.first());
        slot.map(|slot| slot.showing.borrow().clone())
            .expect("the editing area always holds one pane (要件 6.3)")
    }

    /// Put a document in front of this pane.
    fn show(&self, id: PaneId, document: &Rc<OpenDocument>) {
        let slots = self.slots.borrow();
        if let Some(slot) = slots.get(id.index() as usize) {
            *slot.showing.borrow_mut() = document.clone();
        }
    }

    /// Whether two panes are looking at the same document.
    ///
    /// **The question anything that reaches across panes has to ask** (要件
    /// 7.6): carrying a caret out of one, redrawing one because the other was
    /// typed in. Today the answer is always yes, and it stops being always yes
    /// as soon as the panes have tab lists of their own — which is why the
    /// callers ask now rather than when the answer changes.
    fn same_document(&self, one: PaneId, other: PaneId) -> bool {
        Rc::ptr_eq(&self.document(one), &self.document(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionPhase {
    Begin,
    /// A click with Shift held: the caret moves to it and the selection reaches
    /// back to wherever it already was. Everything else about it is a `Begin`,
    /// including that a drag may follow.
    Extend,
    Update,
    End,
}

/// The preview document, kept until the source or the active line changes.
///
/// Rebuilding it walks the whole document, so a drag that only moves the caret
/// must not touch it.
#[derive(Default)]
struct PreviewSlot {
    source: String,
    active_line_start: Option<usize>,
    preview: PreviewDocument,
    started: bool,
}

impl PreviewSlot {
    fn get(&mut self, source: &str, active_line_start: Option<usize>) -> &PreviewDocument {
        let stale =
            !self.started || self.active_line_start != active_line_start || self.source != source;
        if stale {
            // Refreshed rather than rebuilt: the preview keeps its mapping a
            // line at a time, so this recounts the lines that changed and
            // leaves the rest (技術検証 7.1).
            self.preview.refresh(source, active_line_start);
            self.source.clear();
            self.source.push_str(source);
            self.active_line_start = active_line_start;
            self.started = true;
        }
        &self.preview
    }
}

/// Frames slower than this are worth a line of their own in the log.
const SLOW_FRAME_MS: f64 = 8.0;
/// Enough samples to take a median from without the buffer growing while idle.
const FRAME_SAMPLE_LIMIT: usize = 256;

/// The cost on the far side of [`refresh_pane`], which its own timings cannot
/// see.
///
/// Everything measured around a keystroke stops the moment the tile images are
/// handed to Slint. Uploading those images to the GPU and drawing the scene
/// happens afterwards, inside the renderer. A tile is as tall as the pane, so at
/// full screen one is about two megabytes, and a second live pane would double
/// whatever that costs.
///
/// The first attempt reported the sum and the maximum, and that was useless: the
/// sum grew with how long the user paused, and the maximum reached 1.5 seconds
/// on frames that uploaded nothing at all. What separates work from waiting is
/// the *fastest* frame of a burst, so the minimum and the median are what matter
/// here, and the slow ones are recorded individually rather than averaged into
/// the rest.
#[derive(Default)]
struct FrameProbe {
    started: Option<Instant>,
    last_ended: Option<Instant>,
    /// Render spans, in milliseconds.
    spans: Vec<f64>,
    /// Gaps between one frame ending and the next beginning. If the renderer is
    /// throttled or the window is occluded, the time goes here, not into a span.
    gaps: Vec<f64>,
}

/// What one log line says about the frames since the previous one.
#[derive(Default, Clone, Copy)]
struct FrameSummary {
    frames: usize,
    min_ms: f64,
    median_ms: f64,
    max_ms: f64,
    slow: usize,
    gap_median_ms: f64,
}

impl FrameProbe {
    fn begin(&mut self) {
        let now = Instant::now();
        if let Some(ended) = self.last_ended
            && self.gaps.len() < FRAME_SAMPLE_LIMIT
        {
            self.gaps.push(elapsed_ms(ended));
        }
        self.started = Some(now);
    }

    fn end(&mut self) {
        let Some(started) = self.started.take() else {
            return;
        };
        if self.spans.len() < FRAME_SAMPLE_LIMIT {
            self.spans.push(elapsed_ms(started));
        }
        self.last_ended = Some(Instant::now());
    }

    /// Summarize the frames since the last call, and reset. Reported against the
    /// refresh that follows them, because that is what caused them.
    fn take(&mut self) -> FrameSummary {
        let median = |values: &mut Vec<f64>| {
            values.sort_by(f64::total_cmp);
            values.get(values.len() / 2).copied().unwrap_or(0.0)
        };
        let summary = FrameSummary {
            frames: self.spans.len(),
            min_ms: self.spans.iter().copied().fold(f64::INFINITY, f64::min),
            max_ms: self.spans.iter().copied().fold(0.0, f64::max),
            slow: self.spans.iter().filter(|ms| **ms > SLOW_FRAME_MS).count(),
            median_ms: median(&mut self.spans),
            gap_median_ms: median(&mut self.gaps),
        };
        self.spans.clear();
        self.gaps.clear();
        FrameSummary {
            // No frames at all reads better as zero than as infinity.
            min_ms: summary.min_ms.min(summary.max_ms),
            ..summary
        }
    }
}

/// A rendered tile, held under the fingerprint of what it drew.
///
/// The cache is content addressed: the key is the fingerprint, which says what
/// the pixels are and nothing about where they go. An edit changes one block, so
/// every other tile on screen is found again under the same key however far the
/// layout has slid it, and the placement comes from the current plan each time.
#[derive(Clone)]
struct CachedTile {
    image: Image,
    /// The pixels behind that image, kept so the buffer can be drawn into again
    /// once the image is gone (`take_spare`). **A tile is about two megabytes,
    /// and allocating that costs more than drawing it does** (技術検証 7.8).
    pixels: SharedPixelBuffer<Rgba8Pixel>,
    /// Where this image was last placed on the flow axis, for ranking evictions
    /// by distance.
    last_flow: i32,
}

/// What one pane draws with.
///
/// **Bound to the thread that made it.** These are DirectWrite and Direct2D
/// objects; whether several threads may lay text out at once is exactly the
/// question 技術検証 7.3 leaves open. Nothing here can be handed to a worker
/// until that is understood, which is why it is kept apart from [`PaneView`]
/// rather than mixed with it (ペイン分割設計 7.3).
struct PaneGraphics {
    engine: TextEngine,
    tiles: BTreeMap<u64, CachedTile>,
    /// Buffers of evicted tiles, waiting to be drawn into again.
    ///
    /// **Held for their memory, not their contents.** Two megabytes is more
    /// expensive to allocate than to draw (技術検証 7.8), and pages already
    /// touched cost nothing to write again. Taking one back is safe whatever
    /// else may still hold it: `make_mut_slice` copies rather than share
    /// (Slint's `SharedVector::detach`), so the worst case is what allocating
    /// cost anyway.
    spare: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    /// Pixel bytes of the images handed to Slint since the last log line. Every
    /// one of them is a new texture the renderer has to upload.
    uploaded_bytes: usize,
}

impl PaneGraphics {
    /// Hand written because the engine has to be told its writing mode; every
    /// other field is empty until the first refresh.
    fn new(mode: WritingMode) -> Self {
        Self {
            engine: TextEngine::new(mode),
            tiles: BTreeMap::new(),
            spare: Vec::new(),
            uploaded_bytes: 0,
        }
    }
}

/// What one pane knows that is not a graphics object.
///
/// Plain data throughout, and deliberately so: this is the half that could one
/// day be built on a worker thread and handed back (ペイン分割設計 7).
#[derive(Default)]
struct PaneView {
    /// **This pane's own, not the other's.** The preview reveals the Markdown
    /// of the line the caret is on, and the panes keep separate carets (3.7),
    /// so they look at different lines and the texts differ by that one line.
    /// Sharing one would make each pane reveal the other's line.
    preview_slot: PreviewSlot,
    /// Where this pane is meant to be looking, until the writer looks
    /// elsewhere (要件 8.5).
    ///
    /// **Because a restored scroll does not survive the window settling.** The
    /// pane is laid out several times before it stands still — the zoom, the
    /// split and the extent all arrive after the tabs do — and every one of
    /// those changes what a pixel offset means. A byte does not change, so the
    /// view is put back on every refresh until something the writer did says
    /// where to look instead.
    top_anchor: Option<ViewAnchor>,
    /// Caret and selection in the shown text's UTF-16, so a scroll can re-clip
    /// the selection without going back through the document model.
    caret_utf16: Option<u32>,
    /// **A list, because a selection can be a rectangle** (要件 7.1): one run
    /// for an ordinary one, one per layout line for a rectangle, none for no
    /// selection at all.
    selection_utf16: Vec<(u32, u32)>,
    /// The same runs in the document's bytes.
    ///
    /// **Written where the engine is**, because cutting a rectangle into runs
    /// is a question about the lines it laid out. Copying, cutting and deleting
    /// read it back rather than asking again: the pane has been laid out by the
    /// time the writer can press a key, and asking twice could answer twice.
    selection_source: Vec<(usize, usize)>,
    preedit_range: Option<(u32, u32)>,
}

/// A place in the text that a pane is holding its view on (要件 8.5).
#[derive(Clone, Copy)]
struct ViewAnchor {
    /// The source byte to keep at the near edge.
    byte: usize,
    /// Where the caret was when this was set. **The anchor stands until the
    /// caret moves** — that is the writer saying where to look, and it is one
    /// comparison rather than a flag every caret-moving path would have to
    /// remember to clear.
    caret: Option<usize>,
}

/// One editing pane.
struct Pane {
    graphics: PaneGraphics,
    view: PaneView,
    /// The shell this pane is showing, if the tab in front of it is one
    /// (追加要件 Terminal).
    ///
    /// **Held by the pane as well as by the tab** because this is what the
    /// refresh reaches: every path that draws a pane goes through
    /// [`refresh_pane`], and none of them carries a tab.
    terminal: Option<TerminalView>,
    /// The shell along the foot of this pane (追加要件 Terminal: サブTerminal).
    ///
    /// **Only under a document.** A pane already showing a shell has a draft
    /// down there instead, and a draft is text the window keeps.
    below: Option<TerminalView>,
    /// Whether the strip is showing at all.
    ///
    /// **Kept apart from what is in it**: closing the strip must not end the
    /// shell in it, because a shell closed by putting a panel away takes its
    /// running command with it.
    below_open: bool,
    below_height: f32,
    /// The direction this pane's engine was built for.
    ///
    /// **A pane no longer *is* a direction.** The tab in front of it decides
    /// (要件 7.2's four modes belong to the tab), so a pane has to be able to
    /// change, and this is what it is changing from.
    mode: WritingMode,
}

/// One shell being shown, and everything about how it is being looked at.
///
/// **Two of these can be on one pane** — the tab's own, and the one along the
/// foot of a pane showing a document — so none of it can live on the pane.
struct TerminalView {
    /// Shared rather than copied: carrying the tab to another pane carries the
    /// running shell.
    session: Rc<RefCell<TerminalSession>>,
    /// The screen, drawn in bands of rows.
    ///
    /// **A terminal repaints everywhere at once, and most of it is unchanged.**
    /// One image of the whole screen costs 15ms to draw and 9MB to hand to the
    /// renderer at the size a maximized pane asks for — and that is paid per
    /// chunk of output, far more often than once a frame. Cut into bands and
    /// keyed by what is in them, a keystroke redraws the one band it touched.
    bands: TerminalBands,
    /// What the writer has selected with the mouse.
    ///
    /// **In rows counted from the start of the history**, so that output
    /// arriving underneath does not move it and scrolling does not lose it.
    selection: Option<TerminalSelection>,
    /// How far back through the scrollback this is looking, in rows. **Zero is
    /// the bottom**, which is where a terminal lives; anything else means the
    /// writer is reading something that has already gone by.
    looking: usize,
    /// How much scrollback there was at the last refresh.
    ///
    /// **Because a view held back has to hold still.** Output arriving while
    /// the writer reads pushes rows into the history behind them; counted from
    /// the bottom, the same number would show different lines every time.
    history: usize,
    /// What the IME is composing, before it is anything the shell has heard of
    /// (要件 7.2's problem, in a terminal).
    ///
    /// **Drawn by us, at the cursor.** A conversion is not typing yet — the
    /// shell must not see it, and the writer must — so it lives here until it
    /// is committed and only then goes up the pipe as text.
    preedit: String,
    /// How often this shell has been drawn, and since when (2026-09-08追加).
    ///
    /// **描いた回数は、秒ごとにひとことだけ残す。**帯に切ってあるのは升目を
    /// 描く費用を抑えるためで、効いているかどうかは頻度でしか読めない——
    /// 1描画1行では読めない量になり、何も書かなければ推測しか残らない。
    drawn: u32,
    counted_since: Option<Instant>,
}

impl TerminalView {
    fn new(session: TerminalSession) -> Self {
        Self::sharing(&Rc::new(RefCell::new(session)))
    }

    fn sharing(session: &Rc<RefCell<TerminalSession>>) -> Self {
        Self {
            session: session.clone(),
            bands: TerminalBands::default(),
            selection: None,
            looking: 0,
            history: 0,
            preedit: String::new(),
            drawn: 0,
            counted_since: None,
        }
    }

    /// One more draw. Answers with how many there were in the second just
    /// ended, when one has.
    ///
    /// **秒をまたいだときだけ答える**ので、ログは1行/秒より増えない。静かな
    /// シェルは描かれないので、1行も出ない。
    fn drew(&mut self, now: Instant) -> Option<(u32, f32)> {
        let since = *self.counted_since.get_or_insert(now);
        self.drawn += 1;
        let span = now.duration_since(since);
        if span < Duration::from_secs(1) {
            return None;
        }
        let counted = self.drawn;
        self.drawn = 0;
        self.counted_since = Some(now);
        Some((counted, span.as_secs_f32()))
    }
}

/// Which of a pane's two shells something is about (追加要件 Terminal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TerminalSpot {
    /// The tab in front of the pane.
    Front,
    /// The strip along the foot of it.
    Below,
}

/// The images a terminal pane is showing, and the geometry they were drawn for.
#[derive(Default)]
struct TerminalBands {
    /// Columns, rows and the cell size. **Any of them moving invalidates every
    /// band**, because each of them changes where every cell is.
    shape: Option<(usize, usize, u32, u32, u64)>,
    /// The bands, **keyed by where they sit in the history** rather than by
    /// where they are on screen.
    ///
    /// **That is what makes scrolling cheap** (書き手の報告, 2026-09-06:
    /// スクロールが遅い). Keyed by position on screen, moving the view by one
    /// row changes the content of every band and the whole screenful is drawn
    /// again — 15ms and nine megabytes per notch of the wheel. Keyed by the
    /// rows themselves, a scroll leaves every band it did not uncover alone,
    /// and only the two half-bands at the edges are redrawn.
    bands: BTreeMap<usize, (u64, Image)>,
}

/// Two corners of what the writer has selected in a shell (追加要件 Terminal).
///
/// **Where the drag began and where it is now**, not a normalized rectangle: a
/// selection dragged upwards is the same selection dragged downwards, and which
/// end moves is the writer's business.
#[derive(Clone, Copy, PartialEq, Debug)]
struct TerminalSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl TerminalSelection {
    /// The two corners in reading order.
    fn ordered(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The columns of `row` that are inside this selection, if any.
    ///
    /// **A selection runs like text, not like a rectangle**: a drag across three
    /// rows takes the end of the first, all of the second and the start of the
    /// third — which is what copying a wrapped command line has to mean.
    fn columns_in(self, row: usize, columns: usize) -> Option<(usize, usize)> {
        let ((first_row, first_column), (last_row, last_column)) = self.ordered();
        if row < first_row || row > last_row {
            return None;
        }
        let from = if row == first_row { first_column } else { 0 };
        let to = if row == last_row {
            last_column.min(columns)
        } else {
            columns
        };
        (from < to).then_some((from, to))
    }

    fn is_empty(self) -> bool {
        self.anchor == self.head
    }
}

/// How tall the strip opens (追加要件 Terminal), before anybody drags it.
///
/// **Ten rows and a little**: enough to read a command's answer without asking
/// the document above it to give up half the page.
const TERMINAL_BELOW_HEIGHT: f32 = 200.0;

/// The least the strip can be dragged to, and the most.
const TERMINAL_BELOW_RANGE: (f32, f32) = (60.0, 900.0);

/// How many rows one band holds.
///
/// **Eight is a compromise between two costs.** A band is the smallest thing
/// that can be redrawn, so fewer rows means less work per keystroke; but every
/// band is an image the renderer uploads on its own, and a screen cut into
/// eighty of them is eighty textures for a scroll that changes all of them.
const TERMINAL_BAND_ROWS: usize = 8;

impl Pane {
    fn new(mode: WritingMode) -> Self {
        Self {
            graphics: PaneGraphics::new(mode),
            view: PaneView::default(),
            mode,
            terminal: None,
            below: None,
            below_open: false,
            below_height: TERMINAL_BELOW_HEIGHT,
        }
    }

    /// One of this pane's two shells, if it has that one.
    fn shell(&mut self, spot: TerminalSpot) -> Option<&mut TerminalView> {
        match spot {
            TerminalSpot::Front => self.terminal.as_mut(),
            TerminalSpot::Below => self.below.as_mut(),
        }
    }

    /// Set the direction this pane draws in, and say whether it moved.
    ///
    /// **The engine is rebuilt when it moves.** An engine is built for one
    /// writing mode and keeps it (`directwrite_render`'s module doc), and every
    /// block measurement and tile it holds is in that direction — so a change
    /// costs the whole document being measured again, about what a zoom costs
    /// (6.8). That is a price for a deliberate switch, never for a keystroke.
    ///
    /// **And the pane's view goes with it**, the preview slot and the held
    /// anchor included: this is a new pane, not the old one turned. Anything
    /// put in the view has to be put there after the direction is settled, not
    /// before (要件 8.5's restore learned this the hard way).
    fn set_mode(&mut self, mode: WritingMode) -> bool {
        if self.mode == mode {
            return false;
        }
        // **The shells survive the rebuild — both of them.** What is thrown
        // away here is the engine and everything measured in the old direction;
        // a running process is none of that.
        //
        // **The strip was not on this list, and that was the bug** (書き手の
        // 報告, 2026-09-06「一度入らなくなると二度と入りません」). Changing the
        // writing direction dropped the shell along the foot of the pane while
        // the window went on showing the strip: the keys had somewhere to go
        // and nothing to reach, so they vanished, and nothing ever put it back.
        let terminal = self.terminal.take();
        let below = self.below.take();
        let (open, height) = (self.below_open, self.below_height);
        *self = Pane::new(mode);
        self.terminal = terminal;
        self.below = below;
        self.below_open = open;
        self.below_height = height;
        true
    }
}

/// How the drawing keeps up with a burst of keystrokes (要件 2).
///
/// **The text is never held back. Only the drawing is.** Auto-repeat delivers
/// characters faster than a long paragraph can be laid out — an edit near the
/// head of a 35,000 character paragraph costs 78ms of wrap search (技術検証
/// 7.4) against the 30 or so characters a second a held key sends — so drawing
/// every one of them means the event loop never runs. The window stops
/// repainting, the caret stops blinking, and the editor looks frozen while the
/// keys are in fact all registering.
///
/// So a draw that cost real time is followed by a wait as long as it took, and
/// the keystrokes that arrive during it are drawn together at the end of it.
/// **Half the time to drawing and half to the window** is what that ratio buys;
/// the writer sees the text arrive a few characters at a time instead of not at
/// all.
#[derive(Default)]
struct EditPace {
    /// When the last edit was drawn, and what it cost.
    drawn: Option<Instant>,
    took: f64,
    /// The catch-up draw waiting to happen. **Started, never restarted** — a
    /// timer put off by each keystroke is a timer a held key never lets fire.
    timer: Rc<Timer>,
    waiting: bool,
    /// Edits made since the last draw and not yet on screen.
    ///
    /// **Counted, because the pacing is otherwise invisible.** A faster editor
    /// and an editor that stopped drawing look the same from outside; this says
    /// how many keystrokes one draw was carrying (the perf log's `held=`).
    held: u32,
}

impl EditPace {
    /// How long the drawing owes the window before it may draw again.
    ///
    /// Zero while the drawing is cheap, which is every ordinary document: there
    /// is nothing to smooth out and a wait would only add lateness.
    fn owed(&self) -> Duration {
        if self.took <= PACE_FREE_MS {
            return Duration::ZERO;
        }
        let wait = Duration::from_secs_f64(self.took / 1000.0).min(PACE_MAX);
        let since = self.drawn.map(|at| at.elapsed()).unwrap_or(PACE_MAX);
        wait.saturating_sub(since)
    }

    fn drew(&mut self, took: f64) {
        self.drawn = Some(Instant::now());
        self.took = took;
        self.waiting = false;
    }

    /// How many edits this draw is carrying, counting from the last one.
    fn take_held(&mut self) -> u32 {
        std::mem::take(&mut self.held)
    }
}

/// A draw at or under this costs nothing worth pacing (ms).
///
/// **Under a frame.** The editor draws a keystroke in about 2.5ms on an
/// ordinary document (6.9); what this is for is the document where one costs
/// tens of milliseconds.
const PACE_FREE_MS: f64 = 8.0;

/// However long a draw took, the writer sees the next one within this.
const PACE_MAX: Duration = Duration::from_millis(200);

/// The three layers, side by side (ペイン分割設計 2).
///
/// **Everything here is the app's or one pane's.** The document's own half —
/// its text, its file, its history, its counts — is in [`OpenDocument`], which
/// is where a thing goes when it is a fact about the text rather than about
/// how the text is being shown.
struct RenderCache {
    /// Indexed by [`PaneId::index`], like everything else that has one of
    /// something per pane.
    panes: Vec<Pane>,
    /// Shared with the rendering notifier, which runs between refreshes.
    frames: Rc<RefCell<FrameProbe>>,
    /// How long the last push of the source text into the horizontal pane took.
    /// Only Split keeps that pane alive, so this isolates what Split adds.
    source_push_ms: Option<f64>,
    perf_log: PerfLog,
    /// This run's trace. Kept beside the perf log rather than inside it: one is
    /// about cost and the other about what happened (see `diag.rs`).
    diag: DiagLog,
    /// How each pane is keeping up with a run of keystrokes. Indexed the same
    /// way, and grown and shrunk with `panes`.
    pace: Vec<EditPace>,
    /// The outline the left pane is currently showing (要件 7.7).
    ///
    /// **Kept only to know when not to draw it again.** Replacing a repeater's
    /// model rebuilds every row, and the outline is looked at on every refresh
    /// — a keystroke that adds no heading must not cost a rebuild of the list
    /// (6.18's third case, met before it happens).
    outline_drawn: Vec<document::Heading>,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self {
            // 要件 6.3: the editing area is one pane at the least, and that is
            // what an editor with nothing restored opens as. It draws the way
            // the tab in front of it says, so the mode it starts in is the
            // tab's business rather than the pane's.
            panes: vec![Pane::new(WritingMode::Horizontal)],
            frames: Rc::default(),
            source_push_ms: None,
            perf_log: PerfLog::default(),
            diag: DiagLog::default(),
            pace: vec![EditPace::default()],
            outline_drawn: Vec::new(),
        }
    }
}

/// One run's diagnostic log.
///
/// The file holds **this run and nothing else**. It used to be appended to for
/// ever, so reading it meant deleting it first and then reproducing whatever was
/// being looked at; a log that has to be cleared before it is useful is one more
/// step between a symptom and its cause.
///
/// The previous run is moved aside to `perf_log.prev.txt` rather than discarded,
/// because the question asked of these numbers is almost always "is this better
/// than before", and a plain truncation answers it by throwing the before away.
///
/// Best effort throughout. The log exists to explain the editor, so it must
/// never be able to stop it: every failure sets `stopped` and is otherwise
/// ignored.
#[derive(Default)]
struct PerfLog {
    file: Option<File>,
    /// Set once nothing more will be written — the file could not be opened, or
    /// the line limit was reached.
    stopped: bool,
    lines: usize,
}

impl PerfLog {
    /// Begin this run's log, moving the previous run's file aside.
    ///
    /// Called once at startup rather than lazily on the first refresh, so the
    /// header is at the top of the file even when the run ends before drawing
    /// anything, and so an empty log distinguishes "never started" from "started
    /// and measured nothing".
    fn start(&mut self, header: &str) {
        let path = diag::beside_executable(PERF_LOG_PATH);
        let previous = diag::beside_executable(PERF_LOG_PREVIOUS_PATH);
        let _ = std::fs::rename(&path, &previous);
        match File::create(&path) {
            Ok(file) => self.file = Some(file),
            Err(_) => {
                self.stopped = true;
                return;
            }
        }
        self.write(header);
    }

    fn write(&mut self, line: &str) {
        if self.stopped {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.lines >= PERF_LOG_LINE_LIMIT {
            // Said in the file, not just by the file ending. A log that simply
            // stops looks like a crash.
            let _ = writeln!(file, "session stopped=line-limit lines={}", self.lines);
            self.stopped = true;
            return;
        }
        self.lines += 1;
        let _ = writeln!(file, "{line}");
    }
}

impl RenderCache {
    /// Write one line to the performance log, best effort.
    fn log_perf(&mut self, line: &str) {
        self.perf_log.write(line);
    }

    /// Write one line to this run's trace, best effort.
    fn log_diag(&mut self, category: &str, fields: &str) {
        self.diag.write(category, fields);
    }
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
///
/// Not a readable date — there is no calendar formatting in `std` and this is
/// not worth a dependency. It is here to tell two runs apart and to line the log
/// up against anything else recorded at the time; when the run happened is the
/// file's own timestamp.
fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// The first line of a run's log: what was measured, and under what build.
///
/// The build profile is the one figure here that cannot be recovered from the
/// numbers themselves, and it is the one that most changes how to read them —
/// a debug build spends several times longer in the string handling, so a
/// debug log compared against a release one shows a regression that is not
/// there.
fn perf_log_header(window: &AppWindow) -> String {
    // The horizontal pane's spec, because the header is one line and the two
    // panes may be set differently (要件 9). What it is for is the font size
    // and the build profile.
    let here = PaneId::FIRST;
    let typography = pane_typography(window, here);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "session started={} profile={profile} zoom={zoom} font={font:.1} \
         space={space:.2} lead={lead:.2} head={head:.2}",
        epoch_seconds(),
        zoom = here.zoom(window),
        font = typography.font_size,
        space = typography.character_spacing,
        lead = typography.line_spacing,
        head = typography.size_scale(1),
    )
}

fn main() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    // 要件 8.5: the arrangement comes back — which pane held which tabs, in
    // what order, in which of the four modes, and how the area was divided.
    // Nothing there is a first run, and something unreadable is a session from
    // another build. Neither is worth a word on screen: the editor opens the
    // way it would have anyway.
    //
    // **Read before the panes are published** (2026-09-06). It is the session
    // that says how many panes there are, and a row nobody wrote is a pane
    // nobody can be in.
    let session = app_data::app_directory()
        .as_deref()
        .and_then(app_data::read_session);
    // The panes, published before anything can read one. Each row carries its
    // own number and direction; everything else in it is either what a refresh
    // has put there or what the pane itself reports once it exists.
    //
    // 要件 6.3: **one pane at the least.** A session that named none, or that
    // could not be read, opens the way a first run does.
    //
    // **The tree and the list have to agree.** A pane the tree does not name
    // gets no area, and a pane with no area is a strip of tabs nobody can
    // reach; a leaf naming a pane that does not exist is an empty rectangle.
    // Either way the session is one from another build, and the editor opens
    // on one pane — with every tab the session held gathered into it, because
    // an arrangement that cannot be restored is still somebody's work.
    let restored_panes = session
        .as_ref()
        .map(|session| {
            let named = Layout::decode(&session.layout)
                .map(|layout| layout.panes())
                .unwrap_or_default();
            let agrees = named.len() == session.panes.len()
                && (0..named.len()).all(|pane| named.contains(&pane));
            if agrees { named.len() } else { 1 }
        })
        .unwrap_or(1)
        .clamp(1, MAX_PANES);
    publish_panes(&window, restored_panes);
    // 要件 8.1: whatever was being edited when the last run ended comes back
    // before anything is drawn, so the first thing on screen is the writer's
    // own text rather than something they have to clear away first.
    let restored = restore_tabs(&window);
    let was_restored = !restored.is_empty();
    // 要件 7.7: what was opened most recently, carried across runs with the
    // rest of the arrangement.
    let remembered = match &session {
        Some(session) => session.recent.clone(),
        None => Vec::new(),
    };
    // 要件 5.1: the folders worked in before this one. A session written by a
    // build from before there was a history has none, so the folder being
    // restored is put at the head below — the way back has to start somewhere.
    let visited = match &session {
        Some(session) => session.folders.clone(),
        None => Vec::new(),
    };
    // E1の④: 探した語と置き換えた語。**この版より前のセッションには無い**ので、
    // そのときは空の列から始まる——履歴が無いことは、探せないことではない。
    let (needles, replacements) = match &session {
        Some(session) => (session.needles.clone(), session.replacements.clone()),
        None => (Vec::new(), Vec::new()),
    };
    // 要件 5.1, 8.5: the folder the writer was working in, and which of its
    // folders they had open. A folder that has since gone is simply not there.
    let work_folder = match &session {
        Some(session) => WorkFolder {
            root: session.folder.clone().filter(|folder| folder.is_dir()),
            expanded: session.expanded.iter().cloned().collect(),
            // Nothing is selected at startup: a selection is where the writer
            // last put their hand, and they have not put it anywhere yet.
            selected: None,
            // 要件 7.7: **a folder that has gone is no scope at all.** The same
            // rule the work folder above follows — nothing watches the disk
            // between two runs.
            searching: session
                .search_folder
                .clone()
                .filter(|folder| folder.is_dir()),
        },
        None => WorkFolder::default(),
    };
    window.set_tree_open(session.as_ref().is_none_or(|session| session.tree_shown));
    // 追加要件 2026-09-06: the width the writer dragged the left pane to. A
    // session that says nothing says 0, which is not a width anybody chose —
    // the window keeps its own default then (the same rule the zoom uses).
    if let Some(width) = session.as_ref().map(|session| session.tree_width)
        && width > 0
    {
        window.set_tree_width(tree_width_from(width));
    }
    // 要件 9: the zoom each pane was left at. **Before the tabs are opened**,
    // so the first thing drawn is already the size the writer was reading at
    // rather than something that jumps once. A pane the session says nothing
    // about arrives as 0, which `zoom_from` reads as the default.
    if let Some(session) = &session {
        for (id, stored) in PaneId::all(&window).into_iter().zip(session.panes.iter()) {
            id.set_zoom(&window, stored.zoom);
        }
    }
    // 要件 8.5: and where the window itself was. **Before it is shown**, so it
    // opens where it belongs rather than moving there in front of the writer.
    if let Some(session) = &session {
        restore_window_place(&window, session.place, session.maximized);
    }
    let (tabs, arrangement) = open_session(&window, session, restored);
    let opening = tabs
        .of(focused_pane(&window))
        .current()
        .map(PaneTab::document)
        .expect("every strip is given a tab");
    let initial = opening.text.borrow().clone();
    let tab_list = Rc::new(RefCell::new(tabs));
    // Every restored document is already marked; the window shows the one that
    // is in front.
    window.set_document_edited(opening.text.edited());
    show_document_title(&window, &opening.file.borrow());
    // One caret per pane, and what each pane has in front of it. No pane
    // follows another's caret, and every callback a pane raises reaches its
    // own state and its own document through here.
    //
    // **As many as the window has rows** (2026-09-06). Both of these start with
    // one pane in them — that is what an editor with nothing restored is — and
    // a restored session may have more. They used to be left at one, and then
    // every pane read the first pane's state and drew with the first pane's
    // engine: four panes showing one document, and the caret of three of them
    // going nowhere. **Nothing said so**, because both of these answer a number
    // they do not hold with the pane they do (a stale number arrives with a
    // keystroke and must not stop the editor), so the fault only surfaced when
    // closing the tabs of four panes took the list of states below zero.
    let pane_states = PaneStates::new(&opening);
    let render_cache = Rc::new(RefCell::new(RenderCache::default()));
    for _ in 1..PaneId::count(&window) {
        pane_states.add(&opening);
        render_cache.borrow_mut().add_pane(WritingMode::Horizontal);
    }
    // **Every pane starts on its own tab** (要件 8.5).
    //
    // The states above are made from one document because that is what
    // dividing a pane does (要件 6.4): the new pane opens on what the old one
    // was showing. A restored session is the other case — each pane comes back
    // with a strip of its own, and the tab in front of it is what that pane was
    // reading. Without this both panes showed the focused pane's document while
    // their strips named two different files.
    //
    // Everything the pane holds about that tab comes across: the caret and the
    // selection, how far it had scrolled, and which of 要件 7.2's four modes it
    // was in. The positions are rounded to character boundaries first — they
    // were written by another run, and nothing says the text is the same length
    // now (6.7's rule, applied across runs rather than across panes).
    for id in PaneId::all(&window) {
        let Some(tab) = tab_list.borrow().of(id).current().cloned() else {
            continue;
        };
        pane_states.show(id, &tab.document);
        let mut state = tab.view.state.clone();
        {
            let text = tab.document.text.borrow();
            let rounded = |byte: usize| floor_char_boundary(&text, byte);
            state.caret_source_byte = state.caret_source_byte.map(rounded);
            state.selection_anchor_source_byte = state.selection_anchor_source_byte.map(rounded);
        }
        let caret = state.caret_source_byte;
        *pane_states.of(id).borrow_mut() = state;
        id.set_scroll(&window, tab.view.scroll);
        id.set_shows_preview(&window, tab.view.preview);
        set_pane_direction(&window, &render_cache, id, tab.view.vertical);
        // 要件 8.5: the passage it was looking at, which is what actually puts
        // the view back — the scroll above is pixels, and the zoom, the split
        // and the extent all change what those mean before the window stands
        // still (`ViewAnchor`).
        //
        // **After the direction, not before.** A pane told to run the other way
        // is built again from nothing (`Pane::set_mode`), and everything the
        // view held goes with it — which is what this did when it was set
        // first: the session says vertical, the pane starts horizontal, and the
        // anchor was gone before the first refresh could use it.
        hold_view(&render_cache, id, tab.view.top, caret);
    }
    // One bundle for everything a tab operation touches. The editing callbacks
    // keep their own handles; this exists so a switch does not need six.
    let layout = Rc::new(RefCell::new(arrangement));
    // **The searching thread reaches the editor through the event loop and
    // nowhere else** (要件 2). Everything the editor holds is `Rc`, so none of
    // it can go to another thread; a weak handle to the window can, and ringing
    // one callback on it is the whole of what that thread is allowed to do.
    // What to collect and how to draw it stays on this side.
    let weak = window.as_weak();
    let wake_for_search = move || {
        let _ = weak.upgrade_in_event_loop(|window| {
            window.invoke_folder_search_finished();
        });
    };
    // 要件 12: the quick draft's window, once it has been asked for. **Held
    // here rather than by the window itself**, so that asking twice brings the
    // one that is open forward.
    // 要件 11.6: the editor's own list of strings, kept apart from Windows'
    // clipboard. It belongs to the run rather than to a document, because a
    // kill taken from one document is worth putting into another.
    let kill_ring: Rc<RefCell<Kills>> = Rc::default();
    let draft = Rc::new(RefCell::new(quick_draft::QuickDraftWindow::default()));
    let live = Live {
        states: pane_states.clone(),
        folder: Rc::new(RefCell::new(work_folder)),
        tree_paths: Rc::new(RefCell::new(Vec::new())),
        results: Rc::new(RefCell::new(Vec::new())),
        recent: Rc::new(RefCell::new(remembered)),
        recent_folders: Rc::new(RefCell::new(visited)),
        find_terms: Rc::new(RefCell::new(find::Terms::restored(needles))),
        replace_terms: Rc::new(RefCell::new(find::Terms::restored(replacements))),
        layout: layout.clone(),
        pending: Rc::new(RefCell::new(None)),
        close_run: Rc::new(RefCell::new(None)),
        cache: render_cache.clone(),
        tabs: tab_list.clone(),
        writer: Rc::new(FileWriter::start()),
        searcher: Rc::new(Searcher::start(wake_for_search)),
        searched: Rc::new(Cell::new(0)),
    };
    render_cache
        .borrow_mut()
        .perf_log
        .start(&perf_log_header(&window));
    let diag_started = render_cache.borrow_mut().diag.start();
    // **From here on, a panic says so in the log.** The editor has no console
    // to print to, so without this the file simply stopped and the last line
    // before the stop was all there was to go on.
    render_cache.borrow().diag.catch_panics();
    let diag_path = {
        let cache = render_cache.borrow();
        match cache.diag.path() {
            Some(path) => path.display().to_string(),
            None => "(なし)".to_owned(),
        }
    };
    let size = window.window().size();
    render_cache.borrow_mut().log_diag(
        "session",
        &format!(
            "started={started} profile={profile} file={diag_path} \
             window={width}x{height} scale={scale:.2} mode={mode} split={split} \
             zoom={zoom} limit={MAX_DOCUMENT_CHARACTERS}",
            started = diag_started.stamp(),
            profile = if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            width = size.width,
            height = size.height,
            scale = window.window().scale_factor(),
            mode = focused_pane(&window).index(),
            split = PaneId::count(&window),
            // Every pane's (要件 9). A session line that named one number
            // would say nothing about the panes the writer was not in.
            zoom = PaneId::all(&window)
                .iter()
                .map(|id| id.zoom(&window).to_string())
                .collect::<Vec<String>>()
                .join(","),
        ),
    );
    // 要件 7.3.2: what an inline object does to the line it stands in, measured
    // rather than assumed. The tests assert the properties the design needs;
    // these two lines put the actual numbers where the answers to "how much"
    // belong. Two layouts at startup, and nothing is drawn.
    for (name, vertical) in [("across", false), ("down", true)] {
        let status = inline_object_status(name, vertical);
        render_cache.borrow_mut().log_diag("probe", &status);
    }
    // **In the log rather than on the paper.** This used to be a caption above
    // the vertical pane, from the round that was proving DirectWrite could set
    // a column at all; it is a measurement, and measurements live where the
    // rest of them do.
    let vertical_layout = directwrite_status();
    render_cache
        .borrow_mut()
        .log_diag("probe", &vertical_layout);
    if was_restored {
        // The carets went into the panes with their tabs above; this is what
        // came back, for the log.
        let caret = pane_states
            .of(focused_pane(&window))
            .borrow()
            .caret_source_byte;
        render_cache.borrow_mut().log_diag(
            "work",
            &format!("restored bytes={} caret={caret:?}", initial.len()),
        );
    }
    publish_tabs(&window, &live);
    // 書き手の報告 2026-09-07: **どのペインも、いま見ているものが最初の場所。**
    // 復元はタブを直接並べるので、ここで書き入れておかないと最初の`戻る`が
    // 起動時のファイルへ帰れない。
    for id in PaneId::all(&window) {
        let showing = live.tabs.borrow().of(id).current().cloned();
        if let Some(tab) = showing {
            note_navigation(&live, id, &tab);
        }
    }
    // Nothing is on screen until the tree has handed out an area. The window
    // reports its own the moment the editing area exists, and this is the state
    // until then.
    place_panes(&window, &layout.borrow());
    publish_left(&window, &live);
    // 要件 5.1: the folder being restored is the newest one worked in, whether
    // or not the session that named it also carried a history — a build from
    // before there was one leaves the way back to be started here.
    let restored = live.folder.borrow().root.clone();
    if let Some(root) = restored {
        remember_folder(&live, &root);
    }
    publish_folder_history(&window, &live);

    // Bracket the renderer so the log can separate our own work from what Slint
    // does with the images afterwards.
    let frames = render_cache.borrow().frames.clone();
    if let Err(error) =
        window
            .window()
            .set_rendering_notifier(move |state, _graphics| match state {
                RenderingState::BeforeRendering => frames.borrow_mut().begin(),
                RenderingState::AfterRendering => frames.borrow_mut().end(),
                _ => {}
            })
    {
        // Only GPU-accelerated renderers report this. Not being able to measure
        // is worth a line in the log, but nothing here depends on it.
        render_cache
            .borrow_mut()
            .log_perf(&format!("frame probe unavailable: {error:?}"));
    }

    for id in PaneId::all(&window) {
        refresh_pane(
            &window,
            &render_cache,
            &opening,
            id,
            &initial,
            None,
            None,
            PaneSelection::default(),
            "",
        );
    }

    // 要件 8.1: the edited text has to survive the process stopping, so it is
    // written on a schedule of its own rather than when something asks for it.
    // Repeated rather than restarted on each keystroke, because the second rule
    // is about a run of typing that never pauses — there is no keystroke to
    // hang it on.
    let work_timer = Timer::default();
    let weak = window.as_weak();
    let timer_live = live.clone();
    work_timer.start(TimerMode::Repeated, WORK_COPY_TICK, move || {
        if let Some(window) = weak.upgrade() {
            write_work_copy_if_due(&window, &timer_live);
            collect_write_results(&window, &timer_live);
        }
    });

    // 要件 8.3: another program writing the open file has to be noticed rather
    // than found out about by overwriting it.
    let watch_timer = Timer::default();
    let weak = window.as_weak();
    let watch_live = live.clone();
    watch_timer.start(TimerMode::Repeated, EXTERNAL_CHECK_TICK, move || {
        if let Some(window) = weak.upgrade() {
            check_external_change(&window, &watch_live);
        }
    });

    // Both of these run from the event loop rather than from the callback.
    //
    // **The button that raises them lives inside the tab strip's repeater**, and
    // switching or closing replaces the model that button is part of — and then
    // closing asks a question with a modal dialog, which pumps Windows messages
    // while this handler is still on the stack. Closing a tab with nothing
    // unsaved survived that; closing one that put a dialog on top of it froze.
    // Handing the work to the event loop lets the click finish first.
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_tab_selected(move |pane, index| {
        let index = index.max(0) as usize;
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                // Logged before the switch, not inside it: `switch_to_tab` says
                // nothing when the tab asked for is already the one in front,
                // and "the click never arrived" and "the click arrived and
                // there was nothing to do" look the same from outside.
                live.cache
                    .borrow_mut()
                    .log_diag("tab", &format!("choose pane={} at={index}", id.log_name()));
                switch_to_tab(&window, &live, id, index);
            }
        });
    });

    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_tab_stepped(move |pane, backwards| {
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        // **The same door the click goes through**, and put off to the next
        // tick for the same reason: the switch republishes the strip (6.18).
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                step_tab(&window, &live, id, backwards);
            }
        });
    });

    // 追加要件 2026-09-06: a name was typed into a tab.
    //
    // **The same act the tree's rename is** (要件 5.2): `move_entry` moves the
    // file, brings every open document that pointed at it along, and re-titles
    // the tabs. What is new is only where the name was typed.
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_tab_renamed(move |pane, index, typed| {
        let index = index.max(0) as usize;
        let id = PaneId::from_index(pane);
        let typed = typed.to_string();
        let weak = weak.clone();
        let live = tab_live.clone();
        // Put off to the next tick, like every other tab command: renaming
        // republishes the strip, and the field the name was typed into is
        // inside the row that gets rebuilt (6.18).
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                rename_tab(&window, &live, id, index, &typed);
            }
        });
    });

    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_tab_closed(move |pane, index| {
        let index = index.max(0) as usize;
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                close_tab(&window, &live, id, index);
            }
        });
    });

    // 要件 6.5: a carried tab was let go. Put off to the next tick like every
    // other tab command, and for the same reason: the strip is republished, and
    // the element the drag ran in is one of the rows that is rebuilt (6.18).
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_tab_dropped(move |pane, from, to, at_x, at_y| {
        let from = from.max(0) as usize;
        let to = to.max(0) as usize;
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                drop_tab(&window, &live, id, from, to, (at_x, at_y));
            }
        });
    });

    // Both of these are the same shape as the one above, and for the same
    // reason: they are asked for from a row of the pane's menu, and closing the
    // last tab of a pane takes that pane off screen — which rebuilds the
    // repeater the row was clicked in (6.18).
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_close_others(move |pane| {
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                start_close_run(&window, &live, id, true);
            }
        });
    });

    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_close_all(move |pane| {
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let live = tab_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                start_close_run(&window, &live, id, false);
            }
        });
    });

    // 6.18 again, and this time it is the buttons of the question itself: the
    // answer takes the overlay down, and taking an element down from inside its
    // own callback is what froze the editor there. So the answer is acted on
    // from the event loop rather than from the click.
    let weak = window.as_weak();
    let answer_live = live.clone();
    window.on_question_answered(move |choice| {
        let weak = weak.clone();
        let live = answer_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                answer_question(&window, &live, choice);
            }
        });
    });

    // The editing area's own size, which only the window knows. Everything the
    // tree hands out is measured in it, so a change re-places the panes.
    let weak = window.as_weak();
    let area_layout = layout.clone();
    let area_states = pane_states.clone();
    let area_cache = render_cache.clone();
    window.on_editor_area_resized(move || {
        if let Some(window) = weak.upgrade() {
            place_panes(&window, &area_layout.borrow());
            for id in PaneId::all(&window) {
                if !id.is_shown(&window) {
                    continue;
                }
                let showing = area_states.document(id);
                let source = showing.text.borrow().clone();
                let state = area_states.of(id);
                refresh_pane_from_state(&window, &area_cache, &showing, id, &state, &source);
            }
        }
    });

    // Dragging a boundary moves it by a fraction of what it divides, so the
    // ratio is what is stored and a window that is resized keeps the proportion
    // (要件 6.4, 8.5).
    let weak = window.as_weak();
    let drag_layout = layout.clone();
    let drag_cache = render_cache.clone();
    window.on_boundary_moved(move |index, dx, dy| {
        if let Some(window) = weak.upgrade() {
            let index = index.max(0) as usize;
            let mut layout = drag_layout.borrow_mut();
            let area = editor_area(&window);
            let (moved, total) = {
                let (_, boundaries) = layout.place(area);
                let Some(boundary) = boundaries.get(index) else {
                    return;
                };
                if boundary.split == Split::SideBySide {
                    (dx, area.width)
                } else {
                    (dy, area.height)
                }
            };
            if total <= 0.0 {
                return;
            }
            let by = moved / total;
            let took = layout.move_boundary(index, by);
            place_panes(&window, &layout);
            drag_cache.borrow_mut().log_diag(
                "layout",
                &format!("drag at={index} dx={dx:.1} dy={dy:.1} by={by:.4} took={took}"),
            );
        }
    });

    // Back to one pane, and it is the one the writer is in. The other pane's
    // tabs are untouched: a strip belongs to its pane whether or not the pane is
    // on screen.
    // 要件 6.4: bring the arrangement back to this pane alone.
    //
    // **The other panes' tabs come here rather than closing.** A pane holding
    // tabs is never taken away underneath them — that is why 要件 6.4 removes a
    // pane when its *last tab* closes, and why the pane menu has no way to
    // close a pane that still has some. Collapsing gathers instead.
    let weak = window.as_weak();
    let undivide_live = live.clone();
    window.on_close_others_requested(move || {
        let weak = weak.clone();
        let live = undivide_live.clone();
        // **Put off to the next tick**, like every tab command and for the same
        // reason (6.18): taking panes away destroys repeater instances, and the
        // menu row that asked is inside one of them.
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                close_other_panes(&window, &live, focused_pane(&window));
            }
        });
    });

    // 要件 6.4: 編集ペインの入れ替え。The arrangement stays and the panes move,
    // so everything each pane holds — its tabs, its carets — goes with it.
    let weak = window.as_weak();
    let swap_live = live.clone();
    window.on_swap_requested(move |towards| {
        if let Some(window) = weak.upgrade() {
            let Some(towards) = Towards::from_index(towards) else {
                return;
            };
            let here = focused_pane(&window);
            // **The pane that way**, by the rule the arrow keys already use
            // (要件 11.3). Two panes had no question to answer — the other one
            // was the only candidate — and with a list the answer has to be
            // something the writer can see: the pane they are pointing at.
            let placed = placed_panes(&window);
            let Some(next) = neighbour(&placed, here.index() as usize, towards) else {
                return;
            };
            swap_live
                .layout
                .borrow_mut()
                .swap(here.index() as usize, next);
            // **The keyboard goes with the pane, not with the place.** What
            // moved is this pane, and the writer is in it.
            window.set_focused_pane(next as i32);
            swap_live.cache.borrow_mut().log_diag(
                "layout",
                &format!(
                    "swap {}<->{} {towards:?}",
                    here.log_name(),
                    PaneId::from_index(next as i32).log_name()
                ),
            );
            after_layout_change(&window, &swap_live);
        }
    });

    wiring::wire_left_panel(&window, &live);

    wiring::wire_find(&window, &live);

    wiring::wire_terminal_look(&window, &live, &render_cache);

    wiring::wire_word_modes(&window, &live);

    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_new_tab(move |pane| {
        if let Some(window) = weak.upgrade() {
            new_tab(&window, &tab_live, PaneId::from_index(pane));
        }
    });

    // 追加要件 2026-09-07: the three words on a New Tab. **`New File` makes
    // nothing** — the tab is already carrying the untitled document — and
    // `Open File` is not here at all, because the dialog already opens into
    // whatever tab is yielding (`open_path_in_pane`).
    let weak = window.as_weak();
    let answer_live = live.clone();
    window.on_pane_new_file(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let asking = answer_live
                .tabs
                .borrow()
                .of(id)
                .current()
                .is_some_and(|tab| tab.empty);
            if asking {
                answer_new_tab(&window, &answer_live, id, None);
            } else {
                new_file_tab(&window, &answer_live, id);
            }
        }
    });

    let weak = window.as_weak();
    let answer_live = live.clone();
    let switch_live = live.clone();
    // 追加要件 2026-09-07: **走っているタブの接続先を替える。**タブはそのまま、
    // 中のシェルだけが別のものになる——走っていたものは終わる（端末のタブを
    // 閉じるのと同じ重さで、書き手がそう言ったのだからそうする）。
    window.on_pane_switch_shell(move |pane, shell| {
        if let Some(window) = weak.upgrade() {
            let chosen = shell_at(&window, shell);
            switch_shell(&window, &switch_live, PaneId::from_index(pane), chosen);
        }
    });

    let weak = window.as_weak();
    window.on_pane_new_terminal(move |pane, shell| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let chosen = shell_at(&window, shell);
            open_terminal(&window, &answer_live, id, chosen);
        }
    });

    // The one change to a strip that the writer does not make to the strip
    // itself: a document's unsaved marker moving. `SharedText` knows when that
    // happens but nothing else, so it asks the window, and the window asks
    // here.
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_republish_tabs(move || {
        if let Some(window) = weak.upgrade() {
            publish_tabs(&window, &tab_live);
        }
    });

    // From here to the mode switch, **every callback a pane raises carries its
    // number** and nothing else tells the two apart. `PaneId::from_index` turns
    // it back into the one type that knows the difference, so becoming a
    // repeater is a matter of the pane passing `index` instead of a literal
    // (ペイン分割設計 5).
    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_pane_scroll_changed(move |pane, offset| {
        if let Some(window) = weak.upgrade() {
            refresh_after_scroll(&window, &cache, PaneId::from_index(pane), offset);
        }
    });

    // 要件 8.5: where the view actually went when it was told to move.
    let cache = render_cache.clone();
    window.on_pane_scroll_applied(move |pane, asked, went| {
        let id = PaneId::from_index(pane);
        let told = format!("apply to={asked:.0} went={went:.0}");
        cache
            .borrow_mut()
            .log_diag(&format!("scroll.{}", id.diag_suffix()), &told);
    });

    // Resizing changes how long a line may be — a column's height on one side,
    // a line's width on the other — and with it every block measurement.
    // Dragging a window edge produces a change per frame, so the relayout waits
    // for the drag to stop. **One timer per pane**, because a divider resizes
    // the panes on both sides of it at once and a shared timer would let the
    // second cancel the first.
    //
    // **A list, grown as panes arrive** (2026-09-06). It was an array of two,
    // which is what a third pane found: the pane number is an index here like
    // everywhere else, and this was the one place still holding a pair.
    let timers: Rc<RefCell<Vec<Rc<Timer>>>> = Rc::default();
    let reveal_timer = Rc::new(Timer::default());
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_resized(move |pane| {
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let states = states.clone();
        let cache = cache.clone();
        let timer = {
            let mut timers = timers.borrow_mut();
            while timers.len() <= id.index() as usize {
                timers.push(Rc::new(Timer::default()));
            }
            timers[id.index() as usize].clone()
        };
        timer.start(TimerMode::SingleShot, RESIZE_SETTLE, move || {
            if let Some(window) = weak.upgrade() {
                let started = Instant::now();
                let showing = states.document(id);
                let source = showing.text.borrow().clone();
                refresh_pane_from_state(&window, &cache, &showing, id, &states.of(id), &source);
                let (extent, blocks, tile_flow) = {
                    let mut cache = cache.borrow_mut();
                    let engine = &cache.pane(id).graphics.engine;
                    (
                        engine.line_extent(),
                        engine.block_count(),
                        engine.tile_flow_size(),
                    )
                };
                let name = id.log_name();
                cache.borrow_mut().log_perf(&format!(
                    "resize pane={name} extent={extent} blocks={blocks} \
                     tile_flow={tile_flow} total={:.2}",
                    elapsed_ms(started)
                ));
            }
        });
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_selection_start(move |pane, x, y, extend| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let phase = if extend {
                SelectionPhase::Extend
            } else {
                SelectionPhase::Begin
            };
            let state = states.of(id);
            update_pane_selection(&window, &document, &state, &cache, id, x, y, phase);
        }
    });

    // E3の②: 行そのものを動かす・写す・消す。**番号は窓と1対1**で、増えたときに
    // 片方だけ直すことがないよう、対応はここに1つだけ書く。
    let weak = window.as_weak();
    let line_live = live.clone();
    window.on_pane_line_edit(move |pane, what| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let what = match what {
            0 => document::LineEdit::MoveBefore,
            1 => document::LineEdit::MoveAfter,
            2 => document::LineEdit::CopyBefore,
            3 => document::LineEdit::CopyAfter,
            _ => document::LineEdit::Drop,
        };
        edit_lines(&window, &line_live, PaneId::from_index(pane), what);
    });

    // E3の③: Enter。**継ぐものはRustが決める**——画面と同じ行の見方を使うため。
    let weak = window.as_weak();
    let enter_live = live.clone();
    window.on_pane_enter(move |pane| {
        if let Some(window) = weak.upgrade() {
            enter_in_pane(&window, &enter_live, PaneId::from_index(pane));
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_selection_update(move |pane, x, y| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let state = states.of(id);
            let phase = SelectionPhase::Update;
            update_pane_selection(&window, &document, &state, &cache, id, x, y, phase);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_selection_end(move |pane, x, y| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let state = states.of(id);
            let phase = SelectionPhase::End;
            update_pane_selection(&window, &document, &state, &cache, id, x, y, phase);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let typed_live = live.clone();
    window.on_pane_text_input(move |pane, text, committed| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let text = text.as_str();
            // **A conversion committed into a shell is typing, not editing.**
            // The keys themselves never reach here — the pane sends those
            // straight on — but what an IME hands over arrives by this door
            // like any other text (追加要件 Terminal).
            let shell = cache
                .borrow_mut()
                .pane(id)
                .terminal
                .as_ref()
                .map(|shell| shell.session.clone());
            if let Some(session) = shell {
                // **A conversion is typing; the clipboard is a paste.** The
                // difference is the bracketed-paste markers, and a shell shows
                // what arrives between them highlighted until the next key.
                if committed {
                    session.borrow_mut().type_text(text);
                } else {
                    session.borrow_mut().paste(text);
                }
                {
                    let mut borrowed = cache.borrow_mut();
                    if let Some(shell) = borrowed.pane(id).shell(TerminalSpot::Front) {
                        shell.looking = 0;
                        shell.selection = None;
                    }
                }
                // **The field is emptied here.** It holds what was pasted, and
                // left alone it would hand the whole of it over again with the
                // next paste (the editing paths empty it for the same reason).
                id.set_ime_buffer(&window, "");
                cache.borrow_mut().log_diag(
                    "terminal",
                    &format!("typed pane={} spot=Front len={}", id.log_name(), text.len()),
                );
                refresh_terminal(&window, &cache, id, TerminalSpot::Front);
                return;
            }
            // 追加要件 2026-09-07: **打ち始めたら、そのタブは文書になる。**
            // New Tab は「何になるか」を訊いているだけで、答えの一つは
            // 「ここに書く」である。文書は最初からその下にあるので、
            // 一字目はふつうにそこへ入る。
            answer_new_tab(&window, &typed_live, id, None);
            let document = states.document(id);
            insert_pane_text(&window, id, &document, &states, &cache, text, false);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let typed_live = live.clone();
    window.on_pane_tab(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            answer_new_tab(&window, &typed_live, id, None);
            let document = states.document(id);
            let indent = TAB_INDENT;
            insert_pane_text(&window, id, &document, &states, &cache, indent, true);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let typed_live = live.clone();
    window.on_pane_preedit_changed(move |pane, text| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            // **A conversion over a shell is not the document's** (追加要件
            // Terminal). It is drawn in the grid at the cursor and goes up the
            // pipe only when it is committed, which arrives as ordinary text.
            let composing = {
                let mut borrowed = cache.borrow_mut();
                match borrowed.pane(id).shell(TerminalSpot::Front) {
                    Some(shell) => {
                        shell.preedit = text.to_string();
                        true
                    }
                    None => false,
                }
            };
            if composing {
                refresh_terminal(&window, &cache, id, TerminalSpot::Front);
                return;
            }
            // 変換の途中も文字である（追加要件 2026-09-07）——New Tab の面が
            // 覆っていると、変換中の字がどこにも見えないことになる。
            if !text.is_empty() {
                answer_new_tab(&window, &typed_live, id, None);
            }
            let document = states.document(id);
            let state = states.of(id);
            set_pane_preedit(&window, id, &document, &state, &cache, text.as_str());
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_backspace(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            delete_adjacent_grapheme(&window, id, &document, &states, &cache, true);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_delete(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            delete_adjacent_grapheme(&window, id, &document, &states, &cache, false);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let reveal = reveal_timer.clone();
    window.on_pane_move(move |pane, direction, extend_selection| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let state = states.of(id);
            move_pane_caret(
                &window,
                id,
                &document,
                &state,
                &cache,
                &reveal,
                direction,
                extend_selection,
            );
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let reveal = reveal_timer.clone();
    window.on_pane_home_end(move |pane, to_end, document_edge, extend| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let state = states.of(id);
            move_pane_to_line_edge(
                &window,
                id,
                &document,
                &state,
                &cache,
                &reveal,
                to_end,
                document_edge,
                extend,
            );
        }
    });

    // 要件 6.4: 「フォーカス中の編集ペインを、右方向または下方向へ分割できる」。
    // **The focused pane is divided, wherever it sits** — the pane keeps its
    // half and the new one takes the other, which is all the tree has ever
    // done (`Layout::divide`).
    let weak = window.as_weak();
    let divide_live = live.clone();
    window.on_divide_requested(move |side_by_side| {
        if let Some(window) = weak.upgrade() {
            let split = if side_by_side {
                Split::SideBySide
            } else {
                Split::Stacked
            };
            divide_pane(&window, &divide_live, focused_pane(&window), split);
        }
    });

    // Zoom and the three typography controls share one timer: they all ask for
    // the same whole-document relayout, so a press of any of them should cancel
    // a relayout another was still waiting to run.
    let spec_timer = Rc::new(Timer::default());
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_pane_zoom(move |pane, notches| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let percent = id.zoom(&window) + notches * ZOOM_STEP;
            zoom_pane(&window, &states, &cache, &timer, id, percent);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_pane_zoom_set(move |pane, percent| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            zoom_pane(&window, &states, &cache, &timer, id, percent);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_pane_zoom_reset(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            zoom_pane(&window, &states, &cache, &timer, id, ZOOM_DEFAULT);
        }
    });

    // 要件 9's values, two sheets end to end: the horizontal one's and then the
    // vertical one's. **Three models rather than a property per value** — the
    // panel draws rows of the same shape, and the engine reads whichever sheet
    // belongs to the pane it is setting. Rust owns them: a list written in the
    // window is a list the window could not be told to change.
    let numbers = Rc::new(VecModel::from(vec![0; 2 * SHEET_NUMBERS]));
    let palette = Rc::new(VecModel::from(vec![Color::default(); 2 * SHEET_COLOURS]));
    let sheet_fonts = Rc::new(VecModel::from(vec![SharedString::new(); 2 * SHEET_FONTS]));
    reset_settings(&numbers, &palette, &sheet_fonts);
    window.set_sheet_stride(SHEET_NUMBERS as i32);
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(sheet_fonts.clone()));
    // 要件 9: what the last run was set to, before anything is drawn with it.
    if let Some(directory) = app_data::app_directory()
        && let Some(values) = app_data::read_settings(&directory)
    {
        apply_settings(&window, &numbers, &palette, &sheet_fonts, &values);
    }
    // 要件 7.9: **設定を読んだあとで、名指されたファイルを読む。**設定は場所と
    // 色しか覚えていないので、語はここで初めて手に入る。
    open_word_modes(&window, &live);

    wiring::wire_typography(
        &window,
        &pane_states,
        &render_cache,
        spec_timer,
        numbers,
        palette,
        sheet_fonts,
    );

    // The IME lays its candidate list out from the composition font, so it has
    // to be told which pane took the input (技術検証 7.2).
    let weak = window.as_weak();
    window.on_ime_vertical_requested(move |vertical| {
        if let Some(window) = weak.upgrade() {
            ime::set_vertical(&window, vertical);
        }
    });

    // 書き手の報告 2026-09-08: **縦書きのペインで設定画面に名前を打つと、IMEが
    // 縦のままだった。**合成フォントは窓に1つしか無く（技術検証 7.2）、最後に
    // 言ったのがペインなら縦のままである——**欄はペインではない。**
    //
    // **返したときはペインの向きへ戻す**：どこへ戻るかを知っているのはRustで、
    // 欄はそれを知らない。
    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.global::<Ime>().on_field_focus(move |taken| {
        if let Some(window) = weak.upgrade() {
            let vertical = !taken && focused_pane(&window).vertical(&window);
            ime::set_vertical(&window, vertical);
            let told = format!("field taken={taken} vertical={vertical}");
            cache.borrow_mut().log_diag("ime", &told);
        }
    });

    // 書き手の報告 2026-09-09:「検索バーを出してIMEを起動すると、IMEが入力
    // ボックスと重なる」。**直ったかどうかを画面の外から確かめられるように
    // しておく**（技術検証 6.31）。欄が「配置が済んだあとに位置を送り直した」
    // ことがここに残る——出ていなければ時計が動いていない、出ているのに重なる
    // なら送った座標のほうが違う、と切り分けられる。
    let cache = render_cache.clone();
    window.global::<Ime>().on_settled(move || {
        cache.borrow_mut().log_diag("ime", "settle");
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_undo(move |pane, forwards| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            undo_in_pane(&window, id, &document, &states, &cache, forwards);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_preview_toggled(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            id.set_shows_preview(&window, !id.shows_preview(&window));
            let source = document.text.borrow().clone();
            refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &source);
        }
    });

    // The other half of 要件 7.2's four modes. Turning a tab costs its pane's
    // engine being built again and the whole document measured (`set_pane_
    // direction`), which is why it is a button somebody presses and not
    // something that happens while they type.
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_direction_toggled(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            // 追加要件 2026-09-06: **a shell has one direction.** The buttons
            // are dimmed over one, and the answer is the same wherever else the
            // ask could come from.
            if cache.borrow_mut().pane(id).terminal.is_some() {
                return;
            }
            let document = states.document(id);
            set_pane_direction(&window, &cache, id, !id.vertical(&window));
            // The anchor the up and down keys hold on to is a coordinate in the
            // layout that has just stopped existing.
            states.of(id).borrow_mut().preferred_line = None;
            let source = document.text.borrow().clone();
            refresh_pane_from_state(&window, &cache, &document, id, &states.of(id), &source);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_select_all(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            select_whole_document(&window, &document, &states.of(id), &cache, id);
        }
    });

    // 追加要件 2026-09-08: **Copy means the shell's selection when a shell is
    // in front.** The row sends the same signal either way — what is selected
    // on screen is what a writer means by Copy — and this is the end that knows
    // which of the two is showing. Cut never reaches here over a shell: what a
    // shell has written is not the writer's to take away, so that row is not
    // offered (`ui/editor-pane.slint`).
    let weak = window.as_weak();
    let copy_live = live.clone();
    window.on_pane_copy(move |pane, cut| {
        if let Some(window) = weak.upgrade() {
            let live = &copy_live;
            let id = PaneId::from_index(pane);
            if id.shows_terminal(&window) {
                copy_terminal_selection(&window, live, id, TerminalSpot::Front);
                return;
            }
            let document = live.states.document(id);
            copy_selection(&window, id, &document, &live.states, &live.cache, cut);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_mark_toggled(move |pane, rectangular| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            toggle_mark(&window, id, &document, &states, &cache, rectangular);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let kills = kill_ring.clone();
    window.on_pane_kill(move |pane, what| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            kill_ring_action(&window, id, &document, &states, &cache, &kills, what);
        }
    });

    let weak = window.as_weak();
    let focus_cache = render_cache.clone();
    window.on_pane_focus_moved(move |pane, towards| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            move_focus(&window, &focus_cache, id, towards);
        }
    });

    // 追加要件 Terminal: the shells the menu offers, in the order the settings
    // file names them — the first is the default.
    //
    // **設定を読んだあと**（追加要件 2026-09-08）。起動時の一覧は組み込みの
    // 三つで、設定ファイルはそれを丸ごと置き換えることがある——先に並べて
    // しまうと、書き手が足したシェルがどのメニューにも出ない。
    publish_shells(&window);

    // 追加要件 Terminal. **Opened in the pane the writer is in**, like every
    // other new tab: which pane is the writer's business and they said it by
    // clicking.
    let terminal_live = live.clone();
    let weak = window.as_weak();
    let default_cache = render_cache.clone();
    // 追加要件 2026-09-07: which shell everything that does not ask opens.
    window.on_default_shell_chosen(move |shell| {
        if let Some(window) = weak.upgrade() {
            let most = offered_shells(&window).len().saturating_sub(1) as i32;
            window.set_default_shell(shell.clamp(0, most));
            save_settings(&window, &default_cache);
        }
    });

    // 追加要件 2026-09-08: 自動退避のOn/Off（要件 8.1）。**切った瞬間に、
    // 置いてあるものを全部持っていく**——「Offの場合は状態を維持しない」の
    // 「維持しない」は、これから書かないことではなく、いま在るものが残らない
    // ことである。
    //
    // 追加要件 2026-09-09: **入れ直した瞬間も、同じだけのことをする。**Offの
    // あいだに書いた文字の旗は下りたままなので、Onへ戻しただけでは次の打鍵まで
    // 一文字も退避されない——入れたはずのものが働いていない状態で、しかも
    // 画面には何も出ない。
    let weak = window.as_weak();
    let autosave_live = live.clone();
    let autosave_cache = render_cache.clone();
    window.on_autosave_toggled(move |wanted| {
        if let Some(window) = weak.upgrade() {
            window.set_autosave(wanted);
            if wanted {
                keep_work_copies_again(&autosave_live);
            } else {
                discard_all_work_copies(&autosave_live);
            }
            save_settings(&window, &autosave_cache);
        }
    });

    // 要件 7.8（2026-09-09）: ルビを本文の字数に数えるか。**数え直しは要らない**
    // ——両方の数はもう出ている（`DocumentStats::ruby_characters`）ので、
    // 変わるのは status bar の引き算だけである。
    let weak = window.as_weak();
    let ruby_live = live.clone();
    let ruby_cache = render_cache.clone();
    window.on_count_ruby_toggled(move |wanted| {
        if let Some(window) = weak.upgrade() {
            window.set_count_ruby(wanted);
            // 切り替えたその場で数を書き直す——次の打鍵まで前の数が残っていたら、
            // 設定が効いていないのと同じに見える。
            let document = ruby_live.active(&window);
            let source = document.text.borrow().clone();
            update_status(
                &window,
                focused_pane(&window),
                &document,
                &source,
                &[],
                None,
            );
            save_settings(&window, &ruby_cache);
        }
    });

    let weak = window.as_weak();
    window.on_terminal_requested(move |shell| {
        if let Some(window) = weak.upgrade() {
            let chosen = shell_at(&window, shell);
            open_terminal(&window, &terminal_live, focused_pane(&window), chosen);
        }
    });

    let weak = window.as_weak();
    let key_live = live.clone();
    window.on_pane_terminal_key(move |pane, text, number, control, alt, shift| {
        if let Some(window) = weak.upgrade() {
            send_terminal_key(
                &window,
                &key_live,
                PaneId::from_index(pane),
                TerminalSpot::Front,
                text.as_str(),
                number,
                control,
                alt,
                shift,
            );
        }
    });

    let weak = window.as_weak();
    let scroll_live = live.clone();
    window.on_pane_terminal_scrolled(move |pane, delta| {
        if let Some(window) = weak.upgrade() {
            scroll_terminal(
                &window,
                &scroll_live,
                PaneId::from_index(pane),
                TerminalSpot::Front,
                delta,
            );
        }
    });

    // 追加要件 Terminal: the strip along the foot of a pane.
    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_toggled(move |pane| {
        if let Some(window) = weak.upgrade() {
            toggle_below(&window, &below_live, PaneId::from_index(pane));
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_resized(move |pane, height| {
        if let Some(window) = weak.upgrade() {
            resize_below(&window, &below_live, PaneId::from_index(pane), height);
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_key(move |pane, text, number, control, alt, shift| {
        if let Some(window) = weak.upgrade() {
            send_terminal_key(
                &window,
                &below_live,
                PaneId::from_index(pane),
                TerminalSpot::Below,
                text.as_str(),
                number,
                control,
                alt,
                shift,
            );
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_scrolled(move |pane, delta| {
        if let Some(window) = weak.upgrade() {
            scroll_terminal(
                &window,
                &below_live,
                PaneId::from_index(pane),
                TerminalSpot::Below,
                delta,
            );
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_selection(move |pane, x, y, phase| {
        if let Some(window) = weak.upgrade() {
            let phase = match phase {
                0 => SelectionPhase::Begin,
                1 => SelectionPhase::Update,
                _ => SelectionPhase::End,
            };
            select_in_terminal(
                &window,
                &below_live.cache,
                PaneId::from_index(pane),
                TerminalSpot::Below,
                x,
                y,
                phase,
            );
        }
    });

    // 追加要件 2026-09-06: the right button's rows.
    let weak = window.as_weak();
    let clean_live = live.clone();
    window.on_pane_close_clean(move |pane| {
        if let Some(window) = weak.upgrade() {
            close_clean_tabs(&window, &clean_live, PaneId::from_index(pane));
        }
    });

    let weak = window.as_weak();
    let path_live = live.clone();
    window.on_pane_copy_path(move |pane, index| {
        if let Some(window) = weak.upgrade() {
            copy_tab_path(
                &window,
                &path_live,
                PaneId::from_index(pane),
                index.max(0) as usize,
            );
        }
    });

    // 要件 11.3 の手つきで上下に移る（追加要件 Terminal）。
    let weak = window.as_weak();
    window.on_pane_below_focus(move |pane, into| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            if into {
                // **The strip takes the keyboard by being asked for it**, the
                // same way the panes do: a count the element watches, because
                // an element created focused never sees a change (6.11).
                id.update_screen(&window, |screen| {
                    screen.below_focus_generation += 1;
                });
            } else {
                restore_editor_focus(&window);
            }
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_preedit(move |pane, text| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            {
                let mut borrowed = below_live.cache.borrow_mut();
                let Some(shell) = borrowed.pane(id).shell(TerminalSpot::Below) else {
                    return;
                };
                shell.preedit = text.to_string();
            }
            below_live.cache.borrow_mut().log_diag(
                "terminal",
                &format!(
                    "preedit pane={} spot=Below len={}",
                    id.log_name(),
                    text.len()
                ),
            );
            refresh_terminal(&window, &below_live.cache, id, TerminalSpot::Below);
        }
    });

    let weak = window.as_weak();
    let below_live = live.clone();
    window.on_pane_below_text(move |pane, text, committed| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let session = below_live
                .cache
                .borrow_mut()
                .pane(id)
                .shell(TerminalSpot::Below)
                .map(|shell| shell.session.clone());
            let Some(session) = session else {
                return;
            };
            if committed {
                session.borrow_mut().type_text(text.as_str());
            } else {
                session.borrow_mut().paste(text.as_str());
            }
            {
                let mut borrowed = below_live.cache.borrow_mut();
                if let Some(shell) = borrowed.pane(id).shell(TerminalSpot::Below) {
                    shell.looking = 0;
                    shell.selection = None;
                }
            }
            below_live.cache.borrow_mut().log_diag(
                "terminal",
                &format!("typed pane={} spot=Below len={}", id.log_name(), text.len()),
            );
            // The field is emptied here, for the reason the pane's own is.
            id.update_screen(&window, |screen| screen.below_buffer = SharedString::new());
            refresh_terminal(&window, &below_live.cache, id, TerminalSpot::Below);
        }
    });

    let weak = window.as_weak();
    let drafted = window.as_weak();
    // 書き手の報告 2026-09-07: the strip draws the draft's own lines behind the
    // field, and only this side can cut a string into lines.
    window.on_pane_below_draft_edited(move |pane, text| {
        let Some(window) = drafted.upgrade() else {
            return;
        };
        let id = PaneId::from_index(pane);
        let lines = ModelRc::new(VecModel::from(draft_lines(text.as_str())));
        // **欄が持っている字をそのまま行にも書く。**行だけ書き戻すと、双方向の
        // 束縛がまだ届いていない場合に、こちらが読んだ古い字を欄へ押し返して
        // しまう——打った一字が消える形の壊れ方になる。
        id.update_screen(&window, |screen| {
            screen.below_draft = text.clone();
            screen.below_draft_lines = lines.clone();
        });
    });

    let below_live = live.clone();
    window.on_pane_below_sent(move |pane| {
        if let Some(window) = weak.upgrade() {
            send_draft(&window, &below_live, PaneId::from_index(pane));
        }
    });

    // 書き手の報告 2026-09-08: 下段のシェルの選択を写す（要件 11.2）。
    // **上の面の`on_pane_copy`とは別の口**：あちらは前に出ているタブが
    // シェルかどうかで写す先を選ぶが、こちらは訊くまでもなく下段である。
    let weak = window.as_weak();
    let below_copy_live = live.clone();
    window.on_pane_below_copy(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            copy_terminal_selection(&window, &below_copy_live, id, TerminalSpot::Below);
        }
    });

    // **Rung from the reading thread** (要件 2), and all it says is that
    // something arrived.
    //
    // **Not drawn on the spot.** A program redrawing itself sends its frame in
    // whatever pieces the pipe hands over, and drawing at each of them costs a
    // frame's work for a fraction of a frame's change — and shows the half of
    // the picture that has arrived. One wait of a frame's length gathers the
    // rest, and `drain` applies all of it before anything is drawn.
    let weak = window.as_weak();
    let woken_live = live.clone();
    let paint = Rc::new(Timer::default());
    let painting = Rc::new(Cell::new(false));
    window.on_terminal_woken(move || {
        if painting.get() {
            return;
        }
        painting.set(true);
        let weak = weak.clone();
        let live = woken_live.clone();
        let painting = painting.clone();
        paint.start(
            TimerMode::SingleShot,
            Duration::from_millis(TERMINAL_FRAME_MS),
            move || {
                painting.set(false);
                if let Some(window) = weak.upgrade() {
                    refresh_terminal_panes(&window, &live);
                }
            },
        );
    });

    wiring::wire_open_and_draft(&window, &live, &draft);

    wiring::wire_saving(&window, &live);

    // What the shell asked for: a double click on a file associated with the
    // editor, or a path typed after its name. Last of everything in `main`, so
    // that the file is the tab in front — the session came back above, and what
    // the writer just asked for should not open behind what they left.
    //
    // The arguments are logged before any of them is judged. A file that would
    // not open and a shell that named no file look the same from the outside —
    // the editor comes up on yesterday's session either way — and this line is
    // what tells them apart.
    let handed_over: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    render_cache.borrow_mut().log_diag(
        "session",
        &format!("arguments count={} list={handed_over:?}", handed_over.len()),
    );
    //
    // Into the pane the window says is focused, rather than through
    // [`focused_pane`]. `place_panes` above has already moved that flag onto a
    // pane the arrangement actually holds, and it is the one the mode names —
    // while the question `focused_pane` asks, whether the pane has been given
    // an area, is answered "no" for both of them until the window is shown.
    let opening_pane = PaneId::from_index(window.get_focused_pane());
    for path in paths_from_command_line() {
        live.cache.borrow_mut().log_diag(
            "file",
            &format!(
                "command-line exists={} pane={} path={}",
                path.is_file(),
                opening_pane.log_name(),
                path.display()
            ),
        );
        open_path_in_pane(&window, &live, opening_pane, &path, Opening::Kept);
    }

    // 要件 8.1・8.4: **自動退避を切ってあるときだけ、閉じる前に訊く**
    // （2026-09-08）。退避が働いていれば、閉じても未保存の中身は次の起動で
    // 戻ってくるので訊く理由が無い——切ってあるときだけ、`×`は取り返しの
    // つかない操作になる。
    let weak = window.as_weak();
    let closing_live = live.clone();
    window.window().on_close_requested(move || {
        let Some(window) = weak.upgrade() else {
            return CloseRequestResponse::HideWindow;
        };
        let live = &closing_live;
        // **問いが立っているあいだは、もう一度は訊かない。**`×`を続けて
        // 押されても重ねられないのは、`pending`が1つしか持てないからで
        // （`Live::pending`）、ここで返さないと下の`ask_question`が
        // 立っているほうを黙って捨てる。
        if live.pending.borrow().is_some() {
            return CloseRequestResponse::KeepWindowShown;
        }
        // 追加要件 2026-09-09（残り2）: **最後の退避を、まだ訊けるうちに試す。**
        // これまでは窓が閉じたあとに行列を待ち切っていたので、書けなかった
        // ときには誰にも言えないまま終わっていた——打った直後に`×`を押した
        // 数秒ぶんが、静かに消える。**成功すれば何も起きない**（普段はここで
        // 一往復、数ミリ秒）。
        if ask_about_the_last_work_copy(&window, live) {
            return CloseRequestResponse::KeepWindowShown;
        }
        if window.get_autosave() {
            return CloseRequestResponse::HideWindow;
        }
        let unsaved = open_documents(live)
            .iter()
            .filter(|document| document.text.edited())
            .count();
        if unsaved == 0 {
            return CloseRequestResponse::HideWindow;
        }
        live.cache
            .borrow_mut()
            .log_diag("work", &format!("close asked unsaved={unsaved}"));
        ask_question(
            &window,
            live,
            Question::CloseWindow,
            format!(
                "保存していない文書が{unsaved}件あります。\n\n\
                 自動退避を切ってあるので、閉じると元に戻せません。"
            ),
            &["すべて保存して閉じる", "破棄して閉じる", "キャンセル"],
            1,
        );
        CloseRequestResponse::KeepWindowShown
    });

    let outcome = window.run();
    // 要件 8.5: the arrangement as the writer left it, including a boundary
    // moved without anything else happening. The views are taken out of the
    // panes first, because a caret and a scroll live there until they are.
    sync_active_tab(&window, &live);
    // 要件 8.1: **最後の2秒を落とさない**（2026-09-08）。退避は入力が止まって
    // 2秒、続いていても5秒ごとで、その時計は窓が閉じれば止まる——打った直後に
    // `×`を押すと、そのぶんだけがどこにも書かれないまま消えていた。ここで
    // 一度に書けば、時計が回りきらなかったぶんが行列に乗る。
    write_work_copy_now(&window, &live);
    write_session(&window, &live);
    // 要件 12.4: and the draft, which is otherwise waiting on a two-second
    // timer that the end of the process will not let run.
    draft.borrow().store_now();
    // **そして、書き上がるまで待つ**（2026-09-08）。行列に乗せただけで
    // プロセスが終われば、乗せたことに意味は無い。待つのは残っているぶんだけ
    // で、普段は0か1件——`sync_all`込みで2.4〜5.9msの世界である。
    let waited = Instant::now();
    let left = live.writer.finish();
    let failed = left.iter().filter(|result| result.error.is_some()).count();
    live.cache.borrow_mut().log_diag(
        "work",
        &format!(
            "flushed jobs={} failed={failed} ms={:.2}",
            left.len(),
            elapsed_ms(waited)
        ),
    );
    outcome
}

/// Select the whole document in one pane.
///
/// The caret goes to the end and the anchor to the start, which is what typing
/// next has to replace and what an arrow key has to collapse from. The other
/// pane keeps its own caret and selection: the two are separate by design
/// (技術検証 3.7), and selecting here says nothing about there.
fn select_whole_document(
    window: &AppWindow,
    document: &Rc<OpenDocument>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
) {
    let source = document.text.borrow().clone();
    let end = source.len();
    {
        let mut pane = state.borrow_mut();
        pane.selection_anchor_source_byte = Some(0);
        pane.caret_source_byte = Some(end);
        pane.preferred_line = None;
        // The whole document is not a rectangle, and it is not a mark being
        // held either (要件 7.1, 11.4).
        pane.mark = false;
        pane.rectangular = false;
        // Only the pane that keeps the revealed line writes it; the other works
        // it out from the caret that just moved (`PaneId::revealed_line`).
        if !PaneId::reveals_while_moving(id.vertical(window)) {
            pane.active_line_start = Some(source_line_start(&source, end));
        }
    }
    refresh_pane_from_state(window, cache, document, id, state, &source);
}

/// Put a different document in front of the writer.
///
/// Both panes start again from the beginning with no caret, because every
/// position either of them held belongs to text that no longer exists — the
/// failure this avoids is 6.7's, where a position kept across an edit landed
/// inside a character.
///
/// The same path for a file that has just been opened and for the measurement
/// sample: nothing about the old text survives either way.
fn replace_document(
    window: &AppWindow,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    document: &Rc<OpenDocument>,
    text: String,
) {
    *document.text.borrow_mut() = text;
    // Nothing recorded against the old text names anything in this one.
    // `History::undo_into` checks as well, but that check is the last line of
    // defence rather than the plan.
    document.history.borrow_mut().forget();
    // **Only the panes showing this document.** They go back to its beginning,
    // because no position in the old text means anything in the new one (6.7),
    // and the beginning is a different scroll on each side because the flows
    // run opposite ways. A pane looking at another file is not involved.
    for id in PaneId::all(window) {
        if Rc::ptr_eq(&states.document(id), document) {
            *states.of(id).borrow_mut() = EditorState::default();
            id.set_scroll(window, 0.0);
        }
    }
    relayout_panes(window, states, cache);
}

/// Put the name of what is in front of the writer where they can see it.
///
/// **The tab's name, not the document's** (追加要件 2026-09-07). Two kinds of
/// tab carry an untitled document without being one — a shell, and a tab that
/// has not been asked what it is — and the window's title bar was announcing
/// that stand-in as though the writer had made it.
fn show_tab_title(window: &AppWindow, tab: &PaneTab) {
    window.set_document_title(tab_title(tab).into());
}

/// The same, for a document with no tab to hand (startup).
fn show_document_title(window: &AppWindow, file: &DocumentFile) {
    window.set_document_title(file.title().into());
}

/// One tab: a document, and what each pane was showing of it (要件 6.3).
///
/// **A tab is a view, not a copy.** It used to hold the text itself, and a
/// switch put the live text away and brought another one out; now every
/// callback asks the pane it belongs to which document that pane is showing
/// (`PaneStates::document`), so a switch only has to point the panes somewhere
/// else. What a switch still costs is the relayout — the engine measures the
/// new document from nothing, the same 200ms a zoom costs (6.8), paid once per
/// switch rather than once per keystroke.
/// What one pane was showing of one tab.
///
/// 要件 7.6 keeps the caret, the selection, the scroll and the zoom **per pane
/// per tab**, not per document: the same file open in two panes is two places
/// to be looking at it. The three used to be three differently named pairs on
/// the tab (`vertical`/`horizontal`, `vertical_preview`/`horizontal_preview`,
/// `scroll_x`/`scroll_y`), which is the same duplication `PaneId` was made to
/// end.
#[derive(Clone, Debug, Default)]
struct TabView {
    state: EditorState,
    /// Along the flow, in whichever screen axis that is for this pane.
    ///
    /// **A hint, not the view.** It is a number of pixels, and pixels stop
    /// meaning anything when the layout is a different size — which is what
    /// happens between one run and the next, and between one zoom and another.
    /// `top` is what puts the view back.
    scroll: f32,
    /// The source byte at the near edge of the view (要件 8.5).
    top: Option<usize>,
    /// **The four modes of 要件 7.2, and both halves belong to the tab.**
    /// Which way the text runs, and whether it is shown formatted or as its
    /// source. A pane draws whatever the tab in front of it says, so the same
    /// file can be open横書き in one pane and縦書き in another.
    vertical: bool,
    preview: bool,
}

impl TabView {
    /// A view of a document nobody has looked at through this pane yet.
    ///
    /// **The pane's own direction, whatever it is now** (2026-09-06). A fresh
    /// view used to open 縦書き in the right-hand pane and 横書き in the left,
    /// which was the last thing about a tab that a pane's *number* decided. A
    /// pane draws the way the tab in front of it says (要件 7.2), so a new tab
    /// opening the way the pane is already set is the only answer that does not
    /// make the pane jump when the tab arrives.
    fn for_pane(window: &AppWindow, id: PaneId) -> Self {
        Self {
            vertical: id.vertical(window),
            preview: id.shows_preview(window),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct PaneTab {
    /// 要件 7.9（2026-09-08）: この文書の**単語チェックモード**の番号。
    ///
    /// **名前ではなく番号**（同日改訂）。名前で指していたときは、**モードの名前を
    /// 変えた瞬間に文書のモードが切れて**いた——名前は書き手が変えるもので、
    /// 何かを指す仕事には向かない。番号は使い回さない（`next_word_id`）ので、
    /// 消したモードを指していた文書は「なし」になる：**間違った色で出るよりよい。**
    ///
    /// `0`が「なし」。
    word_mode: u32,
    /// **The document, not a copy of it.** A tab is a view; switching away no
    /// longer takes the text out of the pane and switching back no longer puts
    /// it in, so a switch costs the relayout and nothing else.
    document: Rc<OpenDocument>,
    /// The shell, when this tab is a terminal (追加要件 Terminal: TAB一つが
    /// Terminalのウィンドウになる).
    ///
    /// **A terminal tab still carries a document**, empty and untitled. Every
    /// path that closes, carries, counts or saves a tab reaches for one, and a
    /// terminal that had none would be a second kind of tab for all of them to
    /// learn about. What it has instead is this, and one question — is it
    /// `Some` — is the whole of the difference.
    terminal: Option<Rc<RefCell<TerminalSession>>>,
    /// The strip along the foot of the pane while this tab is in front
    /// (追加要件 Terminal).
    ///
    /// **On the tab, not on the pane** (書き手の報告, 2026-09-06). Held by the
    /// pane, opening a draft under a shell opened a terminal under every
    /// document beside it: the flag was shared and the *kind* followed whatever
    /// tab was in front. What is under a document is that document's.
    below: TabBelow,
    /// How *this pane* is looking at that document. Another pane showing the
    /// same file has a tab of its own, with a caret and a scroll of its own
    /// (要件 7.6).
    view: TabView,
    /// Whether this tab is still asking what it is (追加要件 2026-09-07: New Tab).
    ///
    /// **A tab before it is anything.** The `＋` used to ask first — a menu of
    /// three answers — and only then make the tab; now it makes the tab and the
    /// tab asks. What stands in it is three words in the middle of the page,
    /// and every one of them turns it into something: a file, a shell, or
    /// whatever the writer opens next from anywhere in the window.
    ///
    /// **The document under it is already the one a new file would get**, taken
    /// from the same run of untitled numbers, so `New File` is this flag going
    /// down and nothing else.
    empty: bool,
    /// Whether this tab is only being looked through (書き手の報告 2026-09-07).
    ///
    /// **A row clicked in the left panel is a question, not a decision.** A
    /// writer walking a folder to find something opened a tab per file and
    /// ended up with a strip they had to clear by hand; one tab now serves the
    /// whole walk, and the next row takes its place. It stops being one the
    /// moment the writer means it — a second click on the row, or the first
    /// character typed into it.
    ///
    /// **A `Cell`, so that `publish_tabs` can put it down.** Publishing the
    /// strips is where the unsaved marker is read, and that is the same moment
    /// this is answered; it holds the strips by a shared borrow, and taking a
    /// mutable one there is the borrow every path into it would have to be
    /// checked against.
    provisional: Cell<bool>,
}

/// Whether a file being opened gets a tab that stays (書き手の報告 2026-09-07).
#[derive(Clone, Copy, PartialEq)]
enum Opening {
    /// The writer asked for this file by name — the dialog, the command line,
    /// a row opened twice. It gets a tab of its own.
    Kept,
    /// The writer is walking a list. One tab serves the walk.
    Peeked,
}

/// What one tab has along the foot of its pane (追加要件 Terminal).
#[derive(Clone, Default)]
struct TabBelow {
    open: bool,
    /// The shell down there, when this tab is a document.
    ///
    /// **Kept while the strip is closed**, because closing a panel is not
    /// abandoning the command running in it.
    shell: Option<Rc<RefCell<TerminalSession>>>,
    /// What the writer has written in the draft, when this tab is a shell.
    draft: String,
    /// How tall they dragged it. Zero means "the height it opens at".
    height: f32,
}

impl PaneTab {
    fn document(&self) -> Rc<OpenDocument> {
        self.document.clone()
    }

    /// A tab showing a document this pane has not looked at yet.
    fn showing(window: &AppWindow, id: PaneId, document: Rc<OpenDocument>) -> Self {
        Self {
            // 要件 7.9: **開いた面のモードを継ぐ。**同じ作品の次の章を開いて
            // 選び直させるのは、書く手を止めることである（要件 3）。
            word_mode: id.screen(window).word_mode as u32,
            document,
            view: TabView::for_pane(window, id),
            terminal: None,
            below: TabBelow::default(),
            empty: false,
            provisional: Cell::new(false),
        }
    }

    /// Whether opening a file in this pane would take this tab's place rather
    /// than adding another beside it.
    ///
    /// **A tab that has not decided what it is yields to anything** (追加要件
    /// 2026-09-07), however the file was asked for — that is what makes
    /// clicking a file in the tree fill the New Tab in front of the writer
    /// instead of opening a second one beside it. A provisional tab yields only
    /// to another walk through a list ([`Opening::Peeked`]).
    fn yields_to(&self, opening: Opening) -> bool {
        self.empty || (opening == Opening::Peeked && self.is_provisional())
    }

    /// Whether the next file clicked in a list would take this tab's place.
    ///
    /// **Edited is kept, whatever the flag says.** The flag is put down when
    /// the strips are published, and this is asked in between: a tab holding
    /// work is not one to write over.
    fn is_provisional(&self) -> bool {
        self.provisional.get() && self.terminal.is_none() && !self.document.text.edited()
    }
}

/// One pane's strip (要件 6.3: 各編集ペインは独立したタブ列を持つ).
#[derive(Clone, Default)]
struct PaneTabs {
    tabs: Vec<PaneTab>,
    /// Which of them the pane is showing. Always a position in `tabs`: the
    /// strip is never empty, because a pane with no tab has nothing to show and
    /// nowhere to type.
    active: usize,
    /// The documents this pane has stood in front of, oldest first
    /// (書き手の報告 2026-09-07).
    ///
    /// **Documents, not tab numbers.** A number means a different tab the
    /// moment one is closed or carried, and the walk this list is for closes
    /// tabs as it goes: the file a writer wants to go back to is often the one
    /// whose tab was just written over. Holding the `Rc` keeps it readable —
    /// it is the same document, so going back to it shares the text with
    /// anything else showing it (要件 7.6).
    history: Vec<Rc<OpenDocument>>,
    /// Where in that list the pane is standing. Everything after it is what
    /// 進む would reach; a move anywhere else cuts it off.
    at: usize,
}

/// How many places one pane remembers going (書き手の報告 2026-09-07).
///
/// **A walk, not a life.** Each place holds its document open, so the number is
/// also how many closed files the window can be keeping in memory.
const NAVIGATION_PLACES: usize = 16;

impl PaneTabs {
    /// The tab this pane is showing, if it has one.
    ///
    /// **A pane off screen can have an empty strip** — that is what closing its
    /// last tab does while other panes are up (要件 6.4) — so this is asked
    /// rather than assumed.
    fn current(&self) -> Option<&PaneTab> {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
    }
}

/// The smallest untitled number not already taken (要件 8.4).
///
/// Smallest rather than one past the largest, so that closing 無題2 and asking
/// for a new document gives 無題2 again rather than counting away from the
/// numbers actually on screen.
fn next_untitled_number(taken: &[u32]) -> u32 {
    let mut number = 1;
    while taken.contains(&number) {
        number += 1;
    }
    number
}

/// Which tab is active after one is closed.
///
/// Closing the active tab moves to the one that took its place, and closing the
/// last tab moves to the new last. Closing a tab before the active one shifts
/// the active index down so that the same tab stays active.
fn active_after_close(count: usize, active: usize, closed: usize) -> usize {
    let remaining = count.saturating_sub(1);
    if remaining == 0 {
        return 0;
    }
    let next = if closed < active {
        active.saturating_sub(1)
    } else {
        active
    };
    next.min(remaining - 1)
}

/// Where the tab in front sits after one tab is moved (要件 6.5).
///
/// **The tab in front does not change — only its position does.** Carrying the
/// tab in front takes the front with it; carrying another tab past it shifts it
/// by one, in whichever direction the gap it left and the gap it filled sit.
fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        return to;
    }
    if from < active && to >= active {
        return active - 1;
    }
    if from > active && to <= active {
        return active + 1;
    }
    active
}

/// Which of a strip's tabs a まとめて閉じる takes, and in what order (要件 6.3).
///
/// **Left to right**, so the questions come in the order the tabs are drawn in
/// rather than in whatever order the closes happen to reach them. `keep_active`
/// is 他のタブを閉じる; without it nothing is kept.
///
/// The active position is clamped the way [`PaneTabs::current`] clamps it: a
/// strip that has just lost its last tab can carry an active one past the end
/// for a moment, and keeping a position that is not there would keep nothing.
fn close_run_positions(count: usize, active: usize, keep_active: bool) -> Vec<usize> {
    let kept = active.min(count.saturating_sub(1));
    (0..count)
        .filter(|index| !(keep_active && *index == kept))
        .collect()
}

/// Every pane's strip.
///
/// The `view` of each strip's current tab is a placeholder — where the caret and
/// the scroll really are is in the pane, and [`sync_active_tab`] writes them
/// back before anything reads the list. **The documents are not placeholders**:
/// they are what the panes are editing, whatever the lists say about anything
/// else.
struct Tabs {
    /// Indexed by [`PaneId::index`], like everything else that has one of
    /// something per pane. **One entry per pane and no more**: a strip nobody
    /// can see is tabs nobody can reach (要件 6.3).
    panes: Vec<PaneTabs>,
}

impl Tabs {
    /// One pane's strip.
    ///
    /// **The last strip rather than a panic** for a number that names nothing:
    /// pane numbers arrive from the window, and one left over from an
    /// arrangement that has already changed must not be able to stop the
    /// editor. There is always a strip to answer with — 要件 6.3 keeps one pane
    /// alive whatever else happens.
    fn at(&self, at: usize) -> usize {
        at.min(self.panes.len().saturating_sub(1))
    }

    fn of(&self, id: PaneId) -> &PaneTabs {
        &self.panes[self.at(id.index() as usize)]
    }

    fn of_mut(&mut self, id: PaneId) -> &mut PaneTabs {
        let at = self.at(id.index() as usize);
        &mut self.panes[at]
    }

    /// Make room for a pane, at the end, which is the number a split hands out.
    fn add(&mut self, strip: PaneTabs) {
        self.panes.push(strip);
    }

    fn count(&self) -> usize {
        self.panes.len()
    }

    /// Take a pane's strip out, closing the numbering behind it.
    fn remove(&mut self, id: PaneId) -> PaneTabs {
        let at = id.index() as usize;
        if at < self.panes.len() {
            self.panes.remove(at)
        } else {
            PaneTabs::default()
        }
    }
}

/// The handles every tab operation needs.
///
/// Bundled because a tab switch touches all of them at once and passing six
/// arguments to each of half a dozen functions hides which ones matter. The
/// editing callbacks keep their own handles; nothing here changes them.
#[derive(Clone)]
struct Live {
    states: PaneStates,
    /// The work folder and which of its folders are open (要件 5.1, 5.2).
    /// **One folder per window** — 要件 5.1 says so, and the tree is what the
    /// writer reaches their documents through.
    folder: Rc<RefCell<WorkFolder>>,
    /// What each row of the published tree stands for. The window draws names
    /// and depths; a click has to name a path, and a path is not something the
    /// window has any use for.
    tree_paths: Rc<RefCell<Vec<PathBuf>>>,
    /// What the last folder-wide search found (要件 7.7). Held here for the
    /// same reason as `tree_paths`: the window draws lines, and a place in a
    /// document is not one.
    results: Rc<RefCell<Vec<ResultRow>>>,
    /// The files opened most recently, newest first (要件 7.7). Both the 履歴
    /// panel's rows and what each of them stands for: one path is all a row
    /// needs.
    recent: Rc<RefCell<Vec<PathBuf>>>,
    /// The work folders opened most recently, newest first (要件 5.1), with
    /// the one open now at the head.
    ///
    /// **The head is not offered back.** A window holds one work folder at a
    /// time, so the menu is a list of the folders the writer can go to — and
    /// the one they are already in is not one of them (`offered_folders`).
    recent_folders: Rc<RefCell<Vec<PathBuf>>>,
    /// 探した語と、置き換えに使った語（E1の④）。**窓に1つずつ。**探し方の
    /// 切り替えは面ごとに持っているが、それは「いまこの文書で何をしているか」で
    /// あって、履歴は打ち直さないための列である——隣の面で探した語を持って
    /// こられないなら、列が面の数だけある意味が無い。
    find_terms: Rc<RefCell<find::Terms>>,
    replace_terms: Rc<RefCell<find::Terms>>,
    /// How the editing area is divided (要件 6.4). **The whole of the
    /// arrangement**: which panes are on screen, how they sit, and where the
    /// boundaries are.
    layout: Rc<RefCell<Layout>>,
    /// The question standing in front of the writer, if one is (要件 8.3, 8.4).
    /// **One at a time**: the overlay covers the window, so nothing can ask a
    /// second question while the first is up.
    pending: Rc<RefCell<Option<Question>>>,
    /// The tabs a まとめて閉じる has not reached yet (要件 6.3).
    ///
    /// **Written down rather than held in a loop.** Closing several tabs may
    /// have to ask about each of them, and an answer comes back a turn of the
    /// event loop later — by which time no loop is left standing to hold the
    /// rest of the list.
    close_run: Rc<RefCell<Option<CloseRun>>>,
    cache: Rc<RefCell<RenderCache>>,
    tabs: Rc<RefCell<Tabs>>,
    /// The thread that writes work copies, so that the flush at the end of one
    /// does not land in the middle of somebody's sentence (要件 2).
    writer: Rc<FileWriter>,
    /// The thread that searches the work folder (要件 2, 7.7). **The one place
    /// the editor reads many files at once**, and how long that takes is
    /// decided by the folder somebody opened rather than by the editor.
    searcher: Rc<Searcher>,
    /// Which folder-wide search the writer is waiting for.
    ///
    /// **Counting up, and only the newest is shown.** An answer arrives turns
    /// of the event loop after the question, by which time the question may
    /// have changed; there is nothing to cancel on the editor's side, so
    /// instead every answer says which question it is for.
    searched: Rc<Cell<u64>>,
}

impl Live {
    /// The document in front of the pane the writer is in.
    ///
    /// **Everything that is about "the document" without saying which pane** —
    /// saving, the work copy, the questions — means this one. Which pane that
    /// is comes from the window, because the writer decides it by clicking.
    fn active(&self, window: &AppWindow) -> Rc<OpenDocument> {
        self.states.document(focused_pane(window))
    }

    /// What one pane is showing of the tab in front of it.
    ///
    /// Cloned rather than taken: this is also called when nothing is being
    /// switched away from, and taking would empty the caret under the pane. The
    /// document itself is not captured — it is not a copy that can go stale, it
    /// is the thing the pane is editing.
    fn capture_view(&self, window: &AppWindow, id: PaneId) -> TabView {
        TabView {
            state: self.states.of(id).borrow().clone(),
            scroll: id.scroll(window),
            top: self.view_top(window, id),
            vertical: id.vertical(window),
            preview: id.shows_preview(window),
        }
    }

    /// The source byte at the near edge of what a pane is showing (要件 8.5).
    ///
    /// **Asked of the layout, once, when the view is being put away** — a tab
    /// switch or the end of the run. Cheap where it is asked from: the engine
    /// is already holding this tab's text, so the hit test walks a layout that
    /// is already built.
    ///
    /// `None` when the engine cannot answer, which leaves the pixel scroll as
    /// the only hint. That is what the session had before this and is still
    /// right whenever nothing about the layout has changed.
    fn view_top(&self, window: &AppWindow, id: PaneId) -> Option<usize> {
        view_top(window, &self.states, &self.cache, id)
    }

    /// Bring a tab out in front of one pane.
    ///
    /// Everything of the old document goes with it: that pane's caret, that
    /// pane's scroll. Nothing is mapped across, because no position in one
    /// document means anything in another — the same reason `replace_document`
    /// resets rather than clamps (6.7). **The other panes are left alone**:
    /// they have strips of their own and may be showing something else.
    fn show_tab(&self, window: &AppWindow, id: PaneId, tab: &PaneTab) {
        let document = &tab.document;
        self.states.show(id, document);
        // The direction first: everything below is in a screen axis, and which
        // axis that is comes from the tab (要件 7.2).
        set_pane_direction(window, &self.cache, id, tab.view.vertical);
        // **What the pane is showing, told to the pane — twice, because two
        // sides ask.** Every path that draws reaches `refresh_pane` and none of
        // them carries a tab, so the render cache is told; and the keyboard is
        // the window's, so the row is told as well. Saying it in only one of
        // them is what made the first terminal draw its prompt and then send
        // every key to the document behind it (追加要件 Terminal).
        let showing_shell = tab.terminal.is_some();
        let (kind, height) = {
            let mut borrowed = self.cache.borrow_mut();
            let pane = borrowed.pane(id);
            pane.terminal = tab.terminal.as_ref().map(TerminalView::sharing);
            // **The strip belongs to the tab**, so it arrives with it: what was
            // open under this tab is open again, at the height it was left at,
            // with the shell that was running in it (書き手の報告, 2026-09-06).
            pane.below = tab.below.shell.as_ref().map(TerminalView::sharing);
            pane.below_open = tab.below.open;
            pane.below_height = if tab.below.height > 0.0 {
                tab.below.height
            } else {
                TERMINAL_BELOW_HEIGHT
            };
            (below_kind(pane), pane.below_height)
        };
        let asking = tab.empty;
        let mode_id = tab.word_mode;
        id.update_screen(window, |screen| {
            screen.terminal = showing_shell;
            screen.empty = asking;
            // 要件 7.9（2026-09-08）: **モードはタブと一緒に動く。**組版はこの
            // 行から読むので（`lay_out_pane`）、タブを切り替えれば色分けも
            // 切り替わる——コードエディタで別の言語のファイルへ移るのと同じ。
            screen.word_mode = mode_id as i32;
        });
        show_draft(window, id, &tab.below.draft);
        id.set_below(window, kind, height);
        // **A strip restored open has no shell in it yet** (要件 8.5 puts the
        // arrangement back, not the processes). The one it needs is started
        // here, when the tab is actually in front of somebody.
        if kind == 1 && self.cache.borrow_mut().pane(id).below.is_none() {
            open_below_shell(window, self, id);
        }
        *self.states.of(id).borrow_mut() = tab.view.state.clone();
        id.set_scroll(window, tab.view.scroll);
        // The passage this tab was left at. **A tab switch has the same problem
        // a restored session does**: the pane it comes back to may be a
        // different size or zoom from the one it left.
        hold_view(
            &self.cache,
            id,
            tab.view.top,
            tab.view.state.caret_source_byte,
        );
        id.set_shows_preview(window, tab.view.preview);
        let state = self.states.of(id);
        let source = document.text.borrow().clone();
        refresh_pane_from_state(window, &self.cache, document, id, &state, &source);
    }
}

/// How many tabs are showing this document, across every pane.
///
/// The same file open twice is one document (`Rc::ptr_eq`), and the count is
/// what says whether a tab is the last way to it.
fn views_of(live: &Live, document: &Rc<OpenDocument>) -> usize {
    let tabs = live.tabs.borrow();
    tabs.panes
        .iter()
        .flat_map(|strip| strip.tabs.iter())
        .filter(|tab| Rc::ptr_eq(&tab.document, document))
        .count()
}

/// Every document any pane is holding, each once.
///
/// A file open in two panes is one document, so it is written and asked about
/// once — `Rc::ptr_eq` is the whole of "the same one" (要件 7.6).
fn open_documents(live: &Live) -> Vec<Rc<OpenDocument>> {
    let tabs = live.tabs.borrow();
    let mut open: Vec<Rc<OpenDocument>> = Vec::new();
    for strip in &tabs.panes {
        for tab in &strip.tabs {
            if !open.iter().any(|held| Rc::ptr_eq(held, &tab.document)) {
                open.push(tab.document.clone());
            }
        }
    }
    open
}

/// Point a pane at a writing direction (要件 7.2).
///
/// **The engine is rebuilt when the direction moves**, so this costs the whole
/// document being measured again — the price of a deliberate switch, never of a
/// keystroke. The IME is told as well when it is the pane being typed in: its
/// candidate list is laid out from the composition font's direction (7.2).
fn set_pane_direction(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    vertical: bool,
) {
    // The engine's own mode is the truth, and the row mirrors it. Nothing else
    // writes either, so asking the engine to change is the whole test for
    // whether anything has to happen.
    let moved = cache.borrow_mut().pane(id).set_mode(mode_of(vertical));
    if !moved {
        return;
    }
    id.update_screen(window, |screen| {
        screen.vertical = vertical;
        // **Both scrolls go.** The one along the old flow means nothing in the
        // new direction, and — worse — it is the *cross* axis now, where a
        // number meant as "50,000px into the document" becomes 50,000px of
        // blank paper beside it. Whatever the pane should be looking at, the
        // caret pulls it back on the refresh that follows.
        screen.scroll_x = 0.0;
        screen.scroll_y = 0.0;
    });
    if id == focused_pane(window) {
        ime::set_vertical(window, vertical);
    }
    cache.borrow_mut().log_diag(
        "mode",
        &format!("pane={} vertical={}", id.log_name(), u8::from(vertical)),
    );
}

/// The writing mode a direction names.
fn mode_of(vertical: bool) -> WritingMode {
    if vertical {
        WritingMode::Vertical
    } else {
        WritingMode::Horizontal
    }
}

/// Put the file the writer is looking at in front of another pane.
///
/// 要件 6.4: 分割時、新しい編集ペインにはフォーカス中のタブと同じファイルを表示する。
/// **However many tabs that pane already has**: it keeps them, and the one it
/// shows becomes this file — a tab of its own if it has one for it, a new tab
/// if it does not. The document is shared rather than copied (要件 7.6), so the
/// two panes edit one text while keeping a caret each.
fn open_same_file_in(window: &AppWindow, live: &Live, id: PaneId, like: PaneId) {
    let Some(document) = live.tabs.borrow().of(like).current().map(PaneTab::document) else {
        return;
    };
    let index = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let existing = strip
            .tabs
            .iter()
            .position(|tab| Rc::ptr_eq(&tab.document, &document));
        match existing {
            Some(index) => index,
            None => {
                strip.tabs.push(PaneTab {
                    word_mode: id.screen(window).word_mode as u32,
                    document,
                    view: TabView {
                        vertical: like.vertical(window),
                        preview: like.shows_preview(window),
                        ..TabView::default()
                    },
                    terminal: None,
                    below: TabBelow::default(),
                    empty: false,
                    provisional: Cell::new(false),
                });
                strip.tabs.len() - 1
            }
        }
    };
    let incoming = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        strip.active = index;
        strip.current().cloned()
    };
    if let Some(tab) = incoming {
        live.show_tab(window, id, &tab);
    }
}

/// Put everything back on screen after the tree has changed.
///
/// **The panes are placed first and drawn second**, because a pane draws into
/// the area the tree gave it: laying one out against the area it had before is
/// 6.19 by another route.
fn after_layout_change(window: &AppWindow, live: &Live) {
    {
        let layout = live.layout.borrow();
        place_panes(window, &layout);
    }
    // **The four lists indexed by pane number have to be the same length**
    // (2026-09-06). Every one of them answers a number it does not hold with
    // the pane it does — a stale number arrives with a keystroke and must not
    // be able to stop the editor — so a list left short reads as a pane quietly
    // sharing another's state, and nothing says so until something else falls
    // over. It cost a crash in `Close All Tabs` whose real cause was at
    // start-up, four panes earlier. Three places change the count (start-up,
    // `divide_pane`, `remove_pane`); this is where they are checked.
    {
        let panes = PaneId::count(window);
        let states = live.states.count();
        let strips = live.tabs.borrow().count();
        let engines = live.cache.borrow().panes.len();
        if states != panes || strips != panes || engines != panes {
            live.cache.borrow_mut().log_diag(
                "layout",
                &format!(
                    "mismatch panes={panes} states={states} strips={strips} engines={engines}"
                ),
            );
        }
    }
    // **A pane that has just come on screen brings its strip with it.** Its
    // tabs are in its own list and nowhere else until they are published, so a
    // pane could otherwise appear with a document and no tabs above it.
    publish_tabs(window, live);
    for id in PaneId::all(window) {
        if !id.is_shown(window) {
            continue;
        }
        let showing = live.states.document(id);
        let source = showing.text.borrow().clone();
        let state = live.states.of(id);
        refresh_pane_from_state(window, &live.cache, &showing, id, &state, &source);
    }
}

/// Put the boundaries in front of the writer, **without replacing the model
/// unless the number of them has changed**.
///
/// A repeater whose model is swapped throws its instances away and builds them
/// again — and one of those instances may be the `TouchArea` holding the pointer
/// that is dragging this very boundary. Replacing the model on every drag event
/// therefore ends the drag after one pixel. Writing the rows leaves the
/// instances alone (`Repeater::row_changed` updates them in place), which is the
/// same reason the panes' own rows are written rather than republished.
fn publish_boundaries(window: &AppWindow, drawn: Vec<PaneBoundary>) {
    let model = window.get_boundaries();
    if let Some(rows) = model.as_any().downcast_ref::<VecModel<PaneBoundary>>()
        && rows.row_count() == drawn.len()
    {
        for (index, boundary) in drawn.into_iter().enumerate() {
            rows.set_row_data(index, boundary);
        }
        return;
    }
    window.set_boundaries(ModelRc::new(VecModel::from(drawn)));
}

/// 検索と置換が働くペイン（要件 7.7、E1）。
///
/// **帯は自分が探す本文の中にある。**開いているあいだはそのペインが答えで、
/// 欄に打っている書き手が見ているのもそこの本文である。閉じていればキーボードの
/// あるペイン——F3は本文から来る。
///
/// 以前はどちらも`focused_pane`だった。帯を出したまま隣のペインの本文を触り、
/// 欄へ戻って打つと、**見ている帯とは別の文書を数えていた。**
fn find_target(window: &AppWindow) -> PaneId {
    if window.get_find_open() {
        let bar = PaneId::from_index(window.get_find_pane());
        if bar.is_shown(window) {
            return bar;
        }
    }
    focused_pane(window)
}

/// 組版が返した矩形を、窓が読む形へ（要件 7.1、E1）。
///
/// **選択も一致も範囲も同じ形**である——違うのは描く側の色だけで、それが
/// 「選ばれている」「見つかっている」「ここを探している」を一続きに見せる。
fn preview_rects(rects: &[SelectionRect]) -> Vec<PreviewSelectionRect> {
    rects
        .iter()
        .map(|rect| PreviewSelectionRect {
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(0.0),
            height: (rect.bottom - rect.top).max(0.0),
        })
        .collect()
}

/// 探す条件を捨てる（E1、書き手の求め 2026-09-09）。
///
/// **語だけでなく、探し方ぜんぶ。**大小の別も単語単位も範囲も、次に開いたとき
/// 残っていると「なぜ見つからないのか」が画面から辿れない——`Clear`は
/// 「まっさらから探し直す」と読める1つの動きでなければならない。
fn clear_find(window: &AppWindow, live: &Live) {
    let id = find_target(window);
    id.update_screen(window, |screen| {
        screen.find_needle = SharedString::new();
        screen.find_replacement = SharedString::new();
        screen.find_status = SharedString::new();
        screen.find_match_case = false;
        screen.find_whole_word = false;
        screen.find_regex = false;
        screen.find_scope_start = 0;
        screen.find_scope_end = 0;
    });
    // 数え直し＝塗り直し。色も帯の言葉も、これで消える。
    count_in_pane(window, live);
}

/// 行番号の帯が向いているペイン（E4）。
///
/// **帯は自分が動かす本文の中にある**——検索の帯と同じ約束である
/// （`find_target`）。開いていなければ鍵盤のあるペイン。
fn goto_target(window: &AppWindow) -> PaneId {
    if window.get_goto_open() {
        let bar = PaneId::from_index(window.get_goto_pane());
        if bar.is_shown(window) {
            return bar;
        }
    }
    focused_pane(window)
}

/// 帯に、行けるところを言わせる（E4）。
///
/// **開いた瞬間から範囲が出ている。**「128行までです」を打ってから知るのでは
/// 遅く、`1〜128行`はこの文書がどれだけあるかの答えでもある（要件 10 の行数と
/// 同じ数え方——折り返しの表示行ではなく、論理行である）。
///
/// 打っているあいだも同じ道を通るので、**越えた番号はEnterの前に分かる。**
fn tell_goto(window: &AppWindow, live: &Live) {
    let id = goto_target(window);
    let document = live.states.document(id);
    let source = document.text.borrow();
    // **ステータスバーと同じ数**（要件 10）。行数を数え直すのではなく、そこが
    // 持っている数を訊く——2つの数え方があれば、いつか食い違う。
    let lines = document
        .counts
        .borrow_mut()
        .get(&source)
        .stats()
        .logical_lines;
    let typed = window.get_goto_line().to_string();
    let told = match document::read_place(&typed) {
        _ if typed.trim().is_empty() => format!("1〜{lines}行"),
        None => "行番号を打ってください".to_owned(),
        Some((line, _)) if line > lines => format!("{lines}行までです"),
        Some(_) => format!("1〜{lines}行"),
    };
    window.set_goto_status(told.into());
}

/// 行番号の帯を出す／閉じる——`Ctrl+G`（要件 11.2、E4）。
///
/// **同じ鍵が閉じる**（検索の帯と同じ、要件 7.7）。閉じるときは鍵盤を紙へ返す
/// ——帯を閉じたのに打てないのは、閉じていないのと同じことである。
///
/// `taking`は**「鍵盤を連れてくるだけ」**（書き手の報告 2026-09-10：「検索バーが
/// 出ている状態だとCtrl+Gが効きません。切り替わるといいと思いました」）。検索の帯や
/// メニューから来た求めは**そちらへ移りたい**という意味なので、開いている帯を
/// 閉じてしまうと「効かない」と同じに見える——鍵盤だけを渡す。
fn toggle_goto(window: &AppWindow, live: &Live, taking: bool) {
    let here = focused_pane(window);
    if window.get_goto_open() && window.get_goto_pane() == here.index() {
        if taking {
            // 出ている帯へ鍵盤を移すだけ。時計が欄を選び直す（`goto-generation`）。
            window.set_goto_generation(window.get_goto_generation() + 1);
            tell_goto(window, live);
            return;
        }
        close_goto(window);
        return;
    }
    window.set_goto_pane(here.index());
    window.set_goto_open(true);
    window.set_goto_generation(window.get_goto_generation() + 1);
    tell_goto(window, live);
}

/// 帯を畳んで、鍵盤を紙へ返す（E4）。
fn close_goto(window: &AppWindow) {
    window.set_goto_open(false);
    restore_editor_focus(window);
}

/// 打たれた行へ行く（E4）。
///
/// **無い行では動かない**（書き手の選択 2026-09-10）。E4は「他の人や別の道具から
/// 『何行目』と示された箇所へ行くため」の機能なので、越えた番号で末尾へ着地すると
/// **「手元の原稿が違う」という知らせが消える**——行ったのに違う行、が起きる。
///
/// **着いたら帯は畳む。**行の指定は1回の用事で、探す語のように次があるものでは
/// ない。畳めば鍵盤は紙へ戻り、書き手はその行から打ち始められる。
///
/// **横書きでも縦書きでも同じソース位置**（E4）。面の向きは組み方であって、
/// 文書のどこかという話ではない（技術検証 3.12）。
fn go_to_line(window: &AppWindow, live: &Live) {
    let id = goto_target(window);
    let typed = window.get_goto_line().to_string();
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let Some((line, column)) = document::read_place(&typed) else {
        window.set_goto_status("行番号を打ってください".into());
        return;
    };
    let lines = document
        .counts
        .borrow_mut()
        .get(&source)
        .stats()
        .logical_lines;
    let Some(at) = document::place_of(&source, line, column) else {
        window.set_goto_status(format!("{lines}行までです").into());
        live.cache.borrow_mut().log_diag(
            "goto",
            &format!("pane={} line={line} of={lines} outside", id.log_name()),
        );
        return;
    };
    let state = live.states.of(id);
    // **選択は残さない。**行を指すことは範囲を選ぶことではなく、着いた先でそのまま
    // 打てるほうがよい——長さの無い範囲は、そこに立っているカーソルである。
    select_source_range(window, &live.cache, &document, &state, id, &source, at, at);
    let told = format!(
        "pane={} line={line} of={lines} column={} at={at}",
        id.log_name(),
        column.map_or_else(|| "-".to_owned(), |column| column.to_string()),
    );
    live.cache.borrow_mut().log_diag("goto", &told);
    close_goto(window);
}

/// 探した語を1つ遡る／戻る——検索欄と置換欄の↑↓（E1の④）。
///
/// **欄の字が入れ替わるだけで、本文は動かない。**打っているあいだと同じ約束で
/// あり（E1の①）、動かす鍵はEnter・Shift+Enter・F3のほうにある——↑で語を覗いた
/// だけの書き手を、その語の一致へ連れて行くことはしない。
///
/// **数と色はその場で付け直す**（検索欄のとき）。入れ替えた語で何件あるかが
/// 見えなければ、↑は「欄の字が変わっただけ」の鍵になる。
fn walk_find_history(window: &AppWindow, live: &Live, back: bool, replacing: bool) {
    let id = find_target(window);
    let screen = id.screen(window);
    let terms = if replacing {
        &live.replace_terms
    } else {
        &live.find_terms
    };
    let field = if replacing {
        screen.find_replacement.to_string()
    } else {
        screen.find_needle.to_string()
    };
    // **端では止まる。**遡り切ったところで欄を空にすると、書き手には語が消えた
    // ように見える——列の終わりは、列が無くなることではない。
    //
    // 列を借りるのはこの一手だけ。**画面へ書く前に返す**——`update_screen`は
    // 窓を通って戻ってくることがあり、借りたまま入ると二度目の借りで落ちる。
    let stepped = {
        let mut terms = terms.borrow_mut();
        let stepped = terms.step(back, &field);
        let held = terms.kept().len();
        stepped.map(|term| (term, held))
    };
    let Some((term, held)) = stepped else {
        return;
    };
    id.update_screen(window, |screen| {
        if replacing {
            screen.find_replacement = term.as_str().into();
        } else {
            screen.find_needle = term.as_str().into();
        }
    });
    // **語そのものは書かない**——原稿の言葉である（`find`の他の行と同じ）。
    let which = if replacing { "replacement" } else { "needle" };
    let step = if back { "back" } else { "forward" };
    let told = format!(
        "history pane={} {which} {step} term={} kept={held}",
        id.log_name(),
        term.chars().count(),
    );
    live.cache.borrow_mut().log_diag("find", &told);
    if !replacing {
        count_in_pane(window, live);
    }
}

/// この面の探し方で、1回ぶんの検索を組み立てる（E1）。
///
/// **正しくない正規表現は、ここで分かる。**`(`だけ打った書き手に0件と答えるのは
/// 嘘で、`Err`はそのまま帯とステータスバーの言葉になる。
fn find_search(window: &AppWindow, id: PaneId) -> Result<find::Search, String> {
    let needle = id.screen(window).find_needle.to_string();
    find::Search::new(&needle, find_rules(window, id))
}

/// 画面に色を付ける一致の上限（E1）。
///
/// **打鍵の費用の歯止めであって、見せられる数の決まりではない。**1画面に
/// 収まる一致はせいぜい数十で、これはその桁の上にある。
const MAX_SHOWN_MATCHES: usize = 400;

/// この面の探し方（E1）。
///
/// **切り替えは面ごと**——語がそうなのだから、その語をどう探すかも同じところに
/// ある（書き手の報告 2026-09-09）。範囲は「終わりが始まりより大きいときだけ」
/// 効く：0と0は「範囲は無い」である。
/// 書き手が選び直していたら、範囲を取り直す（E1、書き手の指摘 2026-09-09）。
///
/// **「検索のたびに取り直す」では潰れる。**検索は一致を選ぶので、2回目には
/// その一致が範囲になってしまう——E1が「検索結果へ移動しても最初の対象範囲を
/// 維持」と言っているのはそのためである。見るのは**選択が変わったかどうかでは
/// なく、変えたのが誰か**：検索が置いた選択（[`EditorState::search_selection`]）
/// と今の選択が違えば、そのあいだに書き手の手が入っている。
///
/// 選択が無いとき（クリックしただけ）は取り直さない。**範囲を捨てるのは
/// `[ ]`を押したときだけ**である。
fn refresh_find_scope(window: &AppWindow, live: &Live, id: PaneId) {
    let screen = id.screen(window);
    if screen.find_scope_end <= screen.find_scope_start {
        return;
    }
    let state = live.states.of(id);
    let state = state.borrow();
    let Some((start, end)) = selection_source_range(&state) else {
        return;
    };
    if state.search_selection == Some((start, end)) {
        return;
    }
    id.update_screen(window, |screen| {
        screen.find_scope_start = start as i32;
        screen.find_scope_end = end as i32;
    });
    // **効いたかどうかを、画面の外から確かめられるようにしておく**
    // （書き手の報告 2026-09-09：「効いていないように見えます」）。
    let told = format!(
        "scope pane={} took {start}..{end} was {}..{}",
        id.log_name(),
        screen.find_scope_start,
        screen.find_scope_end,
    );
    live.cache.borrow_mut().log_diag("find", &told);
}

fn find_rules(window: &AppWindow, id: PaneId) -> find::Rules {
    let screen = id.screen(window);
    let scope = (screen.find_scope_end > screen.find_scope_start).then(|| {
        (
            screen.find_scope_start.max(0) as usize,
            screen.find_scope_end.max(0) as usize,
        )
    });
    find::Rules {
        match_case: screen.find_match_case,
        whole_word: screen.find_whole_word,
        regex: screen.find_regex,
        within: scope,
    }
}

/// Find the next match and put the selection on it (要件 7.7).
///
/// **The search runs in the pane the writer is in**, over the document that
/// pane is showing, from its caret. Selecting the match rather than only
/// scrolling to it means the next keystroke replaces it and 置換 has something
/// to work with — and it costs nothing, because a selection is what this editor
/// already knows how to show.
fn find_in_pane(window: &AppWindow, live: &Live, forwards: bool) {
    let id = find_target(window);
    let needle = id.screen(window).find_needle.to_string();
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    refresh_find_scope(window, live, id);
    let search = match find_search(window, id) {
        Ok(search) => search,
        Err(trouble) => {
            window.set_count_find(format!("「{needle}」{trouble}").into());
            say_in_bar(window, id, trouble);
            return;
        }
    };
    // E1の④: **探した語はここで覚える。**打っているあいだ（`count_in_pane`）では
    // なく、書き手が「次へ」と言った回である——打鍵ごとに覚えると、`白`『白猫』の
    // 途中の字が10件をすぐ埋める。**読めない正規表現は上で戻っている**ので、
    // 覚えるのは探し方として成り立った語だけ。見つかったかどうかは問わない
    // ——見つからなかった語こそ、打ち直したくないものである。
    live.find_terms.borrow_mut().remember(&needle);
    let caret = id.caret_byte(&state, &source);
    // Forwards from the caret, which after a find sits at the end of the match
    // — so the next one is found rather than the same one again. Backwards from
    // where the selection begins, for the same reason in the other direction.
    let from = if forwards {
        caret
    } else {
        selection_source_range(&state.borrow())
            .map(|(start, _)| start)
            .unwrap_or(caret)
    };
    let step = if forwards { "next" } else { "previous" };
    let Some((start, end)) = search.next(&source, from, forwards) else {
        // **見つからなかったことも、画面に出す。**帯は閉じていることがあり、
        // そのときF3の答えは選択が動くことだけなので、動かなかった回は画面の
        // どこにも現れない（要件 7.7 の「効かない」報告は、たいてい「効いたのが
        // 見えない」である）。ステータスバーは帯より長生きするので、そちらが言う。
        let selected = selection_source_range(&state.borrow());
        tell_find(window, id, &source, selected);
        let told = format!(
            "step pane={} {step} needle={} nothing",
            id.log_name(),
            needle.chars().count(),
        );
        live.cache.borrow_mut().log_diag("find", &told);
        return;
    };
    let (total, which) = search.tally(&source, Some(start));
    let place = which.map_or_else(|| "-".to_owned(), |which| which.to_string());
    let scope = match find_rules(window, id).within {
        Some((from, to)) => format!("{from}..{to}"),
        None => "-".to_owned(),
    };
    let told = format!(
        "step pane={} {step} needle={} at={place}/{total} scope={scope}",
        id.log_name(),
        needle.chars().count(),
    );
    live.cache.borrow_mut().log_diag("find", &told);
    // 帯もステータスバーも、この選択から数え直される（`update_status`）。
    show_source_range(window, live, id, &source, start, end);
}

/// 置換で長さが変わったぶん、範囲の終わりをずらす（E1）。
///
/// **範囲は書き手が選んだものである**ので、置き換えの前と後で同じ本文を囲んで
/// いなければならない——`猫`を`黒猫`にすれば、範囲は1字ぶん伸びる。ずらさない
/// と、範囲の末尾が置き換えのたびに後ろの本文を切り落としていく。
///
/// **打鍵までは追わない。**範囲の中を手で打ち替えれば範囲は意味を失う——
/// そのときは切って選び直すのが早く、編集のたびに範囲を繕うのは、書き手が
/// 選んだものを編集器が黙って作り変えることでもある。
fn shift_find_scope(window: &AppWindow, id: PaneId, by: isize) {
    if by == 0 {
        return;
    }
    id.update_screen(window, |screen| {
        if screen.find_scope_end > screen.find_scope_start {
            let end = screen.find_scope_end as isize + by;
            screen.find_scope_end = end.max(screen.find_scope_start as isize) as i32;
        }
    });
}

/// 探し方の入切（E1）。0＝大小の別、1＝単語単位、2＝範囲の中だけ。
///
/// **範囲だけは押した瞬間に写し取る。**書き手が選んでいる範囲がその場の答えで、
/// あとから訊けるものではない——一致へ移れば選択はその一致になっている。
/// 選ばれていなければ**入らない**：範囲の無い「範囲の中だけ」は、何も見つから
/// ない検索であり、そう見えない（要件 7.7 の「働いていない状態は画面に出て
/// いなければならない」）。
///
/// 押したあとに数え直すので、`12件`が`3件`へ変わるのがその場で見える。
fn choose_find_option(window: &AppWindow, live: &Live, which: i32) {
    let id = find_target(window);
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    let selected = selection_source_range(&state.borrow());
    match which {
        0 => id.update_screen(window, |screen| {
            screen.find_match_case = !screen.find_match_case;
        }),
        1 => id.update_screen(window, |screen| {
            screen.find_whole_word = !screen.find_whole_word;
        }),
        // E1: 正規表現（書き手の求め 2026-09-10）。**入りと切りだけ**で、
        // 誤りは`Search::new`が言う。
        3 => id.update_screen(window, |screen| {
            screen.find_regex = !screen.find_regex;
        }),
        _ => {
            let screen = id.screen(window);
            let on = screen.find_scope_end > screen.find_scope_start;
            let scope = if on { None } else { selected };
            if !on && scope.is_none() {
                say_in_bar(window, id, "範囲が選ばれていません".to_owned());
                return;
            }
            let (start, end) = scope.unwrap_or((0, 0));
            id.update_screen(window, |screen| {
                screen.find_scope_start = start as i32;
                screen.find_scope_end = end as i32;
            });
        }
    }
    tell_find(window, id, &source, selected);
}

/// 件数を数え直すだけで、どこへも動かない（E1）。
///
/// **打っているあいだ帯は数だけを言う。**検索欄で1字打つたびに本文の選択が
/// 飛べば、書き手はまだ打ち終えていない語で連れ回される——IMEで変換の途中なら
/// なおさらで、「IME入力中に本文へフォーカスを奪わない」（E1の完了の目安）は
/// カーソルを動かさないことでもある。動かす鍵はEnterとF3のほうにある。
fn count_in_pane(window: &AppWindow, live: &Live) {
    let id = find_target(window);
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    refresh_find_scope(window, live, id);
    let selected = selection_source_range(&state.borrow());
    tell_find(window, id, &source, selected);
    // **色は打ちながら付いてくる**（E1の③）。一致の矩形は組んだあとにしか
    // 出せないので、数え直しは面の描き直しでもある——帯を閉じたときもここを
    // 通り、そのとき語は無いものとして扱われるので色が消える。
    refresh_pane_from_state(window, &live.cache, &document, id, &state, &source);
}

/// 探している語と、その数を言う——帯の中と、ステータスバーに（E1）。
///
/// **2か所が同じ一巡から出る。**帯は閉じていることがあり、F3はそれでも効く。
/// 答えが帯にしか無ければ、閉じているあいだの検索は画面のどこにも現れない
/// ——**効いたのが見えないのは、効かないのと見分けがつかない**（2026-08-28 の
/// `Ctrl+Tab`と同じ根）。
///
/// **打鍵のたびに数え直すので、古い数が残らない。**本文が変われば数も変わる
/// ものなので、最後に探したときの数を貼り出しておくのは嘘になる。費用は本文を
/// 1回なぞるぶんで、ここで既に数えている行数・文字数と同じ桁である。
///
/// `selected`はいま選ばれている範囲。それが一致であれば「何件目か」が言える
/// ——一致でなければ数だけを言う。**立っていないことは0件目ではない。**
fn tell_find(window: &AppWindow, id: PaneId, source: &str, selected: Option<(usize, usize)>) {
    let needle = id.screen(window).find_needle.to_string();
    if needle.is_empty() {
        window.set_count_find(SharedString::new());
        id.update_screen(window, |screen| {
            screen.find_status = SharedString::new();
        });
        return;
    }
    let search = match find_search(window, id) {
        Ok(search) => search,
        Err(trouble) => {
            // **書きかけの正規表現は「0件」ではない。**打っている途中の`(`に
            // 0件と答えるのは、探し方の話を数の話に見せかけることである。
            window.set_count_find(format!("「{needle}」{trouble}").into());
            id.update_screen(window, |screen| {
                screen.find_status = trouble.into();
            });
            return;
        }
    };
    let standing = selected
        .filter(|(start, end)| search.covers(source, *start, *end))
        .map(|(start, _)| start);
    let (total, which) = search.tally(source, standing);
    let counted = found_status(total, which);
    // 帯の中では語を繰り返さない——書き手が打った欄がすぐ隣にある。
    // ステータスバーでは語を言う。帯が閉じていれば、何を探しているかは
    // 画面のどこにも無いからである。
    // **範囲の中を数えているなら、そう言う**（書き手の報告 2026-09-09：
    // 「`[]`の挙動が不安」）。`3 / 12`が文書ぜんぶの数なのか選んだ範囲の数なのか、
    // 数だけでは見分けられない——見えない状態は、無い状態と同じに見える。
    let scope = if find_rules(window, id).within.is_some() {
        "・範囲内"
    } else {
        ""
    };
    let line = if total == 0 {
        format!("「{needle}」は見つかりません{scope}")
    } else {
        format!("「{needle}」{counted}{scope}")
    };
    window.set_count_find(line.into());
    id.update_screen(window, |screen| {
        screen.find_status = counted.into();
    });
}

/// 帯に一言だけ言わせる——**起きたこと**を（要件 7.7）。
///
/// 数は`tell_find`が言う。ここを通るのは「12件置換しました」のように、数え直せば
/// 消えてしまう出来事だけである。
fn say_in_bar(window: &AppWindow, id: PaneId, told: String) {
    id.update_screen(window, |screen| {
        screen.find_status = told.into();
    });
}

/// 何件あって、いま何件目か（E1）。
///
/// **`3 / 12`は「12件のうちの3件目」**で、一致の上に立っているときだけ言える。
/// 立っていなければ数だけを言い、無ければ無いと言う——数が0のときに`0 / 0`と
/// 出すのは、探し方の話を数の話に見せかけることである。
fn found_status(total: usize, which: Option<usize>) -> String {
    match (total, which) {
        (0, _) => "見つかりません".to_owned(),
        (total, Some(which)) => format!("{which} / {total}"),
        (total, None) => format!("{total}件"),
    }
}

/// Select a range of a pane's source and show it (要件 7.7).
///
/// **Source bytes, whichever mode the pane is in.** The four modes differ in
/// what is drawn, not in what the document is, so a place in it is named the
/// same way in all of them (技術検証 3.12).
fn show_source_range(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    source: &str,
    start: usize,
    end: usize,
) {
    let document = live.states.document(id);
    let state = live.states.of(id);
    select_source_range(
        window,
        &live.cache,
        &document,
        &state,
        id,
        source,
        start,
        end,
    );
    // **これは検索が置いた選択である**（E1）。範囲内検索がこれを見て、書き手が
    // 選び直したのかどうかを見分ける——**選び方が増えても見分けは1つ**なので、
    // 置いたのが検索であることは、置いたあとにここだけが言う。
    state.borrow_mut().search_selection = Some((start, end));
}

/// 範囲を選んで、そこを見せる（要件 7.1、E3）。
///
/// **選ぶ道は1つ。**検索が見つけた一致も、ダブルクリックの語も、行番号を押して
/// 選んだ行も、選ばれている状態としては同じものである——別々に組み立てていると、
/// どれか1つがIMEの下書きを消し忘れる（それが起きるのは、選んだ直後に打った
/// 1文字が消えるときで、原因からいちばん遠いところで見つかる）。
///
/// **`search_selection`は消す。**検索が置いた選択だけがそれを名乗ってよい。
#[allow(clippy::too_many_arguments)]
fn select_source_range(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    state: &Rc<RefCell<EditorState>>,
    id: PaneId,
    source: &str,
    start: usize,
    end: usize,
) {
    {
        let mut state = state.borrow_mut();
        state.selection_anchor_source_byte = Some(start);
        state.caret_source_byte = Some(end);
        state.active_line_start = Some(source_line_start(source, end));
        state.preferred_line = None;
        state.preedit.clear();
        state.rectangular = false;
        state.search_selection = None;
    }
    id.set_ime_buffer(window, "");
    refresh_pane_from_state(window, cache, document, id, state, source);
}

/// Replace what a search found, and go to the next one (要件 7.7).
///
/// **Through the ordinary editing path**, so the replacement is one undo step,
/// reaches every pane showing the document, and is written to the work copy
/// like anything else typed.
fn replace_in_pane(window: &AppWindow, live: &Live) {
    let id = find_target(window);
    let needle = id.screen(window).find_needle.to_string();
    if needle.is_empty() {
        return;
    }
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    refresh_find_scope(window, live, id);
    let selected = selection_source_range(&state.borrow());
    // Only what the search found. A selection that is not the needle means the
    // writer has moved on, and replacing it would take out something they chose
    // themselves.
    //
    // 追加要件 2026-09-09: **訊くのは`find`である。**ここが`==`で訊いていた
    // あいだ、`alpha`で見つけた`Alpha`は選ばれているのに置換されず、次の一致へ
    // 飛んでいた——検索が畳む大小を、置換だけが畳んでいなかった。見つける道と
    // 置き換える道で一致の意味が違えば、画面が言っていることと動作が違う。
    let Ok(search) = find_search(window, id) else {
        // 誤りは`find_in_pane`が言う——置換は探すところから始まる。
        find_in_pane(window, live, true);
        return;
    };
    let on_a_match = selected
        .filter(|(start, end)| search.covers(&source, *start, *end))
        .is_some();
    if !on_a_match {
        find_in_pane(window, live, true);
        return;
    }
    let replacement = id.screen(window).find_replacement.to_string();
    // E1の④: 置き換えに使った語も列に入る。**空は入らない**——「消す」は
    // 置換語ではなく、欄が空であることそのものだからである。
    live.replace_terms.borrow_mut().remember(&replacement);
    shift_find_scope(
        window,
        id,
        replacement.len() as isize - needle.len() as isize,
    );
    insert_pane_text(
        window,
        id,
        &document,
        &live.states,
        &live.cache,
        &replacement,
        false,
    );
    find_in_pane(window, live, true);
}

/// Replace every match, as one change (要件 7.7).
///
/// One undo step and one entry in the history: a replace-all is a single thing
/// the writer asked for, and taking it back a match at a time would be a
/// different operation from the one they did.
fn replace_all_in_pane(window: &AppWindow, live: &Live) {
    let id = find_target(window);
    let screen = id.screen(window);
    let needle = screen.find_needle.to_string();
    if needle.is_empty() {
        return;
    }
    let replacement = screen.find_replacement.to_string();
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    refresh_find_scope(window, live, id);
    let search = match find_search(window, id) {
        Ok(search) => search,
        Err(trouble) => {
            say_in_bar(window, id, trouble);
            return;
        }
    };
    let (next, replaced) = search.replace_all(&source, &replacement);
    if replaced == 0 {
        say_in_bar(window, id, format!("「{needle}」は見つかりません"));
        return;
    }
    // 2026-09-08: **置換も上限の内側にいる。**打鍵も貼り付けも
    // `fits_document_limit`を通るのに、ここだけが結果をそのまま代入していた
    // ——短い語を長い語へ一括で置き換えれば文書は上限を越え、**保存はできて
    // 開き直せないファイル**になる（読み込み側は同じ上限で断る）。
    // **確定の前に測る**ので、断るときは何も起きていない。
    if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
        let told = format!(
            "文書の上限{MAX_DOCUMENT_CHARACTERS}文字を超えるため、{replaced}件の置換を取り消しました"
        );
        say_in_bar(window, id, told);
        return;
    }
    // E1の④: 全置換も「探して直した」1回である。**ここまで来たら確かに起きる**
    // ので、上限で取り消した回は覚えない。
    live.find_terms.borrow_mut().remember(&needle);
    live.replace_terms.borrow_mut().remember(&replacement);
    shift_find_scope(window, id, next.len() as isize - source.len() as isize);
    let (at, removed, inserted) = find::changed_span(&source, &next);
    let change = Change {
        at,
        removed,
        inserted,
    };
    document.record(
        at,
        source[at..at + removed].to_owned(),
        next[at..at + inserted].to_owned(),
    );
    *document.text.borrow_mut() = next.clone();
    let caret = at + inserted;
    {
        let state = live.states.of(id);
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(caret);
        state.selection_anchor_source_byte = Some(caret);
        state.active_line_start = Some(source_line_start(&next, caret));
        state.preferred_line = None;
    }
    id.draw_edit(
        window,
        &live.states,
        &live.cache,
        &document,
        &next,
        caret,
        change,
    );
    say_in_bar(window, id, format!("{replaced}件置換しました"));
}

/// Bring another pane along after an edit (要件 7.6).
///
/// **Named by pane, not by direction.** It used to be two functions — one that
/// pushed the source into "the horizontal pane" and one that redrew "the
/// vertical" — from when a pane *was* a direction. Two panes both showing
/// vertical text made that a pane pushing into itself.
///
/// The caret is clamped whether or not the pane is on screen: a position it
/// holds has to survive every edit made while it was away, or the first refresh
/// after it comes back works from somewhere that no longer exists (6.7).
fn draw_followed_edit(
    window: &AppWindow,
    id: PaneId,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    source: &str,
) {
    if !id.is_shown(window) {
        // The counts under the status bar are this document's and have just
        // changed, whoever is showing it.
        update_status(window, id, document, source, &[], None);
        cache.borrow_mut().source_push_ms = None;
        return;
    }
    let started = Instant::now();
    refresh_pane_from_state(window, cache, document, id, state, source);
    cache.borrow_mut().source_push_ms = Some(elapsed_ms(started));
}

/// The editing area, as the window last reported it.
///
/// The tree cannot hand out pixels it has not been told about, and the window is
/// the only one who knows how many there are.
fn editor_area(window: &AppWindow) -> Rect {
    Rect::new(
        0.0,
        0.0,
        window.get_editor_area_width(),
        window.get_editor_area_height(),
    )
}

/// The work folder, and which of its folders the writer has opened.
#[derive(Default)]
struct WorkFolder {
    root: Option<PathBuf>,
    /// **Paths, not indices.** A row's position changes whenever anything above
    /// it opens or closes; the folder it stands for does not.
    expanded: BTreeSet<PathBuf>,
    /// The row the file commands act on (要件 5.2), by path for the same
    /// reason. Cleared when what it named is no longer among the rows — a
    /// command on something the writer cannot see is a command on nothing.
    selected: Option<PathBuf>,
    /// Which folder the full-text search walks (要件 7.7、2026-09-07追加).
    ///
    /// **`None` is the work folder itself**, and it is written that way rather
    /// than as a copy of `root`: opening another folder would otherwise leave
    /// the search pointing into the one before it, which is the kind of filter
    /// a writer finds by wondering where their words went. Opening a work
    /// folder clears it for the same reason.
    searching: Option<PathBuf>,
}

impl WorkFolder {
    /// Where a folder-wide search starts.
    fn searched_root(&self) -> Option<PathBuf> {
        self.searching.clone().or_else(|| self.root.clone())
    }
}

/// Which of the left pane's three things is showing (要件 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftTab {
    Explorer,
    Search,
    Recent,
    Outline,
}

impl LeftTab {
    /// The number the window holds for this one.
    fn index(self) -> i32 {
        match self {
            Self::Explorer => 0,
            Self::Search => 1,
            Self::Recent => 2,
            Self::Outline => 3,
        }
    }

    /// The number the window holds, in the order the tabs are drawn.
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Search,
            2 => Self::Recent,
            3 => Self::Outline,
            _ => Self::Explorer,
        }
    }
}

/// One line of the 検索 panel (要件 7.7).
#[derive(Clone, Debug)]
struct ResultRow {
    /// What the row says.
    text: String,
    /// A file's own row sits at the left and the lines that matched sit one
    /// step in, the way the tree draws what is inside a folder.
    under_file: bool,
    /// Where clicking it goes.
    path: PathBuf,
    at: usize,
}

/// Draw whichever of the left pane's three things is showing (要件 6.2).
///
/// **One list, three fillers.** A row is the same shape whatever is in it — a
/// name and how far in it sits — so which panel is showing is the only thing
/// that decides what a click on one means (`activate_left_row`).
fn publish_left(window: &AppWindow, live: &Live) {
    show_searched_folder(window, live);
    match LeftTab::from_index(window.get_left_tab()) {
        LeftTab::Explorer => publish_tree(window, live),
        LeftTab::Search => publish_results(window, live),
        LeftTab::Recent => publish_recent(window, live),
        LeftTab::Outline => publish_outline(window, live),
    }
}

/// Say which folder a search would walk (要件 7.7、2026-09-07追加).
///
/// **The name, not the path.** A path long enough to hold a chapter's folder is
/// longer than the panel, and what the writer needs from it is which of the
/// folders they can see they have narrowed to. The whole path is the tooltip.
fn show_searched_folder(window: &AppWindow, live: &Live) {
    let folder = live.folder.borrow();
    let scoped = folder.searching.is_some();
    let told = match folder.searched_root() {
        Some(root) => entry_name(&root),
        None => "作業フォルダがありません".to_owned(),
    };
    let path = folder
        .searched_root()
        .map(|root| root.display().to_string())
        .unwrap_or_default();
    window.set_search_folder(told.into());
    window.set_search_folder_path(path.into());
    window.set_search_folder_scoped(scoped);
}

/// Narrow the folder-wide search to one folder, or widen it back (要件 7.7).
fn search_in_folder(window: &AppWindow, live: &Live, chosen: Option<PathBuf>) {
    live.folder.borrow_mut().searching = chosen.clone();
    show_searched_folder(window, live);
    write_session(window, live);
    live.cache.borrow_mut().log_diag(
        "search",
        &match &chosen {
            Some(path) => format!("scope path={}", path.display()),
            None => "scope whole".to_owned(),
        },
    );
    // **The list on screen answers the old folder**, so it is asked again —
    // and asking with an empty field clears it, which is what the writer would
    // otherwise be left staring at.
    search_work_folder(window, live);
}

/// Draw the headings of the document in front of the writer (要件 7.7).
///
/// **Where a row goes is not kept.** The outline is the document read again,
/// so there is no list of positions to hold and none to keep in step with an
/// edit: a click works the headings out the same way (`go_to_heading`), and
/// what it finds is what the source says now. The copy in `outline_drawn` is
/// only there to answer "has this changed", never to say where anything is.
fn publish_outline(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let source = document.text.borrow();
    let mut cache = live.cache.borrow_mut();
    // Switching to the panel draws it whatever was drawn last: the rows in
    // front of the writer are another panel's, however unchanged the headings.
    cache.outline_drawn.clear();
    draw_outline(window, &mut cache, source.as_str());
}

/// Put an outline of `source` in the left pane.
fn draw_outline(window: &AppWindow, cache: &mut RenderCache, source: &str) {
    let headings = document::outline(source);
    if cache.outline_drawn == headings {
        return;
    }
    let drawn = headings
        .iter()
        .map(|heading| LeftRow {
            name: heading.text.clone().into(),
            // H1 sits at the left and each level steps in, which is the same
            // reading of `depth` the tree has.
            depth: i32::from(heading.level).saturating_sub(1),
            folder: false,
            open: false,
            parent: -1,
        })
        .collect::<Vec<_>>();
    cache.outline_drawn = headings;
    window.set_tree_selected(-1);
    window.set_left_rows(ModelRc::new(VecModel::from(drawn)));
}

/// Draw the outline again if it is what the left pane is showing.
///
/// Called from every pane refresh, so a heading appears in the list as it is
/// typed. Reading the source costs one `split('\n')` in which a line that does
/// not begin with a hash is dismissed on its first character; **the list is
/// only handed to the window when it has actually changed**, so a keystroke
/// that adds no heading costs the read and nothing else.
fn draw_outline_if_showing(window: &AppWindow, cache: &mut RenderCache, source: &str) {
    if LeftTab::from_index(window.get_left_tab()) == LeftTab::Outline {
        draw_outline(window, cache, source);
    }
}

/// Draw what the last folder-wide search found (要件 7.7).
fn publish_results(window: &AppWindow, live: &Live) {
    let results = live.results.borrow();
    let drawn = results
        .iter()
        .map(|row| LeftRow {
            name: row.text.clone().into(),
            depth: i32::from(row.under_file),
            folder: false,
            open: false,
            parent: -1,
        })
        .collect::<Vec<_>>();
    // The file commands act on the tree, and nothing here is a row of it.
    window.set_tree_selected(-1);
    window.set_left_rows(ModelRc::new(VecModel::from(drawn)));
}

/// Draw the files opened most recently (要件 7.7).
fn publish_recent(window: &AppWindow, live: &Live) {
    let recent = live.recent.borrow();
    let drawn = recent
        .iter()
        .map(|path| LeftRow {
            name: remembered_name(path).into(),
            depth: 0,
            folder: false,
            open: false,
            parent: -1,
        })
        .collect::<Vec<_>>();
    window.set_tree_selected(-1);
    window.set_left_rows(ModelRc::new(VecModel::from(drawn)));
}

/// A row of the left pane was clicked (要件 6.2).
///
/// **What it means is the panel's business.** The rows are one shape; a click
/// on one is three different things.
fn activate_left_row(window: &AppWindow, live: &Live, index: usize) {
    match LeftTab::from_index(window.get_left_tab()) {
        LeftTab::Explorer => activate_tree_row(window, live, index),
        LeftTab::Search => open_result(window, live, index),
        LeftTab::Recent => open_remembered(window, live, index),
        LeftTab::Outline => go_to_heading(window, live, index),
    }
}

/// Go to a heading the outline lists (要件 7.7).
///
/// The outline is worked out again rather than remembered, so the row and the
/// document cannot disagree: whatever the source says now is both what was
/// drawn and where this goes.
fn go_to_heading(window: &AppWindow, live: &Live, index: usize) {
    let id = focused_pane(window);
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let Some(heading) = document::outline(&source).into_iter().nth(index) else {
        return;
    };
    show_source_range(window, live, id, &source, heading.at, heading.at);
}

/// Go to what a folder-wide search found (要件 7.7).
///
/// **The match is looked for again in the text that is open**, rather than the
/// byte from the search being trusted. The file may have been edited since —
/// by the writer, in another tab — and a byte in a document that has moved
/// under it is a place nobody asked for. `next_match` from there lands on the
/// same match when nothing has changed, and on the nearest one after it when
/// something has.
fn open_result(window: &AppWindow, live: &Live, index: usize) {
    let Some(found) = live.results.borrow().get(index).cloned() else {
        return;
    };
    open_path_in_focused_pane(window, live, &found.path, Opening::Peeked);
    let id = focused_pane(window);
    let document = live.states.document(id);
    let showing = document.file.borrow().path() == Some(found.path.as_path());
    if !showing {
        return;
    }
    let needle = window.get_folder_needle().to_string();
    let source = document.text.borrow().clone();
    // **フォルダ全文検索は帯の切り替えを持たない**ので、既定の探し方で探し直す
    // ——`hits_in`がその規則で見つけたものを、同じ規則で指し直すのでなければ、
    // 一覧の行と本文の位置が食い違う。
    let Ok(search) = find::Search::new(&needle, find::Rules::default()) else {
        return;
    };
    let Some((start, end)) = search.next(&source, found.at, true) else {
        return;
    };
    show_source_range(window, live, id, &source, start, end);
}

/// Open something from the history (要件 7.7).
fn open_remembered(window: &AppWindow, live: &Live, index: usize) {
    let Some(path) = live.recent.borrow().get(index).cloned() else {
        return;
    };
    open_path_in_focused_pane(window, live, &path, Opening::Peeked);
    // Opening it moves it to the top, so the list under the writer's hand has
    // changed and has to be drawn again.
    publish_left(window, live);
}

/// What something is called in a history (要件 5.1, 7.7).
///
/// **With the folder holding it**, because a work folder full of chapters has
/// several files called the same thing, and a list of identical names is not a
/// list anybody can choose from. Folders are named the same way, for the same
/// reason: several years of notes are all called `notes`.
fn remembered_name(path: &Path) -> String {
    let name = entry_name(path);
    match path.parent().map(entry_name) {
        Some(folder) if !folder.is_empty() => format!("{name} — {folder}"),
        _ => name,
    }
}

/// How many files the history keeps.
const REMEMBERED_FILES: usize = 30;

/// How many work folders the history keeps (要件 5.1).
///
/// **Shorter than the file history**, because this one is a menu rather than a
/// panel: a list that runs off the bottom of the screen is not one anybody
/// picks from.
const REMEMBERED_FOLDERS: usize = 10;

/// Put a path at the top of a history (要件 5.1, 7.7).
///
/// **Newest first, each path once.** Opening something already in the list
/// moves it up rather than repeating it, which is what makes a short list worth
/// reading. Files and work folders are two lists of different lengths and the
/// same rule, so the rule is written once.
fn remember_path(history: &mut Vec<PathBuf>, path: &Path, keep: usize) {
    history.retain(|held| held != path);
    history.insert(0, path.to_path_buf());
    history.truncate(keep);
}

/// Put a file at the top of the history (要件 7.7).
fn remember_recent(live: &Live, path: &Path) {
    let mut recent = live.recent.borrow_mut();
    remember_path(&mut recent, path, REMEMBERED_FILES);
}

/// Put a work folder at the top of the history (要件 5.1).
fn remember_folder(live: &Live, path: &Path) {
    let mut folders = live.recent_folders.borrow_mut();
    remember_path(&mut folders, path, REMEMBERED_FOLDERS);
}

/// Stop offering a folder (要件 5.1).
fn forget_folder(live: &Live, path: &Path) {
    let mut folders = live.recent_folders.borrow_mut();
    folders.retain(|held| held != path);
}

/// The folders the writer can go to (要件 5.1).
///
/// **The head of the history is where they already are**, so it is left out:
/// every row of the menu goes somewhere. Worked out again rather than kept
/// beside the rows, so what a click means cannot drift from what was drawn.
fn offered_folders(live: &Live) -> Vec<PathBuf> {
    let here = live.folder.borrow().root.clone();
    let folders = live.recent_folders.borrow();
    folders
        .iter()
        .filter(|path| Some(path.as_path()) != here.as_deref())
        .cloned()
        .collect()
}

/// Draw the folders the writer has worked in (要件 5.1).
///
/// **The count is logged**, because "the menu is empty" and "the menu is not
/// there" look the same from the outside and are two different faults.
fn publish_folder_history(window: &AppWindow, live: &Live) {
    let folders = offered_folders(live);
    let drawn = folders
        .iter()
        .map(|path| SharedString::from(remembered_name(path)))
        .collect::<Vec<_>>();
    let count = drawn.len();
    let held = live.recent_folders.borrow().len();
    window.set_recent_folders(ModelRc::new(VecModel::from(drawn)));
    let mut cache = live.cache.borrow_mut();
    cache.log_diag("folder", &format!("history held={held} offered={count}"));
}

/// Work in a folder (要件 5.1).
///
/// **One folder to a window**, so the one that was open goes — and with it
/// every folder that was open inside it. The session is written on the way out
/// rather than on the way in: nothing here closes a tab, so what it holds is
/// the arrangement as it stands, now against the folder just opened.
fn open_work_folder(window: &AppWindow, live: &Live, chosen: &Path) {
    {
        let mut folder = live.folder.borrow_mut();
        folder.root = Some(chosen.to_path_buf());
        folder.expanded.clear();
        // 要件 7.7: **絞り込みは、絞り込んだフォルダと一緒に去る。**別の作業
        // フォルダを開いたのに検索だけ前のフォルダを歩いていたら、書き手は
        // 「言葉がどこへ行ったのか」を探すことになる。
        folder.searching = None;
    }
    remember_folder(live, chosen);
    publish_folder_history(window, live);
    publish_left(window, live);
    write_session(window, live);
    let mut cache = live.cache.borrow_mut();
    cache.log_diag("folder", &format!("opened path={}", chosen.display()));
}

/// Go back to a folder from the history (要件 5.1).
///
/// **A folder that is no longer there is dropped rather than opened.** Nothing
/// watches the history — a folder can be renamed or unplugged between two runs
/// — so the answer to picking one that has gone is to stop offering it.
fn go_to_remembered_folder(window: &AppWindow, live: &Live, index: usize) {
    let Some(path) = offered_folders(live).into_iter().nth(index) else {
        return;
    };
    if path.is_dir() {
        open_work_folder(window, live, &path);
        return;
    }
    forget_folder(live, &path);
    publish_folder_history(window, live);
    write_session(window, live);
    let mut cache = live.cache.borrow_mut();
    cache.log_diag("folder", &format!("gone path={}", path.display()));
}

/// The most files one folder-wide search reads.
const SEARCHED_FILES: usize = 5_000;
/// The most lines it reports in all, and the most from any one file.
const REPORTED_HITS: usize = 500;
const HITS_PER_FILE: usize = 50;

/// Search every file in the work folder (要件 7.7).
///
/// **Asked for here and answered somewhere else.** This is the one place the
/// editor reads many files at once, and 要件 2 wants that off the UI thread —
/// so what happens here is that a question is written down and handed over.
/// The answer comes back through [`collect_search`], turns of the event loop
/// later, and may be for a word the writer has already stopped typing.
///
/// The bounds stay. They were the whole defence when this ran on the UI thread
/// and they are still what makes a work folder somebody dropped a repository
/// into merely unhelpful: a list of ten thousand lines is no better than a
/// wait, whichever thread built it.
///
/// **The old way is still here**, for a run where the thread could not be
/// started. It is the same search on the same data; only who waits for it
/// differs.
fn search_work_folder(window: &AppWindow, live: &Live) {
    let needle = window.get_folder_needle().to_string();
    let Some(root) = live.folder.borrow().searched_root() else {
        window.set_folder_status("作業フォルダがありません".into());
        return;
    };
    if needle.is_empty() {
        // **Nothing to wait for, so nothing is waited for.** The generation
        // still moves, or an answer already in flight would land on the empty
        // list a moment after it was cleared.
        live.searched.set(live.searched.get() + 1);
        live.results.borrow_mut().clear();
        window.set_folder_status(SharedString::new());
        publish_left(window, live);
        return;
    }
    let generation = live.searched.get() + 1;
    live.searched.set(generation);
    let job = SearchJob {
        root,
        needle,
        generation,
        files: SEARCHED_FILES,
        hits_per_file: HITS_PER_FILE,
        hits_in_all: REPORTED_HITS,
        characters: MAX_DOCUMENT_CHARACTERS,
    };
    let handed_over = match live.searcher.dispatch(job) {
        Ok(()) => {
            window.set_folder_status("検索しています…".into());
            true
        }
        Err(job) => {
            if let Some(outcome) = searcher::search(&job, &mut NeverSuperseded) {
                show_search(window, live, &outcome);
            }
            false
        }
    };
    // **The question, not only the answer.** What this thread does that the
    // old one could not is give up on a search and answer a newer one instead,
    // and none of that is visible on screen when the folder is small enough to
    // answer at once: the pane simply shows the newest list either way. In the
    // log the asking and the dropping each leave a line, so the order they
    // happened in can be read afterwards.
    let asked = format!("asked gen={generation} thread={}", u8::from(handed_over));
    live.cache.borrow_mut().log_diag("search", &asked);
}

/// Take whatever the searching thread has finished (要件 7.7).
///
/// Rung by that thread through the event loop, so it runs here, on the editor's
/// own thread, with everything the editor holds to hand.
///
/// **An outcome for an older question is dropped without being drawn.** The
/// thread abandons a search as soon as a newer one reaches it, but one that was
/// already finished when the next was asked still arrives, and drawing it would
/// answer a word that is no longer in the field.
fn collect_search(window: &AppWindow, live: &Live) {
    let wanted = live.searched.get();
    let finished = live.searcher.drain();
    for outcome in &finished {
        if outcome.generation == wanted {
            continue;
        }
        let (stale, ms) = (outcome.generation, outcome.ms);
        let dropped = format!("stale gen={stale} wanted={wanted} ms={ms:.2}");
        live.cache.borrow_mut().log_diag("search", &dropped);
    }
    let Some(outcome) = finished.iter().find(|found| found.generation == wanted) else {
        return;
    };
    show_search(window, live, outcome);
}

/// Put one search's matches in the left pane (要件 6.2, 7.7).
///
/// **A file's row, then the lines under it**, the way the tree draws what is
/// inside a folder. The thread hands over matches; what a row says is decided
/// here, where the pane is.
fn show_search(window: &AppWindow, live: &Live, outcome: &SearchOutcome) {
    let mut rows: Vec<ResultRow> = Vec::new();
    for file in &outcome.files {
        let Some(first) = file.hits.first() else {
            continue;
        };
        rows.push(ResultRow {
            text: entry_name(&file.path),
            under_file: false,
            path: file.path.clone(),
            at: first.at,
        });
        for hit in &file.hits {
            rows.push(ResultRow {
                text: format!("{}: {}", hit.line, hit.preview),
                under_file: true,
                path: file.path.clone(),
                at: hit.at,
            });
        }
    }
    *live.results.borrow_mut() = rows;
    let total = outcome.total;
    let answered = outcome.files.len();
    let told = if total == 0 {
        format!("「{}」は見つかりません", outcome.needle)
    } else {
        format!("{total}件 / {answered}ファイル")
    };
    window.set_folder_status(told.into());
    let (generation, ms) = (outcome.generation, outcome.ms);
    let message = format!("found gen={generation} hits={total} files={answered} ms={ms:.2}");
    live.cache.borrow_mut().log_diag("search", &message);
    publish_left(window, live);
}

/// Put the work folder's tree in front of the writer (要件 5.2).
///
/// Read afresh each time rather than watched: a tree is redrawn when the writer
/// opens a folder, opens the work folder or opens a file, and each of those is
/// something they did. Watching the disk is a separate thing, and 要件 8.3 only
/// asks for it on the file being edited.
fn publish_tree(window: &AppWindow, live: &Live) {
    let mut folder = live.folder.borrow_mut();
    let Some(root) = folder.root.clone() else {
        window.set_work_folder(SharedString::new());
        window.set_left_rows(ModelRc::new(VecModel::from(Vec::<LeftRow>::new())));
        window.set_tree_selected(-1);
        return;
    };
    let rows = file_tree::rows(&root, &folder.expanded, &file_tree::read_folder);
    // The selection is held by path, so **where it sits is worked out afresh
    // every time the rows are** — and a path that is no longer among them is
    // let go, because a command on a row nobody can see is a command on
    // nothing.
    let selected = folder
        .selected
        .as_ref()
        .and_then(|path| rows.iter().position(|row| &row.path == path));
    if selected.is_none() {
        folder.selected = None;
    }
    window.set_tree_selected(selected.map_or(-1, |at| at as i32));
    let holders = file_tree::holders(&rows);
    let drawn = rows
        .iter()
        .zip(&holders)
        .map(|(row, &parent)| LeftRow {
            name: row.name.clone().into(),
            depth: row.depth as i32,
            folder: row.folder,
            open: row.open,
            parent,
        })
        .collect::<Vec<_>>();
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    window.set_work_folder(name.into());
    window.set_left_rows(ModelRc::new(VecModel::from(drawn)));
    // Held beside the rows so a click can name one: the model the window has is
    // only what it draws, and a path is not part of that.
    *live.tree_paths.borrow_mut() = rows.into_iter().map(|row| row.path).collect();
}

/// A row of the tree was clicked (要件 5.2).
///
/// A folder opens or closes; a file is opened in the pane the writer is in. A
/// file already open somewhere is not opened twice — the tab holding it is
/// brought forward instead, which is what 要件 6.3 means by a tab per file.
fn activate_tree_row(window: &AppWindow, live: &Live, index: usize) {
    let Some(path) = live.tree_paths.borrow().get(index).cloned() else {
        return;
    };
    // Whatever was clicked is what the file commands act on (要件 5.2).
    live.folder.borrow_mut().selected = Some(path.clone());
    if path.is_dir() {
        {
            let mut folder = live.folder.borrow_mut();
            if !folder.expanded.remove(&path) {
                folder.expanded.insert(path);
            }
        }
        publish_left(window, live);
        write_session(window, live);
        return;
    }
    open_path_in_focused_pane(window, live, &path, Opening::Peeked);
    publish_left(window, live);
}

/// Choose a row without acting on it (要件 5.2).
///
/// **Nothing is drawn again.** The window has already moved the highlight, and
/// drawing the tree here would replace the rows — including the one whose menu
/// is opening. What is left to do is remember which path the commands act on,
/// and `publish_tree` works the row number out from that path the next time it
/// runs.
fn pick_tree_row(live: &Live, index: usize) {
    let Some(path) = live.tree_paths.borrow().get(index).cloned() else {
        return;
    };
    live.folder.borrow_mut().selected = Some(path);
}

/// Put a file in front of the writer, opening it only if it is not open already.
fn open_path_in_focused_pane(window: &AppWindow, live: &Live, path: &Path, opening: Opening) {
    open_path_in_pane(window, live, focused_pane(window), path, opening);
}

/// The same, into a pane the caller names.
///
/// Startup is why this is separate: [`focused_pane`] asks whether a pane is on
/// screen, and before the window has been shown nothing has a width, so it
/// answers for the pane that is *not* about to be in front. A file named on the
/// command line went to the hidden pane and looked like it had not opened at
/// all.
fn open_path_in_pane(window: &AppWindow, live: &Live, id: PaneId, path: &Path, opening: Opening) {
    let (held, yielding) = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        let held = strip
            .tabs
            .iter()
            .position(|tab| tab.document.file.borrow().path() == Some(path));
        // **場所を譲るタブは1枚だけ**（書き手の報告 2026-09-07、追加要件
        // 2026-09-07）。位置は毎回数え直す——タブは並び替えられるし閉じられるので、
        // 覚えた番号は次の瞬間には別のタブを指している（6.4で3度やった間違い）。
        // **空のタブが先**：まだ何でもないものが開いているなら、そこが開き先である。
        let yielding = strip
            .tabs
            .iter()
            .position(|tab| tab.empty)
            .or_else(|| strip.tabs.iter().position(|tab| tab.yields_to(opening)));
        (held, yielding)
    };
    if let Some(index) = held {
        // 要件 7.7: bringing a file forward is opening it, tab or no tab.
        remember_recent(live, path);
        if opening == Opening::Kept {
            let tabs = live.tabs.borrow();
            if let Some(tab) = tabs.of(id).tabs.get(index) {
                tab.provisional.set(false);
            }
        }
        switch_to_tab(window, live, id, index);
        // **切り替えは同じタブなら何もしない**ので、傾いた字を立てるための
        // 描き直しはこちらから頼む。
        publish_tabs(window, live);
        return;
    }
    // Open somewhere else in the window is still the same document, and 要件 7.6
    // wants one text however many panes are showing it.
    let elsewhere = open_documents(live)
        .into_iter()
        .find(|document| document.file.borrow().path() == Some(path));
    let shown = path.display().to_string();
    let document = match elsewhere {
        Some(document) => document,
        None => match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
            Ok((file, text)) => {
                let bytes = text.len();
                let mixed = file.mixed_newlines();
                live.cache.borrow_mut().log_diag(
                    "file",
                    &format!("open ok bytes={bytes} mixed={mixed} path={shown}"),
                );
                // **改行の混在だけは言う**（2026-09-08、ダイアログの道から
                // 移してきた）。読み込みが黙って揃えたことを、揃えられた側は
                // 画面からしか知りようがない。開けたこと自体は画面が言って
                // いる——タブがそこに増えている。
                if mixed {
                    window.set_render_status("改行コードが混在していました".into());
                }
                OpenDocument::new(file, text, window.as_weak())
            }
            Err(error) => {
                window.set_render_status(format!("開けません: {error}").into());
                live.cache
                    .borrow_mut()
                    .log_diag("file", &format!("open failed path={shown} error={error}"));
                return;
            }
        },
    };
    // 要件 4.2（書き手の報告 2026-09-08）: **Markdownでないものは横書きのソースで
    // 開く。**縦書きのペインで設定ファイルを開いたら設定ファイルまで縦書きに
    // なっていた——記法の無いテキストに整形表示は無く、`key: value`の並びを縦に
    // 組んでも読めない。
    //
    // **開くときだけである。**そのあと縦書きにするのは書き手の自由——縦書きで
    // 書く人は`.txt`の原稿も縦で読みたい（要件 3）。
    let markdown = is_markdown_path(path);
    let tab = PaneTab {
        view: TabView {
            vertical: markdown && id.vertical(window),
            preview: markdown && id.shows_preview(window),
            ..TabView::default()
        },
        provisional: Cell::new(opening == Opening::Peeked),
        ..PaneTab::showing(window, id, document)
    };
    match yielding {
        Some(index) => replace_tab(window, live, id, index, tab),
        None => add_tab(window, live, id, tab),
    }
    // Recorded once it is open, so a file that could not be read does not sit
    // in the history as though it had been (要件 7.7).
    remember_recent(live, path);
}

/// 要件 4.2: この道はMarkdownか。
///
/// **拡張子で判じる。**中身を見て決めると、`#`で始まる設定ファイルがMarkdownに
/// なり、見出しの無い原稿がそうでなくなる——どちらも書き手には理由が見えない。
fn is_markdown_path(path: &Path) -> bool {
    path.extension().is_some_and(|held| {
        held.eq_ignore_ascii_case("md")
            || held.eq_ignore_ascii_case("markdown")
            || held.eq_ignore_ascii_case("mdown")
    })
}

/// Put a file in the tab the pane is only looking through (書き手の報告 2026-09-07).
///
/// **A close and an open in one step, and it can lose nothing**: the tab it
/// writes over is provisional, and a tab holding unsaved work stopped being
/// provisional at the first character (`is_provisional`). It keeps the tab's
/// place in the strip, which is the whole point — the strip does not grow, and
/// the file the writer is walking towards stays under the same finger.
fn replace_tab(window: &AppWindow, live: &Live, id: PaneId, index: usize, tab: PaneTab) {
    write_work_copy_now(window, live);
    sync_active_tab(window, live);
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        if index >= strip.tabs.len() {
            return;
        }
        strip.tabs[index] = tab.clone();
        strip.active = index;
    }
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("peek pane={} at={index}", id.log_name()));
    live.show_tab(window, id, &tab);
    note_navigation(live, id, &tab);
    publish_tabs(window, live);
}

/// The files named on the command line, in the order they were named.
///
/// This is the whole of "double click a `.md` file and it opens here": the
/// shell runs the editor with the file's path as its argument, so an
/// association is a matter of reading the arguments at startup and nothing
/// else. Several are handled because a selection of files can be opened at
/// once, and they become tabs in the order the shell named them.
///
/// **Everything but a flag is taken as a path**, and a path that cannot be read
/// says so in the status bar. Whether the file exists is deliberately not asked
/// here: a name the shell handed over and this quietly dropped is the one case
/// that looks exactly like the shell having handed over nothing, and those two
/// have to be told apart.
///
/// Made absolute while the current directory is still the one the editor was
/// started in: opening a folder moves it, and a relative path kept until then
/// would name a different file (the same reason the logs are written beside the
/// executable — see [`diag::beside_executable`]).
fn paths_from_command_line() -> Vec<PathBuf> {
    std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .filter(|path| {
            let name = path.to_string_lossy();
            !name.is_empty() && !name.starts_with('-')
        })
        .map(|path| std::path::absolute(&path).unwrap_or(path))
        .collect()
}

/// What the buttons over the tree ask for (要件 5.2).
///
/// **One callback carrying a number**, the way a pane is named by its index:
/// the window knows which button was pressed and nothing else about it, and
/// every one of these lands in the same place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeCommand {
    NewFile,
    NewFolder,
    Rename,
    Duplicate,
    Delete,
    Reveal,
    /// 要件 7.7（2026-09-07追加）: 全文検索をこのフォルダだけにする。
    ///
    /// **The gesture belongs to the tree.** Narrowing a search means naming a
    /// folder, and the folders are on screen already — sending the writer to a
    /// file dialog to point at one they can see is the long way round.
    SearchIn,
}

impl TreeCommand {
    /// The number the window sends, in the order the buttons are drawn.
    fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::NewFile),
            1 => Some(Self::NewFolder),
            2 => Some(Self::Rename),
            3 => Some(Self::Duplicate),
            4 => Some(Self::Delete),
            5 => Some(Self::Reveal),
            6 => Some(Self::SearchIn),
            _ => None,
        }
    }
}

/// What to call something in the tree: the last part of its path.
fn entry_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

/// A file command from the buttons over the tree (要件 5.2).
///
/// The ones that need a name ask for it and stop; the answer comes back a turn
/// of the event loop later, like every other question (`answer_question`). The
/// ones that do not are done here.
fn tree_command(window: &AppWindow, live: &Live, command: TreeCommand) {
    let Some(root) = live.folder.borrow().root.clone() else {
        return;
    };
    let selected = live.folder.borrow().selected.clone();
    // Asked of the disk rather than remembered: what the row stands for is a
    // path, and a folder is a folder however the row was drawn.
    let is_folder = selected.as_deref().map(Path::is_dir).unwrap_or(false);
    match command {
        TreeCommand::NewFile => {
            let chosen = selected.as_deref();
            let into = file_tree::destination_folder(chosen, is_folder, &root);
            let taken = file_tree::names_in(&into);
            let suggested = file_tree::unique_name(&taken, "無題.md");
            ask_for_name(
                window,
                live,
                Question::NewFile(into),
                "新しいファイルの名前を入れてください。".to_string(),
                &suggested,
            );
        }
        TreeCommand::NewFolder => {
            let chosen = selected.as_deref();
            let into = file_tree::destination_folder(chosen, is_folder, &root);
            let taken = file_tree::names_in(&into);
            let suggested = file_tree::unique_name(&taken, "新しいフォルダー");
            ask_for_name(
                window,
                live,
                Question::NewFolder(into),
                "新しいフォルダーの名前を入れてください。".to_string(),
                &suggested,
            );
        }
        TreeCommand::Rename => {
            let Some(path) = selected else {
                return;
            };
            let now_called = entry_name(&path);
            ask_for_name(
                window,
                live,
                Question::RenameEntry(path),
                "新しい名前を入れてください。".to_string(),
                &now_called,
            );
        }
        TreeCommand::Duplicate => {
            let Some(path) = selected else {
                return;
            };
            match file_tree::duplicate(&path) {
                Ok(copy) => {
                    // The copy is what the writer is now standing on: it is
                    // what they asked for, and the next thing they do — rename
                    // it, open it — is to it.
                    live.folder.borrow_mut().selected = Some(copy);
                    publish_left(window, live);
                    write_session(window, live);
                }
                Err(error) => {
                    let told = format!("複製できません: {error}");
                    window.set_render_status(told.into());
                }
            }
        }
        TreeCommand::Delete => {
            let Some(path) = selected else {
                return;
            };
            let going = entry_name(&path);
            ask_question(
                window,
                live,
                Question::DeleteEntry(path),
                format!(
                    "「{going}」をごみ箱へ移動します。\n\n\
                     Windowsのごみ箱から戻せます。"
                ),
                &["ごみ箱へ移動", "キャンセル"],
                0,
            );
        }
        TreeCommand::Reveal => {
            let Some(path) = selected else {
                return;
            };
            shell::reveal(&path);
        }
        TreeCommand::SearchIn => {
            // **A file names its folder.** The row the writer pressed is the
            // one they mean, and what a search can be given is a folder — the
            // one holding that file is the only reading of it.
            let into = match selected {
                Some(path) if path.is_dir() => Some(path),
                Some(path) => path.parent().map(Path::to_path_buf),
                None => None,
            };
            let Some(into) = into else {
                return;
            };
            search_in_folder(window, live, Some(into));
            window.set_left_tab(LeftTab::Search.index());
            publish_left(window, live);
        }
    }
}

/// Make the file or folder the writer has just named (要件 5.2).
fn make_entry(window: &AppWindow, live: &Live, parent: &Path, folder: bool) {
    let typed = window.get_question_name().to_string();
    let name = match file_tree::check_name(&typed) {
        Ok(name) => name,
        Err(problem) => {
            window.set_render_status(problem.message().into());
            return;
        }
    };
    let path = parent.join(name);
    let made = if folder {
        file_tree::create_folder(&path)
    } else {
        file_tree::create_file(&path)
    };
    if let Err(error) = made {
        let told = format!("作れません: {error}");
        window.set_render_status(told.into());
        return;
    }
    {
        let mut open = live.folder.borrow_mut();
        open.selected = Some(path.clone());
        // The folder it went into is opened, or what was just made would not
        // be on screen at all.
        open.expanded.insert(parent.to_path_buf());
    }
    // A new file is opened in the pane the writer is in: making one is how a
    // note starts, and the step to it is not one they meant to take.
    if !folder {
        open_path_in_focused_pane(window, live, &path, Opening::Kept);
    }
    publish_left(window, live);
    write_session(window, live);
}

/// Give the selected file or folder the name the writer has typed (要件 5.2).
fn rename_entry(window: &AppWindow, live: &Live, from: &Path) {
    let typed = window.get_question_name().to_string();
    let name = match file_tree::check_name(&typed) {
        Ok(name) => name,
        Err(problem) => {
            window.set_render_status(problem.message().into());
            return;
        }
    };
    let Some(parent) = from.parent() else {
        return;
    };
    let to = parent.join(name);
    if let Err(error) = move_entry(window, live, from, &to) {
        let told = format!("名前を変えられません: {error}");
        window.set_render_status(told.into());
        return;
    }
    publish_left(window, live);
    write_session(window, live);
}

/// A row carried through the tree was let go (要件 5.2).
///
/// **The window has already said which folder it would land in**, and this is
/// that same answer rather than a second one: `onto` is the row the marks lit,
/// which is the row itself when a folder was pointed at and the folder holding
/// it when anything else was. Nothing is asked of the disk about where a thing
/// may go — the rules that decided the marks are the rules
/// (`file_tree::move_target` states the last of them, for the paths).
fn drop_tree_row(window: &AppWindow, live: &Live, from: usize, onto: i32) {
    let Some(root) = live.folder.borrow().root.clone() else {
        return;
    };
    let (source, into) = {
        let paths = live.tree_paths.borrow();
        let Some(source) = paths.get(from).cloned() else {
            return;
        };
        // `-1` is the work folder itself, which the panel's heading stands for.
        // Anything else that is not a row is a hand let go over nothing.
        let into = match onto {
            -1 => Some(root),
            at if at >= 0 => paths.get(at as usize).cloned(),
            _ => None,
        };
        (source, into)
    };
    let Some(into) = into else {
        return;
    };
    let Some(to) = file_tree::move_target(&source, &into) else {
        return;
    };
    // **A name already taken is a question, not a refusal** (要件 5.2). The
    // writer aimed at a folder, not at the thing that happens to be in it, so
    // the answer they have not given yet is whether that thing may go.
    if to.exists() {
        let going = entry_name(&to);
        ask_question(
            window,
            live,
            Question::ReplaceOnMove(source, to),
            format!(
                "「{going}」はすでにあります。\n\n\
                 いまある「{going}」はごみ箱へ移ります。Windowsのごみ箱から戻せます。"
            ),
            &["上書きする", "キャンセル"],
            0,
        );
        return;
    }
    finish_move(window, live, &source, &to);
}

/// Put what is already there in the recycle bin, then move (要件 5.2).
///
/// **Overwriting is deleting**, and 要件 5.2 says what deleting means here: the
/// bin, never the void. The move follows only if the bin took it, so a refusal
/// leaves both the writer's file and the one that was there.
fn replace_on_move(window: &AppWindow, live: &Live, from: &Path, to: &Path) {
    let owner = ime::window_handle(window);
    if !shell::recycle(owner, to) {
        window.set_render_status("ごみ箱へ移動できませんでした".into());
        return;
    }
    finish_move(window, live, from, to);
}

/// Carry out a move that has nothing left to ask (要件 5.2).
fn finish_move(window: &AppWindow, live: &Live, from: &Path, to: &Path) {
    if let Err(error) = move_entry(window, live, from, to) {
        let told = format!("移動できません: {error}");
        window.set_render_status(told.into());
        return;
    }
    // The folder it went into is opened, or what was just carried there would
    // not be on screen at all — the same as a file that has just been made.
    if let Some(into) = to.parent() {
        live.folder.borrow_mut().expanded.insert(into.to_path_buf());
        let message = format!("move {} into={}", from.display(), into.display());
        live.cache.borrow_mut().log_diag("folder", &message);
    }
    publish_left(window, live);
    write_session(window, live);
}

/// Put a file or folder where it is going, and take what points at it along
/// (要件 5.2).
///
/// **A rename and a move are the same act**: both give something a different
/// path, and everything that has to follow — the open documents, the folders
/// that were unfolded, what the writer is standing on — follows the path and
/// not the name. Drawing the tree again is left to the caller, because the one
/// that carried something into a folder has that folder to open first.
fn move_entry(window: &AppWindow, live: &Live, from: &Path, to: &Path) -> std::io::Result<()> {
    file_tree::rename(from, to)?;
    documents_follow(window, live, from, to);
    let mut open = live.folder.borrow_mut();
    open.selected = Some(to.to_path_buf());
    // A folder that was open stays open where it has gone, and so does every
    // folder inside it: both are held by path.
    let mut moved = BTreeSet::new();
    for path in &open.expanded {
        let after = file_tree::moved_path(from, to, path);
        moved.insert(after.unwrap_or_else(|| path.clone()));
    }
    open.expanded = moved;
    Ok(())
}

/// Point every open document at where its file has just gone (要件 5.2).
///
/// **A rename is not a way to make somebody re-open their work.** The document
/// is the same one — same text, same history, same tab — and only the name it
/// is filed under has changed.
fn documents_follow(window: &AppWindow, live: &Live, from: &Path, to: &Path) {
    for document in open_documents(live) {
        let moved = {
            let file = document.file.borrow();
            let path = file.path();
            path.and_then(|path| file_tree::moved_path(from, to, path))
        };
        let Some(moved) = moved else {
            continue;
        };
        let was = work_identity(&document.file.borrow());
        document.file.borrow_mut().follow_rename(moved);
        // **Written under the new name before the copy under the old one is
        // dropped.** The other order has a moment with no copy at all, and
        // 要件 8.1 is about the work surviving exactly such moments.
        document.text.mark_pending();
        write_work_copy_of(window, live, &document);
        discard_work_copy(live, &was);
    }
    // A tab is titled after its file, and the file has just been renamed.
    window.invoke_republish_tabs();
}

/// Move the selected file or folder to the recycle bin (要件 5.2).
///
/// **The tabs are left where they are.** A document whose file has gone is one
/// 要件 8.3's watcher already knows how to report, and closing it would be the
/// editor throwing away the work that the recycle bin was chosen to keep.
fn delete_entry(window: &AppWindow, live: &Live, path: &Path) {
    let owner = ime::window_handle(window);
    if !shell::recycle(owner, path) {
        window.set_render_status("ごみ箱へ移動できませんでした".into());
        return;
    }
    {
        let mut open = live.folder.borrow_mut();
        open.selected = None;
        open.expanded.retain(|held| !held.starts_with(path));
    }
    publish_left(window, live);
    write_session(window, live);
}

/// Put the panes where the tree says, and the boundaries between them.
///
/// **A pane is on screen exactly when it has an area.** Everything that asks
/// whether a pane is showing reads its row's width, so the tree is the only
/// place the arrangement is decided (要件 6.4) and nothing has to be kept in
/// step with it.
/// The most panes an arrangement may hold.
///
/// **Not a product limit** — 要件 6.4 puts none, and `MIN_PANE` is what really
/// stops a window being divided further. This only keeps a session file that
/// says something impossible from asking for a million rows before anything
/// has had a chance to look at it.
const MAX_PANES: usize = 64;

/// How narrow and how wide the left pane may be dragged (追加要件 2026-09-06).
///
/// The floor is a file name's worth: a tree narrower than that shows depth and
/// no name. The ceiling is there because the writing is the point — a left pane
/// that can take the window is one the writer has to put back.
const TREE_WIDTH_MIN: f32 = 160.0;
const TREE_WIDTH_MAX: f32 = 640.0;

/// Make the window's pane model hold exactly `count` rows (要件 6.3).
///
/// A row appended here opens as horizontal source text; a pane made by a split
/// is told what to show straight afterwards, because it opens as a copy of the
/// tab it was split from (要件 6.4).
fn publish_panes(window: &AppWindow, count: usize) {
    let count = count.clamp(1, MAX_PANES);
    let panes = window.get_panes();
    let Some(rows) = panes.as_any().downcast_ref::<VecModel<PaneScreen>>() else {
        // Nothing published yet: this is the first list there has been.
        let made = (0..count)
            .map(|row| PaneId(row as u32).initial_screen(false, false))
            .collect::<Vec<PaneScreen>>();
        window.set_panes(ModelRc::new(VecModel::from(made)));
        return;
    };
    while rows.row_count() > count {
        rows.remove(rows.row_count() - 1);
    }
    while rows.row_count() < count {
        rows.push(PaneId(rows.row_count() as u32).initial_screen(false, false));
    }
}

fn place_panes(window: &AppWindow, layout: &Layout) {
    let (placed, boundaries) = layout.place(editor_area(window));
    // **Everything the window believes about the arrangement is set here**, so
    // that none of it can be left behind by a path that forgot to. A session
    // that restored two panes without this said "not split" until something
    // else happened to change the arrangement — and then *both* panes answered
    // a request for the keyboard, so the pane the writer was in was whichever
    // one moved last. The flag itself is gone (2026-09-06): **the pane model's
    // own length says how many panes there are**, and a second opinion about
    // that is exactly what went stale.
    let focused = PaneId::from_index(window.get_focused_pane());
    let on_screen = placed
        .iter()
        .any(|(pane, _)| *pane == focused.index() as usize);
    if !on_screen && let Some((first, _)) = placed.first() {
        window.set_focused_pane(*first as i32);
    }
    for id in PaneId::all(window) {
        let found = placed
            .iter()
            .find(|(pane, _)| *pane == id.index() as usize)
            .map(|(_, rect)| *rect)
            .unwrap_or_default();
        id.update_screen(window, |screen| {
            screen.x = found.x;
            screen.y = found.y;
            screen.width = found.width;
            screen.height = found.height;
        });
    }
    // 要件 6.4: which ways each pane has somebody to change places with. Done
    // here because this is where the rectangles are, and **the menu leaves out
    // a direction there is nothing in** — a row that does nothing looks exactly
    // like a row that is broken.
    let neighbours = placed
        .iter()
        .map(|(pane, _)| {
            let ways = [Towards::Left, Towards::Right, Towards::Up, Towards::Down]
                .map(|towards| neighbour(&placed, *pane, towards).is_some());
            (*pane, ways)
        })
        .collect::<Vec<(usize, [bool; 4])>>();
    for (pane, ways) in neighbours {
        PaneId::from_index(pane as i32).update_screen(window, |screen| {
            screen.swap_left = ways[0];
            screen.swap_right = ways[1];
            screen.swap_up = ways[2];
            screen.swap_down = ways[3];
        });
    }
    let drawn = boundaries
        .iter()
        .map(|boundary| PaneBoundary {
            x: boundary.rect.x,
            y: boundary.rect.y,
            width: boundary.rect.width,
            height: boundary.rect.height,
            side_by_side: boundary.split == Split::SideBySide,
        })
        .collect::<Vec<_>>();
    publish_boundaries(window, drawn);
}

/// Where every pane on screen is, as `neighbour` wants it (要件 6.4, 11.3).
///
/// **Read back from the rows rather than placed again.** The rectangles the
/// panes are drawn in are the ones in the model; laying the tree out a second
/// time to answer "what is beside me" would be a second opinion about a thing
/// the writer can already see.
fn placed_panes(window: &AppWindow) -> Vec<(usize, Rect)> {
    PaneId::all(window)
        .iter()
        .filter(|id| id.is_shown(window))
        .map(|id| {
            let screen = id.screen(window);
            let rect = Rect::new(screen.x, screen.y, screen.width, screen.height);
            (id.index() as usize, rect)
        })
        .collect()
}

/// A stored left-pane width, held to what the writer can drag it to.
///
/// **The same bounds the drag uses** (`ui/app-window.slint`), because a session
/// from another build — or one hand-edited — must not be able to put the panel
/// somewhere the pointer cannot bring it back from.
fn tree_width_from(stored: u32) -> f32 {
    (stored as f32).clamp(TREE_WIDTH_MIN, TREE_WIDTH_MAX)
}

/// Which pane the writer is in.
///
/// **Every question about "the document" without a pane in it comes here**:
/// saving, the work copy, closing a tab. The window keeps the answer because
/// the writer decides it by clicking into a pane, and it is the same flag the
/// panes use to decide which of them takes the keyboard back (6.14).
fn focused_pane(window: &AppWindow) -> PaneId {
    let remembered = PaneId::from_index(window.get_focused_pane());
    // **What is remembered is not always what is on screen.** The flag only
    // moves when a pane takes the keyboard, and the mode can put another pane
    // in front without anybody clicking — at start-up, most of all.
    if remembered.is_shown(window) {
        remembered
    } else {
        // **The first pane that is on screen.** There is always one — 要件 6.3
        // keeps the editing area from being empty — and which it is matters
        // less than that the answer names a pane the writer can be in.
        PaneId::all(window)
            .into_iter()
            .find(|id| id.is_shown(window))
            .unwrap_or(PaneId::FIRST)
    }
}

/// Write the live state back into the tab it belongs to.
///
/// Called before anything that reads or reorders the list, so the entry for the
/// active tab is never the stale one left there when it was brought out.
fn sync_active_tab(window: &AppWindow, live: &Live) {
    // Every pane, not only the focused one: each has its own caret in its own
    // tab, and the one being written back may not be the one being written in.
    let views = PaneId::all(window)
        .into_iter()
        .map(|id| live.capture_view(window, id))
        .collect::<Vec<_>>();
    // The strip's own state, taken while the cache is not otherwise borrowed
    // (追加要件 Terminal).
    let strips_below = PaneId::all(window)
        .into_iter()
        .map(|id| {
            let mut borrowed = live.cache.borrow_mut();
            let pane = borrowed.pane(id);
            (
                pane.below_open,
                pane.below_height,
                pane.below.as_ref().map(|shell| shell.session.clone()),
                id.screen(window).below_draft.to_string(),
            )
        })
        .collect::<Vec<_>>();
    let mut tabs = live.tabs.borrow_mut();
    for ((id, view), below) in PaneId::all(window).into_iter().zip(views).zip(strips_below) {
        let strip = tabs.of_mut(id);
        if let Some(tab) = strip.tabs.get_mut(strip.active) {
            tab.view = view;
            let (open, height, shell, draft) = below;
            tab.below.open = open;
            tab.below.height = height;
            // **The draft is the tab's**, and it is only ever the tab's when
            // the tab is a shell; a document's strip has no draft to keep.
            if tab.terminal.is_some() {
                tab.below.draft = draft;
            } else {
                tab.below.shell = shell;
            }
        }
    }
}

/// Put the tab strip and the window title in front of the writer.
///
/// **One strip is on screen while the strips are still drawn by the window**,
/// and it is the focused pane's — the one whose tabs the buttons above would
/// act on. Moving them inside the panes is the next step (ペイン分割設計 6).
fn publish_tabs(window: &AppWindow, live: &Live) {
    // 要件 7.9・10: ステータスバーのモードは、前に出ているタブのもの。
    // **タブが動けばここも動く**ので、publishの入口で一緒に言う。
    publish_word_mode_of(window, live);
    let strips = PaneId::all(window).into_iter().map(|id| {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        let infos = strip
            .tabs
            .iter()
            .map(|tab| {
                // **編集の始まったタブは、もう覗いているだけではない**
                // （書き手の報告 2026-09-07）。ここが不変の借りしか持たないので
                // 旗は`Cell`にしてある。最初の一字で`set_edited`がこの publish を
                // 呼ぶので、傾いた字が立つのはその瞬間。
                if tab.provisional.get() && tab.document.text.edited() {
                    tab.provisional.set(false);
                }
                TabInfo {
                    // Every name and marker is read from the document. There is no
                    // second copy to be fresher than the list any more — except a
                    // terminal, whose document is an empty stand-in and whose name
                    // is the shell it is running (追加要件 Terminal), and a tab
                    // that has not been asked what it is yet (追加要件
                    // 2026-09-07). **Both carry an untitled document**, and
                    // neither is one.
                    title: tab_title(tab).into(),
                    edited: tab.terminal.is_none() && !tab.empty && tab.document.text.edited(),
                    // 追加要件 2026-09-06: only a tab standing for a file has a
                    // name on disk to change. 無題1 reads like a name on screen,
                    // and nothing is filed under it.
                    renamable: tab.terminal.is_none()
                        && !tab.empty
                        && tab.document.file.borrow().path().is_some(),
                    stem_length: stem_length(&tab.document.file.borrow().title()),
                    terminal: tab.terminal.is_some(),
                    provisional: tab.is_provisional(),
                }
            })
            .collect::<Vec<_>>();
        (infos, strip.active as i32)
    });
    for (id, (infos, active)) in PaneId::all(window).into_iter().zip(strips) {
        // **どの行がシェルかを、窓へ渡したそのままの形で残す**（書き手の報告
        // 2026-09-07: 切り替えの行が出てこない）。出ない理由が「旗が立って
        // いない」のか「旗は立っているのに窓が読んでいない」のかは、ここが
        // 答える。シェルが1つも無い間は黙っている。
        if infos.iter().any(|info| info.terminal) {
            let shells: String = infos
                .iter()
                .map(|info| if info.terminal { '1' } else { '0' })
                .collect();
            live.cache.borrow_mut().log_diag(
                "tab",
                &format!(
                    "strip pane={} active={active} shells={shells}",
                    id.log_name()
                ),
            );
        }
        id.update_screen(window, |screen| {
            screen.tabs = ModelRc::new(VecModel::from(infos));
            screen.active_tab = active;
        });
    }
    // The name and the unsaved marker in the status bar are the focused pane's
    // document's, which after a switch is not the one they were last set from.
    let showing = live.active(window);
    window.set_document_edited(showing.text.edited());
    if let Some(tab) = live.tabs.borrow().of(focused_pane(window)).current() {
        show_tab_title(window, tab);
    }
    // **The strips and the arrangement are one thing** (要件 8.5), and this is
    // where the strips change. A boundary dragged without touching a strip is
    // caught by the write on the way out.
    write_session(window, live);
}

/// Write down that a pane is standing in front of this tab (書き手の報告 2026-09-07).
///
/// **The place it is already at is not a new place**, which is what lets 戻る
/// and 進む move without cutting off the way they came: they put `at` where
/// they are going first, and this then finds nothing to record.
///
/// A shell is not a place. Its document is an empty stand-in (`PaneTab`), and
/// going back to one would open an empty untitled tab rather than the terminal.
fn note_navigation(live: &Live, id: PaneId, tab: &PaneTab) {
    if tab.terminal.is_some() {
        return;
    }
    let mut tabs = live.tabs.borrow_mut();
    let strip = tabs.of_mut(id);
    let standing = strip
        .history
        .get(strip.at)
        .is_some_and(|held| Rc::ptr_eq(held, &tab.document));
    if standing {
        return;
    }
    strip.history.truncate(strip.at + 1);
    strip.history.push(tab.document.clone());
    while strip.history.len() > NAVIGATION_PLACES {
        strip.history.remove(0);
    }
    strip.at = strip.history.len() - 1;
}

/// 戻る and 進む: the file this pane was looking at before, or after
/// (書き手の報告 2026-09-07).
///
/// **The document is still here even when its tab is not.** A walk through a
/// folder writes over one provisional tab again and again, so the way back
/// almost always leads to a tab that no longer exists; the history holds the
/// document itself, and a tab for it is opened again — provisional, because
/// coming back to look is still looking.
fn navigate(window: &AppWindow, live: &Live, id: PaneId, forward: bool) {
    let step = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        stepped_place(strip.history.len(), strip.at, forward)
            .map(|next| (next, strip.history[next].clone()))
    };
    let Some((next, document)) = step else {
        return told_no_way(window, forward);
    };
    let (held, peeked) = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        (
            strip
                .tabs
                .iter()
                .position(|tab| Rc::ptr_eq(&tab.document, &document)),
            strip
                .tabs
                .iter()
                .position(|tab| tab.yields_to(Opening::Peeked)),
        )
    };
    // **Where it is going, written down before it goes.** Every road out of
    // here records the arrival, and one that found `at` still on the place
    // being left would cut off everything ahead of it.
    live.tabs.borrow_mut().of_mut(id).at = next;
    live.cache.borrow_mut().log_diag(
        "tab",
        &format!(
            "navigate pane={} to={next} forward={forward} open={}",
            id.log_name(),
            u8::from(held.is_some())
        ),
    );
    match held {
        Some(index) => {
            switch_to_tab(window, live, id, index);
            publish_tabs(window, live);
        }
        None => {
            let tab = PaneTab {
                provisional: Cell::new(true),
                ..PaneTab::showing(window, id, document)
            };
            match peeked {
                Some(index) => replace_tab(window, live, id, index, tab),
                None => add_tab(window, live, id, tab),
            }
        }
    }
}

/// Which place 戻る or 進む lands on, or `None` at either end of the walk.
fn stepped_place(places: usize, at: usize, forward: bool) -> Option<usize> {
    let next = if forward {
        at.checked_add(1)?
    } else {
        at.checked_sub(1)?
    };
    (next < places).then_some(next)
}

/// There is nothing that way (書き手の報告 2026-09-07).
fn told_no_way(window: &AppWindow, forward: bool) {
    let told = if forward {
        "これより先はありません"
    } else {
        "これより前はありません"
    };
    window.set_render_status(told.into());
}

/// What a tab is called on screen.
///
/// **Three kinds of tab and one name each**: a shell is called after the shell,
/// a tab that has not been asked what it is says so, and everything else is its
/// document. Asked in one place because the strip, the window's title bar and
/// the list of all tabs must not disagree about it.
fn tab_title(tab: &PaneTab) -> String {
    match &tab.terminal {
        Some(session) => session.borrow().name().to_owned(),
        None if tab.empty => NEW_TAB_NAME.to_owned(),
        None => tab.document.file.borrow().title(),
    }
}

/// 追加要件 2026-09-07: what a tab is called before it is anything.
const NEW_TAB_NAME: &str = "New Tab";

/// Show another tab in one pane (要件 6.3).
fn switch_to_tab(window: &AppWindow, live: &Live, id: PaneId, index: usize) {
    {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        if index >= strip.tabs.len() || index == strip.active {
            return;
        }
    }
    // The tab being left has its work copy brought up to date here rather than
    // at the next tick, so that nothing is riding on the timer across a switch.
    write_work_copy_now(window, live);
    sync_active_tab(window, live);
    let Some(incoming) = ({
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        strip.active = index;
        strip.current().cloned()
    }) else {
        return;
    };
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("switch pane={} to={index}", id.log_name()));
    live.show_tab(window, id, &incoming);
    note_navigation(live, id, &incoming);
    publish_tabs(window, live);
}

/// Let go of a carried tab (要件 6.5).
///
/// **Where it landed is a question about the editing area, not about the strip
/// it started in.** The panes are placed into that area and the point is in the
/// same coordinates, so which pane the hand was over is asked of the
/// arrangement rather than guessed at from how far the pointer travelled.
/// Outside every pane — over the boundary, or past the edge of the area — is
/// not a pane, and then it is the ordinary reordering: the writer let go
/// somewhere that names nothing, and the strip they started in is the only
/// answer that can be right.
fn drop_tab(window: &AppWindow, live: &Live, id: PaneId, from: usize, to: usize, at: (f32, f32)) {
    let (placed, _) = live.layout.borrow().place(editor_area(window));
    let landed = placed
        .iter()
        .find(|(_, rect)| rect.holds(at.0, at.1))
        .map(|(pane, _)| PaneId::from_index(*pane as i32));
    match landed {
        Some(other) if other != id => carry_tab_to_pane(window, live, id, from, other),
        _ => move_tab(window, live, id, from, to),
    }
}

/// Carry a tab out of one pane and into another (要件 6.5).
///
/// **The tab moves; the document does not.** What a strip holds is a view of a
/// document — a caret, a scroll and a mode (要件 7.6) — and all of that goes
/// with it, so the tab arrives showing what it showed. It lands at the end of
/// the strip it is dropped into and in front, because a tab carried somewhere
/// is a tab the writer wants to be looking at.
///
/// **One tab per document in a strip, and the carried view wins.** A pane
/// almost always already holds the document being carried into it: 要件 6.4
/// puts the tab in front into the new pane when the area is divided, so the two
/// strips start out sharing it. Landing beside that twin would leave two tabs
/// with the same name in one strip, which is a strip nobody can read. The twin
/// goes and the view in the writer's hand is the one that stays — they carried
/// this one.
///
/// Emptying the pane it left undivides that pane, the same as closing its last
/// tab does (要件 6.4). It is the same event: a pane with nothing in it.
fn carry_tab_to_pane(window: &AppWindow, live: &Live, id: PaneId, index: usize, other: PaneId) {
    write_work_copy_now(window, live);
    sync_active_tab(window, live);
    let emptied = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        if index >= strip.tabs.len() {
            return;
        }
        let before = strip.tabs.len();
        let carried = strip.tabs.remove(index);
        strip.active = active_after_close(before, strip.active, index);
        let emptied = strip.tabs.is_empty();
        let arriving = carried.document();
        let landing = tabs.of_mut(other);
        landing
            .tabs
            .retain(|held| !Rc::ptr_eq(&held.document(), &arriving));
        landing.tabs.push(carried);
        landing.active = landing.tabs.len() - 1;
        emptied
    };
    let mut cache = live.cache.borrow_mut();
    let message = format!(
        "carry pane={} at={index} to={}",
        id.log_name(),
        other.log_name()
    );
    cache.log_diag("tab", &message);
    drop(cache);
    // The keyboard follows the tab: it is in front of the pane it landed in,
    // and that is where the writer put it.
    window.set_focused_pane(other.index());
    if emptied && live.layout.borrow().panes().len() > 1 {
        // 要件 6.4: a pane whose last tab has closed is taken out of the
        // arrangement, and its area goes to the pane beside it. **Nothing is
        // stranded** — the strip was empty, which is the whole reason this is
        // the only way a pane goes.
        remove_pane(window, live, id);
    } else if emptied {
        refill_strip(window, live, id);
    }
    if let Some(staying) = live.tabs.borrow().of(id).current().cloned() {
        live.show_tab(window, id, &staying);
    }
    let Some(arriving) = live.tabs.borrow().of(other).current().cloned() else {
        return;
    };
    live.show_tab(window, other, &arriving);
    publish_tabs(window, live);
}

/// Carry a tab to another place in its own strip (要件 6.5).
///
/// **Nothing is opened, closed or shown.** The same documents are in the same
/// pane; only the order the writer reads them in has changed. That order is
/// part of the arrangement (要件 8.5), and `publish_tabs` writes it down.
fn move_tab(window: &AppWindow, live: &Live, id: PaneId, from: usize, to: usize) {
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let last = strip.tabs.len().saturating_sub(1);
        if from > last || to > last || from == to {
            return;
        }
        let carried = strip.tabs.remove(from);
        strip.tabs.insert(to, carried);
        strip.active = active_after_move(strip.active, from, to);
    }
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("move pane={} {from}->{to}", id.log_name()));
    publish_tabs(window, live);
}

/// Add a tab to one pane's strip and show it there.
fn add_tab(window: &AppWindow, live: &Live, id: PaneId, tab: PaneTab) {
    write_work_copy_now(window, live);
    sync_active_tab(window, live);
    let index = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        strip.tabs.push(tab.clone());
        strip.active = strip.tabs.len() - 1;
        strip.active
    };
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("add pane={} at={index}", id.log_name()));
    live.show_tab(window, id, &tab);
    note_navigation(live, id, &tab);
    publish_tabs(window, live);
}

/// A tab that has not been asked what it is yet (追加要件 2026-09-07).
///
/// **The `＋` makes the tab and the tab asks the question.** It used to be the
/// other way round — a menu of three answers, and only then somewhere to look
/// at the answer — which meant deciding before there was anywhere to decide in.
/// What opens now is a page with three words on it; the writer may also simply
/// click a file in the tree, and it fills this tab (`yields_to`).
///
/// The document it carries is already the one 要件 8.4 would have given it,
/// number and all, so `New File` is nothing but putting the flag down.
fn new_tab(window: &AppWindow, live: &Live, id: PaneId) {
    open_tab(window, live, id, true);
}

/// A new empty document (要件 8.4).
fn new_file_tab(window: &AppWindow, live: &Live, id: PaneId) {
    open_tab(window, live, id, false);
}

fn open_tab(window: &AppWindow, live: &Live, id: PaneId, empty: bool) {
    sync_active_tab(window, live);
    let number = {
        let tabs = live.tabs.borrow();
        // Across every pane: two strips can hold 無題1 and 無題2, and a third
        // one has to be 無題3 whichever pane asks for it.
        let taken: Vec<u32> = tabs
            .panes
            .iter()
            .flat_map(|strip| strip.tabs.iter())
            .map(|tab| tab.document.file.borrow().untitled_number())
            .collect();
        next_untitled_number(&taken)
    };
    // A new tab has no mode of its own to remember, so it opens the way the
    // pane is being used now (要件 7.2).
    let document = OpenDocument::untitled(number, window.as_weak());
    let tab = PaneTab {
        view: TabView {
            vertical: id.vertical(window),
            preview: id.shows_preview(window),
            ..TabView::default()
        },
        empty,
        ..PaneTab::showing(window, id, document)
    };
    add_tab(window, live, id, tab);
}

/// The writer answered the New Tab (追加要件 2026-09-07).
///
/// **The tab stays where it is.** Nothing is made and nothing is added: with no
/// shell it is only the page coming down, because the untitled document was
/// under it all along; with one, the shell moves in beside that document
/// exactly as it would in a tab of its own.
///
/// A tab that has already been answered is left alone, so a writer typing into
/// a file cannot turn it into something else.
fn answer_new_tab(window: &AppWindow, live: &Live, id: PaneId, shell: Option<TerminalShell>) {
    let asking = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .is_some_and(|tab| tab.empty);
    if !asking {
        return;
    }
    // **Started before the strip is touched**: a shell that cannot start leaves
    // the tab asking rather than half answered.
    let session = match shell {
        Some(shell) => {
            let started = start_shell(window, live, id, &shell, id.shown_height(window));
            let Some(started) = started else {
                return;
            };
            Some(Rc::new(RefCell::new(started)))
        }
        None => None,
    };
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let active = strip.active;
        let Some(tab) = strip.tabs.get_mut(active) else {
            return;
        };
        tab.empty = false;
        if session.is_some() {
            tab.terminal = session;
            tab.view.vertical = false;
            tab.view.preview = false;
        }
    }
    let Some(showing) = live.tabs.borrow().of(id).current().cloned() else {
        return;
    };
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("answered pane={}", id.log_name()));
    live.show_tab(window, id, &showing);
    note_navigation(live, id, &showing);
    publish_tabs(window, live);
}

/// What is left of a 他のタブを閉じる or すべてのタブを閉じる (要件 6.3).
///
/// **The documents, not the positions.** Closing one tab shifts every tab after
/// it, and a question answered a turn of the event loop later would come back
/// to a number that by then means a different tab. A document is the same
/// document wherever the strip has moved it to.
struct CloseRun {
    /// **Which pane each tab is in, beside which document it holds.** The run
    /// crosses the panes on screen, and the same document can be in front of
    /// two of them — one tab each, with a caret each (要件 7.6), and closing
    /// one of them is not closing the other.
    left: VecDeque<(PaneId, Rc<OpenDocument>)>,
}

/// A question the writer has been asked, and what its answer decides.
///
/// **Asked and answered across two turns of the event loop.** The question is
/// drawn in the window rather than by a dialog that runs its own message loop
/// (6.18), so nothing waits for the answer: the callback that had to ask
/// returns, and [`answer_question`] picks the work back up.
#[derive(Clone, Debug)]
enum Question {
    /// Closing a tab holding work that is not in its file (要件 8.4).
    ///
    /// The pane is carried as well as the position: the answer comes back a
    /// turn of the event loop later, and the writer may have clicked into
    /// another pane by then.
    CloseTab { pane: PaneId, index: usize },
    /// The one answer to that question that throws work away, asked again on
    /// its own. **Never a button beside the one that keeps it.**
    DiscardOnClose { pane: PaneId, index: usize },
    /// Saving over a file another program has changed since it was opened
    /// (要件 8.3).
    SaveConflict,
    /// A new file in the folder named, waiting for its name (要件 5.2).
    NewFile(PathBuf),
    /// A new folder in the folder named, waiting for its name (要件 5.2).
    NewFolder(PathBuf),
    /// The file or folder named, waiting for the name to give it (要件 5.2).
    RenameEntry(PathBuf),
    /// The file or folder named, waiting to be told to go (要件 5.2).
    DeleteEntry(PathBuf),
    /// Something carried onto a name that is already taken (要件 5.2): what is
    /// being moved, and where it would land. **The order is the same as
    /// `move_entry`'s**, from and to.
    ReplaceOnMove(PathBuf, PathBuf),
    /// 窓を閉じようとしているが、自動退避が切ってあって未保存の文書がある
    /// （2026-09-08、要件 8.1・8.4）。**退避が働いていれば訊かない**——
    /// そちらは閉じても失われないので、問いは書き手の邪魔でしかない。
    CloseWindow,
    /// 終了の直前の退避が書けなかった（追加要件 2026-09-09、残り2）。
    /// **退避が働いているつもりで閉じようとしている**ときにだけ立つ問いで、
    /// 書けた件数が0のときは何も訊かない——要件 8.1 は静かな約束である。
    LastWorkCopyFailed,
}

/// Write the last work copy and, if it could not be written, ask what to do
/// about it (追加要件 2026-09-09、残り2).
///
/// **True when a question is now standing**, which is what tells the caller to
/// keep the window open. False means there was nothing to report — either the
/// copies landed, or the writer thread ran out of time and the join after
/// `run()` will wait the rest out.
///
/// 要件 8.1 は「入力の2秒後に退避する」という約束で、その最後の1回だけは
/// **窓が閉じたあと**に判定されていた。書けなかったときログに1行残るだけで、
/// 書き手は最後の数秒を失ったことを知らない。ここで訊けば、3つの道がある
/// ——もう一度試す、本文そのものをファイルへ入れる、閉じるのをやめる。
fn ask_about_the_last_work_copy(window: &AppWindow, live: &Live) -> bool {
    let lost = flush_work_copies(window, live);
    if lost == 0 {
        return false;
    }
    live.cache
        .borrow_mut()
        .log_diag("work", &format!("close blocked, unwritten={lost}"));
    ask_question(
        window,
        live,
        Question::LastWorkCopyFailed,
        format!(
            "最後の自動退避を{lost}件書けませんでした。\n\n             このまま閉じると、退避していない変更は戻せません。"
        ),
        &["もう一度試す", "文書を保存する…", "閉じない"],
        -1,
    );
    true
}

/// Put a question in front of the writer.
///
/// The choices are shown in the order they are named, and **the last one is
/// the way out**: Esc and every other dismissal answer with it, so it has to be
/// the answer that changes nothing.
fn ask_question(
    window: &AppWindow,
    live: &Live,
    question: Question,
    text: String,
    choices: &[&str],
    danger: i32,
) {
    // Described before it is handed over: a question carries a path now, so
    // storing it moves it.
    let described = format!("{question:?}");
    *live.pending.borrow_mut() = Some(question);
    let named = choices
        .iter()
        .map(|choice| SharedString::from(*choice))
        .collect::<Vec<_>>();
    // Split where every one of these messages already splits: what is being
    // asked, a blank line, and what it costs.
    let (asked, detail) = match text.split_once("\n\n") {
        Some((asked, detail)) => (asked.to_owned(), detail.to_owned()),
        None => (text, String::new()),
    };
    window.set_question_text(asked.into());
    window.set_question_detail(detail.into());
    window.set_question_choices(ModelRc::new(VecModel::from(named)));
    window.set_question_danger(danger);
    window.set_question_asks_name(false);
    window.set_question_open(true);
    live.cache.borrow_mut().log_diag("ask", &described);
}

/// Put a question in front of the writer that is answered by typing a name
/// (要件 5.2).
///
/// The same overlay with a field in it, and **the same two turns of the event
/// loop**: what the name is for is decided in [`answer_question`], beside every
/// other answer.
fn ask_for_name(
    window: &AppWindow,
    live: &Live,
    question: Question,
    text: String,
    suggested_name: &str,
) {
    window.set_question_name(suggested_name.into());
    ask_question(window, live, question, text, &["決定", "キャンセル"], -1);
    window.set_question_asks_name(true);
    let asked = window.get_question_generation();
    window.set_question_generation(asked + 1);
}

/// Act on the answer to the question that was standing.
///
/// An answer that is not one of the ones below changes nothing, which is what
/// the last choice always is.
fn answer_question(window: &AppWindow, live: &Live, choice: i32) {
    // Taken before anything else: an answer may ask the next question, and the
    // borrow must not still be open when it does.
    let question = live.pending.borrow_mut().take();
    let Some(question) = question else {
        return;
    };
    window.set_question_open(false);
    // The question took the keyboard away from the pane to ask (6.14).
    restore_editor_focus(window);
    live.cache
        .borrow_mut()
        .log_diag("answer", &format!("{question:?} choice={choice}"));
    // 追加要件 2026-09-08: **並びが短いときの番号を、いつもの番号へ直す。**
    // 自動退避が切れているとタブを閉じる問いは3つしか出さない（真ん中の
    // 「作業コピーを残して閉じる」が無い）ので、そのままでは「破棄して
    // 閉じる」が「残して閉じる」として読まれる。ここで直せば、下の腕は
    // 4つの並びのつもりのままでいられる。
    let closing_tab = matches!(question, Question::CloseTab { .. });
    let choice = if closing_tab && !window.get_autosave() {
        match choice {
            0 => 0,
            1 => 2,
            _ => 3,
        }
    } else {
        choice
    };
    match (question, choice) {
        (Question::CloseTab { pane, index }, 0) => {
            save_document(window, live, false);
            // A save that was cancelled, failed, or stopped to ask a question
            // of its own leaves the work where it was, and closing on the back
            // of a save that did not happen is not what was asked for. The rest
            // of the run goes with it: the writer is answering something else
            // now, and the next tab's question would land on top of it.
            if live.active(window).text.edited() {
                cancel_close_run(live);
                return;
            }
            finish_close(window, live, pane, index);
            advance_close_run(window, live);
        }
        (Question::CloseTab { pane, index }, 1) => {
            finish_close(window, live, pane, index);
            advance_close_run(window, live);
        }
        (Question::CloseTab { pane, index }, 2) => {
            let title = live.active(window).file.borrow().title();
            ask_question(
                window,
                live,
                Question::DiscardOnClose { pane, index },
                format!(
                    "「{title}」の保存していない変更を破棄します。\n\n\
                     作業コピーも削除するので、元に戻せません。"
                ),
                &["破棄する", "キャンセル"],
                0,
            );
        }
        (Question::DiscardOnClose { pane, index }, 0) => {
            let asked_about = {
                let tabs = live.tabs.borrow();
                let strip = tabs.of(pane);
                strip.tabs.get(index).map(|tab| tab.document.clone())
            };
            let Some(document) = asked_about else {
                cancel_close_run(live);
                return;
            };
            discard_work_copy(live, &work_identity(&document.file.borrow()));
            // Nothing is waiting to be written any more, so neither the tick
            // nor the close can put the copy back.
            document.text.mark_saved();
            finish_close(window, live, pane, index);
            advance_close_run(window, live);
        }
        (Question::CloseWindow, 0) => {
            save_all(window, live);
            // **保存が済んでいなければ閉じない。**「名前を付けて保存」を
            // 取り消した文書がまだ編集中のまま残っている——そのまま閉じるのは
            // 書き手が断ったことをやることになる。
            if open_documents(live).iter().any(|held| held.text.edited()) {
                window.set_render_status("保存していない文書が残っています".into());
                return;
            }
            window.hide().ok();
        }
        (Question::CloseWindow, 1) => {
            // 破棄して閉じる。**退避は切ってあるので、消すものは無い**——
            // 作業コピーはそもそも書かれていない。
            window.hide().ok();
        }
        // 追加要件 2026-09-09（残り2）: 最後の退避が書けなかったときの3つ。
        // **どれも勝手には閉じない**——閉じてよいと決められるのは、書けた
        // ことを確かめたときだけである。
        (Question::LastWorkCopyFailed, 0) => {
            if !ask_about_the_last_work_copy(window, live) {
                window.hide().ok();
            }
        }
        (Question::LastWorkCopyFailed, 1) => {
            save_all(window, live);
            // 本文がファイルへ入ったなら、作業コピーが書けないことはもう
            // 何も失わせない。**残っていれば閉じない**：「名前を付けて保存」を
            // 取り消した文書がまだ編集中で、そこには失うものがある。
            if open_documents(live).iter().any(|held| held.text.edited()) {
                window.set_render_status("保存していない文書が残っています".into());
                return;
            }
            window.hide().ok();
        }
        (Question::SaveConflict, 0) => overwrite_the_outside_change(window, live),
        (Question::SaveConflict, 1) => reload_from_file(window, live),
        (Question::SaveConflict, 2) => save_document(window, live, true),
        (Question::NewFile(parent), 0) => make_entry(window, live, &parent, false),
        (Question::NewFolder(parent), 0) => make_entry(window, live, &parent, true),
        (Question::RenameEntry(path), 0) => rename_entry(window, live, &path),
        (Question::DeleteEntry(path), 0) => delete_entry(window, live, &path),
        (Question::ReplaceOnMove(from, to), 0) => replace_on_move(window, live, &from, &to),
        // キャンセル, and every other way out of a question about closing. The
        // run stops here rather than going on to ask about the next tab.
        (Question::CloseTab { .. } | Question::DiscardOnClose { .. }, _) => {
            cancel_close_run(live);
        }
        _ => {}
    }
}

/// Close a tab (要件 6.3).
///
/// A document with unsaved work asks first (要件 8.4), and the answer arrives
/// later — the question is part of the window, not a dialog that blocks — so
/// this half puts the question up and [`finish_close`] is the half that runs
/// once it is answered.
///
/// **True when a question is now standing**, which is what stops a run of
/// closes ([`advance_close_run`]) until it has been answered.
fn close_tab(window: &AppWindow, live: &Live, id: PaneId, index: usize) -> bool {
    // Brought to the front first: a question about a document nobody can see is
    // a question about nothing, and it makes the tab being closed the one in
    // front of the pane, so saving it means the ordinary save.
    switch_to_tab(window, live, id, index);
    let Some(document) = live.tabs.borrow().of(id).current().map(PaneTab::document) else {
        return false;
    };
    // **Nothing is lost while another pane still holds it.** The question is
    // about work disappearing, and closing one of two views of a document
    // leaves the document — and its unsaved text — exactly where it was
    // (要件 7.6).
    let last_view = views_of(live, &document) <= 1;
    if !document.text.edited() || !last_view {
        finish_close(window, live, id, index);
        return false;
    }
    // The pane the question is about becomes the pane the writer is in.
    // **A run of closes crosses the panes** (`start_close_run`), and the answer
    // is carried out against the focused pane (`Live::active`): without this,
    // 保存して閉じる on one pane's tab would save the other pane's document.
    window.set_focused_pane(id.index());
    let title = document.file.borrow().title();
    // 追加要件 2026-09-08: **自動退避が切れていれば、真ん中の答えは無い。**
    // 「作業コピーを残して閉じる」は要件 8.1 の退避があってはじめて意味を
    // 持つ約束で、切ってあるときにそれを差し出すのは嘘になる——押せて、
    // 閉じて、次の起動には戻ってこない。答えの並びが変わるので、返ってきた
    // 番号は`answer_question`で読み直す。
    if window.get_autosave() {
        ask_question(
            window,
            live,
            Question::CloseTab { pane: id, index },
            format!(
                "「{title}」には保存していない変更があります。\n\n\
                 作業コピーを残して閉じると、次に起動したときに戻ってきます。"
            ),
            &[
                "保存して閉じる",
                "作業コピーを残して閉じる",
                "破棄して閉じる",
                "キャンセル",
            ],
            2,
        );
        return true;
    }
    ask_question(
        window,
        live,
        Question::CloseTab { pane: id, index },
        format!(
            "「{title}」には保存していない変更があります。\n\n\
             自動退避を切ってあるので、閉じると元に戻せません。"
        ),
        &["保存して閉じる", "破棄して閉じる", "キャンセル"],
        1,
    );
    true
}

/// Close every tab in the window, or every one but the tab in front of the pane
/// that asked (要件 6.3).
///
/// **すべて means the window, not the pane.** The pane the menu hangs from
/// decides one thing only: with `keep_active`, which single tab is kept.
///
/// **Only the panes on screen.** A pane that has been undivided away keeps its
/// tabs (要件 6.4), and closing one of those would put a question about a
/// document nobody can see in front of the writer — the same reason
/// [`close_tab`] brings a tab to the front before asking about it.
///
/// The list is taken before anything closes, so the run is over what was on
/// screen when the writer asked — not over whatever the strips hold by the time
/// the questions have been answered.
fn start_close_run(window: &AppWindow, live: &Live, id: PaneId, keep_active: bool) {
    // The pane that asked first, then the rest: its tabs are the ones the
    // writer is looking at, and with 他のタブを閉じる the kept tab is there.
    let mut order = vec![id];
    for index in live.layout.borrow().panes() {
        let pane = PaneId::from_index(index as i32);
        if pane != id {
            order.push(pane);
        }
    }
    let left = {
        let tabs = live.tabs.borrow();
        let mut left = VecDeque::new();
        for pane in order {
            let strip = tabs.of(pane);
            let keep = keep_active && pane == id;
            let taken = close_run_positions(strip.tabs.len(), strip.active, keep);
            for index in taken {
                if let Some(tab) = strip.tabs.get(index) {
                    left.push_back((pane, tab.document()));
                }
            }
        }
        left
    };
    *live.close_run.borrow_mut() = Some(CloseRun { left });
    advance_close_run(window, live);
}

/// 追加要件 2026-09-06: close every tab in this pane with nothing waiting to be
/// saved from it.
///
/// **A shell is not a clean tab.** Nothing about it is waiting to be saved, but
/// closing one ends whatever is running in it — which is the opposite of what a
/// row called "clean" is for.
fn close_clean_tabs(window: &AppWindow, live: &Live, id: PaneId) {
    let left = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        strip
            .tabs
            .iter()
            .filter(|tab| tab.terminal.is_none() && !tab.document.text.edited())
            .map(|tab| (id, tab.document()))
            .collect::<VecDeque<_>>()
    };
    if left.is_empty() {
        window.set_render_status("閉じられるタブはありません".into());
        return;
    }
    *live.close_run.borrow_mut() = Some(CloseRun { left });
    advance_close_run(window, live);
}

/// 追加要件 2026-09-06: the path of the file a tab stands for, to the clipboard.
fn copy_tab_path(window: &AppWindow, live: &Live, id: PaneId, index: usize) {
    let path = {
        let tabs = live.tabs.borrow();
        tabs.of(id)
            .tabs
            .get(index)
            .and_then(|tab| tab.document.file.borrow().path().map(Path::to_path_buf))
    };
    // **A tab that stands for nothing on disk has no path to give.** 無題1 reads
    // like a name on screen and is filed under nothing.
    let Some(path) = path else {
        window.set_render_status("このタブにはまだ保存先がありません".into());
        return;
    };
    if !clipboard::put_text(ime::window_handle(window), &path.display().to_string()) {
        window.set_render_status("クリップボードへ渡せませんでした".into());
    }
}

/// Close the tabs of the run until one of them has to ask something.
///
/// **The loop stops at the first question**, and the answer starts it again
/// ([`answer_question`]). So what runs here is only the tabs that need nothing
/// decided about them, however many of those there are between two questions.
fn advance_close_run(window: &AppWindow, live: &Live) {
    loop {
        let next = {
            let mut run = live.close_run.borrow_mut();
            let Some(run) = run.as_mut() else {
                return;
            };
            match run.left.pop_front() {
                Some(next) => next,
                None => break,
            }
        };
        let (id, document) = next;
        // Gone before the run reached it: closed by hand, or taken off with the
        // pane it was in. Nothing to close and nothing to ask.
        let Some(index) = tab_position(live, id, &document) else {
            continue;
        };
        if close_tab(window, live, id, index) {
            return;
        }
    }
    *live.close_run.borrow_mut() = None;
}

/// Where a document sits in one pane's strip, if it is still in it.
fn tab_position(live: &Live, id: PaneId, document: &Rc<OpenDocument>) -> Option<usize> {
    let tabs = live.tabs.borrow();
    for (index, tab) in tabs.of(id).tabs.iter().enumerate() {
        if Rc::ptr_eq(&tab.document, document) {
            return Some(index);
        }
    }
    None
}

/// Give up on the rest of a run.
///
/// **A question answered with キャンセル cancels the whole run.** The writer
/// said no to losing this document's work; being asked the same thing about the
/// next four tabs is not what that answer meant.
fn cancel_close_run(live: &Live) {
    *live.close_run.borrow_mut() = None;
}

/// Take the tab out of the list. Everything that had to be decided about its
/// work has been decided by the time this runs.
fn finish_close(window: &AppWindow, live: &Live, id: PaneId, index: usize) {
    write_work_copy_now(window, live);
    sync_active_tab(window, live);
    let emptied = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        if index >= strip.tabs.len() {
            return;
        }
        let before = strip.tabs.len();
        strip.tabs.remove(index);
        strip.active = active_after_close(before, strip.active, index);
        strip.tabs.is_empty()
    };
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("close pane={} at={index}", id.log_name()));
    if emptied && live.layout.borrow().panes().len() > 1 {
        // 要件 6.4: あるペインのタブをすべて閉じるとその分割を解除する。**The
        // pane is gone**, and with it its row, its strip and its engine; the
        // panes after it move down one number. Nothing is lost with it, because
        // the strip it took away was empty.
        remove_pane(window, live, id);
        publish_tabs(window, live);
        return;
    }
    if emptied {
        refill_strip(window, live, id);
    }
    let Some(incoming) = live.tabs.borrow().of(id).current().cloned() else {
        return;
    };
    live.show_tab(window, id, &incoming);
    publish_tabs(window, live);
}

/// Take a pane off screen, giving its area to the rest (要件 6.4).
///
/// **The pane is not gone** — it keeps its tabs, its carets and its scroll, and
/// dividing again brings all of it back. What it loses is a place to be drawn.
fn remove_pane(window: &AppWindow, live: &Live, id: PaneId) {
    if PaneId::count(window) <= 1 {
        return;
    }
    {
        let mut layout = live.layout.borrow_mut();
        if !layout.remove(id.index() as usize) {
            return;
        }
        // **A pane's number is its row of the window's model**, and a model has
        // no holes, so every pane after this one moves down — here, in the
        // states, in the strips, in the render cache and in the rows
        // themselves. All five are closed in this one place, because a number
        // that means one pane in one list and another in the next is the kind
        // of fault that shows up as somebody else's text.
        layout.renumber_above(id.index() as usize);
    }
    live.states.remove(id);
    live.tabs.borrow_mut().remove(id);
    live.cache.borrow_mut().remove_pane(id);
    drop_pane_row(window, id);
    // **A まとめて閉じる already under way is renumbered too** (要件 6.3). Its
    // queue holds pane numbers, and a run that empties one pane before reaching
    // the next would otherwise close tabs in whichever pane inherited the
    // number — the same fault as a stale row, arriving one event loop later.
    if let Some(run) = live.close_run.borrow_mut().as_mut() {
        run.left.retain(|(pane, _)| *pane != id);
        for (pane, _) in run.left.iter_mut() {
            if *pane > id {
                *pane = PaneId(pane.0 - 1);
            }
        }
    }
    // The keyboard cannot stay with a pane that has gone. It goes to whichever
    // pane took the area over, which is the one now standing where this was.
    let focused = PaneId::from_index(window.get_focused_pane());
    let landed = match focused.cmp(&id) {
        std::cmp::Ordering::Less => focused,
        // The pane that was here is gone; the numbering has closed over it, so
        // this number now names the pane that took its place.
        std::cmp::Ordering::Equal => PaneId(id.0.min(PaneId::count(window) as u32 - 1)),
        std::cmp::Ordering::Greater => PaneId(focused.0 - 1),
    };
    window.set_focused_pane(landed.index());
    live.cache.borrow_mut().log_diag(
        "layout",
        &format!("pane gone={} left={}", id.log_name(), PaneId::count(window)),
    );
    after_layout_change(window, live);
}

/// Take one row out of the window's pane model, closing the numbering behind it.
///
/// **One row goes and the rest are renumbered in place**, for the reason
/// `publish_panes` gives: the panes that stay must not be built again.
fn drop_pane_row(window: &AppWindow, id: PaneId) {
    let panes = window.get_panes();
    let Some(rows) = panes.as_any().downcast_ref::<VecModel<PaneScreen>>() else {
        return;
    };
    let at = id.index() as usize;
    if at >= rows.row_count() || rows.row_count() <= 1 {
        return;
    }
    rows.remove(at);
    // **A row's number is its position**, and the positions after the gap have
    // all moved down one.
    for row in at..rows.row_count() {
        let Some(mut screen) = rows.row_data(row) else {
            continue;
        };
        screen.id = row as i32;
        rows.set_row_data(row, screen);
    }
}

/// Divide a pane, putting a new one to its right or below it (要件 6.4).
///
/// **Only this pane is divided**, however deep it sits: its area becomes the
/// two, and every other pane keeps what it had. The new pane opens showing the
/// same file, which is what 要件 6.4 asks a new pane to start with.
fn divide_pane(window: &AppWindow, live: &Live, here: PaneId, split: Split) {
    let room = here.screen(window);
    let across = match split {
        Split::SideBySide => room.width,
        Split::Stacked => room.height,
    };
    // **A pane thinner than this is not a pane** (`MIN_PANE`). Refused with a
    // word rather than silently, because a menu row that does nothing looks
    // exactly like one that is broken.
    if (across - pane_layout::DIVIDER) / 2.0 < pane_layout::MIN_PANE {
        window.set_render_status("分割: これ以上は狭くなりすぎます".into());
        return;
    }
    if PaneId::count(window) >= MAX_PANES {
        window.set_render_status("分割: ペインが多すぎます".into());
        return;
    }
    let new = PaneId(PaneId::count(window) as u32);
    {
        // **Tried before anything is added.** A pane number the tree does not
        // hold cannot be divided, and half-adding one would leave a row with
        // no place on screen.
        let mut layout = live.layout.borrow_mut();
        if !layout.divide(here.index() as usize, split, new.index() as usize) {
            return;
        }
    }
    // The pane exists as a value before it exists on screen: the row is what
    // makes it drawable, and everything indexed by pane number has to be as
    // long as the rows are before one is published.
    live.states.add(&live.states.document(here));
    live.tabs.borrow_mut().add(PaneTabs::default());
    live.cache.borrow_mut().add_pane(if here.vertical(window) {
        WritingMode::Vertical
    } else {
        WritingMode::Horizontal
    });
    publish_panes(window, new.index() as usize + 1);
    new.update_screen(window, |screen| {
        // The new pane opens the way the one it came from is set, because the
        // tab it opens with is a copy of that pane's (要件 6.4, 7.2).
        screen.vertical = here.vertical(window);
        screen.preview = here.shows_preview(window);
        screen.zoom = here.zoom(window);
    });
    // Placed before the pane is filled, so that the tab it opens is laid out
    // into the area it will have rather than the one it had.
    place_panes(window, &live.layout.borrow());
    open_same_file_in(window, live, new, here);
    // **The writer keeps their place.** The new pane opens on the same file
    // (要件 6.4), and a pane showing the same text with the caret back at the
    // top is a pane the writer has to find their way in again.
    if live.states.same_document(here, new) {
        let source = live.states.document(new).text.borrow().clone();
        carry_caret_between_panes(&live.states.of(here), &live.states.of(new), &source);
    }
    // **The keyboard goes to the new pane.** Splitting again then adds a third
    // beside the second, which is what "右にペインが増える" reads as; leaving
    // the keyboard behind would divide the same pane over and over instead.
    window.set_focused_pane(new.index());
    live.cache.borrow_mut().log_diag(
        "layout",
        &format!(
            "pane new={} from={} split={split:?} panes={}",
            new.log_name(),
            here.log_name(),
            PaneId::count(window)
        ),
    );
    after_layout_change(window, live);
}

/// Bring the arrangement back to one pane, keeping this one (要件 6.4).
///
/// **The other panes' tabs come here.** A pane is never taken away from under
/// the tabs it holds — 要件 6.4 removes a pane when its last tab closes, and
/// the pane menu offers no way to close one that still has some — so gathering
/// is the only way to collapse that keeps that promise. A document already open
/// here arrives as nothing, because a strip holds one tab per document.
fn close_other_panes(window: &AppWindow, live: &Live, here: PaneId) {
    // **Every pane to go is named before any of them does** (2026-09-06). The
    // first version asked "which pane is not `here`" once per turn of a loop,
    // and `here` is a number: as soon as a lower-numbered pane went, the
    // numbering closed and that name meant a different pane — the second turn
    // picked the writer's own. It took the pane apart underneath the menu that
    // had asked for it.
    let others = here.others(window);
    if others.is_empty() {
        return;
    }
    // **Emptied rather than removed here.** `remove_pane` takes each strip out
    // itself, and taking one out twice would take the wrong one the second
    // time — the numbering closes as soon as one goes.
    let carried = {
        let mut tabs = live.tabs.borrow_mut();
        others
            .iter()
            .map(|other| std::mem::take(&mut tabs.of_mut(*other).tabs))
            .collect::<Vec<Vec<PaneTab>>>()
    };
    // **From the far end**, so that the panes still to go keep the numbers they
    // were named by. `here` is the only number that moves, and it moves once
    // for every pane below it that goes.
    let mut here = here;
    for other in others.iter().rev() {
        remove_pane(window, live, *other);
        if *other < here {
            here = PaneId(here.0 - 1);
        }
    }
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(here);
        for tab in carried.into_iter().flatten() {
            // 要件 6.3: one tab per document in a strip. The pane the writer is
            // in almost always already holds what is coming — a split opens the
            // new pane on the same file (要件 6.4) — and two tabs with one name
            // is a strip nobody can read.
            if !strip
                .tabs
                .iter()
                .any(|held| Rc::ptr_eq(&held.document, &tab.document))
            {
                strip.tabs.push(tab);
            }
        }
    }
    window.set_focused_pane(here.index());
    publish_tabs(window, live);
    after_layout_change(window, live);
}

/// How much of a file's name is the name without its extension, in characters
/// (追加要件 2026-09-06).
///
/// **The last dot, and never the first character**: `.gitignore` is a name that
/// begins with a dot rather than an extension with nothing in front of it. A
/// name with no dot at all is all stem.
///
/// **Counted in bytes** (2026-09-09). It is handed to `set-selection-offsets`,
/// and Slint's own description of that is "selects the text between two UTF-8
/// offsets" — `safe_byte_offset` treats the number as a byte position. Counting
/// characters looked right and was wrong for every Japanese name: `第一章.md`
/// gave 3, and 3 bytes into it is the end of `第`, so renaming it selected one
/// character out of three.
fn stem_length(title: &str) -> i32 {
    title
        .char_indices()
        .filter(|(at, character)| *character == '.' && *at > 0)
        .next_back()
        .map(|(at, _)| at)
        .unwrap_or(title.len()) as i32
}

/// Give the file a tab stands for the name typed into that tab
/// (追加要件 2026-09-06, 要件 5.2).
///
/// **The same rules as the tree's rename**, and the same act: `check_name`
/// first, then `move_entry`, which brings every open document along and
/// re-titles the tabs. What is new is only where the name was typed.
fn rename_tab(window: &AppWindow, live: &Live, id: PaneId, index: usize, typed: &str) {
    let Some(tab) = live.tabs.borrow().of(id).tabs.get(index).cloned() else {
        return;
    };
    let path = tab.document.file.borrow().path().map(Path::to_path_buf);
    let Some(path) = path else {
        // Nothing is filed under 無題1, so there is nothing to rename. **Said
        // rather than ignored**: the press was held on purpose, and a gesture
        // that does nothing without a word looks broken.
        window.set_render_status("名前の変更: 先に保存してください".into());
        return;
    };
    if typed == entry_name(&path) {
        return;
    }
    let name = match file_tree::check_name(typed) {
        Ok(name) => name,
        Err(problem) => {
            window.set_render_status(problem.message().into());
            return;
        }
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let to = parent.join(name);
    if let Err(error) = move_entry(window, live, &path, &to) {
        window.set_render_status(format!("名前を変えられません: {error}").into());
        return;
    }
    publish_left(window, live);
    write_session(window, live);
}

/// Put a New Tab in a strip that has nothing left in it.
///
/// The last pane always has a tab: one with nothing to show has nowhere to
/// type. **It comes back asking** (追加要件 2026-09-07) — the writer closed
/// everything, and three words offering a file, a shell or a folder is a better
/// answer to that than a 無題1 nobody asked for. The document under it is that
/// 無題1 all the same, so the moment they type there is somewhere for it to go.
/// The number is the smallest free one **across every pane**, because another
/// strip may be holding 無題1 (要件 8.4).
fn refill_strip(window: &AppWindow, live: &Live, id: PaneId) {
    let mut tabs = live.tabs.borrow_mut();
    let taken: Vec<u32> = tabs
        .panes
        .iter()
        .flat_map(|strip| strip.tabs.iter())
        .map(|tab| tab.document.file.borrow().untitled_number())
        .collect();
    let number = next_untitled_number(&taken);
    let empty = OpenDocument::untitled(number, window.as_weak());
    let strip = tabs.of_mut(id);
    strip.tabs.push(PaneTab {
        empty: true,
        ..PaneTab::showing(window, id, empty)
    });
    strip.active = 0;
}

/// Put the keyboard back in the pane that had it.
///
/// A pane is typed into through its IME `TextInput`, and that only takes focus
/// on a pointer-up inside the pane. A modal dialog takes the window's focus
/// away and gives it back, and what it gives it back to is not necessarily the
/// same item — so after every command that opens one, the panes are asked to
/// take it again. The panes decide which of them answers; see
/// `app-window.slint`.
fn restore_editor_focus(window: &AppWindow) {
    let generation = window.get_focus_generation();
    window.set_focus_generation(generation + 1);
}

fn usable_preview_height(height: f32) -> u32 {
    if height.is_finite() && height >= MIN_PREVIEW_HEIGHT as f32 {
        height as u32
    } else {
        PREVIEW_HEIGHT
    }
}

/// What a stored zoom percentage means (要件 9).
///
/// **Zero is not a zoom.** It is a pane row nothing has written yet and a
/// session from a build that did not keep one, and both mean "however the
/// editor opens" rather than "invisible". Everything else is held inside the
/// bounds the keys hold it to, so neither a hand-edited session nor a wheel
/// spun a hundred notches can leave a pane somewhere it cannot be read at.
fn zoom_from(stored: i32) -> i32 {
    match stored {
        0 => ZOOM_DEFAULT,
        percent => percent.clamp(ZOOM_MIN, ZOOM_MAX),
    }
}

fn font_size_for(base_px: i32, zoom_percent: i32) -> f32 {
    base_px.max(1) as f32 * zoom_percent as f32 / 100.0
}

/// The spec a pane is set with: the zoomed body size and 要件 9's numbers.
///
/// **Which way the pane is writing decides two of them.** 要件 9 asks for the
/// line spacing of horizontal text, and separately for the character advance
/// and column gap of vertical text; the engine has one flow-axis multiplier,
/// and what 行間 is to a row of horizontal text 列間隔 is to a column of
/// vertical text. Asking for the direction here is what keeps the writer from
/// moving a number that cannot do anything in the pane they are looking at.
///
/// Zoom, direction and preview are passed rather than read from a pane, because
/// the perf log's header has a spec to describe and no pane to describe it for.
/// Everything that actually sets a pane goes through [`pane_typography`].
/// The spec one pane is set with, read entirely from that pane.
///
/// **Every path that lays a pane out comes through here**, so none of them can
/// set a pane at another pane's zoom or in the other pane's direction. The
/// three things it asks for all live in the pane's own row: how far the writer
/// has magnified it (要件 9), which way the tab in front of it runs, and
/// whether that tab is showing the formatted text or its source (要件 7.2).
fn pane_typography(window: &AppWindow, id: PaneId) -> Typography {
    typography_for(
        window,
        id.zoom(window),
        id.vertical(window),
        id.shows_preview(window),
    )
}

fn typography_for(
    window: &AppWindow,
    zoom_percent: i32,
    vertical: bool,
    preview: bool,
) -> Typography {
    let sheet = usize::from(vertical);
    let percent = |value: i32| value as f32 / 100.0;
    let number = |setting: Setting| setting.read(window, sheet);
    let base = font_size_for(number(Setting::BodySize), zoom_percent);
    let mut spec = Typography::new(base);
    spec.line_spacing = percent(number(Setting::LineAdvance));
    // 要件 9（2026-09-07追加）: the numbers widen the page's own margin, so this
    // travels with the spec that decides that margin.
    spec.line_numbers = number(Setting::LineNumbers) != 0;
    // 要件 7.8（書き手の決定 2026-09-09）: 縦中横。**寸法に効く**ので、
    // 切り替えれば組み直しが起きる。
    spec.upright_digits = number(Setting::UprightDigits) != 0;
    // 要件 7.8（書き手の報告 2026-09-09）: ルビの入る空き。**行送りの下限**を
    // 上げるだけなので、書き手が広く取った行間はそのままである。
    spec.character_spacing = percent(number(Setting::CharAdvance));
    // 要件 7.8: ルビと傍点の大きさと位置。**組版の仕様と一緒に運ぶ**ので、
    // タイルの署名（`hash_style_runs`が混ぜる`Typography`）にも自然に入る
    // ——大きさだけ変えたときに古い絵が残る、が起きない。
    spec.ruby_scale = percent(number(Setting::RubySize));
    spec.ruby_offset = percent(number(Setting::RubyOffset));
    for (level, scale) in spec.heading_scale.iter_mut().enumerate() {
        *scale = percent(number(Setting::Heading(level)));
    }
    let palette = window.get_palette();
    let colour = |slot: usize| {
        channels(
            palette
                .row_data(colour_row(sheet, slot))
                .unwrap_or_default(),
        )
    };
    spec.ink = colour(0);
    for (level, ink) in spec.heading_ink.iter_mut().enumerate() {
        *ink = colour(level + 1);
    }
    spec.paper = colour(PAPER_SLOT);
    let fonts = window.get_sheet_fonts();
    let family = |slot: usize| {
        fonts
            .row_data(font_row(sheet, slot))
            .map(|name| name.to_string())
            .unwrap_or_default()
    };
    spec.body_font = family(0);
    for (level, name) in spec.heading_font.iter_mut().enumerate() {
        *name = family(level + 1);
    }
    spec.code_font = family(CODE_SLOT);
    if !preview {
        plain_source(&mut spec, zoom_percent);
    }
    spec
}

/// Take the text settings back out for a pane showing the source (要件 9).
///
/// **The source is the markup, not the document.** A heading set at twice the
/// size in a colour of its own is a help when reading what the writer wrote and
/// a hindrance when reading what they typed: the marks that make it a heading
/// are what is being edited, and they sit in a line that has been pushed out of
/// shape by the very thing they describe.
///
/// What is left alone is the page rather than the text: the line and character
/// advances, the margin and the paper. Those are how much room the面 gives what
/// is on it, and the source needs that as much as the preview does.
fn plain_source(spec: &mut Typography, zoom_percent: i32) {
    spec.font_size = font_size_for(BASE_FONT_SIZE, zoom_percent);
    spec.heading_scale = [1.0; MAX_HEADING_LEVEL];
    spec.ink = DEFAULT_INK;
    spec.heading_ink = [DEFAULT_INK; MAX_HEADING_LEVEL];
    spec.body_font = DEFAULT_BODY_FONT.to_owned();
    spec.heading_font = std::array::from_fn(|_| DEFAULT_BODY_FONT.to_owned());
    spec.code_font = DEFAULT_BODY_FONT.to_owned();
}

/// How many of each kind of value one sheet holds (要件 9).
///
/// **A sheet is a writing direction**, and the models below hold two of them
/// end to end: the horizontal sheet's values and then the vertical sheet's.
/// One model each rather than a property per value — the panel draws them as
/// rows of the same shape, and the engine reads whichever sheet the pane it is
/// setting belongs to.
/// How many numbers one sheet holds (要件 9).
///
/// **The window is told this rather than knowing it** (`sheet-stride`): the
/// panel reads the model by `sheet * stride + row`, and when this grew from 10
/// to 12 the two places that had written the absolute row instead were missed —
/// the vertical pane then took its page margin from the line height. One
/// definition, sent over.
const SHEET_NUMBERS: usize = 10 + MAX_HEADING_LEVEL;
/// `Setting::WrapMode` set to "the width the writer named" (要件 9). The other
/// two values are `2`, the pane's own width, and `0`, not wrapping at all —
/// **which is written down and not yet built**: tiles are cut along the flow
/// only, so a line that never wraps would want one tile as wide as the longest
/// line in the document. Until they are cut across the flow as well, a sheet
/// set to `0` is laid out like the pane, and the panel does not offer it.
const WRAP_CHARACTERS: i32 = 1;
/// And set to "do not wrap at all" (要件 9). The third value is `2`, the pane's
/// own width.
const WRAP_NEVER: i32 = 0;
const SHEET_COLOURS: usize = 2 + MAX_HEADING_LEVEL;
const SHEET_FONTS: usize = 2 + MAX_HEADING_LEVEL;
/// Where the paper sits among a sheet's colours: after the body ink and the six
/// heading inks.
const PAPER_SLOT: usize = 1 + MAX_HEADING_LEVEL;
/// And the code family among a sheet's fonts, after the body and the six
/// heading families.
const CODE_SLOT: usize = 1 + MAX_HEADING_LEVEL;

fn colour_row(sheet: usize, slot: usize) -> usize {
    sheet * SHEET_COLOURS + slot
}

fn font_row(sheet: usize, slot: usize) -> usize {
    sheet * SHEET_FONTS + slot
}

/// A colour as the engine wants it: three channels from 0 to 1.
fn channels(colour: Color) -> [f32; 3] {
    [
        colour.red() as f32 / 255.0,
        colour.green() as f32 / 255.0,
        colour.blue() as f32 / 255.0,
    ]
}

/// The other way round, for the two colours the window paints itself with.
fn slint_colour(rgb: [f32; 3]) -> Color {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::from_rgb_u8(channel(rgb[0]), channel(rgb[1]), channel(rgb[2]))
}

/// `#rrggbb` as three channels, or nothing.
///
/// **What the settings file holds.** The panel picks colours in the window
/// Windows draws; this is how one is written down, and how a hand-edited file
/// is read back.
fn parse_hex_colour(text: &str) -> Option<[f32; 3]> {
    let digits = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if digits.len() != 6 || !digits.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    let mut channels = [0.0; 3];
    for (index, channel) in channels.iter_mut().enumerate() {
        let at = index * 2;
        let value = u8::from_str_radix(&digits[at..at + 2], 16).ok()?;
        *channel = value as f32 / 255.0;
    }
    Some(channels)
}

/// How a colour is written in the settings file.
fn hex_colour(colour: Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        colour.red(),
        colour.green(),
        colour.blue()
    )
}

/// Put a colour in front of both the panes and the window (要件 9).
fn set_colour(palette: &VecModel<Color>, sheet: usize, slot: usize, rgb: [f32; 3]) {
    if slot > PAPER_SLOT {
        return;
    }
    palette.set_row_data(colour_row(sheet, slot), slint_colour(rgb));
}

/// One of 要件 9's numbers, within one sheet.
///
/// **One callback carries all of them**, the way `tree-command` carries the six
/// file commands: ten handlers that differ only in which row they read are ten
/// places to forget one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    BodySize,
    /// The advance from one line to the next: 行間 on the horizontal sheet,
    /// 列間隔 on the vertical one. **One quantity either way** — it is the flow
    /// axis, and which axis that is on screen is what the sheet says.
    LineAdvance,
    CharAdvance,
    PageMargin,
    /// How long a line may be (要件 9): the pane's width, or a width the writer
    /// names in body characters. **Both sheets have their own** — a comfortable
    /// line is a different length written down the page than across it.
    WrapMode,
    /// That width, in characters of body text. Read only when `WrapMode` is
    /// `WRAP_CHARACTERS`, and kept when it is not, so that turning the setting
    /// off and on again does not lose the number.
    WrapChars,
    /// One heading level, 0 being H1.
    Heading(usize),
    /// Whether the page carries its line numbers (要件 9、2026-09-07追加).
    ///
    /// **On the sheet, like every other thing about how the page is set**, and
    /// on both of them: the numbers stand in the margin at the head of each
    /// line, which is the left edge of a horizontal page and the top of a
    /// vertical one. Upright either way.
    LineNumbers,
    /// ルビと傍点の大きさ、親文字に対する百分率（要件 7.8・要件 9）。
    ///
    /// **シートごとに持つ**——要件 7.8 がそう言っている。縦書きと横書きでは
    /// ルビの置き場所そのものが違うので、読める大きさも同じとは限らない。
    RubySize,
    /// ルビと傍点の位置——行の箱の中で、字へどれだけ寄せるか（要件 7.8）。
    ///
    /// **本文の大きさに対する百分率で、正が字へ近づく向き。**0は行の箱の端
    /// （前の行がある側）で、そこが既定である。行間を詰めて使う人はここを
    /// 負にして逃がせる——**大きさと位置のどちらで直すかは書き手のもの**で、
    /// 編集器が決められることではない。
    RubyOffset,
    /// 縦中横を効かせるか（要件 7.8、書き手の決定 2026-09-09）。
    ///
    /// **縦書きのシートにしか出さない。**縦中横は縦書きの中でだけ起きるので、
    /// 横書きのシートに置けば「押しても何も起きない切り替え」になる
    /// ——働いていない状態を画面に置かない（単語チェックモード要件 2.1.1）。
    UprightDigits,
}

impl Setting {
    /// The number the window sends, which is also the row it sits in.
    fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::BodySize),
            1 => Some(Self::LineAdvance),
            2 => Some(Self::CharAdvance),
            3 => Some(Self::PageMargin),
            4..=9 => Some(Self::Heading(index as usize - 4)),
            // **Added after the headings**, so that the number every existing
            // row is named by stays the number it was.
            10 => Some(Self::WrapMode),
            11 => Some(Self::WrapChars),
            12 => Some(Self::LineNumbers),
            13 => Some(Self::RubySize),
            14 => Some(Self::RubyOffset),
            15 => Some(Self::UprightDigits),
            _ => None,
        }
    }

    fn row_in_sheet(self) -> usize {
        match self {
            Self::BodySize => 0,
            Self::LineAdvance => 1,
            Self::CharAdvance => 2,
            Self::PageMargin => 3,
            Self::Heading(level) => 4 + level.min(MAX_HEADING_LEVEL - 1),
            Self::WrapMode => 4 + MAX_HEADING_LEVEL,
            Self::WrapChars => 5 + MAX_HEADING_LEVEL,
            Self::LineNumbers => 6 + MAX_HEADING_LEVEL,
            Self::RubySize => 7 + MAX_HEADING_LEVEL,
            Self::RubyOffset => 8 + MAX_HEADING_LEVEL,
            Self::UprightDigits => 9 + MAX_HEADING_LEVEL,
        }
    }

    /// How far one press moves it.
    fn step(self) -> i32 {
        match self {
            Self::BodySize => 1,
            Self::WrapMode => 1,
            Self::LineNumbers => 1,
            Self::UprightDigits => 1,
            Self::WrapChars => 2,
            Self::RubySize => 2,
            Self::RubyOffset => 2,
            Self::PageMargin => 4,
            Self::CharAdvance => 5,
            Self::Heading(_) => 5,
            Self::LineAdvance => 10,
        }
    }

    /// How far it may go. **Wide rather than tasteful**: 要件 9 says the
    /// numbers are the writer's, and a limit is only here to keep a document
    /// from becoming unreadable by one held-down button.
    fn range(self) -> (i32, i32) {
        match self {
            Self::BodySize => (8, 96),
            Self::LineAdvance => (70, 400),
            Self::CharAdvance => (-20, 100),
            Self::PageMargin => (0, 160),
            Self::WrapMode => (0, 2),
            Self::LineNumbers => (0, 1),
            Self::UprightDigits => (0, 1),
            Self::WrapChars => (10, 200),
            // 親文字より大きいルビは、ルビではなく別の本文である。
            Self::RubySize => (20, 100),
            Self::RubyOffset => (-50, 50),
            Self::Heading(_) => (50, 400),
        }
    }

    fn default_value(self) -> i32 {
        match self {
            Self::BodySize => BASE_FONT_SIZE,
            Self::LineAdvance => 100,
            Self::CharAdvance => 0,
            Self::PageMargin => 12,
            // The pane, which is what the editor did before the setting existed.
            Self::WrapMode => 2,
            // 40 characters of body text: a line a reader's eye can come back
            // from without losing its place, and the length a page of Japanese
            // prose is usually set to.
            Self::WrapChars => 40,
            // Off: a page of prose is not a program, and the writer asks for
            // the numbers when they want them.
            Self::LineNumbers => 0,
            // 入。要件 7.8 は「書き手が何も書かなくても効く」と言っている
            // ——切りたい書き手が切る側であって、既定が何もしない側ではない。
            Self::UprightDigits => 1,
            // 半分が日本語の組版の当たり前である。
            Self::RubySize => 50,
            // 行の箱の端。行間の空きがそのままルビの帯になる。
            Self::RubyOffset => 0,
            Self::Heading(level) => HEADING_DEFAULTS.get(level).copied().unwrap_or(100),
        }
    }

    /// What this sheet has it set to.
    fn read(self, window: &AppWindow, sheet: usize) -> i32 {
        let row = sheet * SHEET_NUMBERS + self.row_in_sheet();
        window
            .get_sheet_numbers()
            .row_data(row)
            .unwrap_or_else(|| self.default_value())
    }

    /// **Written through the model**, which is Rust's; the window reads it.
    fn write(self, numbers: &VecModel<i32>, sheet: usize, value: i32) {
        numbers.set_row_data(sheet * SHEET_NUMBERS + self.row_in_sheet(), value);
    }

    /// Every one of them, for 「初期値へ戻す」 and for the settings file.
    fn all() -> impl Iterator<Item = Self> {
        (0..SHEET_NUMBERS as i32).filter_map(Self::from_index)
    }

    /// The name this is written under in the settings file (要件 9).
    fn name(self) -> &'static str {
        match self {
            Self::BodySize => "body-size",
            Self::LineAdvance => "line-advance",
            Self::CharAdvance => "char-advance",
            Self::PageMargin => "page-margin",
            Self::WrapMode => "wrap-mode",
            Self::LineNumbers => "line-numbers",
            Self::UprightDigits => "upright-digits",
            Self::WrapChars => "wrap-chars",
            Self::RubySize => "ruby-size",
            Self::RubyOffset => "ruby-offset",
            Self::Heading(0) => "h1",
            Self::Heading(1) => "h2",
            Self::Heading(2) => "h3",
            Self::Heading(3) => "h4",
            Self::Heading(4) => "h5",
            Self::Heading(_) => "h6",
        }
    }

    /// The setting of that name, if this build has one.
    fn from_name(name: &str) -> Option<Self> {
        Self::all().find(|setting| setting.name() == name)
    }
}

/// Move one setting by one press of its buttons (要件 9).
fn step_setting(window: &AppWindow, numbers: &VecModel<i32>, setting: Setting, by: i32) {
    let sheet = shown_sheet(window);
    let (low, high) = setting.range();
    let next = (setting.read(window, sheet) + by * setting.step()).clamp(low, high);
    setting.write(numbers, sheet, next);
}

/// Which sheet the panel is showing, held to one that exists.
fn shown_sheet(window: &AppWindow) -> usize {
    usize::from(window.get_sheet() != 0)
}

/// What a colour is called in the settings file.
///
/// Separate names from the heading sizes — `h1` is how large H1 is set, and its
/// colour is something else about the same heading.
fn colour_name(slot: usize) -> &'static str {
    match slot {
        0 => "ink",
        1 => "ink-h1",
        2 => "ink-h2",
        3 => "ink-h3",
        4 => "ink-h4",
        5 => "ink-h5",
        6 => "ink-h6",
        _ => "paper",
    }
}

fn colour_slot(name: &str) -> Option<usize> {
    (0..SHEET_COLOURS).find(|slot| colour_name(*slot) == name)
}

/// And a family.
fn font_name(slot: usize) -> &'static str {
    match slot {
        0 => "font-body",
        1 => "font-h1",
        2 => "font-h2",
        3 => "font-h3",
        4 => "font-h4",
        5 => "font-h5",
        6 => "font-h6",
        _ => "font-code",
    }
}

fn font_slot(name: &str) -> Option<usize> {
    (0..SHEET_FONTS).find(|slot| font_name(*slot) == name)
}

/// What a sheet's colours are before anybody chooses.
///
/// The two papers differ by a shade so that the two panes answer 「どちらの向き
/// で書いているか」 without a word being read; everything else starts the same.
fn default_colour(sheet: usize, slot: usize) -> [f32; 3] {
    if slot != PAPER_SLOT {
        return DEFAULT_INK;
    }
    if sheet == 0 {
        DEFAULT_PAPER
    } else {
        text_blocks::DEFAULT_VERTICAL_PAPER
    }
}

/// And its families.
fn default_font(slot: usize) -> &'static str {
    match slot {
        0 => DEFAULT_BODY_FONT,
        CODE_SLOT => DEFAULT_CODE_FONT,
        _ => DEFAULT_HEADING_FONT,
    }
}

/// Which sheet a settings-file name belongs to, and the name without it.
///
/// **`h.` and `v.`**, because every one of these is a direction's now. A name
/// with no prefix is one from before the sheets were split; it is taken as
/// both, so a settings file written by an older build opens with what it said
/// rather than with the defaults.
fn sheet_prefix(name: &str) -> (Option<usize>, &str) {
    if let Some(rest) = name.strip_prefix("h.") {
        return (Some(0), rest);
    }
    if let Some(rest) = name.strip_prefix("v.") {
        return (Some(1), rest);
    }
    (None, name)
}

/// Every display setting as a name and a value (要件 9).
/// 追加要件 2026-09-07: what a terminal opens as when nobody says otherwise.
///
/// **Not one of the sheets' settings** (要件 9 は書字方向ごと): a shell has no
/// writing direction. It rides in the same file because that file is where what
/// the editor is set to lives.
const DEFAULT_SHELL_SETTING: &str = "terminal.default";

/// 追加要件 2026-09-08: whether the editor keeps work copies at all (要件 8.1).
///
/// **On unless the file says otherwise**, which is also what a fresh install
/// has: 要件 8.1 is the promise the editor makes about unsaved work, and a
/// writer who has not said anything has not asked to give it up.
const AUTOSAVE_SETTING: &str = "work.autosave";
/// 本文文字数がルビの読みを数えるか（要件 7.8・要件 10、2026-09-09）。
///
/// **紙の設定（要件 9）のシートには置かない。**シートが持つのは組み方であり、
/// これは数え方である——縦書きの原稿と横書きの原稿で、投稿サイトへ出す字数の
/// 決まりが変わるわけではない。
const COUNT_RUBY_SETTING: &str = "count.ruby";

/// 追加要件 2026-09-08: the shell list, one entry per numbered name
/// (`terminal.shell.0`, `terminal.shell.1`, …).
///
/// **The numbers are the order, not an identity.** They are read in the order
/// the file writes them and renumbered on the way out, so a writer who deletes
/// the middle one gets a list of two rather than a hole.
const SHELL_SETTING: &str = "terminal.shell";

/// 追加要件 2026-09-08: 端末の見た目（要件 6.8）。
///
/// **紙とは別に持つ。**要件9のシートは原稿の紙の設定で、端末を黒地で使う人が多いことと
/// 何の関係も無い。書字方向も持たないので、シートの外にいる。
const TERMINAL_PAPER_SETTING: &str = "terminal.paper";
const TERMINAL_INK_SETTING: &str = "terminal.ink";
const TERMINAL_FONT_SETTING: &str = "terminal.font";
const TERMINAL_SIZE_SETTING: &str = "terminal.size";
/// 端末の字の大きさの幅。**紙より狭い**——升目が壊れるほど大きくしても読めない。
const TERMINAL_SIZE_RANGE: (i32, i32) = (9, 32);

thread_local! {
    /// 鍵盤を持っている素の欄の数（書き手の報告 2026-09-08）。
    ///
    /// **0でなければIMEは横書き。**縦書きはペインの組版の話で、設定画面や検索欄へ
    /// 打ち込む語はどこまでも横書きである（技術検証 7.2）。
    static FIELDS_TYPING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// 次に配る番号（要件 7.9、2026-09-08）。**消した番号は二度と使わない。**
    static WORD_NEXT_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
    /// どの語群にも属さない覚え書き（2026-09-08）。**書き手が書いたものを、
    /// 画面から一度も見えないまま消さないため**に持ち歩く。
    static WORD_NOTES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// 表を書き出してよいか。**読めない表を見つけたら偽になる**——書き手が
    /// 積み上げた辞書を、読めなかったこの実行が上書きしてしまわないように。
    static WORD_STORING: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// 書き手が持っているモードぜんぶと、モードごとに建てた木（要件 7.9）。
    ///
    /// **窓の外に置いてある。**Slintのプロパティは任意のRustの値を持てず、しかし
    /// これは`refresh_pane`——組版のたびに走る道——から`Arc`1つの複製で届く必要が
    /// ある。木を組版のたびに建て直しては、木にした意味が消える。
    ///
    /// **UIスレッドのものである。**書き換えるのは`hold_word_modes`だけで、読むのは
    /// 組版を組み立てる側だけ——どちらも窓のスレッドにいる。
    static WORD_MODES: RefCell<Rc<Vec<Arc<word_marks::WordMarks>>>> =
        RefCell::new(Rc::new(Vec::new()));
}

/// いま持っているモードぜんぶ。**複製は`Rc`1つぶん。**
fn word_modes() -> Rc<Vec<Arc<word_marks::WordMarks>>> {
    WORD_MODES.with(|held| held.borrow().clone())
}

/// 番号でモードを引く。**無い番号は「なし」**——モードを消したあとの文書は、
/// 色分けの無い文書になる（間違った色で出るよりよい）。
fn word_mode_with(id: u32) -> Arc<word_marks::WordMarks> {
    if id == 0 {
        return Arc::default();
    }
    word_modes()
        .iter()
        .find(|marks| marks.mode.id == id)
        .cloned()
        .unwrap_or_default()
}

/// 表の1つを、編集器の中の形へ。**色が読めなければ既定の色**——表が壊れていても
/// 語群ごと消えるよりはよい。
fn mode_from_stored(stored: &app_data::StoredMode) -> word_marks::WordMode {
    word_marks::WordMode {
        id: stored.id,
        name: stored.name.clone(),
        groups: stored
            .groups
            .iter()
            .map(|group| word_marks::WordGroup {
                id: group.id,
                name: group.name.clone(),
                // **`none`は色を持たない語群**（除外語群、2026-09-08）。読めない
                // 綴りは灰色にする——色が消えるより、変な色のほうが直しやすい。
                colour: (group.colour != app_data::NO_COLOUR)
                    .then(|| parse_hex_colour(&group.colour).unwrap_or([0.5, 0.5, 0.5])),
                words: group.words.clone(),
            })
            .collect(),
    }
}

fn stored_from_mode(mode: &word_marks::WordMode) -> app_data::StoredMode {
    app_data::StoredMode {
        id: mode.id,
        name: mode.name.clone(),
        groups: mode
            .groups
            .iter()
            .map(|group| app_data::StoredGroup {
                id: group.id,
                name: group.name.clone(),
                colour: match group.colour {
                    Some(ink) => hex_colour(slint_colour(ink)),
                    // **色を持たない語群**（除外語群、2026-09-08）。
                    None => app_data::NO_COLOUR.to_owned(),
                },
                words: group.words.clone(),
            })
            .collect(),
    }
}

/// 表を読んで木を建て、画面へ出す（要件 7.9）。**起動のときに一度。**
fn open_word_modes(window: &AppWindow, live: &Live) {
    let Some(directory) = app_data::app_directory() else {
        hold_word_modes(window, live, Vec::new(), false);
        return;
    };
    // **この実行が始まる前の姿を1つ控えておく**（2026-09-08）。守りたいのは
    // 「この実行が辞書を壊した」で、そのとき直前の姿が要る。
    if let Err(error) = app_data::keep_previous_words(&directory) {
        live.cache
            .borrow_mut()
            .log_diag("spec", &format!("words prev not kept error={error}"));
    }
    match app_data::read_words(&directory) {
        Some((stored, damaged)) => {
            if damaged > 0 {
                // **黙って半分になった辞書は、いちばん気づきにくい失い方**である。
                let told = format!("単語帳の{damaged}行を読めませんでした");
                window.set_render_status(told.into());
                live.cache
                    .borrow_mut()
                    .log_diag("spec", &format!("words damaged lines={damaged}"));
            }
            WORD_NEXT_ID.with(|next| next.set(stored.next_id.max(1)));
            WORD_NOTES.with(|notes| *notes.borrow_mut() = stored.notes.clone());
            let modes: Vec<word_marks::WordMode> =
                stored.modes.iter().map(mode_from_stored).collect();
            hold_word_modes(window, live, modes, false);
        }
        None => {
            // **読めない表は上書きしない。**辞書は書き手が積み上げたもので、
            // 読めないからといって捨ててよいものではない——この実行は色分けの
            // 無いまま進み、書き手がファイルを見に行ける。
            if app_data::words_path(&directory).exists() {
                window.set_render_status("単語帳を読めませんでした（上書きしません）".into());
                live.cache.borrow_mut().log_diag("spec", "words unreadable");
                WORD_STORING.with(|storing| storing.set(false));
            }
            hold_word_modes(window, live, Vec::new(), false);
        }
    }
}

/// モードを置き換え、木を建て直し、画面へ出し、必要なら書き出す（要件 7.9）。
///
/// **どの操作もここを通る**：モードを作る、語群を作る、語を足す、色を変える、
/// 取り込む。順番を守る場所が1つで済み、**重複を数える場所も1つ**になる。
fn hold_word_modes(window: &AppWindow, live: &Live, modes: Vec<word_marks::WordMode>, store: bool) {
    let started = Instant::now();
    let cache = &live.cache;
    let storing = WORD_STORING.with(std::cell::Cell::get);
    if store
        && storing
        && let Some(directory) = app_data::app_directory()
    {
        let held = app_data::StoredWords {
            modes: modes.iter().map(stored_from_mode).collect(),
            next_id: WORD_NEXT_ID.with(std::cell::Cell::get),
            notes: WORD_NOTES.with(|notes| notes.borrow().clone()),
        };
        // **書くのは別のスレッド**（2026-09-08）。作業コピーと同じ行列へ乗せる
        // ——1語足すたびに数百KBを`sync_all`まで待って書くのは、書く手を止める
        // ことである（要件 2）。行列は同じパスの古い仕事を畳むので、続けて足せば
        // 書き込みは1回になる。終了のときに`FileWriter::finish`が待つ（要件 8.1）。
        let path = app_data::words_path(&directory);
        let bytes = app_data::encode_words(&held).into_bytes();
        if !live.writer.write(path.clone(), bytes.clone())
            && let Err(error) = file_io::write_atomically(&path, &bytes)
        {
            cache
                .borrow_mut()
                .log_diag("spec", &format!("words not saved error={error}"));
        }
    }
    let words: usize = modes.iter().map(word_marks::WordMode::words).sum();
    let built: Vec<Arc<word_marks::WordMarks>> = modes
        .into_iter()
        .map(|mode| Arc::new(word_marks::WordMarks::build(mode)))
        .collect();
    let conflicts: usize = built.iter().map(|marks| marks.conflicts()).sum();
    let told = format!(
        "words modes={} words={words} conflicts={conflicts} built={:.2}ms",
        built.len(),
        elapsed_ms(started)
    );
    cache.borrow_mut().log_diag("spec", &told);
    WORD_MODES.with(|held| *held.borrow_mut() = Rc::new(built));
    publish_word_modes(window);
    // **働いていない理由も出し直す**（要件 2.1.1）。衝突は語を1つ足しただけで
    // 増えたり消えたりするので、表が変わるこの1本を必ず通る。
    publish_word_mode_of(window, live);
    // **色が変わったので描き直す。測り直しはしない**（要件 7.9）。単語セットは
    // `Typography`の外にいるので`matches`が真のままで、`update`は何も捨てずに
    // 戻る——**動くのはタイルの署名だけ**である（技術検証 9.3.1）。
    relayout_panes(window, &live.states, cache);
    window.invoke_republish_tabs();
}

/// いまのモードぜんぶ（画面の操作が手を入れる元）。
fn word_modes_now() -> Vec<word_marks::WordMode> {
    word_modes()
        .iter()
        .map(|marks| marks.mode.clone())
        .collect()
}

/// 前に出ているタブのモードの名前。
fn pane_word_mode(live: &Live, id: PaneId) -> u32 {
    let tabs = live.tabs.borrow();
    tabs.of(id).current().map_or(0, |tab| tab.word_mode)
}

/// モードと語群を画面へ（要件 7.9）。
///
/// **重複はここで初めて言葉になる。**モードを建てたときに数えてあり、画面は
/// それを読むだけ——数え役は`WordMarks`ひとつである。
fn publish_word_modes(window: &AppWindow) {
    let modes = word_modes();
    // **編集面の右ボタンが並べる名前。**前に出ている文書のモードの語群である
    // ——足す先は、いまその文書に効いているモードの中にしかない。
    let front = word_mode_with(window.get_word_mode().max(0) as u32);
    let group_names: Vec<SharedString> = front
        .mode
        .groups
        .iter()
        .map(|group| SharedString::from(group.name.as_str()))
        .collect();
    window.set_word_group_names(ModelRc::new(VecModel::from(group_names)));

    // ステータスバーの選び口。**「なし」が先頭**——色分けを止めるのに、モードを
    // 消す必要は無い（コードエディタの`Plain Text`にあたる）。
    let mut names: Vec<WordModeRow> = vec![WordModeRow {
        id: 0,
        name: word_marks::NO_MODE.into(),
    }];
    names.extend(modes.iter().map(|marks| WordModeRow {
        id: marks.mode.id as i32,
        name: marks.mode.name.as_str().into(),
    }));
    window.set_word_mode_names(ModelRc::new(VecModel::from(names)));

    // 設定画面が開いているモード（-1でどれも開いていない）。
    let opened = window.get_word_mode_opened_at();
    let Some(marks) = modes.get(opened.max(0) as usize).filter(|_| opened >= 0) else {
        window.set_word_group_rows(ModelRc::new(VecModel::from(Vec::<WordGroupRow>::new())));
        window.set_word_troubles(ModelRc::new(VecModel::from(Vec::<WordTroubleRow>::new())));
        window.set_word_trouble_count(0);
        window.set_word_group_words(ModelRc::new(VecModel::from(Vec::<WordRow>::new())));
        return;
    };

    let rows: Vec<WordGroupRow> = marks
        .mode
        .groups
        .iter()
        .enumerate()
        .map(|(at, group)| {
            let told = marks.troubles_of(at);
            let conflicts = told
                .iter()
                .filter(|trouble| trouble.kind == word_marks::Trouble::Conflict)
                .count();
            WordGroupRow {
                name: group.name.clone().into(),
                words: group.word_count() as i32,
                conflicts: conflicts as i32,
                repeated: (told.len() - conflicts) as i32,
                paints: group.colour.is_some(),
                shown: slint_colour(group.colour.unwrap_or([0.5, 0.5, 0.5])),
            }
        })
        .collect();
    window.set_word_group_rows(ModelRc::new(VecModel::from(rows)));

    // **重複の一覧。**語群の番号ではなく名前で言う——書き手が見るのは名前である。
    let named = |at: &usize| {
        marks
            .mode
            .groups
            .get(*at)
            .map(|group| group.name.clone())
            .unwrap_or_default()
    };
    let troubles: Vec<WordTroubleRow> = marks
        .troubles
        .iter()
        .take(word_marks::SHOWN_TROUBLES)
        .map(|trouble| WordTroubleRow {
            word: trouble.word.clone().into(),
            conflict: trouble.kind == word_marks::Trouble::Conflict,
            told: match trouble.kind {
                word_marks::Trouble::Conflict => trouble
                    .groups
                    .iter()
                    .map(named)
                    .collect::<Vec<String>>()
                    .join(" と ")
                    .into(),
                word_marks::Trouble::Repeated => {
                    let name = trouble.groups.first().map_or(String::new(), |at| named(at));
                    format!("{name} の中で二度").into()
                }
            },
        })
        .collect();
    window.set_word_troubles(ModelRc::new(VecModel::from(troubles)));
    window.set_word_trouble_count(marks.troubles.len() as i32);

    // **開いている語群の語だけ**を窓へ渡す——数千語を毎回渡さない。
    let group = window.get_word_group_opened_at();
    let words: Vec<WordRow> = marks
        .mode
        .groups
        .get(group.max(0) as usize)
        .filter(|_| group >= 0)
        .map(|group| {
            group
                .words
                .iter()
                .map(|line| WordRow {
                    // **覚え書きは薄く、消す釦も出さない**（2026-09-08）。
                    // 語ではないので、語の一覧の中では見出しとして立つ。
                    note: word_marks::is_note(line),
                    text: line.as_str().into(),
                })
                .collect()
        })
        .unwrap_or_default();
    window.set_word_group_words(ModelRc::new(VecModel::from(words)));
}

/// そのペインの前に出ているタブのモードを決める（要件 7.9、2026-09-08）。
///
/// **タブのメニューとステータスバー、どちらの口もここへ来る。**指す先が違う
/// だけで、することは同じである——モードは文書ごとのものなので、変えるのは
/// 「そのタブ」であって「そのペイン」ではない。
fn set_word_mode_of(window: &AppWindow, live: &Live, id: PaneId, mode: u32) {
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let at = strip.active;
        let Some(tab) = strip.tabs.get_mut(at) else {
            return;
        };
        tab.word_mode = mode;
    }
    // 組版はこの行から読む（`lay_out_pane`）。
    id.update_screen(window, |screen| screen.word_mode = mode as i32);
    publish_word_mode_of(window, live);
    // **色が変わったので描き直す。測り直しはしない**（技術検証 9.3.1）。
    relayout_panes(window, &live.states, &live.cache);
    write_session(window, live);
}

/// ステータスバーへ、前に出ているタブのモードを出す（要件 10）。
///
/// **コードエディタが言語モードを出しているのと同じ場所**である。いまどのモードか
/// が常に見えていて、押せば切り替わる。
fn publish_word_mode_of(window: &AppWindow, live: &Live) {
    let id = pane_word_mode(live, focused_pane(window));
    let marks = word_mode_with(id);
    let name = marks.mode.name.clone();
    window.set_word_mode(id as i32);
    // **番号ではなく名前を見せる。**指しているのは番号でも、書き手が読むのは名前。
    window.set_word_mode_name(if name.is_empty() {
        word_marks::NO_MODE.into()
    } else {
        name.into()
    });
    // **色分けが働いていない状態は、画面に出す**（単語チェックモード要件 2.1.1）。
    //
    // この機能の値打ちは**色が付かなかったこと**にある——`リオン`のつもりが
    // `リオソ`だったと気づくため。だから「辞書が読めなかった」「その語は衝突して
    // いて色が付かない」も**打ち間違えたのと同じ見た目になる**。黙っていると、
    // 書き手は自分の原稿のほうを疑うことになる。
    let conflicts = marks.conflicts();
    window.set_word_mode_trouble(if !WORD_STORING.with(std::cell::Cell::get) {
        "辞書を読めていません".into()
    } else if conflicts > 0 {
        format!("色が付かない語 {conflicts}").into()
    } else {
        SharedString::new()
    });
}

/// 足す語群に与える色（要件 7.9）。
///
/// **まだ使っていない色から順に。**同じ色が2つ並ぶと、どちらの色分けを見ているのか
/// 画面が言えない。並びは要件9の墨と同じ家族から選んである——紙の上で読める濃さで、
/// 互いに見分けが付く。
fn next_word_colour(groups: &[word_marks::WordGroup]) -> [f32; 3] {
    const OFFERED: [[f32; 3]; word_marks::MAX_WORD_GROUPS] = [
        [0.70, 0.16, 0.16], // 紅
        [0.16, 0.32, 0.66], // 藍
        [0.18, 0.45, 0.20], // 緑
        [0.63, 0.42, 0.09], // 山吹
        [0.53, 0.22, 0.62], // 紫
        [0.11, 0.45, 0.48], // 青緑
        [0.60, 0.30, 0.12], // 煉瓦
        [0.35, 0.35, 0.38], // 鈍色
    ];
    OFFERED
        .into_iter()
        .find(|colour| !groups.iter().any(|group| group.colour == Some(*colour)))
        .unwrap_or(OFFERED[0])
}

/// 次の番号を1つ配る（要件 7.9、2026-09-08）。
///
/// **使い回さない。**消した番号を配り直すと、古いセッションが指していた番号が
/// **別のモード**を指すことになり、「切れている」より悪い。
fn next_word_id() -> u32 {
    WORD_NEXT_ID.with(|next| {
        let given = next.get().max(1);
        next.set(given + 1);
        given
    })
}

/// 名前を訊いていた欄を畳む（単語チェックモード要件 7.4）。
///
/// **できたときだけ畳む。**駄目だったときに畳むと、書き手が打った名前が消えて、
/// 何が起きたのかも消える。
fn close_word_naming(window: &AppWindow) {
    window.set_word_naming(0);
    window.set_word_new_name(SharedString::new());
    window.set_word_naming_trouble(SharedString::new());
}

/// 空のモードを1つ作る（単語チェックモード要件 7.2）。
///
/// **ファイルは要らない。**辞書は編集器が持つもので、書き手がファイルを管理する
/// 必要は無い。
///
/// **駄目だった理由は文字で返す**（同要件 7.4、書き手の求め 2026-09-08）。設定は
/// 一枚で終えるので、言う先は下の帯ではなく**名前を打った欄のすぐ下**である
/// ——目はいまそこにある。
fn new_word_mode(window: &AppWindow, live: &Live, name: &str) -> Result<(), String> {
    let mut modes = word_modes_now();
    if modes.len() >= word_marks::MAX_WORD_MODES {
        return Err(format!("モードは{}個までです", word_marks::MAX_WORD_MODES));
    }
    let name = name.trim();
    if name.is_empty() || name == word_marks::NO_MODE {
        return Err("その名前は使えません".to_owned());
    }
    if modes.iter().any(|mode| mode.name == name) {
        return Err(format!("「{name}」はもうあります"));
    }
    modes.push(word_marks::WordMode {
        id: next_word_id(),
        name: name.to_owned(),
        groups: Vec::new(),
    });
    // **作ったモードを開いておく。**作った直後に中身が出ていなければ、何が
    // 起きたのか画面に無い（同要件 7.4）。
    window.set_word_mode_opened_at(modes.len() as i32 - 1);
    window.set_word_group_opened_at(-1);
    hold_word_modes(window, live, modes, true);
    Ok(())
}

/// 開いているモードへ語群を1つ足す（単語チェックモード要件 7.2）。
fn new_word_group(window: &AppWindow, live: &Live, at: usize, name: &str) -> Result<(), String> {
    let mut modes = word_modes_now();
    let Some(mode) = modes.get_mut(at) else {
        return Err("先にモードを開いてください".to_owned());
    };
    if mode.groups.len() >= word_marks::MAX_WORD_GROUPS {
        return Err(format!("語群は{}つまでです", word_marks::MAX_WORD_GROUPS));
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("その名前は使えません".to_owned());
    }
    if mode.groups.iter().any(|group| group.name == name) {
        return Err(format!("「{name}」はもうあります"));
    }
    let colour = next_word_colour(&mode.groups);
    mode.groups.push(word_marks::WordGroup {
        id: next_word_id(),
        name: name.to_owned(),
        colour: Some(colour),
        words: Vec::new(),
    });
    hold_word_modes(window, live, modes, true);
    Ok(())
}

/// モードの名前を変える（単語チェックモード要件 7.4、書き手の求め 2026-09-08）。
///
/// **文書のモードは切れない。**文書が指しているのは番号であって名前ではない
/// （同要件 3.2）——名前は書き手が変えるものだから、変えた瞬間に色が消えるのでは
/// 名前の仕事として重すぎる。**この関数が番号に触らないことが、その約束である。**
fn rename_word_mode(window: &AppWindow, live: &Live, at: usize, name: &str) -> Result<(), String> {
    let mut modes = word_modes_now();
    let name = name.trim();
    if name.is_empty() || name == word_marks::NO_MODE {
        return Err("その名前は使えません".to_owned());
    }
    if modes
        .iter()
        .enumerate()
        .any(|(index, mode)| index != at && mode.name == name)
    {
        return Err(format!("「{name}」はもうあります"));
    }
    let Some(mode) = modes.get_mut(at) else {
        return Err("そのモードはもうありません".to_owned());
    };
    if mode.name == name {
        return Ok(());
    }
    mode.name = name.to_owned();
    hold_word_modes(window, live, modes, true);
    // ステータスバーとタブのメニューが出している名前も、いまの名前にする。
    publish_word_mode_of(window, live);
    Ok(())
}

/// 語群の名前を変える（同上）。
fn rename_word_group(
    window: &AppWindow,
    live: &Live,
    mode: usize,
    at: usize,
    name: &str,
) -> Result<(), String> {
    let mut modes = word_modes_now();
    let Some(held) = modes.get_mut(mode) else {
        return Err("先にモードを開いてください".to_owned());
    };
    let name = name.trim();
    if name.is_empty() {
        return Err("その名前は使えません".to_owned());
    }
    if held
        .groups
        .iter()
        .enumerate()
        .any(|(index, group)| index != at && group.name == name)
    {
        return Err(format!("「{name}」はもうあります"));
    }
    let Some(group) = held.groups.get_mut(at) else {
        return Err("その語群はもうありません".to_owned());
    };
    if group.name == name {
        return Ok(());
    }
    group.name = name.to_owned();
    hold_word_modes(window, live, modes, true);
    Ok(())
}

/// 単語帳のファイルをタブで開く（単語チェックモード要件 5.4、書き手の求め
/// 2026-09-08）。
///
/// **開く前に、いまの表をその場で書く。**書き込みはふだん別のスレッドの行列を
/// 通る（同要件 4.3）ので、**行列に残っている仕事より先に読んでしまうと、
/// 書き手は一つ前の姿を編集することになる**。同じ中身をもう一度書くだけなので、
/// 行列に残ったぶんが後から着いても害は無い。
fn edit_word_file(window: &AppWindow, live: &Live) {
    let Some(directory) = app_data::app_directory() else {
        window.set_render_status("単語帳の置き場所が分かりません".into());
        return;
    };
    let held = app_data::StoredWords {
        modes: word_modes_now().iter().map(stored_from_mode).collect(),
        next_id: WORD_NEXT_ID.with(std::cell::Cell::get),
        notes: WORD_NOTES.with(|notes| notes.borrow().clone()),
    };
    let path = app_data::words_path(&directory);
    // **読めない表のときは書かない**（同要件 4.4）。書き手が直しに行く先を、
    // こちらが上書きしてしまう。
    if WORD_STORING.with(std::cell::Cell::get)
        && let Err(error) =
            file_io::write_atomically(&path, app_data::encode_words(&held).as_bytes())
    {
        window.set_render_status(format!("単語帳を書けません: {error}").into());
        return;
    }
    if !path.exists() {
        window.set_render_status("単語帳のファイルがありません".into());
        return;
    }
    open_path_in_focused_pane(window, live, &path, Opening::Kept);
    window.set_render_status("単語帳を開きました（保存すると取り込みます）".into());
}

/// 単語帳のファイルが保存されたので、そこから読み直す（同要件 5.4）。
///
/// **取り込みではなく、置き換えである。**書き手が直したのは表そのもので、
/// 画面の表と食い違ったまま進むと、次に語を1つ足した拍子に**書き手の編集が
/// 消える**。
///
/// **読めなければ、いまの表のまま止める**（同要件 4.4）。そして書き込みを止める
/// ——直している最中のファイルを上書きしないためで、次に読めたときに戻る。
fn adopt_word_file(window: &AppWindow, live: &Live) {
    let Some(directory) = app_data::app_directory() else {
        return;
    };
    match app_data::read_words(&directory) {
        Some((stored, damaged)) => {
            WORD_STORING.with(|storing| storing.set(true));
            WORD_NEXT_ID.with(|next| next.set(stored.next_id.max(1)));
            WORD_NOTES.with(|notes| *notes.borrow_mut() = stored.notes.clone());
            let modes: Vec<word_marks::WordMode> =
                stored.modes.iter().map(mode_from_stored).collect();
            let told = if damaged > 0 {
                format!("単語帳を取り込みました（{damaged}行は読めませんでした）")
            } else {
                "単語帳を取り込みました".to_owned()
            };
            // **書き戻さない。**いま読んだものがファイルの中身なので、書けば
            // 開いているタブに外部変更として立つだけである（要件 8.2）。
            hold_word_modes(window, live, modes, false);
            publish_word_mode_of(window, live);
            window.set_render_status(told.into());
        }
        None => {
            WORD_STORING.with(|storing| storing.set(false));
            window.set_render_status("単語帳を読めませんでした（取り込みません）".into());
            live.cache
                .borrow_mut()
                .log_diag("spec", "words unreadable after edit");
        }
    }
}

/// いま保存されたのが単語帳そのものか（同要件 5.4）。
pub fn is_word_file(path: &Path) -> bool {
    app_data::app_directory().is_some_and(|directory| app_data::words_path(&directory) == path)
}

/// 選んでいる語を、語群へ足す（要件 7.9）。
///
/// **書きながら足すいちばん普通の道。**書いていて気づいた名前を、その場で、書く手を
/// 止めずに入れる（要件 3）。一覧を開いて打ち込むのは、その次である。
///
/// **改行をまたぐ選択は語にしない。**語とは1行に収まるもので、段落を丸ごと選んで
/// 足せてしまうと、その語は本文のどこにも当たらないまま一覧を汚す。
fn add_word_to_group(window: &AppWindow, live: &Live, mode: u32, at: usize, word: &str) {
    let word = word.trim();
    if word.is_empty() || word.contains('\n') {
        window.set_render_status("1行に収まる語だけを足せます".into());
        return;
    }
    // **`#`で始まる語は足せない**（2026-09-08）。その形は覚え書きのもので、
    // 足せてしまうと一覧の中で見出しに化ける——見出しの行を選んで足そうとした
    // ときに起きる。
    if word_marks::is_note(word) {
        window.set_render_status("`#`で始まる語は足せません（覚え書きの印です）".into());
        return;
    }
    let mut modes = word_modes_now();
    let Some(held) = modes.iter_mut().find(|held| held.id == mode) else {
        return;
    };
    let Some(group) = held.groups.get_mut(at) else {
        return;
    };
    // **もう入っているなら、足さずに言う。**黙って二度目を入れると、「二重」と
    // 言われるだけの語が増える。
    if group
        .words
        .iter()
        .any(|held| held.eq_ignore_ascii_case(word))
    {
        let told = format!("「{word}」は{}にもう入っています", group.name);
        window.set_render_status(told.into());
        return;
    }
    group.words.push(word.to_owned());
    let name = group.name.clone();
    hold_word_modes(window, live, modes, true);
    window.set_render_status(format!("「{word}」を{name}へ足しました").into());
}

/// 語群から語を1つ落とす（要件 7.9）。
fn remove_word_from_group(window: &AppWindow, live: &Live, mode: usize, at: usize, word: &str) {
    let mut modes = word_modes_now();
    let Some(group) = modes.get_mut(mode).and_then(|held| held.groups.get_mut(at)) else {
        return;
    };
    group.words.retain(|held| held != word);
    hold_word_modes(window, live, modes, true);
}

/// 語群を1行1語で書き出す（要件 7.9）。
///
/// **控えを取るため、そして他の道具と行き来するため**にある。独自の形式にしないのが
/// 値打ちなので、読むほうと同じ形である。
fn export_word_group(window: &AppWindow, mode: usize, at: usize) {
    let modes = word_modes_now();
    let Some(group) = modes.get(mode).and_then(|held| held.groups.get(at)) else {
        return;
    };
    let owner = ime::window_handle(window);
    let suggested = format!("{}.txt", group.name);
    let Some(target) = file_dialog::save_document_as(owner, &suggested) else {
        return;
    };
    // **覚え書きごと書き出す。**取り込みの形も「1行1語、`#`は覚え書き」なので
    // （単語チェックモード要件 5.1）、書き出して直して取り込む道で並べ方が消えない。
    let mut out = String::with_capacity(group.words.len() * 8);
    for word in &group.words {
        out.push_str(word);
        out.push('\n');
    }
    match file_io::write_atomically(&target, out.as_bytes()) {
        Ok(_) => {
            let told = format!("{}語を書き出しました", group.word_count());
            window.set_render_status(told.into());
        }
        Err(error) => window.set_render_status(format!("書き出せません: {error}").into()),
    }
}

/// 取り込み元のファイルを読む（要件 7.9）。
///
/// **読めなければ`None`。**取り込みは書き手が起こす操作なので、黙って空の語群を
/// 作るより、読めなかったと言うほうがよい。
fn read_word_source(path: &Path) -> Option<Vec<String>> {
    let loaded = file_io::read(path, word_marks::MAX_WORDS_PER_GROUP * 64).ok()?;
    Some(word_marks::read_word_file(&loaded.text))
}

fn settings_values(window: &AppWindow) -> Vec<(String, String)> {
    let mut values = Vec::new();
    // **一覧が先、既定が後。**読むほうは二度なめるので順に頼ってはいないが、
    // 人が開いたときに「何があるか」を見てから「どれが既定か」を読むほうが
    // 素直である。番号は書き出すたびに振り直すので、真ん中を消した一覧は
    // 穴のあいた一覧ではなく、二つの一覧になる。
    for (at, shell) in configured_shells(window).iter().enumerate() {
        values.push((format!("{SHELL_SETTING}.{at}"), shell.written()));
    }
    // 追加要件 2026-09-08: **既定は名前で書く。**番号は一覧が変われば別の
    // シェルを指し、その一覧はいまや書き手のものである。
    values.push((
        DEFAULT_SHELL_SETTING.to_owned(),
        offered_shells(window)
            .get(window.get_default_shell().max(0) as usize)
            .map(|shell| shell.name.clone())
            .unwrap_or_default(),
    ));
    values.push((
        AUTOSAVE_SETTING.to_owned(),
        i32::from(window.get_autosave()).to_string(),
    ));
    values.push((
        COUNT_RUBY_SETTING.to_owned(),
        i32::from(window.get_count_ruby()).to_string(),
    ));
    values.push((
        TERMINAL_PAPER_SETTING.to_owned(),
        hex_colour(window.get_terminal_paper()),
    ));
    values.push((
        TERMINAL_INK_SETTING.to_owned(),
        hex_colour(window.get_terminal_ink()),
    ));
    values.push((
        TERMINAL_FONT_SETTING.to_owned(),
        window.get_terminal_font().to_string(),
    ));
    values.push((
        TERMINAL_SIZE_SETTING.to_owned(),
        window.get_terminal_size().to_string(),
    ));
    let palette = window.get_palette();
    let fonts = window.get_sheet_fonts();
    for sheet in 0..2 {
        let mark = if sheet == 0 { "h" } else { "v" };
        for setting in Setting::all() {
            let name = format!("{mark}.{}", setting.name());
            values.push((name, setting.read(window, sheet).to_string()));
        }
        for slot in 0..SHEET_COLOURS {
            let colour = palette
                .row_data(colour_row(sheet, slot))
                .unwrap_or_default();
            values.push((format!("{mark}.{}", colour_name(slot)), hex_colour(colour)));
        }
        for slot in 0..SHEET_FONTS {
            let family = fonts.row_data(font_row(sheet, slot)).unwrap_or_default();
            values.push((format!("{mark}.{}", font_name(slot)), family.to_string()));
        }
    }
    values
}

/// Put what a settings file holds in front of the writer (要件 9).
///
/// **Anything unrecognisable is left alone rather than argued with**: a name
/// this build does not know, a number that is not a number, a colour that is
/// not six digits. What is not read keeps its default, which is what a fresh
/// install has anyway. Numbers are held to the range their buttons hold them
/// to, so a hand-edited file cannot set a body size nothing can read.
fn apply_settings(
    window: &AppWindow,
    numbers: &VecModel<i32>,
    palette: &VecModel<Color>,
    fonts: &VecModel<SharedString>,
    values: &[(String, String)],
) {
    // 追加要件 2026-09-08: **一覧が先、二度なめてでも。**既定はその一覧の中の
    // 一つを名前で指すので、指される側が揃っていなければ答えようがない——
    // そして手で書き換えられた設定ファイルの並び順は、誰も約束していない。
    let read: Vec<TerminalShell> = values
        .iter()
        .filter(|(name, _)| name.starts_with(SHELL_SETTING))
        .filter_map(|(_, value)| TerminalShell::read(value))
        .collect();
    // **一件も読めなければ組み込みのまま。**シェルの無い編集器は、端末の
    // タブを開く道がどこにも無い編集器である。
    if !read.is_empty() {
        hold_shells(window, &read);
    }
    for (written, value) in values {
        // 追加要件 2026-09-07: the default shell, which belongs to neither
        // sheet. **Held to the shells this machine offers**, so a file naming
        // one it does not have opens the first one it does.
        if written == DEFAULT_SHELL_SETTING {
            let offered = offered_shells(window);
            let most = offered.len().saturating_sub(1) as i32;
            // 番号で書かれた古い設定ファイルもそのまま読める（2026-09-08 まで
            // はそちらだった）。
            let at = match value.trim().parse::<i32>() {
                Ok(number) => number,
                Err(_) => offered
                    .iter()
                    .position(|shell| shell.name == value.trim())
                    .map_or(0, |at| at as i32),
            };
            window.set_default_shell(at.clamp(0, most));
            continue;
        }
        // 追加要件 2026-09-08: **`0`だけがOff。**読めない値は書いた覚えの
        // 無い値なので、約束しているほう（要件 8.1 を守る側）へ倒す。
        if written == AUTOSAVE_SETTING {
            window.set_autosave(value.trim() != "0");
            continue;
        }
        // 要件 7.8: **`1`だけがOn。**初期値は数えないほうなので、読めない値は
        // そちらへ倒す。
        if written == COUNT_RUBY_SETTING {
            window.set_count_ruby(value.trim() == "1");
            continue;
        }
        // 追加要件 2026-09-08: 端末の見た目（要件 6.8）。読めない値は既定のまま
        // ——手で書いた設定ファイルが、端末を読めない色にできてはならない。
        if written == TERMINAL_PAPER_SETTING {
            if let Some(rgb) = parse_hex_colour(value) {
                window.set_terminal_paper(slint_colour(rgb));
            }
            continue;
        }
        if written == TERMINAL_INK_SETTING {
            if let Some(rgb) = parse_hex_colour(value) {
                window.set_terminal_ink(slint_colour(rgb));
            }
            continue;
        }
        if written == TERMINAL_FONT_SETTING {
            if !value.trim().is_empty() {
                window.set_terminal_font(value.into());
            }
            continue;
        }
        if written == TERMINAL_SIZE_SETTING {
            if let Ok(size) = value.trim().parse::<i32>() {
                let (low, high) = TERMINAL_SIZE_RANGE;
                window.set_terminal_size(size.clamp(low, high));
            }
            continue;
        }
        let (only, name) = sheet_prefix(written);
        // No prefix is a name from before the sheets, and it meant both.
        let sheets: &[usize] = match only {
            Some(0) => &[0],
            Some(_) => &[1],
            None => &[0, 1],
        };
        if let Some(setting) = Setting::from_name(name) {
            let (low, high) = setting.range();
            if let Ok(number) = value.parse::<i32>() {
                for sheet in sheets {
                    setting.write(numbers, *sheet, number.clamp(low, high));
                }
            }
            continue;
        }
        if let Some(slot) = colour_slot(name)
            && let Some(rgb) = parse_hex_colour(value)
        {
            for sheet in sheets {
                set_colour(palette, *sheet, slot, rgb);
            }
            continue;
        }
        // **A family name is taken as written.** Whether the machine has it is
        // DirectWrite's business — it falls back to something readable — and a
        // settings file carried to another machine should not lose the name of
        // a font that is merely not installed here.
        if let Some(slot) = font_slot(name) {
            for sheet in sheets {
                fonts.set_row_data(font_row(*sheet, slot), value.as_str().into());
            }
        }
    }
}

/// Put every sheet back to what a fresh install has (要件 9).
fn reset_settings(
    numbers: &VecModel<i32>,
    palette: &VecModel<Color>,
    fonts: &VecModel<SharedString>,
) {
    for sheet in 0..2 {
        for setting in Setting::all() {
            setting.write(numbers, sheet, setting.default_value());
        }
        for slot in 0..SHEET_COLOURS {
            set_colour(palette, sheet, slot, default_colour(sheet, slot));
        }
        for slot in 0..SHEET_FONTS {
            fonts.set_row_data(font_row(sheet, slot), default_font(slot).into());
        }
    }
}

/// Write the display settings down (要件 9).
///
/// **Called where every change to them ends up.** The relayout they all ask for
/// is already waited out by 200ms, so a run of presses on one button writes the
/// file once rather than once per press.
fn save_settings(window: &AppWindow, cache: &Rc<RefCell<RenderCache>>) {
    let Some(directory) = app_data::app_directory() else {
        return;
    };
    let values = settings_values(window);
    if let Err(error) = app_data::write_settings(&directory, &values) {
        cache
            .borrow_mut()
            .log_diag("spec", &format!("settings not saved error={error}"));
    }
}

/// Ask for a relayout once the controls stop being pressed.
///
/// The caller has already applied the new value, so the toolbar answers the
/// press at once; what waits is the 200ms of re-measuring behind it (6.8).
/// Restarting the timer on each press means a run of them costs one relayout
/// rather than one each — and every relayout but the last was going to be
/// thrown away by the next press regardless.
///
/// The zoom goes through here too. It has always had exactly this cost; the
/// typography controls only made it obvious, because they are the ones a person
/// sweeps through a range looking for a value they like.
/// Magnify one pane, and leave it looking at what it was looking at
/// (要件 9, 8.5).
///
/// **The character at the near edge, asked before the size changes.** Where the
/// document sits is a pixel offset, and every pixel offset means somewhere else
/// once the text is set larger: magnifying a pane by following the scroll bar
/// would carry the writer off the passage they were reading, which is the one
/// thing a zoom must not do. The hold stands until the caret moves, the same
/// rule a restored session and a tab switch use.
///
/// The relayout is scheduled rather than run: a held key or a spun wheel sends
/// these faster than a document can be measured, and only the last one matters.
fn zoom_pane(
    window: &AppWindow,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    timer: &Rc<Timer>,
    id: PaneId,
    percent: i32,
) {
    if id.zoom(window) == zoom_from(percent) {
        return;
    }
    let caret = states.of(id).borrow().caret_source_byte;
    // **A hold already standing is kept rather than taken again.** A spun
    // wheel arrives faster than the 150ms the relayout waits, so by the second
    // notch the engine is measured at a size the pane is no longer set to, and
    // asking it where the near edge is would re-measure the whole document —
    // paying, once per notch, exactly what the wait exists to avoid. The
    // passage to come back to is the one the writer was on when they started
    // spinning, which is what the standing anchor already says.
    let standing = held_view(cache.borrow_mut().pane(id).view.top_anchor, caret);
    let top = match standing {
        Some(byte) => Some(byte),
        None => view_top(window, states, cache, id),
    };
    id.set_zoom(window, percent);
    hold_view(cache, id, top, caret);
    schedule_relayout(window, states, cache, timer);
}

fn schedule_relayout(
    window: &AppWindow,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    timer: &Rc<Timer>,
) {
    let weak = window.as_weak();
    let states = states.clone();
    let cache = cache.clone();
    timer.start(TimerMode::SingleShot, SPEC_SETTLE, move || {
        if let Some(window) = weak.upgrade() {
            // Each pane is asked what it is showing when the timer fires, not
            // when it was set: by then they may be showing something else.
            relayout_panes(&window, &states, &cache);
        }
    });
}

/// Re-measure and redraw whichever panes are on screen.
///
/// Everything that changes how the document is set ends up here: the zoom, and
/// each of the three typography controls. They all invalidate every block
/// measurement in both engines, so there is nothing finer to do than this.
fn relayout_panes(window: &AppWindow, states: &PaneStates, cache: &Rc<RefCell<RenderCache>>) {
    let started = Instant::now();
    // Every anchor goes, whether or not its pane is on screen: the anchor is a
    // coordinate in a layout that is about to stop existing, and a pane that
    // comes back holding one would step the caret to a line by the old
    // measurements.
    for id in PaneId::all(window) {
        states.of(id).borrow_mut().preferred_line = None;
    }
    // The spec decides the layout on every side, so a change re-measures
    // whichever panes are on screen — **each from the document it is showing**,
    // which is not always the same one (要件 7.6). In reverse pane order: each
    // refresh writes the status line, and the horizontal pane's is the one that
    // has always been left standing (`draw_edit` keeps the same rule).
    let mut bytes = 0;
    for id in PaneId::all(window).into_iter().rev() {
        if id.is_shown(window) {
            let document = states.document(id);
            let source = document.text.borrow().clone();
            bytes = source.len();
            refresh_pane_from_state(window, cache, &document, id, &states.of(id), &source);
        }
    }
    // Logged as its own kind of line. A keystroke re-measures one block; this
    // re-measures every block in both panes, so it belongs to a different cost
    // class and averaging it in with the keystrokes would hide both.
    // 要件 9: the settings are the app's and outlive the run. Written here
    // because everything that changes one of them asks for this relayout.
    save_settings(window, cache);
    // The focused pane's spec, because the line is one line. **Every pane's
    // zoom** though: 要件 9 gives each its own, and the wheel magnifies the pane
    // it is over rather than the one holding the keyboard — a line naming one
    // number would leave out the pane that just changed.
    let here = focused_pane(window);
    let typography = pane_typography(window, here);
    let zooms = PaneId::all(window)
        .iter()
        .map(|id| id.zoom(window).to_string())
        .collect::<Vec<String>>()
        .join(",");
    cache.borrow_mut().log_perf(&format!(
        "relayout total={total:.2} zoom={zoom} font={font:.1} \
         space={space:.2} lead={lead:.2} head={head:.2} bytes={bytes}",
        total = elapsed_ms(started),
        zoom = zooms,
        font = typography.font_size,
        space = typography.character_spacing,
        lead = typography.line_spacing,
        head = typography.size_scale(1),
        bytes = bytes,
    ));
}

/// Reveal the Markdown of the caret's line once the caret stops moving.
///
/// Restarting the timer on every move means a held key never pays for it, and a
/// caret that lands somewhere and stays gets the reveal a moment later.
///
/// **It redraws the pane that asked, and no other.** Which pane waits for its
/// caret to settle is decided by the writing direction that pane is in
/// (`PaneId::reveals_while_moving`), not by which of the two it is — so pane 0
/// asks for this whenever its tab is turned vertical. Before the panes were put
/// on one path this only ever ran for pane 1 and named it outright; left that
/// way, a caret moving in pane 0 wrote pane 0's caret and scroll into pane 1 a
/// tenth of a second later, and the writer saw the other pane jump to where
/// they were working.
fn schedule_active_line_reveal(
    timer: &Rc<Timer>,
    window: &AppWindow,
    id: PaneId,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    document: &Rc<OpenDocument>,
) {
    let weak = window.as_weak();
    let state = state.clone();
    let cache = cache.clone();
    let document = document.clone();
    timer.start(TimerMode::SingleShot, REVEAL_SETTLE, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let source = document.text.borrow().clone();
        let revealed = {
            let mut state = state.borrow_mut();
            let Some(caret) = state.caret_source_byte else {
                return;
            };
            let line = source_line_start(&source, caret);
            if state.active_line_start == Some(line) {
                None
            } else {
                state.active_line_start = Some(line);
                Some(line)
            }
        };
        if revealed.is_some() {
            refresh_pane_from_state(&window, &cache, &document, id, &state, &source);
        }
    });
}

/// Lay a pane out again from what its own state holds.
///
/// **This is how a pane is redrawn after something it did not do**: an edit in
/// the other pane, a mode change, a resize, a toolbar command. Nothing here
/// moves a caret — the pane comes back showing what it was already showing.
fn refresh_pane_from_state(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    id: PaneId,
    state: &Rc<RefCell<EditorState>>,
    source: &str,
) {
    let (caret_source_byte, selection, preedit) = {
        let state = state.borrow();
        (
            state.caret_source_byte,
            pane_selection(&state),
            state.preedit.clone(),
        )
    };
    refresh_pane(
        window,
        cache,
        document,
        id,
        source,
        PaneId::revealed_line(id.vertical(window), state, source),
        caret_source_byte,
        selection,
        &preedit,
    );
}

/// Where a line's text and its caret land once a fixed-width box stands in for
/// the marker at its head (要件 7.3.2).
///
/// `across` is the horizontal pane's direction and `down` is the vertical
/// pane's. Both are asked, because a box that only worked one way would be no
/// use here.
fn inline_object_status(name: &str, vertical: bool) -> String {
    let probed = directwrite_probe::probe_inline_object("- 箇条書き", 2, 48.0, vertical);
    let report = match probed {
        Ok(report) => report,
        Err(error) => return format!("inline_object {name} failed error={error}"),
    };
    let directwrite_probe::InlineObjectProbeReport {
        head,
        box_far_side,
        first_text,
        second_text,
        box_rects,
        box_rect_extent,
        metrics_asked,
    } = report;
    format!(
        "inline_object {name} head={head:.1} box_far={box_far_side:.1} \
         first={first_text:.1} second={second_text:.1} rects={box_rects} \
         extent={box_rect_extent:.1} asked={metrics_asked}"
    )
}

fn directwrite_status() -> String {
    match directwrite_probe::probe_vertical_layout("日本語ABC123") {
        Ok(report) => format!(
            "DirectWrite縦書き: OK / layout {:.0}×{:.0}px / caret Δy {:.1}px",
            report.layout_width,
            report.layout_height,
            report.second_caret_y - report.first_caret_y
        ),
        Err(error) => format!("DirectWrite縦書き: NG / {error}"),
    }
}

/// Which pane, for everything that is the same either way.
///
/// **The whole of the difference between the two panes is behind this.** They
/// were written as two of everything, and the same fault then had to be found
/// twice: 6.15 and 6.16 are each one pane repaired and the other left as it
/// was. What differs is which axis the document flows along and which Slint
/// properties the pane reads and writes, and both are here.
///
/// This is also the layer that went first. Now that the panes are a model
/// (ペイン分割設計 5), the accessors below are reads and writes of one row of
/// it, and everything that has one of something per pane — the states, the
/// views a tab keeps, the model itself — is indexed by [`PaneId::index`].
///
/// **A number, and nothing else** (要件 6.3, 2026-09-06). It was two named
/// panes, and every name in it was a second opinion about something the layout
/// tree already knew: which pane is on the right, which of them is "the other
/// one", which draws downward. The tree says where a pane is; the tab in front
/// of it says which way it draws; and this says only which pane it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PaneId(u32);

impl PaneId {
    /// The pane an editor with no split has. **Always exists**: 要件 6.3 says
    /// the editing area is one or more panes, so there is no arrangement
    /// without this one in it.
    const FIRST: PaneId = PaneId(0);

    /// Every pane that exists, in order.
    ///
    /// **The window's own model is the count.** A pane is a row of it — that is
    /// what makes it a thing on screen — so nothing else can hold a second
    /// opinion about how many there are. Everything with one of something per
    /// pane is built by mapping over this.
    fn all(window: &AppWindow) -> Vec<PaneId> {
        (0..window.get_panes().row_count() as u32)
            .map(PaneId)
            .collect()
    }

    /// How many panes exist.
    fn count(window: &AppWindow) -> usize {
        window.get_panes().row_count()
    }

    /// Every pane but this one.
    fn others(self, window: &AppWindow) -> Vec<PaneId> {
        Self::all(window)
            .into_iter()
            .filter(|id| *id != self)
            .collect()
    }

    /// Which way this pane is drawing now.
    ///
    /// From the pane's row, which follows the tab in front of it. Everything
    /// about a flow axis, a scroll axis or the shape of a caret asks this.
    fn vertical(self, window: &AppWindow) -> bool {
        self.screen(window).vertical
    }

    /// The pane's number, left to right across the window. **The same numbering
    /// as `editor-mode`**, so the two cannot drift apart, and the row this pane
    /// will be once the panes are a model (ペイン分割設計 5).
    fn index(self) -> i32 {
        self.0 as i32
    }

    /// The pane a number from the UI names.
    ///
    /// Anything unexpected is the horizontal pane rather than a panic: this
    /// number arrives with a keystroke, and a keystroke must not be able to
    /// stop the editor.
    fn from_index(index: i32) -> Self {
        PaneId(index.max(0) as u32)
    }

    /// This pane's row of the window's pane model.
    ///
    /// An empty row rather than a panic if it is not there: the row is missing
    /// only before the model is published, and nothing a pane does afterwards
    /// may be able to stop the editor.
    fn screen(self, window: &AppWindow) -> PaneScreen {
        window
            .get_panes()
            .row_data(self.index() as usize)
            .unwrap_or_default()
    }

    /// Change part of this pane's row.
    ///
    /// **Read, change, write back, every time.** Some of the fields are written
    /// by the pane itself — its scroll, the size it shows and its IME field —
    /// straight into this model, so a row read any earlier than the write is
    /// not the row that is on screen.
    fn update_screen(self, window: &AppWindow, edit: impl FnOnce(&mut PaneScreen)) {
        let panes = window.get_panes();
        let row = self.index() as usize;
        let mut screen = panes.row_data(row).unwrap_or_default();
        edit(&mut screen);
        panes.set_row_data(row, screen);
    }

    /// This pane's row before anything has been laid out.
    ///
    /// The sizes are the same fallbacks the layout uses until the pane reports
    /// its own, so the first refresh works against one set of numbers whether
    /// or not the pane exists yet.
    ///
    /// **`vertical` and `preview` are given rather than worked out** (要件 7.2,
    /// 2026-09-06). They used to come from which of the two panes this was,
    /// which was the last place a pane's number decided how it draws. A pane
    /// draws the way the tab in front of it says, and a pane made by a split
    /// opens with a copy of the tab it was split from (要件 6.4) — so what a
    /// new row starts as is the splitting pane's business, not its number's.
    fn initial_screen(self, vertical: bool, preview: bool) -> PaneScreen {
        PaneScreen {
            id: self.index(),
            vertical,
            preview,
            zoom: ZOOM_DEFAULT,
            content_width: 640,
            content_height: 520,
            shown_width: 640.0,
            shown_height: 520.0,
            caret_width: 1.0,
            caret_height: 22.0,
            ime_anchor_width: 1.0,
            ime_anchor_height: 22.0,
            ..PaneScreen::default()
        }
    }

    /// The height the pane reports, held to the height it was given.
    fn shown_height(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        bounded_extent(screen.shown_height, screen.height)
    }

    /// The width the pane reports, held to the width it was given.
    ///
    /// **A pane's report of its own size is right except in one moment**: the
    /// one between a change to the layout and the layout running. A pane is not
    /// re-created when the area is divided or a boundary moves, so no `init`
    /// covers it (6.11) and the width is corrected by `changed visible-width` —
    /// which arrives *after* the refresh the change itself runs. That refresh
    /// reads the width the pane had before (6.19), and erring high there
    /// truncates the scroll a caret may ask for and jumps the document (6.16).
    ///
    /// The tree knows the answer exactly, which is what makes this a `min` and
    /// not a guess: the pane cannot be showing more than it was given.
    fn shown_width(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        bounded_extent(screen.shown_width, screen.width)
    }

    /// How far the tree let this pane reach along its flow.
    ///
    /// **What the pane was given, not what it says it is showing.** The two
    /// differ by the pane's own padding, and for a frame after the area is
    /// divided they differ by much more (6.19).
    fn given_along_flow(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if screen.vertical {
            screen.width
        } else {
            screen.height
        }
    }

    /// How much of the flow the pane reports it is showing.
    fn shown_along_flow(self, window: &AppWindow) -> f32 {
        if self.vertical(window) {
            self.shown_width(window)
        } else {
            self.shown_height(window)
        }
    }

    /// How far the pane reaches across the flow, which is how long a line in it
    /// may be.
    fn shown_across_flow(self, window: &AppWindow) -> f32 {
        if self.vertical(window) {
            self.shown_height(window)
        } else {
            self.shown_width(window)
        }
    }

    /// What this pane is called in a message.
    fn label(self, window: &AppWindow) -> &'static str {
        if self.vertical(window) {
            "縦書き"
        } else {
            "横書き"
        }
    }

    /// How the logs name this pane's lines.
    ///
    /// **The pane's number** (2026-09-06). The names used to be `refresh` and
    /// `horizontal`, from the two panes the editor had; with a pane list there
    /// is no pair to name, and a line has to say which of N it came from.
    /// README carries the new names.
    fn log_name(self) -> String {
        format!("pane{}", self.0)
    }

    /// How the diagnostic log tells this pane's lines from the others'.
    fn diag_suffix(self) -> String {
        format!("p{}", self.0)
    }

    /// How long a line in this pane may be (要件 9): the pane's own extent
    /// across the flow — a vertical pane sets its columns into its height, a
    /// horizontal one its lines into its width — or the width the writer named,
    /// or nothing at all.
    ///
    /// **The pane is not a bound on the other two.** A width the pane cannot
    /// show is shown by scrolling across it, because a line length that the
    /// pane silently overruled would be a setting nobody could check.
    fn line_fit(self, window: &AppWindow, typography: &Typography) -> LineFit {
        let vertical = self.vertical(window);
        let sheet = usize::from(vertical);
        match Setting::WrapMode.read(window, sheet) {
            WRAP_NEVER => LineFit::Free,
            WRAP_CHARACTERS => {
                let asked = Setting::WrapChars.read(window, sheet).max(1) as u32;
                LineFit::Extent(text_blocks::line_extent_for_cells(asked, typography))
            }
            _ => {
                let across = self.shown_across_flow(window);
                LineFit::Extent(if vertical {
                    usable_preview_height(across)
                } else {
                    usable_horizontal_width(across)
                })
            }
        }
    }

    /// How far the laid-out document reaches across the flow, as the pane was
    /// last told (`set_content_size`).
    ///
    /// **The number the sheet is drawn at**, read back rather than worked out
    /// again, so that chasing the caret across cannot aim at a rectangle other
    /// than the one the reader is looking at.
    fn content_across(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if self.vertical(window) {
            screen.content_height as f32
        } else {
            screen.content_width as f32
        }
    }

    /// The scroll across the flow — the axis the pane only has to move when the
    /// line is longer than the pane is wide (要件 9).
    fn scroll_across(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if self.vertical(window) {
            screen.scroll_y
        } else {
            screen.scroll_x
        }
    }

    fn set_scroll_across(self, window: &AppWindow, offset: f32) {
        if (offset - self.scroll_across(window)).abs() < 0.5 {
            return;
        }
        let vertical = self.vertical(window);
        self.update_screen(window, |screen| {
            if vertical {
                screen.scroll_y = offset;
            } else {
                screen.scroll_x = offset;
            }
            // The same count as the other axis: one move is one event, and the
            // pane follows both from it.
            screen.scroll_generation += 1;
        });
    }

    /// How much of the flow the pane shows, **erring high**. This decides which
    /// tiles are cut, and erring high there costs a tile while erring low leaves
    /// the reader looking at blank paper (6.16).
    fn shown_flow(self, window: &AppWindow) -> f32 {
        // **This pane's own area**, and no other's. Erring high is the whole of
        // it: a pane cannot show more of the flow than it was given, and the
        // padding it loses inside costs a tile at most.
        let given = self.given_along_flow(window);
        self.shown_along_flow(window).max(given).max(320.0)
    }

    /// How much of the flow the pane really shows, **erring low**. The scroll a
    /// caret asks for is clamped against this, so a viewport reported larger
    /// than it is stops the document before its last line (6.16).
    fn viewport_flow(self, window: &AppWindow) -> f32 {
        // The pane's own report, which is smaller than what it was given by the
        // padding around it. Before it has reported anything, what it was given
        // is the only number there is — and erring *low* is the rule here, so a
        // pane with neither is left with nothing to scroll.
        let reported = self.shown_along_flow(window);
        if reported > 0.0 {
            reported
        } else {
            self.given_along_flow(window)
        }
    }

    fn scroll(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if self.vertical(window) {
            screen.scroll_x
        } else {
            screen.scroll_y
        }
    }

    /// Move the view along the flow, and tell the pane to follow (要件 8.5).
    ///
    /// **Telling it is a separate act from writing it down.** `Flickable` drops
    /// the binding that would have carried the value the first time the writer
    /// presses inside it (`items/flickable.rs`), so what actually moves the view
    /// is the count changing — and a count raised for a value the pane already
    /// has would send it to where it is on every notch of the wheel.
    fn set_scroll(self, window: &AppWindow, offset: f32) {
        if (offset - self.scroll(window)).abs() < 0.5 {
            return;
        }
        self.record_scroll(window, offset);
        self.update_screen(window, |screen| screen.scroll_generation += 1);
    }

    /// Write down where the pane says it has gone. **The other direction**: the
    /// pane moved itself, and recording that is not a command.
    fn record_scroll(self, window: &AppWindow, offset: f32) {
        let vertical = self.vertical(window);
        self.update_screen(window, |screen| {
            if vertical {
                screen.scroll_x = offset;
            } else {
                screen.scroll_y = offset;
            }
        });
    }

    /// The global flow range the pane shows. Everything that clips work to the
    /// viewport goes through here, so the bounds cannot drift apart.
    fn flow_range(self, window: &AppWindow, total_flow: f32) -> (f32, f32) {
        visible_flow_range(self.scroll(window), self.shown_flow(window), total_flow)
    }

    /// Whether this pane is on screen at all.
    ///
    /// **Having an area is what being on screen means** (要件 6.4): the layout
    /// tree hands out the editing area, and a pane it does not name gets none.
    fn is_shown(self, window: &AppWindow) -> bool {
        self.screen(window).width > 0.0
    }

    /// Whether this pane shows the formatted text rather than the source, which
    /// is one half of the four modes (要件 7.2).
    ///
    /// In the pane's row rather than on the window, because it belongs to the
    /// tab in front of the pane — and so does the button that changes it.
    fn shows_preview(self, window: &AppWindow) -> bool {
        self.screen(window).preview
    }

    fn set_shows_preview(self, window: &AppWindow, shows: bool) {
        self.update_screen(window, |screen| screen.preview = shows);
    }

    /// Whether the tab in front of this pane is a shell (追加要件 Terminal).
    ///
    /// **The same flag the pane itself reads**, so Rust and the screen cannot
    /// disagree about what is in front. Asked wherever a command means one
    /// thing over text and another over a shell — or nothing at all.
    fn shows_terminal(self, window: &AppWindow) -> bool {
        self.screen(window).terminal
    }

    /// How much this pane magnifies the text (要件 9).
    ///
    /// In the pane's row rather than on the window, because 要件 9 asks for it
    /// per pane: two views of one document are two distances to read it from.
    /// **Everything that sets a pane's text asks this rather than being handed
    /// it**, so no path can lay one pane out at another's size.
    ///
    /// A row that has never been written says 0, which [`zoom_from`] reads as
    /// no zoom at all rather than as 0%.
    fn zoom(self, window: &AppWindow) -> i32 {
        zoom_from(self.screen(window).zoom)
    }

    fn set_zoom(self, window: &AppWindow, percent: i32) {
        let held = zoom_from(percent);
        self.update_screen(window, |screen| screen.zoom = held);
    }

    /// Where the caret was last placed, along the flow.
    fn caret_flow(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if self.vertical(window) {
            screen.caret_x
        } else {
            screen.caret_y
        }
    }

    /// Where the IME was told to open its candidate list, along the flow.
    fn ime_anchor_flow(self, window: &AppWindow) -> f32 {
        let screen = self.screen(window);
        if self.vertical(window) {
            screen.ime_anchor_x
        } else {
            screen.ime_anchor_y
        }
    }

    /// The laid-out document's size: how far it reaches along the flow, and how
    /// far across it.
    fn set_content_size(self, window: &AppWindow, flow: u32, line_extent: u32) {
        let flow = flow as i32;
        let line_extent = line_extent as i32;
        let vertical = self.vertical(window);
        self.update_screen(window, |screen| {
            if vertical {
                screen.content_width = flow;
                screen.content_height = line_extent;
            } else {
                screen.content_width = line_extent;
                screen.content_height = flow;
            }
        });
    }

    /// Where one rendered slice sits. A vertical tile is as tall as the pane and
    /// stacked along x; a horizontal one spans the pane and is stacked along y.
    fn tile(vertical: bool, span: TileSpan, source: Image) -> PreviewTile {
        let flow_start = span.flow_start as i32;
        let flow_size = span.flow_size as i32;
        // 要件 9: where this slice sits across the page. A page that fits its
        // pane is one slice starting at nothing, which is what every tile was.
        let cross_start = span.cross_start as i32;
        let cross_size = span.cross_size as i32;
        if vertical {
            PreviewTile {
                x: flow_start,
                y: cross_start,
                width: flow_size,
                height: cross_size,
                source,
            }
        } else {
            PreviewTile {
                x: cross_start,
                y: flow_start,
                width: cross_size,
                height: flow_size,
                source,
            }
        }
    }

    fn set_tiles(self, window: &AppWindow, tiles: Vec<PreviewTile>) {
        let model = ModelRc::new(VecModel::from(tiles));
        self.update_screen(window, |screen| screen.tiles = model);
    }

    /// The images of the strip along the foot of the pane (追加要件 Terminal).
    fn set_below_tiles(self, window: &AppWindow, tiles: Vec<PreviewTile>) {
        let model = ModelRc::new(VecModel::from(tiles));
        self.update_screen(window, |screen| screen.below_tiles = model);
    }

    /// What the strip is and how tall it stands. **0 is nothing, 1 a shell
    /// under a document, 2 a draft under a shell** — the same numbering the
    /// pane reads.
    fn set_below(self, window: &AppWindow, kind: i32, height: f32) {
        self.update_screen(window, |screen| {
            screen.below_kind = kind;
            screen.below_height = height;
        });
    }

    /// E1: 範囲内検索の範囲の矩形。**いちばん薄く敷かれる。**
    fn set_scope(self, window: &AppWindow, rects: &[SelectionRect]) {
        let model = ModelRc::new(VecModel::from(preview_rects(rects)));
        self.update_screen(window, |screen| screen.scope_rects = model);
    }

    /// E1: 見えている一致の矩形。**選択と同じ形で渡し、描く側が薄く敷く。**
    fn set_matches(self, window: &AppWindow, rects: &[SelectionRect]) {
        let model = ModelRc::new(VecModel::from(preview_rects(rects)));
        self.update_screen(window, |screen| screen.match_rects = model);
    }

    fn set_selection(self, window: &AppWindow, rects: &[SelectionRect]) {
        let model = ModelRc::new(VecModel::from(preview_rects(rects)));
        self.update_screen(window, |screen| screen.selection_rects = model);
    }

    /// The caret lies across the writing direction, and each pane reads the one
    /// measurement that is not its own line extent. Both are written: the row
    /// carries them, and which one is read is the pane's business.
    fn set_caret(self, window: &AppWindow, caret: Option<&CaretGeometry>) {
        self.update_screen(window, |screen| {
            screen.caret_visible = caret.is_some();
            if let Some(caret) = caret {
                screen.caret_x = caret.x;
                screen.caret_y = caret.y;
                screen.caret_width = caret.width;
                screen.caret_height = caret.height;
            }
        });
    }

    /// What the pane's hidden IME field holds. Cleared whenever the caret moves
    /// somewhere the field did not put it.
    fn set_ime_buffer(self, window: &AppWindow, text: &str) {
        self.update_screen(window, |screen| screen.ime_buffer = text.into());
    }

    fn set_ime_anchor(self, window: &AppWindow, x: f32, y: f32, caret: &CaretGeometry) {
        self.update_screen(window, |screen| {
            screen.ime_anchor_x = x;
            screen.ime_anchor_y = y;
            screen.ime_anchor_width = caret.width;
            screen.ime_anchor_height = caret.height;
        });
    }

    /// This pane's caret, as a position in the document.
    ///
    /// Rounded down to a character start. The other pane's edits can leave a
    /// stored byte inside a character (6.7), and an unrounded one is what every
    /// slice downstream panics on.
    fn caret_byte(self, state: &Rc<RefCell<EditorState>>, source: &str) -> usize {
        let byte = state.borrow().caret_source_byte.unwrap_or(source.len());
        floor_char_boundary(source, byte)
    }

    /// Which line's Markdown this pane is showing, if any.
    ///
    /// **Every position asked of a pane has to be asked against this line.**
    /// The preview hides the markers of every other line, so a lookup made
    /// against a different one comes out of a text nobody is looking at (6.15).
    ///
    /// The vertical pane keeps it: it lags the caret on purpose, because
    /// revealing a line rewrites its block (`schedule_active_line_reveal`). The
    /// horizontal pane reveals the caret's line as the caret arrives and so has
    /// nothing to keep. Both answer `None` until a caret has been placed, which
    /// is what the panes are showing then.
    fn revealed_line(
        vertical: bool,
        state: &Rc<RefCell<EditorState>>,
        source: &str,
    ) -> Option<usize> {
        if vertical {
            state.borrow().active_line_start
        } else {
            horizontal_active_line(state, source)
        }
    }

    /// Whether a moving caret reveals its new line as it arrives, or only once
    /// it stops.
    ///
    /// Revealing rewrites that line's block, which redraws every tile the block
    /// touches. A vertical block is a whole column, and paying for one per key
    /// repeat is what made a held arrow key stutter, so there the reveal waits
    /// for the caret to settle.
    fn reveals_while_moving(vertical: bool) -> bool {
        !vertical
    }

    /// The caret coordinate the up and down keys hold on to across a run of
    /// moves, so that passing a short line does not pull the caret in.
    ///
    /// It lies across the flow — a y where the text runs down the page, an x
    /// where it runs across — which is why it means nothing in the other pane
    /// (`carry_caret_between_panes`).
    fn line_anchor(vertical: bool, at: &CaretGeometry) -> f32 {
        if vertical { at.y } else { at.x }
    }

    /// Draw an edit made in this pane: here, and in the other pane showing the
    /// same document (要件 7.6).
    ///
    /// **The order differs between the panes and is not free to choose.** The
    /// vertical pane pushes first because the refresh that follows is what
    /// reports the push's cost in the performance log; the horizontal pane
    /// refreshes first because the push back writes the status line when the
    /// vertical pane is hidden, and that has to be the line left standing.
    fn draw_edit(
        self,
        window: &AppWindow,
        states: &PaneStates,
        cache: &Rc<RefCell<RenderCache>>,
        document: &OpenDocument,
        source: &str,
        caret: usize,
        change: Change,
    ) {
        // **Only a pane showing this document follows the edit** (要件 7.6).
        // Another pane may be looking at another file, where these positions
        // mean nothing (6.7) and this text is not the text it is drawing.
        //
        // **Every one of them, not "the other one"** (2026-09-06). With two
        // panes there was one candidate and the question never had to be asked
        // of a list; a document open in three panes has to move in all three.
        //
        // **Their carets follow the edit whether or not anything is drawn
        // now.** A position is about the text, and the text has changed;
        // holding this back with the drawing would leave those carets pointing
        // at where the text used to be.
        for other in self.followers(window, states) {
            carry_state_across(&states.of(other), change, source);
        }
        let owed = {
            let mut borrowed = cache.borrow_mut();
            let pace = borrowed.pace_of(self);
            pace.held += 1;
            pace.owed()
        };
        if !owed.is_zero() {
            // The last draw cost real time, so this edit joins the ones already
            // waiting and they are drawn together (`EditPace`).
            schedule_catch_up(window, states, cache, self, owed);
            return;
        }
        let started = Instant::now();
        self.draw_both(window, states, cache, document, source, Some(caret));
        let took = elapsed_ms(started);
        cache.borrow_mut().pace_of(self).drew(took);
    }

    /// This pane and, if it is showing the same document, the other one.
    ///
    /// **The order differs between the panes and is not free to choose** — see
    /// [`PaneId::draw_edit`]. `caret` is where the edit left it, and `None`
    /// means the panes come back showing what their own state holds, which is
    /// what a catch-up draw wants.
    fn draw_both(
        self,
        window: &AppWindow,
        states: &PaneStates,
        cache: &Rc<RefCell<RenderCache>>,
        document: &OpenDocument,
        source: &str,
        caret: Option<usize>,
    ) {
        // **The followers first, and the pane that was typed in last.**
        // A refresh writes the status line, and the line left standing has to
        // be the one about the pane the writer is in. The rule used to be
        // "the right-hand pane goes last", which was the same rule while there
        // were two panes and one of them always had the keyboard.
        for other in self.followers(window, states) {
            draw_followed_edit(window, other, &states.of(other), cache, document, source);
        }
        match caret {
            Some(caret) => refresh_pane(
                window,
                cache,
                document,
                self,
                source,
                Some(source_line_start(source, caret)),
                Some(caret),
                PaneSelection::default(),
                "",
            ),
            None => {
                refresh_pane_from_state(window, cache, document, self, &states.of(self), source)
            }
        }
    }

    /// Every other pane showing the same document (要件 7.6).
    fn followers(self, window: &AppWindow, states: &PaneStates) -> Vec<PaneId> {
        self.others(window)
            .into_iter()
            .filter(|other| states.same_document(self, *other))
            .collect()
    }
}

/// Draw the edits a held key has been leaving undrawn (`EditPace`).
///
/// **Started once and left alone.** Putting the timer off again on the next
/// keystroke would be a debounce, and a debounce never fires while the key is
/// still down — which is the case this exists for.
fn schedule_catch_up(
    window: &AppWindow,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    owed: Duration,
) {
    {
        let mut borrowed = cache.borrow_mut();
        let pace = borrowed.pace_of(id);
        if pace.waiting {
            return;
        }
        pace.waiting = true;
    }
    let weak = window.as_weak();
    let states = states.clone();
    let cache = cache.clone();
    let timer = cache.borrow_mut().pace_of(id).timer.clone();
    timer.start(TimerMode::SingleShot, owed, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // **Whatever the pane is showing now**, which after a tab change is not
        // the document the keystrokes went into.
        let document = states.document(id);
        let source = document.text.borrow().clone();
        let started = Instant::now();
        id.draw_both(&window, &states, &cache, &document, &source, None);
        let took = elapsed_ms(started);
        cache.borrow_mut().pace_of(id).drew(took);
    });
}

impl RenderCache {
    /// The pane an id names. **The only place the two are told apart by
    /// anything other than a [`PaneId`].**
    fn pane(&mut self, id: PaneId) -> &mut Pane {
        let at = (id.index() as usize).min(self.panes.len().saturating_sub(1));
        &mut self.panes[at]
    }

    /// How this pane is keeping up with a run of keystrokes (`EditPace`).
    ///
    /// **The last pane's rather than a panic** for a number that names nothing,
    /// like every other list indexed by pane: a stale number arrives with a
    /// keystroke and must not be able to stop the editor.
    fn pace_of(&mut self, id: PaneId) -> &mut EditPace {
        let at = (id.index() as usize).min(self.pace.len().saturating_sub(1));
        &mut self.pace[at]
    }

    /// Make room for a pane, at the end, which is the number a split hands out.
    fn add_pane(&mut self, mode: WritingMode) {
        self.panes.push(Pane::new(mode));
        self.pace.push(EditPace::default());
    }

    /// Take a pane out, closing the numbering behind it.
    fn remove_pane(&mut self, id: PaneId) {
        let at = id.index() as usize;
        if at < self.panes.len() && self.panes.len() > 1 {
            self.panes.remove(at);
            self.pace.remove(at);
        }
    }

    /// Render the tiles the viewport needs and drop the ones it no longer does.
    ///
    /// A tile is a slice of one block, so this cost tracks the viewport, not the
    /// document, and an edit only invalidates the block it changed.
    ///
    /// This was deliberately two functions, one per pane: the version shared
    /// between them read worse than the duplication, because every line had to
    /// say which pane it meant. What changed is that the difference now has a
    /// name. With the axis and the properties behind [`PaneId`], what is left
    /// here is the same arithmetic either way.
    fn refresh_pane_tiles(
        &mut self,
        window: &AppWindow,
        id: PaneId,
        prefetch: u32,
    ) -> windows::core::Result<(usize, usize, usize, usize, usize)> {
        let scroll = id.scroll(window);
        let shown_flow = id.shown_flow(window);
        // 要件 9: and where the pane is looking across the flow, which is only
        // ever anywhere but the near edge when the line is longer than the pane.
        let scroll_across = id.scroll_across(window);
        let shown_across = id.shown_across_flow(window);
        // Which way the tiles stack, asked before the cache is borrowed.
        let vertical = id.vertical(window);
        // Taken apart so the engine and the images can be held at once: reaching
        // through `self` for each of them would borrow the whole cache.
        let Pane { graphics, view, .. } = self.pane(id);
        let PaneGraphics {
            engine,
            tiles: images,
            spare,
            uploaded_bytes,
        } = graphics;
        if engine.total_flow_size() == 0 {
            return Ok((0, 0, 0, 0, 0));
        }
        // Tiles are cut out of the blocks the viewport crosses. Their size along
        // the flow tracks the pane's extent across it, so a taller window makes
        // tiles narrower rather than making each one costlier to rasterize.
        let desired =
            engine.visible_tiles(scroll, shown_flow, prefetch, scroll_across, shown_across);

        // Keyed by the fingerprint, never by position. The fingerprint names one
        // block's text at one slice of it, so a tile the layout moved is found
        // again unchanged, and two blocks with the same text share one image.
        let preedit = view.preedit_range;
        let keyed = desired
            .iter()
            .map(|span| (*span, engine.tile_signature(*span, preedit)))
            .collect::<Vec<_>>();
        let mut missing: Vec<(TileSpan, u64)> = Vec::new();
        for (span, signature) in &keyed {
            let already = images.contains_key(signature)
                || missing.iter().any(|(_, queued)| queued == signature);
            if !already {
                missing.push((*span, *signature));
            }
        }
        let rendered = missing.len();
        let mut reused = 0_usize;

        if !missing.is_empty() {
            let spans = missing.iter().map(|(span, _)| *span).collect::<Vec<_>>();
            let mut drawn = TileImages {
                spare,
                drawing: None,
                produced: Vec::with_capacity(spans.len()),
                uploaded: 0,
                reused: 0,
            };
            engine.render_tiles(&spans, preedit, &mut drawn)?;
            *uploaded_bytes += drawn.uploaded;
            reused = drawn.reused;
            for (span, image, pixels) in drawn.produced {
                let queued = missing.iter().find(|(other, _)| *other == span);
                let Some((_, signature)) = queued else {
                    continue;
                };
                images.insert(
                    *signature,
                    CachedTile {
                        image,
                        pixels,
                        last_flow: span.flow_start as i32,
                    },
                );
            }
        }

        // Placement is not cached, so a tile that slid along with the document's
        // growing edge costs one property assignment rather than a rasterization.
        let tiles = keyed
            .iter()
            .filter_map(|(span, signature)| {
                let cached = images.get_mut(signature)?;
                cached.last_flow = span.flow_start as i32;
                Some(PaneId::tile(vertical, *span, cached.image.clone()))
            })
            .collect::<Vec<_>>();

        // **What is evicted leaves its buffer behind, for the refresh after
        // this one.** Not for this one: the pane is still showing the tiles of
        // the last refresh, so their images are alive until `set_tiles` below
        // replaces them — drawn into now, a buffer would be copied rather than
        // reused (Slint's `SharedVector::detach`), which is what allocating one
        // cost in the first place (技術検証 7.8).
        let viewport_center = -scroll + shown_flow * 0.5;
        let wanted = keyed
            .iter()
            .map(|(_, signature)| *signature)
            .collect::<Vec<_>>();
        evict_distant_tiles(images, spare, &wanted, viewport_center);
        let spare_held = spare.len();

        let tile_count = tiles.len();
        id.set_tiles(window, tiles);
        // **What was asked for, beside what was placed.** A tile whose image is
        // not in the cache when the placement runs is dropped without a word
        // (`images.get_mut`), and a pane missing one shows paper where its text
        // should be — which looks exactly like a document that has not been
        // drawn yet. The two numbers differing is the only sign from outside.
        Ok((tile_count, keyed.len(), rendered, reused, spare_held))
    }

    /// Re-cut the selection rectangles for what the pane now shows.
    fn refresh_pane_selection(
        &mut self,
        window: &AppWindow,
        id: PaneId,
    ) -> windows::core::Result<()> {
        let selection = self.pane(id).view.selection_utf16.clone();
        if selection.is_empty() {
            return Ok(());
        }
        let engine = &mut self.pane(id).graphics.engine;
        let visible = id.flow_range(window, engine.total_flow_size() as f32);
        // **One ask per run.** A run outside the viewport costs the early
        // return in `selection_rects` and nothing else, so a rectangle over a
        // thousand lines is charged for the lines on screen.
        let mut rects = Vec::new();
        for run in selection {
            rects.extend(engine.selection_rects(Some(run), visible)?);
        }
        id.set_selection(window, &rects);
        Ok(())
    }
}

/// One tile's pixels, as Slint wants them.
///
/// Direct2D hands back BGRA and this reads it in place, so the pixels are
/// walked once and copied once.
/// Where one pane's tiles are drawn, and what they are drawn into.
///
/// **The buffers come from the pane and go back to it.** The engine writes each
/// tile once, straight into the image the window will hold; nothing here copies
/// a tile again, and a buffer whose image has been evicted is drawn into rather
/// than allocated (技術検証 7.8).
struct TileImages<'a> {
    spare: &'a mut Vec<SharedPixelBuffer<Rgba8Pixel>>,
    /// The one being drawn into, between `buffer` and `filled`.
    drawing: Option<SharedPixelBuffer<Rgba8Pixel>>,
    produced: Vec<(TileSpan, Image, SharedPixelBuffer<Rgba8Pixel>)>,
    uploaded: usize,
    /// How many of these tiles went into memory that had already been written
    /// to. **Counted rather than assumed**: a buffer the window has not let go
    /// of is copied instead of reused (`SharedVector::detach`), and that looks
    /// exactly like reuse from here. The pointer says which happened.
    reused: usize,
}

impl TileSink for TileImages<'_> {
    fn buffer(&mut self, _span: TileSpan, width: u32, height: u32) -> &mut [u8] {
        let taken = take_spare(self.spare, width, height);
        // **Two things have to be true to have saved anything**: it came from
        // the pool, and the pool's copy was the only one left — a buffer the
        // window has not let go of is copied instead (`SharedVector::detach`),
        // which is what allocating one cost in the first place. A buffer just
        // allocated keeps its pointer too, so the pointer alone says nothing.
        let from_pool = taken.is_some();
        let mut buffer =
            taken.unwrap_or_else(|| SharedPixelBuffer::<Rgba8Pixel>::new(width, height));
        let was = buffer.as_bytes().as_ptr();
        let is = buffer.make_mut_bytes().as_ptr();
        if from_pool && was == is {
            self.reused += 1;
        }
        self.drawing = Some(buffer);
        self.drawing
            .as_mut()
            .expect("the buffer was just put there")
            .make_mut_bytes()
    }

    fn filled(&mut self, span: TileSpan) {
        let Some(mut pixels) = self.drawing.take() else {
            return;
        };
        // The bitmap holds BGRA and Slint wants RGBA. **Swapped where it lies**:
        // written into a second buffer, this pass costs what allocating that
        // buffer costs, which is more than the drawing did (技術検証 7.8).
        for four in pixels.make_mut_bytes().chunks_exact_mut(4) {
            four.swap(0, 2);
        }
        self.uploaded += pixels.as_bytes().len();
        self.produced
            .push((span, Image::from_rgba8(pixels.clone()), pixels));
    }
}

/// A buffer of exactly this size that no image is showing any more.
///
/// **Tiles are not all one size** — the last slice of every block is short — so
/// this looks for the size asked for and leaves the others where they are.
///
/// **A miss lets the oldest one go.** The sizes follow the pane's extent, so
/// after a resize every buffer in here is of a size that will never be asked
/// for again; dropping one per miss empties the pool of them over the next few
/// refreshes without ever throwing away one that still fits.
fn take_spare(
    spare: &mut Vec<SharedPixelBuffer<Rgba8Pixel>>,
    width: u32,
    height: u32,
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let fits = |buffer: &SharedPixelBuffer<Rgba8Pixel>| {
        buffer.width() == width && buffer.height() == height
    };
    // **The oldest that fits**, because the oldest is the one whose image the
    // pane has had time to let go of.
    if let Some(at) = spare.iter().position(fits) {
        return Some(spare.remove(at));
    }
    if !spare.is_empty() {
        spare.remove(0);
    }
    None
}

/// The passage a pane is holding its view on, while the hold stands
/// (要件 8.5).
///
/// **The caret having moved is the writer saying where to look**, and that is
/// the end of it. Asked this way rather than cleared by every path that moves a
/// caret: there are nine of those, and the tenth one added later would not know
/// it had to.
fn held_view(anchor: Option<ViewAnchor>, caret: Option<usize>) -> Option<usize> {
    anchor
        .filter(|anchor| anchor.caret == caret)
        .map(|anchor| anchor.byte)
}

/// The source byte at the near edge of what a pane is showing (要件 8.5).
///
/// **Free rather than a method** because a zoom asks it too, and a zoom is
/// carried out by a callback holding the pane states and the cache and nothing
/// else. [`Live::view_top`] is the same question asked when a view is being
/// put away.
fn view_top(
    window: &AppWindow,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
) -> Option<usize> {
    let document = states.document(id);
    let source = document.text.borrow().clone();
    let state = states.of(id);
    let active_line_start = PaneId::revealed_line(id.vertical(window), &state, &source);
    // The near edge of the view, in the content's own coordinates. The flow
    // axis is x where the text runs down the page and y where it runs
    // across; the other axis is the head of the line, which is where a line
    // is named from.
    let near = -id.scroll(window) + 1.0;
    let (x, y) = if id.vertical(window) {
        (near, 1.0)
    } else {
        (1.0, near)
    };
    let mut borrowed = cache.borrow_mut();
    let cache = &mut *borrowed;
    // **見えている先頭の位置だけが要る。**行番号の欄かどうかは、点を選んだのが
    // 書き手ではなくここ自身である以上、訊く意味が無い。
    hit_test_pane(
        window,
        cache,
        &document,
        id,
        &source,
        active_line_start,
        x,
        y,
    )
    .map(|hit| hit.byte)
}

/// Hold a pane's view on a passage, until the writer looks somewhere else
/// (要件 8.5).
fn hold_view(
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    top: Option<usize>,
    caret: Option<usize>,
) {
    cache.borrow_mut().pane(id).view.top_anchor = top.map(|byte| ViewAnchor { byte, caret });
}

/// Keep the tiles the viewport wants plus the nearest others, up to the cap.
fn evict_distant_tiles(
    tiles: &mut BTreeMap<u64, CachedTile>,
    spare: &mut Vec<SharedPixelBuffer<Rgba8Pixel>>,
    wanted: &[u64],
    viewport_center: f32,
) {
    // A tall window makes tiles narrower, so more of them are on screen at once.
    // The cap has to leave room for every wanted tile or the cache would evict
    // what it is about to be asked for again.
    let limit = TILE_CACHE_LIMIT.max(wanted.len() + 2);
    if tiles.len() <= limit {
        return;
    }
    let mut keys = tiles.keys().copied().collect::<Vec<_>>();
    keys.sort_by(|left, right| {
        // Tiles are keyed by content, but "far away" is a question about pixels,
        // so distance is measured against the x the tile was last placed at.
        let rank = |key: u64| {
            let x = tiles.get(&key).map(|cached| cached.last_flow as f32);
            (
                !wanted.contains(&key),
                x.map(|x| (x - viewport_center).abs() as u32)
                    .unwrap_or(u32::MAX),
            )
        };
        rank(*left).cmp(&rank(*right))
    });
    for key in keys.into_iter().skip(limit) {
        // **The pixels stay, the image goes.** What is being thrown away is a
        // tile nobody is looking at; its two megabytes are worth keeping for
        // the next tile to be drawn into (技術検証 7.8).
        if let Some(evicted) = tiles.remove(&key) {
            if spare.len() < SPARE_TILE_BUFFERS {
                spare.push(evicted.pixels);
            }
        }
    }
}

/// What laying one pane out produced.
struct PaneLayout {
    /// The caret in the shown text, after the IME's string was spliced in.
    render_caret: Option<u32>,
    /// The place the view is being held on, in the shown text (要件 8.5).
    anchor_utf16: Option<u32>,
    selection: Vec<(u32, u32)>,
    /// The same runs in the document's bytes, for the status bar's count.
    selection_source: Vec<(usize, usize)>,
    /// 見えている一致（E1、書き手の求め 2026-09-09：「ヒットした語がすべて
    /// ハイライトされた上で、対象が動く」）。**組んだ本文の「始まりと長さ」**
    /// ——選択の走りと同じ形で、同じ道で矩形になる。
    matches: Vec<(u32, u32)>,
    /// 範囲内検索の範囲（E1、書き手の求め 2026-09-09）。**検索が選択を動かす
    /// ので、範囲は選択では見えない**——だから別に出す。
    scope: Option<(u32, u32)>,
    /// How far the content reached before this layout, for the panes whose
    /// document start is not at the origin.
    previous_flow: u32,
    measured: directwrite_render::UpdateCost,
    preview_ms: f64,
    layout_ms: f64,
}

/// Lay one pane out: choose its text, map the positions into it, splice the
/// IME's string, and measure.
///
/// **This is the half of a refresh that is the same for both panes**, and the
/// half where getting it wrong is expensive: 6.15 was a copy of this logic that
/// read the other text. Written once, it cannot drift again. The other half,
/// the geometry and the reporting, is [`refresh_pane`].
///
/// `None` means the engine refused the text; the caller has already been told.
#[allow(clippy::too_many_arguments)]
fn lay_out_pane(
    window: &AppWindow,
    cache: &mut RenderCache,
    document: &OpenDocument,
    id: PaneId,
    source: &str,
    line_fit: LineFit,
    typography: &Typography,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection: PaneSelection,
    preedit: &str,
) -> Option<PaneLayout> {
    let mut counts = document.counts.borrow_mut();
    let styles = counts.get(source).line_styles();
    let pane = cache.pane(id);

    let preview_started = Instant::now();
    // Which text this pane lays out — the whole of the third and fourth modes
    // (3.12). Everything below is written against `PaneText` and does not ask
    // which one it got.
    let slot = &mut pane.view.preview_slot;
    let shown = pane_text(window, id, slot, source, active_line_start);
    let caret = caret_source_byte.map(|byte| shown.utf16_at_source_byte(byte) as u32);
    // 要件 8.5: the place this pane is holding its view on, if it still is.
    // **The caret having moved is the writer saying where to look**, and that
    // is the end of the hold — one comparison here rather than a flag every
    // caret-moving path would have to remember to clear.
    let anchor_utf16 = held_view(pane.view.top_anchor, caret_source_byte)
        .map(|byte| shown.utf16_at_source_byte(byte) as u32);
    let selection_utf16 = selection.ends.map(|(start, end)| {
        (
            shown.utf16_at_source_byte(start) as u32,
            shown.utf16_at_source_byte(end) as u32,
        )
    });
    // E1: **探している語のありか**を、組んだ本文の位置へ写す。空の欄では
    // 一周も歩かない——これは打鍵のたびに通る道である。
    // **光るのは帯が出ているあいだだけ**（書き手の報告 2026-09-09：「その色を
    // 解除できません」）。語は面に残り続けるので（F3のため）、語があるかぎり
    // 光らせると、探し終えた紙が色を持ったままになる。**`Esc`で帯を閉じれば
    // 消える**——閉じる鍵が消す鍵でもある、というのがいちばん短い説明になる。
    let showing = window.get_find_open() && window.get_find_pane() == id.index();
    let needle = if showing {
        id.screen(window).find_needle.to_string()
    } else {
        String::new()
    };
    let rules = find_rules(window, id);
    let scope = rules
        .within
        .filter(|_| showing)
        .map(|(start, end)| {
            let start = shown.utf16_at_source_byte(start) as u32;
            let end = shown.utf16_at_source_byte(end) as u32;
            (start, end.saturating_sub(start))
        })
        .filter(|(_, length)| *length > 0);
    let matches = if needle.is_empty() {
        Vec::new()
    } else {
        find::Search::new(&needle, rules)
            .map(|search| search.spans(source, MAX_SHOWN_MATCHES))
            .unwrap_or_default()
            .into_iter()
            // **いま選ばれている一致には敷かない。**そこは選択が濃く出して
            // いるので、下に薄いのを重ねると同じ語なのに3段の濃さになる
            // （書き手の報告 2026-09-09：「色の変わり方が壊れています」）。
            .filter(|span| Some(*span) != selection.ends)
            .map(|(start, end)| {
                let start = shown.utf16_at_source_byte(start) as u32;
                let end = shown.utf16_at_source_byte(end) as u32;
                // **`selection_rects`が取るのは「始まりと長さ」**であって
                // 「始まりと終わり」ではない（書き手の報告 2026-09-09、
                // `検索.png`：語ではなく行が塗られていた。終わりを長さとして
                // 渡していたので、7文字目の2文字が「7文字目から9文字ぶん」に
                // なっていた）。選択の走りも同じ形で持っている。
                (start, end.saturating_sub(start))
            })
            // 組んだ本文で幅を持たないものは色を付けない——プレビューでは
            // ルビの読みのように**隠れている字**があり、そこに入った一致は
            // 写した先で長さ0になる。
            .filter(|(_, length)| *length > 0)
            .collect()
    };
    let (render_text, render_caret, preedit_range) = text_with_preedit(&shown, caret, preedit);
    let preview_ms = elapsed_ms(preview_started);

    let layout_started = Instant::now();
    let engine = &mut pane.graphics.engine;
    let previous_flow = engine.total_flow_size();
    // **While a preedit stands, nothing is marked.** The composition is put
    // into the line at the caret, and everything the markers pointed at after
    // that sits somewhere else until it is committed. Half a second of plain
    // text is better than emphasis on the wrong characters.
    let marks = if preedit.is_empty() {
        shown.marks()
    } else {
        // Nothing, for as long as the composition sits in the line.
        &[]
    };
    // **The boxes are not held back the way the marks are.** A composition sits
    // at the caret, the caret's line is the active one, and the active line has
    // no box over its marker (要件 7.3.1) — so there is no range here for a
    // preedit to move. Holding them back would instead make the text jump by an
    // indent for as long as somebody is converting.
    let marked = StyledText::marked(&render_text, styles, marks);
    let styled = marked
        .with_markers(shown.markers())
        .with_source_line(shown.source_line());
    // 要件 7.9: **この面のモードの語を渡す**（`set_words`）。モードは文書ごと
    // なので、2つのペインが別の作品を開いていれば別の色分けになる。
    // **組み直しの判定には入らない**——幾何を1画素も動かさないので、変わっても
    // タイルだけが古くなる（技術検証 9.3.1）。
    engine.set_words(word_mode_with(id.screen(window).word_mode as u32));
    let measured = match engine.update(styled, line_fit, typography) {
        Ok(measured) => measured,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}整形: NG / {error}").into());
            return None;
        }
    };
    let layout_ms = elapsed_ms(layout_started);

    // **Cut into runs only now.** A rectangle's runs are one per *layout* line,
    // and which lines those are is what the update just decided (要件 7.1).
    let runs = match selection_utf16 {
        Some((start, end)) if selection.rectangular => {
            rectangular_runs(window, id, engine, start, end)
        }
        Some((start, end)) if start < end => vec![(start, end - start)],
        _ => Vec::new(),
    };
    let selection_source = runs
        .iter()
        .map(|(start, length)| {
            (
                shown.source_byte_at_utf16(*start as usize),
                shown.source_byte_at_utf16((*start + *length) as usize),
            )
        })
        .collect::<Vec<_>>();

    pane.view.caret_utf16 = render_caret;
    pane.view.selection_utf16 = runs.clone();
    pane.view.selection_source = selection_source.clone();
    pane.view.preedit_range = preedit_range;

    Some(PaneLayout {
        render_caret,
        anchor_utf16,
        selection: runs,
        selection_source,
        matches,
        scope,
        previous_flow,
        measured,
        preview_ms,
        layout_ms,
    })
}

/// Lay one pane out, place its caret and selection, draw what it shows, and
/// report what that cost.
///
/// One function for both panes. It used to be two, and the half that was easy
/// to get wrong is already shared ([`lay_out_pane`], where 6.15 came from); what
/// is left here is the geometry and the reporting, which differed only in which
/// Slint property they wrote. That difference is behind [`PaneId`] now, and goes
/// altogether when the panes become a model (ペイン分割設計 5.2).
#[allow(clippy::too_many_arguments)]
/// Draw one of a pane's shells (追加要件 Terminal).
///
/// **The pane is told its size in cells before anything is drawn**, because the
/// shell draws for the screen it was told about: a prompt redrawn for 80 columns
/// on a pane that holds 60 wraps in the wrong place, and nothing later can undo
/// it. Then everything waiting is applied.
///
/// **The drawing is by bands, and a band nothing touched is not drawn.** A full
/// screen at the size of a maximized pane costs 15ms and nine megabytes of
/// texture (measured, 181×85); paying that for every chunk of output is what
/// makes a program that redraws itself look broken rather than slow. Each band
/// is keyed by what is in it, so a keystroke redraws one.
fn refresh_terminal(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    spot: TerminalSpot,
) {
    let Some(session) = cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.session.clone())
    else {
        return;
    };
    let look = terminal_look(window);
    let Ok(cell) = cells::terminal_cell_size(&look) else {
        return;
    };
    // **The strip is as wide as the pane and as tall as it was dragged**; the
    // tab's own shell has the whole of it.
    let extent = match spot {
        TerminalSpot::Front => id.shown_height(window),
        TerminalSpot::Below => cache.borrow_mut().pane(id).below_height,
    };
    let (columns, rows) = cell.grid_for(id.shown_width(window), extent);
    let mut session = session.borrow_mut();
    session.resize(columns, rows);
    session.drain();
    let screen = session.screen();

    // **What this shell is being looked at from**: the last `rows` of the
    // history and the screen together, moved back by however far the writer has
    // scrolled. A view held back holds still while output arrives — the rows it
    // shows are pushed further into the history, and the count follows them.
    let history = screen.scrollback().len();
    let looking = {
        let mut borrowed = cache.borrow_mut();
        let Some(shell) = borrowed.pane(id).shell(spot) else {
            return;
        };
        if shell.looking > 0 {
            shell.looking += history.saturating_sub(shell.history);
        }
        shell.history = history;
        shell.looking = shell.looking.min(history);
        shell.looking
    };
    // Where the view starts in the history, and what the writer has picked out.
    let top = history.saturating_sub(looking);
    let selection = cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .and_then(|shell| shell.selection);
    let width = (columns as f32 * cell.advance).ceil().max(1.0) as u32;
    let line = cell.line.max(1.0);
    // 要件 7.1 の意味でのキャレットではない。**シェルが隠せと言えば隠す**もので、
    // 全画面のプログラムは自分で消して自分で描く。
    let cursor = screen.modes().cursor_visible.then(|| {
        let at = screen.cursor();
        (at.row, at.column)
    });
    let preedit = cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.preedit.clone())
        .unwrap_or_default();

    // **署名に見た目を混ぜる**（追加要件 2026-09-08）。升目の大きさが変われば
    // `advance`／`line`が動くが、**色だけを変えても動かない**——混ぜないと、
    // 帯の絵置き場にある古い色のままの絵がそのまま出る（6.18の色と同じ罠の5例目）。
    let painted_as = {
        let mut hasher = DefaultHasher::new();
        for channel in look.paper.iter().chain(look.ink.iter()) {
            channel.to_bits().hash(&mut hasher);
        }
        look.family.hash(&mut hasher);
        hasher.finish()
    };
    let shape = (
        columns,
        rows,
        cell.advance.to_bits(),
        cell.line.to_bits(),
        painted_as,
    );
    {
        let mut borrowed = cache.borrow_mut();
        let Some(shell) = borrowed.pane(id).shell(spot) else {
            return;
        };
        if shell.bands.shape != Some(shape) {
            shell.bands.shape = Some(shape);
            shell.bands.bands.clear();
        }
    }

    // **The bands are cut out of the history, not out of the view.** A band is
    // whichever rows fall in one stretch of `TERMINAL_BAND_ROWS`, counted from
    // the start of the history, so scrolling slides the view across bands that
    // are already drawn. The two at the edges are cut short by the view, and
    // they are the only ones a scroll has to draw again.
    let total = history + rows;
    let first_band = top / TERMINAL_BAND_ROWS;
    let last_band = (top + rows).div_ceil(TERMINAL_BAND_ROWS);
    let mut tiles: Vec<PreviewTile> = Vec::new();
    let mut wanted: Vec<usize> = Vec::new();
    for band in first_band..last_band {
        let from = (band * TERMINAL_BAND_ROWS).max(top);
        let to = ((band + 1) * TERMINAL_BAND_ROWS).min(top + rows).min(total);
        if to <= from {
            continue;
        }
        wanted.push(band);
        let lines: Vec<&crate::terminal::Line> = (from..to)
            .filter_map(|at| {
                if at < history {
                    screen.scrollback().get(at)
                } else {
                    screen.line(at - history)
                }
            })
            .collect();
        if lines.is_empty() {
            continue;
        }
        // **The caret belongs to the screen**, so it is only anywhere at all
        // when the pane is looking at the bottom.
        let inside = cursor.filter(|(row, _)| {
            let at = history + row;
            looking == 0 && (from..to).contains(&at)
        });
        let picked: Vec<Option<(usize, usize)>> = (from..to)
            .map(|at| selection.and_then(|picked| picked.columns_in(at, columns)))
            .collect();
        let signature = {
            let mut hasher = DefaultHasher::new();
            for line in &lines {
                line.hash(&mut hasher);
            }
            picked.hash(&mut hasher);
            // The trim, so a band the view cuts short is not mistaken for the
            // same band whole.
            (from, to).hash(&mut hasher);
            inside
                .map(|(row, column)| (history + row - from, column))
                .hash(&mut hasher);
            // **The band holding the cursor is also the band the conversion is
            // drawn in**, so what is being composed is part of how it looks.
            if inside.is_some() {
                preedit.hash(&mut hasher);
            }
            hasher.finish()
        };
        let height = (lines.len() as f32 * line).ceil().max(1.0) as u32;
        let held = {
            let mut borrowed = cache.borrow_mut();
            borrowed
                .pane(id)
                .shell(spot)
                .and_then(|shell| shell.bands.bands.get(&band).cloned())
        };
        let image = match held {
            // **The same cells drawn the same way**: nothing to do but place it
            // again, which costs one property assignment.
            Some((was, image)) if was == signature && image.size().width == width => image,
            _ => {
                let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                // Copied only for the band being drawn: the rows this one
                // holds, and never the screenful.
                let band_lines: Vec<crate::terminal::Line> =
                    lines.iter().map(|line| (*line).clone()).collect();
                let painted = cells::draw_terminal(
                    &band_lines,
                    &picked,
                    inside.map(|(row, column)| (history + row - from, column)),
                    &preedit,
                    &look,
                    cell,
                    pixels.make_mut_bytes(),
                    width,
                    height,
                );
                if let Err(error) = painted {
                    cache
                        .borrow_mut()
                        .log_diag("terminal", &format!("draw pane={} {error}", id.log_name()));
                    return;
                }
                // The bitmap holds BGRA and Slint wants RGBA, swapped where it
                // lies for the reason the document's tiles swap theirs.
                for four in pixels.make_mut_bytes().chunks_exact_mut(4) {
                    four.swap(0, 2);
                }
                let image = Image::from_rgba8(pixels);
                if let Some(shell) = cache.borrow_mut().pane(id).shell(spot) {
                    shell.bands.bands.insert(band, (signature, image.clone()));
                }
                image
            }
        };
        tiles.push(PreviewTile {
            x: 0,
            y: ((from - top) as f32 * line) as i32,
            width: width as i32,
            height: height as i32,
            source: image,
        });
    }
    // **What is not on screen is not worth keeping.** A screenful of bands is a
    // few megabytes; a session's scrollback would be hundreds.
    if let Some(shell) = cache.borrow_mut().pane(id).shell(spot) {
        shell.bands.bands.retain(|band, _| wanted.contains(band));
    }

    // **どれだけ描いているかを、秒ごとにひとことだけ**（2026-09-08追加）。
    // 帯に切ってあるのは升目を描く費用を抑えるためで（この関数の注記）、
    // 効いているかどうかは頻度でしか読めない。静かなシェルは1行も出さない。
    let counted = {
        let mut borrowed = cache.borrow_mut();
        let now = Instant::now();
        borrowed
            .pane(id)
            .shell(spot)
            .and_then(|shell| shell.drew(now))
    };
    if let Some((drawn, span)) = counted {
        cache.borrow_mut().log_diag(
            "terminal",
            &format!(
                "frames pane={} spot={spot:?} drawn={drawn} in={span:.1}s",
                id.log_name()
            ),
        );
    }

    match spot {
        TerminalSpot::Front => {
            let height = (rows as f32 * line).ceil().max(1.0) as i32;
            id.set_tiles(window, tiles);
            id.set_selection(window, &[]);
            id.set_caret(window, None);
            // **Where the conversion window opens** (要件 7.2). The hidden
            // field is what Windows asks, and it has to stand where the writing
            // is or the candidates appear in the corner of the pane.
            if let (Some((row, column)), true) = (cursor, preedit.is_empty()) {
                let caret = CaretGeometry {
                    x: column as f32 * cell.advance,
                    y: row as f32 * line,
                    width: cell.advance,
                    height: line,
                };
                // **Past the cell, not on it** (書き手の報告, 2026-09-06: 候補の
                // 窓が入力位置に重なって見えない). Windows opens the candidate
                // list at the field, so the field stands one cell down and one
                // along — the same offset the document uses.
                // **One line below the caret, even past the foot of the pane**
                // (書き手の報告, 2026-09-06: 最下行でまだ重なる). The candidate
                // list opens downwards from the field, so a field held inside
                // the pane is a list drawn over the row being typed — and the
                // row being typed is the last one nearly always, because that
                // is where a prompt is. Below the pane there is the status bar
                // and then the edge of the window; the list opens over those,
                // which hide nothing anybody is reading. **Clipping is not
                // placement**: the field is invisible either way, and where it
                // is reported from is what Windows uses.
                let (x, y) = ime_candidate_anchor(&caret, false);
                let x = x.clamp(0.0, (width as f32 - cell.advance).max(0.0));
                // At most one line past the foot, which is as far as it ever
                // needs to go — beyond that is off the window, and a field
                // nobody can place is one the IME has nowhere to open on.
                let y = y.min(id.shown_height(window) + line);
                id.set_ime_anchor(window, x, y, &caret);
            }
            id.update_screen(window, |screen| {
                screen.terminal = true;
                screen.content_width = width as i32;
                screen.content_height = height;
            });
        }
        TerminalSpot::Below => {
            id.set_below_tiles(window, tiles);
            // Where the strip's own hidden field stands, so a conversion opens
            // over the writing rather than in the corner (要件 7.2).
            //
            // **Held still while one is up.** The shell's cursor moves with
            // every line it prints, and a field that moves under a conversion
            // takes the conversion with it.
            if let (Some((row, column)), true) = (cursor, preedit.is_empty()) {
                let caret = CaretGeometry {
                    x: column as f32 * cell.advance,
                    y: row as f32 * line,
                    width: cell.advance,
                    height: line,
                };
                let (x, y) = ime_candidate_anchor(&caret, false);
                // **Held inside the strip.** The prompt lives on its last row,
                // so the field — which stands one cell *past* the caret — falls
                // outside a strip that clips, and a field nobody can place is a
                // field the IME cannot compose in (書き手の報告, 2026-09-06:
                // 下段で日本語が打てない). Clamped, it stands at the edge
                // instead, which is where the candidates should open anyway.
                // Below the caret's row, past the foot of the strip if that is
                // where it falls: the list has to open somewhere that is not
                // the line being typed (see the pane's own anchor above).
                let x = x.clamp(0.0, (width as f32 - cell.advance).max(0.0));
                let strip = cache.borrow_mut().pane(id).below_height;
                let y = y.min(strip + line);
                id.update_screen(window, |screen| {
                    screen.below_caret_x = x;
                    screen.below_caret_y = y;
                });
            }
        }
    }
}

/// A tab that is a shell (追加要件 Terminal), in the pane the writer is in.
///
/// **The default is WSL**, which is what the requirement says and what the
/// writer works in. `wsl.exe` starts the distribution if it is not running, so
/// there is nothing here to do about that.
/// Put another shell in a terminal tab (追加要件 2026-09-07).
///
/// **The tab is not replaced, only what runs in it.** Its place in the strip,
/// the strip along its foot and the draft in that strip are all the tab's and
/// none of them is about which shell it was — so a writer who opened the wrong
/// one holds the tab down and picks the right one, rather than closing it and
/// starting again where it left off in the order.
fn switch_shell(window: &AppWindow, live: &Live, id: PaneId, shell: TerminalShell) {
    // **Logged before anything can turn it down.** The gesture that opens the
    // list is a long press and the list is drawn by the window, so when nothing
    // happens the first question is whether the answer ever arrived here
    // ([[editor-diag-log-answers-it-works-questions]]).
    live.cache.borrow_mut().log_diag(
        "tab",
        &format!("shell asked pane={} {shell:?}", id.log_name()),
    );
    let running = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .is_some_and(|tab| tab.terminal.is_some());
    if !running {
        return;
    }
    let Some(session) = start_shell(window, live, id, &shell, id.shown_height(window)) else {
        return;
    };
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let active = strip.active;
        let Some(tab) = strip.tabs.get_mut(active) else {
            return;
        };
        // **The old one goes when the last hand lets go of it**, which is here:
        // the pane's copy is put down below by `show_tab`.
        tab.terminal = Some(Rc::new(RefCell::new(session)));
    }
    let Some(showing) = live.tabs.borrow().of(id).current().cloned() else {
        return;
    };
    live.cache
        .borrow_mut()
        .log_diag("tab", &format!("shell pane={} to={shell:?}", id.log_name()));
    live.show_tab(window, id, &showing);
    publish_tabs(window, live);
}

fn open_terminal(window: &AppWindow, live: &Live, id: PaneId, shell: TerminalShell) {
    // 追加要件 2026-09-07: **まだ何でもないタブは、それになる。**新しいタブを
    // 増やさないのは、そのタブが「何になるか」を訊いている最中だからで、
    // どのメニューから頼まれたかは関係がない。
    let asking = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .is_some_and(|tab| tab.empty);
    if asking {
        answer_new_tab(window, live, id, Some(shell));
    } else {
        new_terminal_tab(window, live, id, shell);
    }
}

fn new_terminal_tab(window: &AppWindow, live: &Live, id: PaneId, shell: TerminalShell) {
    sync_active_tab(window, live);
    let number = {
        let tabs = live.tabs.borrow();
        let taken: Vec<u32> = tabs
            .panes
            .iter()
            .flat_map(|strip| strip.tabs.iter())
            .map(|tab| tab.document.file.borrow().untitled_number())
            .collect();
        next_untitled_number(&taken)
    };
    let Some(session) = start_shell(window, live, id, &shell, id.shown_height(window)) else {
        return;
    };
    let document = OpenDocument::untitled(number, window.as_weak());
    let tab = PaneTab {
        view: TabView {
            vertical: false,
            preview: false,
            ..TabView::default()
        },
        terminal: Some(Rc::new(RefCell::new(session))),
        ..PaneTab::showing(window, id, document)
    };
    add_tab(window, live, id, tab);
}

/// Start a shell for a pane, sized to the space it will be drawn in.
///
/// **The size is worked out before the shell exists**, because the first thing
/// it does is draw a prompt for the screen it was told about.
fn start_shell(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    shell: &TerminalShell,
    extent: f32,
) -> Option<TerminalSession> {
    let look = terminal_look(window);
    let cell = cells::terminal_cell_size(&look).unwrap_or(cells::CellSize {
        advance: 8.0,
        line: 18.0,
    });
    let (columns, rows) = cell.grid_for(id.shown_width(window), extent);
    let weak = window.as_weak();
    let wake = move || {
        // **One ring, on the window's own thread.** What to draw and how is
        // decided over there; this side is a reading thread and may not touch
        // any of it (要件 2).
        let _ = weak.upgrade_in_event_loop(|window| window.invoke_terminal_woken());
    };
    match TerminalSession::start(&shell.name, &shell.command, columns, rows, wake) {
        Ok(session) => {
            live.cache.borrow_mut().log_diag(
                "terminal",
                &format!(
                    "open pane={} {} {columns}x{rows}",
                    id.log_name(),
                    shell.command
                ),
            );
            Some(session)
        }
        Err(error) => {
            live.cache
                .borrow_mut()
                .log_diag("terminal", &format!("open {} {error}", shell.command));
            let told = format!("{}を開けませんでした: {error}", shell.name);
            window.set_render_status(told.into());
            None
        }
    }
}

/// Send one keystroke to one of a pane's shells (追加要件 Terminal).
///
/// **The pane does not decide what a key means.** Which bytes an arrow is
/// depends on modes the shell set, so the key is named here and encoded in
/// `terminal`, where those modes live.
fn send_terminal_key(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    spot: TerminalSpot,
    text: &str,
    code: i32,
    control: bool,
    alt: bool,
    shift: bool,
) {
    let Some(session) = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.session.clone())
    else {
        // **A key with nowhere to go is written down.** Dropped in silence, it
        // is a terminal that has stopped answering with nothing anywhere to say
        // why — which is exactly how long the last one took to find.
        live.cache.borrow_mut().log_diag(
            "terminal",
            &format!("key pane={} spot={spot:?} with no shell", id.log_name()),
        );
        return;
    };
    // 要件 11.2. **`Ctrl+Shift+C`, because `Ctrl+C` is the interrupt** — the one
    // chord a shell cannot be asked to give up. The pair is what every terminal
    // uses, and pasting comes back the other way: `Ctrl+V` is left to the pane's
    // own field, which fills it and hands the text over as typing.
    if control && shift && text.eq_ignore_ascii_case("c") {
        copy_terminal_selection(window, live, id, spot);
        return;
    }
    let Some(key) = named_key(code, text, control) else {
        return;
    };
    {
        // **Typing puts the writer back at the bottom**, which is where what
        // they type will appear. Every terminal does this, and the reason is
        // that the alternative — typing into a screen you cannot see — has no
        // use. The selection goes with it: the screen is about to change.
        let mut borrowed = live.cache.borrow_mut();
        if let Some(shell) = borrowed.pane(id).shell(spot) {
            shell.looking = 0;
            shell.selection = None;
        }
    }
    let modifiers = TerminalModifiers {
        shift,
        alt,
        control,
    };
    let sent = {
        let mut session = session.borrow_mut();
        session.send_key(key, modifiers);
        session.screen().modes()
    };
    live.cache.borrow_mut().log_diag(
        "terminal",
        &format!(
            "key pane={} spot={spot:?} code={code} u={:04x} ctrl={control} alt={alt} shift={shift} app_keys={}",
            id.log_name(),
            text.chars().next().map(u32::from).unwrap_or(0),
            sent.application_cursor_keys
        ),
    );
    refresh_terminal(window, &live.cache, id, spot);
}

/// Which key the window says was pressed, or `None` for one the shell should
/// never hear about (追加要件 Terminal).
///
/// **A modifier key is not a keystroke.** Slint hands them over as characters —
/// Shift is `U+0010`, Control `U+0011` — and sent as text they are `Ctrl+P` and
/// `Ctrl+Q`, which is why the first terminal walked back through the history
/// every time Shift was touched. There is no ambiguity to weigh: winit gives the
/// *logical* key, so `Ctrl+Q` arrives as `q` with the modifier flag set, never
/// as `U+0011`.
fn named_key(code: i32, text: &str, control: bool) -> Option<TerminalKey> {
    let key = match code {
        1 => TerminalKey::Up,
        2 => TerminalKey::Down,
        3 => TerminalKey::Right,
        4 => TerminalKey::Left,
        5 => TerminalKey::Home,
        6 => TerminalKey::End,
        7 => TerminalKey::PageUp,
        8 => TerminalKey::PageDown,
        9 => TerminalKey::Delete,
        10 => TerminalKey::Insert,
        11 => TerminalKey::Backspace,
        12 => TerminalKey::Tab,
        13 => TerminalKey::Enter,
        14 => TerminalKey::Escape,
        20..=31 => TerminalKey::Function((code - 19) as u8),
        // The window has already decided this one is not for the shell.
        number if number < 0 => return None,
        _ => {
            let character = text.chars().next()?;
            // A second net under the window's: a key with no name and no text
            // of its own — the Windows key, a function key past F12 — arrives
            // as a private-use character that means nothing to a shell.
            if matches!(character as u32, 0x10..=0x18 | 0xe000..=0xf8ff) {
                return None;
            }
            // **Ctrl+C arrives as the control code it already is.** Named back
            // into the letter, because what the shell is sent is decided in one
            // place — otherwise Ctrl+C would be encoded here and every other key
            // over there.
            let named = if control && (character as u32) < 0x20 {
                char::from_u32(character as u32 + 0x60).unwrap_or(character)
            } else {
                character
            };
            TerminalKey::Char(named)
        }
    };
    Some(key)
}

/// Look further back through what a shell has written, or nearer the bottom
/// (追加要件 Terminal).
///
/// **The wheel moves rows, not pixels.** A terminal has no half-lines to stop
/// on, and stopping on one would put every glyph a fraction out of its cell.
fn scroll_terminal(window: &AppWindow, live: &Live, id: PaneId, spot: TerminalSpot, delta: f32) {
    let Some(session) = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.session.clone())
    else {
        return;
    };
    let history = session.borrow().screen().scrollback().len();
    let look = terminal_look(window);
    let line = cells::terminal_cell_size(&look)
        .map(|cell| cell.line)
        .unwrap_or(18.0);
    // Whole rows, and at least one: a notch that moved nothing would read as a
    // wheel that is not working.
    let rows = ((delta.abs() / line).round() as usize).max(1);
    {
        let mut borrowed = live.cache.borrow_mut();
        if let Some(shell) = borrowed.pane(id).shell(spot) {
            shell.looking = if delta > 0.0 {
                // The wheel's positive direction is "towards the start", which
                // is further back through the history.
                (shell.looking + rows).min(history)
            } else {
                shell.looking.saturating_sub(rows)
            };
        }
    }
    // **The number is read before the log line is written.** Reading it inside
    // the `format!` borrows the cache a second time while the borrow that is
    // writing the line is still held, and a `RefCell` says so by ending the
    // program — which is what every turn of the wheel did (書き手の報告,
    // 2026-09-06「スクロールすると強制終了しました」).
    let back = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.looking)
        .unwrap_or(0);
    live.cache.borrow_mut().log_diag(
        "terminal",
        &format!(
            "scroll pane={} spot={spot:?} delta={delta:.0} rows={rows} back={back}",
            id.log_name()
        ),
    );
    refresh_terminal(window, &live.cache, id, spot);
}

/// Pick out cells with the mouse (追加要件 Terminal).
///
/// **The rows are counted from the start of the history**, not from the top of
/// the screen: output arriving while a selection stands would otherwise carry it
/// up the screen, and scrolling back would lose it.
fn select_in_terminal(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    spot: TerminalSpot,
    x: f32,
    y: f32,
    phase: SelectionPhase,
) {
    let Some(session) = cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| shell.session.clone())
    else {
        return;
    };
    let look = terminal_look(window);
    let Ok(cell) = cells::terminal_cell_size(&look) else {
        return;
    };
    let extent = match spot {
        TerminalSpot::Front => id.shown_height(window),
        TerminalSpot::Below => cache.borrow_mut().pane(id).below_height,
    };
    let (columns, rows) = cell.grid_for(id.shown_width(window), extent);
    let (history, looking) = {
        let history = session.borrow().screen().scrollback().len();
        let looking = cache
            .borrow_mut()
            .pane(id)
            .shell(spot)
            .map(|shell| shell.looking)
            .unwrap_or(0);
        (history, looking)
    };
    let top = history.saturating_sub(looking);
    let row = top + ((y.max(0.0) / cell.line) as usize).min(rows.saturating_sub(1));
    // **One past the last column is a place too**: a drag that ends past the end
    // of a line means the whole line, which is what dragging down a screen of
    // output has to mean.
    let column = ((x.max(0.0) / cell.advance).round() as usize).min(columns);
    {
        let mut borrowed = cache.borrow_mut();
        let Some(shell) = borrowed.pane(id).shell(spot) else {
            return;
        };
        match phase {
            SelectionPhase::Begin => {
                shell.selection = Some(TerminalSelection {
                    anchor: (row, column),
                    head: (row, column),
                });
            }
            SelectionPhase::Extend | SelectionPhase::Update | SelectionPhase::End => {
                if let Some(selection) = &mut shell.selection {
                    selection.head = (row, column);
                }
            }
        }
        // A click that picked nothing takes the last selection away with it.
        if matches!(phase, SelectionPhase::End)
            && shell.selection.is_some_and(TerminalSelection::is_empty)
        {
            shell.selection = None;
        }
    }
    refresh_terminal(window, cache, id, spot);
}

/// What the writer picked out, as text (追加要件 Terminal・要件 11.2).
///
/// **A wrapped line is one line.** The screen broke it because the pane is that
/// wide; pasting it back with a newline in the middle would run half a command.
fn terminal_selection_text(session: &TerminalSession, selection: TerminalSelection) -> String {
    let screen = session.screen();
    let history = screen.scrollback().len();
    let (first, last) = selection.ordered();
    let mut text = String::new();
    for row in first.0..=last.0 {
        let Some(line) = (if row < history {
            screen.scrollback().get(row)
        } else {
            screen.line(row - history)
        }) else {
            continue;
        };
        let Some((from, to)) = selection.columns_in(row, line.cells.len()) else {
            continue;
        };
        let mut taken = String::new();
        for cell in &line.cells[from..to.min(line.cells.len())] {
            if !cell.trailing {
                taken.push(cell.text);
            }
        }
        // Trailing blanks are the paper the terminal is written on, not spaces
        // anybody typed.
        while taken.ends_with(' ') {
            taken.pop();
        }
        text.push_str(&taken);
        if row < last.0 && !line.wrapped {
            text.push('\n');
        }
    }
    text
}

/// 要件 11.2 for a shell: hand what is picked out to the clipboard.
fn copy_terminal_selection(window: &AppWindow, live: &Live, id: PaneId, spot: TerminalSpot) {
    let picked = {
        let mut borrowed = live.cache.borrow_mut();
        borrowed.pane(id).shell(spot).and_then(|shell| {
            shell
                .selection
                .map(|selection| (shell.session.clone(), selection))
        })
    };
    let Some((session, selection)) = picked else {
        return;
    };
    let text = terminal_selection_text(&session.borrow(), selection);
    if text.is_empty() {
        return;
    }
    if !clipboard::put_text(ime::window_handle(window), &text) {
        window.set_render_status("クリップボードへ渡せませんでした".into());
    }
}

/// Which strip this pane can have, from what is in front of it (追加要件
/// Terminal). 0 nothing, 1 a shell under a document, 2 a draft under a shell.
fn below_kind(pane: &mut Pane) -> i32 {
    if !pane.below_open {
        return 0;
    }
    if pane.terminal.is_some() { 2 } else { 1 }
}

/// Open or close the strip along the foot of a pane (追加要件 Terminal).
///
/// **Closing does not end the shell in it.** A panel put away is not a command
/// abandoned, and the writer who opens it again expects to find what they left.
fn toggle_below(window: &AppWindow, live: &Live, id: PaneId) {
    let (open, needs_shell) = {
        let mut borrowed = live.cache.borrow_mut();
        let pane = borrowed.pane(id);
        pane.below_open = !pane.below_open;
        (
            pane.below_open,
            pane.below_open && pane.terminal.is_none() && pane.below.is_none(),
        )
    };
    if needs_shell && !open_below_shell(window, live, id) {
        live.cache.borrow_mut().pane(id).below_open = false;
        return;
    }
    let (kind, height) = {
        let mut borrowed = live.cache.borrow_mut();
        let pane = borrowed.pane(id);
        (below_kind(pane), pane.below_height)
    };
    id.set_below(window, kind, height);
    if open {
        // **The keyboard follows the strip.** Opening it to type into it and
        // then having to click is a gesture with a hole in the middle.
        id.update_screen(window, |screen| {
            screen.below_focus_generation += 1;
        });
    } else {
        restore_editor_focus(window);
    }
    live.cache.borrow_mut().log_diag(
        "terminal",
        &format!("below pane={} kind={kind} open={open}", id.log_name()),
    );
    store_below_on_tab(window, live, id);
    if kind == 1 {
        refresh_terminal(window, &live.cache, id, TerminalSpot::Below);
    }
}

/// Start the shell that stands in a pane's strip (追加要件 Terminal).
///
/// **The default one**: the writer asked for a terminal under what they are
/// writing, not for a distribution.
fn open_below_shell(window: &AppWindow, live: &Live, id: PaneId) -> bool {
    let height = live.cache.borrow_mut().pane(id).below_height;
    // 追加要件 2026-09-07: **the writer's default**, not WSL because WSL was
    // first. The strip has no room to ask and no name to show, so the one thing
    // it can be right about is being the same shell as everything else.
    let shell = shell_at(window, window.get_default_shell());
    let Some(session) = start_shell(window, live, id, &shell, height) else {
        return false;
    };
    live.cache.borrow_mut().pane(id).below = Some(TerminalView::new(session));
    store_below_on_tab(window, live, id);
    true
}

/// Write the strip's state back onto the tab in front (追加要件 Terminal).
///
/// **The pane holds it while it is on screen; the tab holds it between times.**
/// Same as everything else about a view (要件 7.6).
fn store_below_on_tab(window: &AppWindow, live: &Live, id: PaneId) {
    let (open, height, shell) = {
        let mut borrowed = live.cache.borrow_mut();
        let pane = borrowed.pane(id);
        (
            pane.below_open,
            pane.below_height,
            pane.below.as_ref().map(|view| view.session.clone()),
        )
    };
    let draft = id.screen(window).below_draft.to_string();
    let mut tabs = live.tabs.borrow_mut();
    let strip = tabs.of_mut(id);
    let active = strip.active;
    if let Some(tab) = strip.tabs.get_mut(active) {
        tab.below.open = open;
        tab.below.height = height;
        if tab.terminal.is_some() {
            tab.below.draft = draft;
        } else {
            tab.below.shell = shell;
        }
    }
}

/// The writer dragged the boundary above the strip.
fn resize_below(window: &AppWindow, live: &Live, id: PaneId, height: f32) {
    let (least, most) = TERMINAL_BELOW_RANGE;
    // The pane has to keep something of itself: a strip dragged over the whole
    // of it would leave the document with nothing.
    let most = most.min(id.shown_height(window) + live.cache.borrow_mut().pane(id).below_height);
    let height = height.clamp(least, most.max(least));
    let kind = {
        let mut borrowed = live.cache.borrow_mut();
        let pane = borrowed.pane(id);
        pane.below_height = height;
        below_kind(pane)
    };
    id.set_below(window, kind, height);
    store_below_on_tab(window, live, id);
    if kind == 1 {
        refresh_terminal(window, &live.cache, id, TerminalSpot::Below);
    }
}

/// 追加要件 Terminal: send the draft to the shell the pane is showing.
///
/// **The text and nothing else.** It lands at the prompt as if it had been
/// typed, and what happens next is the writer's — they can read it, change it,
/// and press Return when they mean it (書き手の指摘, 2026-09-06: 「lsのままで
/// 実行は行われないはず」). The draft is emptied behind it, because what has
/// been sent is in the way of what comes next.
/// The draft cut into the lines the strip draws (書き手の報告 2026-09-07).
///
/// **Everything in the draft is sent, so everything in it is coloured**
/// (書き手の報告 2026-09-07・4回目). Which is more than pedantry: `ls` and
/// `ls` with a return look the same on screen and do different things — one
/// waits at the prompt, one runs — and the difference is *the empty line under
/// it*. Colouring that line is the only thing on screen that can say so, now
/// that the newline has no character of its own. The last line of `ls\n` holds
/// nothing and is still part of what goes.
///
/// An empty draft colours nothing, because nothing is what it would send.
fn draft_lines(draft: &str) -> Vec<DraftLine> {
    let sent = !draft.is_empty();
    let mut lines = Vec::new();
    let mut at = 0;
    for piece in draft.split('\n') {
        let end = at + piece.len();
        lines.push(DraftLine {
            // The carriage return of a pasted CRLF is not a character the
            // writer put there, and drawing it would push the rule a column
            // out.
            text: piece.trim_end_matches('\r').into(),
            sent,
            breaks: end < draft.len(),
        });
        at = end + 1;
    }
    lines
}

/// Put a draft, and the lines it is made of, in front of one pane.
fn show_draft(window: &AppWindow, id: PaneId, draft: &str) {
    let lines = ModelRc::new(VecModel::from(draft_lines(draft)));
    id.update_screen(window, |screen| {
        screen.below_draft = draft.into();
        screen.below_draft_lines = lines.clone();
    });
}

fn send_draft(window: &AppWindow, live: &Live, id: PaneId) {
    let text = id.screen(window).below_draft.to_string();
    if text.is_empty() {
        return;
    }
    let Some(session) = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(TerminalSpot::Front)
        .map(|shell| shell.session.clone())
    else {
        return;
    };
    // **打ったものが、打ったとおりに行く**（書き手の報告 2026-09-07）。末尾を
    // 落としていたので、下書きの終わりに置いた改行——上のコマンドを走らせる、
    // まさにその一打——だけが届かなかった。改行を置くかどうかは書き手が決める。
    //
    // **貼り付けではなく打鍵として送る**（同・3回目）。`paste`は括弧付き貼り付け
    // の印を付けるので、シェルは受け取ったものを読まずに抱える——`ls`と改行を
    // 送ってもプロンプトが下がるだけで走らなかったのはそれ。
    session.borrow_mut().send(&terminal::encode_typing(&text));
    {
        let mut borrowed = live.cache.borrow_mut();
        if let Some(shell) = borrowed.pane(id).shell(TerminalSpot::Front) {
            shell.looking = 0;
        }
    }
    show_draft(window, id, "");
    store_below_on_tab(window, live, id);
    refresh_terminal(window, &live.cache, id, TerminalSpot::Front);
}

/// Every shell on screen, drawn again (追加要件 Terminal).
///
/// **Rung by the reading thread**, which knows only that bytes arrived. Which
/// pane they belong to is a question for this side, and asking every pane is
/// cheaper than carrying an answer across a thread that could be stale by the
/// time it lands.
fn refresh_terminal_panes(window: &AppWindow, live: &Live) {
    for id in PaneId::all(window) {
        if live.cache.borrow_mut().pane(id).terminal.is_some() {
            refresh_terminal(window, &live.cache, id, TerminalSpot::Front);
        }
        if below_kind(live.cache.borrow_mut().pane(id)) == 1 {
            refresh_terminal(window, &live.cache, id, TerminalSpot::Below);
        }
    }
}

fn refresh_pane(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    id: PaneId,
    source: &str,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection: PaneSelection,
    preedit: &str,
) {
    // **A pane showing a shell is not laying anything out** (追加要件
    // Terminal). None of what follows applies: there is no document to measure,
    // no wrapping to search and no caret of the editor's to place. The check is
    // here rather than at each of the twenty callers, because what they all
    // have in common is that they end up here.
    if cache.borrow_mut().pane(id).terminal.is_some() {
        refresh_terminal(window, cache, id, TerminalSpot::Front);
        return;
    }
    // The strip along the foot, if this pane is showing one, is drawn whatever
    // the document above it is doing (追加要件 Terminal). **Only when it is a
    // shell**: a shell held behind a draft goes on running, and drawing it
    // would be drawing something nobody can see.
    if below_kind(cache.borrow_mut().pane(id)) == 1 {
        refresh_terminal(window, cache, id, TerminalSpot::Below);
    }
    // **A strip showing with nothing behind it is a hole**, and the writer
    // cannot see that it is one: it looks like a terminal that has stopped
    // answering. Whatever left it that way, one is started here.
    if below_kind(cache.borrow_mut().pane(id)) == 1 && cache.borrow_mut().pane(id).below.is_none() {
        cache.borrow_mut().log_diag(
            "terminal",
            &format!("below pane={} lost its shell", id.log_name()),
        );
    }
    let refresh_started = Instant::now();
    // Read once and logged: how far this pane is magnified is half of why a
    // draw cost what it did, and the two panes may be set differently now.
    let zoom_percent = id.zoom(window);
    let typography = pane_typography(window, id);
    let font_size = typography.font_size;
    let line_fit = id.line_fit(window, &typography);
    let mut borrowed = cache.borrow_mut();
    // Reborrow once so the field accesses below are disjoint. Going through
    // `RefMut` for each of them would borrow the whole cache every time.
    let cache = &mut *borrowed;

    let laid_out = lay_out_pane(
        window,
        cache,
        document,
        id,
        source,
        line_fit,
        &typography,
        active_line_start,
        caret_source_byte,
        selection,
        preedit,
    );
    let Some(laid_out) = laid_out else {
        return;
    };
    let PaneLayout {
        render_caret,
        anchor_utf16,
        selection,
        selection_source,
        matches,
        scope,
        previous_flow,
        measured,
        preview_ms,
        layout_ms,
    } = laid_out;

    let (content_flow, line_extent) = {
        let engine = &cache.pane(id).graphics.engine;
        (engine.total_flow_size(), engine.line_extent())
    };
    // Vertical text anchors the document's start at the right edge, so blocks
    // are placed right to left and a new column widens the content there: the
    // text *before* the edit slides right unless the viewport slides with it.
    // Keeping the distance from that edge constant leaves the earlier text where
    // it was and lets the later text flow leftwards, which is the direction
    // Japanese vertical text actually grows. This used to be skipped whenever a
    // caret existed, so it never ran while editing.
    //
    // The horizontal pane wants none of it. That document starts at the top and
    // grows downwards, so a new line moves nothing that is already above it.
    if id.vertical(window) && previous_flow > 0 && previous_flow != content_flow {
        let scrolled = scroll_after_content_resize(
            id.scroll(window),
            id.shown_flow(window),
            previous_flow as f32,
            content_flow as f32,
        );
        id.set_scroll(window, scrolled);
    }
    id.set_content_size(window, content_flow, line_extent);

    // 要件 8.5: put the view back where the writer left it, in spite of the
    // layout changing size under it. **Every refresh until the caret moves**,
    // because the window settles into its zoom, its split and its extent over
    // the first few of them, and each of those changes what a pixel offset
    // means (`ViewAnchor`).
    let anchored = anchor_utf16.and_then(|at| {
        let engine = &mut cache.pane(id).graphics.engine;
        let place = engine.caret_geometry(at).ok()?;
        let flow = if id.vertical(window) {
            place.x
        } else {
            place.y
        };
        // **Never past either end.** The content is measured several times
        // before the window stands still, and a place found in one of the
        // earlier ones can sit beyond the last — a view pinned there shows
        // paper, which is what "the top is cut off" looks like.
        let last = (content_flow as f32 - id.shown_flow(window)).max(0.0);
        Some(flow.clamp(0.0, last))
    });
    if let Some(flow) = anchored {
        id.set_scroll(window, -flow);
    }

    let geometry_started = Instant::now();
    let visible = id.flow_range(window, content_flow as f32);
    // Both results are bound to locals first: a call left in a `match` scrutinee
    // keeps its borrow of the cache alive through every arm.
    let caret_result = {
        let engine = &mut cache.pane(id).graphics.engine;
        match render_caret {
            Some(position) => engine.caret_geometry(position).map(Some),
            None => Ok(None),
        }
    };
    let caret = match caret_result {
        Ok(caret) => caret,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}座標計算: NG / {error}").into());
            update_status(
                window,
                id,
                document,
                source,
                &selection_source,
                source_caret(window, id, caret_source_byte),
            );
            return;
        }
    };
    let selection_result = {
        let engine = &mut cache.pane(id).graphics.engine;
        // One ask per run: an ordinary selection is one, a rectangle is one per
        // line it covers (要件 7.1), and a run off screen returns at once.
        let mut rects = Vec::new();
        let mut outcome = Ok(());
        for run in &selection {
            match engine.selection_rects(Some(*run), visible) {
                Ok(found) => rects.extend(found),
                Err(error) => outcome = Err(error),
            }
        }
        outcome.map(|()| rects)
    };
    let selection_rects = match selection_result {
        Ok(rects) => rects,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}選択座標: NG / {error}").into());
            update_status(
                window,
                id,
                document,
                source,
                &selection_source,
                source_caret(window, id, caret_source_byte),
            );
            return;
        }
    };
    // E1（書き手の求め 2026-09-09）: **見えている一致を薄く出す。**いま選ばれて
    // いる一致は選択そのものが強く出しているので、ここは同じ色の弱いほうを
    // 全部に敷く——「どこにあるか」と「いまどれにいるか」が一目で分かる。
    //
    // **画面の外の走りはその場で空を返す**ので、上限まで訊いても文書の長さには
    // 効かない。
    let match_rects = {
        let engine = &mut cache.pane(id).graphics.engine;
        let mut rects = Vec::new();
        for run in &matches {
            if let Ok(found) = engine.selection_rects(Some(*run), visible) {
                rects.extend(found);
            }
        }
        rects
    };
    id.set_matches(window, &match_rects);
    let scope_rects = {
        let engine = &mut cache.pane(id).graphics.engine;
        match scope {
            Some(run) => engine
                .selection_rects(Some(run), visible)
                .unwrap_or_default(),
            None => Vec::new(),
        }
    };
    id.set_scope(window, &scope_rects);
    let rects = selection_rects.len();
    apply_pane_geometry(
        window,
        &mut cache.diag,
        id,
        content_flow as f32,
        caret,
        &selection_rects,
        anchored.is_some(),
    );
    let geometry_ms = elapsed_ms(geometry_started);

    // Prefetch here too. Moving the caret scrolls the pane to keep it visible,
    // and without a tile in hand on the leading edge every such scroll stalls on
    // a rasterization. Tiles whose content did not change are already cached, so
    // the neighbour usually costs nothing.
    let tiles_started = Instant::now();
    let tiles = cache.refresh_pane_tiles(window, id, TILE_PREFETCH_COUNT);
    let tiles_ms = elapsed_ms(tiles_started);

    // Counting lines and characters walks the source again; timed here so its
    // share of a keystroke is visible rather than assumed. The count belongs to
    // the document, so in Split whichever pane acted last owns it.
    let stats_started = Instant::now();
    let place = source_caret(window, id, caret_source_byte);
    update_status(window, id, document, source, &selection_source, place);
    // 要件 7.7: the outline is of the document in front of the writer, and the
    // pane that has just drawn is only sometimes the one they are in.
    if id == focused_pane(window) {
        draw_outline_if_showing(window, cache, source);
    }
    let stats_ms = elapsed_ms(stats_started);

    let (tile_count, tile_want, rendered, tiles_reused, spare_held) = match tiles {
        Ok(counts) => counts,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}遅延タイル: NG / {error}").into());
            return;
        }
    };

    let blocks = cache.pane(id).graphics.engine.block_count();
    let max_block = cache.pane(id).graphics.engine.largest_block_utf16();
    // 要件 7.3.2: how many blocks a list item's own indent would add, if it got
    // the one a quote has. A number about real documents, not an argument.
    // **Both halves**: every item, then the ones that take more than one line
    // and are therefore the only ones an indent does anything for.
    let items = cache.pane(id).graphics.engine.list_items();
    let wrapping_items = cache.pane(id).graphics.engine.wrapping_items();
    let upload_kb = cache.pane(id).graphics.uploaded_bytes / 1024;
    cache.pane(id).graphics.uploaded_bytes = 0;
    let caret_at = cache
        .pane(id)
        .view
        .caret_utf16
        .map(|position| position as i64)
        .unwrap_or(-1);
    // What the pane says it shows, where it is scrolled to, and where the caret
    // ended up. Both extents, because they are deliberately different numbers
    // and every figure derived from them is quietly wrong when the pane has not
    // reported its size (6.7, 6.16).
    let shown_flow = id.shown_flow(window);
    let viewport_flow = id.viewport_flow(window);
    let scroll = id.scroll(window);
    // Where the candidate list was told to open, and how far the caret was from
    // the pane's own edge. The second is what an anchor that moves with the
    // available room would key off; it is logged because deciding that from a
    // threshold was tried and did not work (7.2).
    let ime_at = id.ime_anchor_flow(window);
    let ime_room = viewport_flow - (id.caret_flow(window) + scroll);
    let preedit_chars = preedit.chars().count();
    let measured_utf16 = measured.utf16;
    let wrapped = measured.wrapped;
    let wrap_asked = measured.wrap_asked;
    let wrap_exact = measured.wrap_exact;
    let wrap_resumed = measured.wrap_resumed;
    let wrap_divided = measured.wrap_divided;
    let wrap_shared = measured.wrap_shared;
    let wrap_starts = measured.wrap_starts;
    let divided = measured.divided;
    let measured = measured.blocks;

    let total_ms = elapsed_ms(refresh_started);
    let push_ms = cache.source_push_ms.unwrap_or(0.0);
    // The frames these numbers describe are the ones the *previous* refresh
    // caused: Slint renders after the callback returns, not during it. Whichever
    // pane refreshes takes the frames since the last one, so in Split the two
    // share them out rather than double-counting. The horizontal pane reports
    // them too, because horizontal-only display could otherwise not be measured
    // at all — no line was written there, so no frame was ever counted (7.5).
    let frame = cache.frames.borrow_mut().take();
    // One status line for every pane, and **the focused one owns it**
    // (2026-09-06). Panes showing the same document all refresh on the same
    // keystroke, so letting each write would leave the line flickering between
    // as many sets of numbers as there are panes. The rule used to be "the
    // right-hand pane", which was the same rule while there were two of them
    // and the writer was always in one. Short enough to survive an unwrapped
    // narrow pane; the full breakdown goes to the log, where nothing is clipped
    // and every pane has a line of its own.
    if id == focused_pane(window) {
        window.set_render_status(
            format!("縦書き {total_ms:.1}ms / tiles {tiles_ms:.1}ms {tile_count}枚{rendered}新 / 横 {push_ms:.1}ms")
                .into(),
        );
    }
    // Taken before the line is built: the log borrows the cache for the whole
    // of it.
    let held = cache.pace_of(id).take_held();
    cache.log_perf(&format!(
        "{kind} total={total_ms:.2} preview={preview_ms:.2} layout={layout_ms:.2} \
         geom={geometry_ms:.2} tiles={tiles_ms:.2} stats={stats_ms:.2} push={push_ms:.2} \
         frames={frames} frame_min={frame_min:.2} frame_med={frame_med:.2} \
         frame_max={frame_max:.2} frame_slow={frame_slow} gap_med={gap_med:.2} \
         upload_kb={upload_kb} split={split} mode={mode} zoom={zoom_percent} \
         blocks={blocks} items={items}/{wrapping_items} measured={measured}/{divided} \
         measured_utf16={measured_utf16} \
         wrapped={wrapped} wrap={wrap_asked}/{wrap_exact}/{wrap_resumed}/{wrap_divided} \
         miss={wrap_shared}/{wrap_starts} max_block={max_block} \
         content={content_flow} extent={line_extent} shown={shown_flow:.0} \
         viewport={viewport_flow:.0} scroll={scroll:.0} caret={caret_at} \
         ime={ime_at:.0}/{ime_room:.0} \
         held={held} \
         tiles_shown={tile_count} tiles_new={rendered}/{tiles_reused} spare={spare_held} \
         rects={rects} font={font_size:.1} \
         space={space:.2} lead={lead:.2} head={head:.2} preedit={preedit_chars}",
        kind = id.log_name(),
        // How many edits this draw is carrying (`EditPace`). One is the
        // ordinary case; more means a held key was outrunning the drawing.
        held = held,
        // How many panes are alive, and which one has the keyboard. Without
        // these the log cannot tell a slow frame caused by another pane from
        // one caused by a large document, because the two arrive together.
        split = PaneId::count(window),
        mode = focused_pane(window).index(),
        space = typography.character_spacing,
        lead = typography.line_spacing,
        head = typography.size_scale(1),
        frames = frame.frames,
        frame_min = frame.min_ms,
        frame_med = frame.median_ms,
        frame_max = frame.max_ms,
        frame_slow = frame.slow,
        gap_med = frame.gap_median_ms,
    ));
    cache.log_diag(
        &format!("refresh.{}", id.diag_suffix()),
        &format!(
            "content={content_flow} extent={line_extent} shown={shown_flow:.0} \
             viewport={viewport_flow:.0} scroll={scroll:.0} hold={hold} blocks={blocks} \
             measured={measured} tiles={tile_count}/{tile_want} new={rendered} caret={caret_at} \
             preview={preview} mode={mode} split={split} zoom={zoom_percent}",
            // 要件 8.5: where the view is being held, if it is. **Written down
            // because a view in the wrong place says nothing about why** — a
            // hold that resolved somewhere odd and a hold that never stood look
            // the same on screen.
            hold = match anchored {
                Some(flow) => format!("{flow:.0}"),
                None => "-".to_owned(),
            },
            preview = u8::from(id.shows_preview(window)),
            mode = focused_pane(window).index(),
            split = PaneId::count(window),
        ),
    );
}

/// Place the caret and the selection a refresh produced, and scroll along the
/// flow to keep the caret in view.
fn apply_pane_geometry(
    window: &AppWindow,
    diag: &mut DiagLog,
    id: PaneId,
    content_flow: f32,
    caret: Option<CaretGeometry>,
    selection_rects: &[SelectionRect],
    held: bool,
) {
    id.set_selection(window, selection_rects);
    id.set_caret(window, caret.as_ref());
    let Some(caret) = caret else {
        return;
    };
    // 要件 8.5: while the view is being held where the writer left it, the
    // caret does not drag it away. **The caret is where they left it too** —
    // following it would be answering a question nobody asked, and it is what
    // took the restored view away before (`ViewAnchor`).
    if held {
        let (ime_x, ime_y) = ime_candidate_anchor(&caret, id.vertical(window));
        id.set_ime_anchor(window, ime_x, ime_y, &caret);
        return;
    }
    let (caret_flow, caret_size) = if id.vertical(window) {
        (caret.x, caret.width)
    } else {
        (caret.y, caret.height)
    };
    let was = id.scroll(window);
    let viewport = id.viewport_flow(window);
    let scroll = caret_visible_scroll(was, viewport, content_flow, caret_flow, caret_size);
    // Both extents, because they differ on purpose and the difference is
    // invisible in every other figure: the tile side errs high and this side
    // must not (6.16).
    diag.write(
        &format!("geom.{}", id.diag_suffix()),
        &format!(
            "content={content_flow:.0} viewport={viewport:.0} shown={shown:.0} \
             scroll={was:.0}->{scroll:.0} caret_x={x:.0} caret_y={y:.0} \
             caret_w={w:.0} caret_h={h:.0}",
            shown = id.shown_flow(window),
            x = caret.x,
            y = caret.y,
            w = caret.width,
            h = caret.height,
        ),
    );
    id.set_scroll(window, scroll);
    // 要件 9: the line may be longer than the pane is wide — the writer named a
    // length — and then the caret has to be chased across the flow as well.
    // **The same rule and the same arithmetic**: which axis is the flow is the
    // only thing that differs, and a caret that has run off the side is off the
    // screen exactly as one that has run off the end. When the sheet fits, the
    // content is no larger than the viewport and this answers zero.
    let (caret_across, across_size) = if id.vertical(window) {
        (caret.y, caret.height)
    } else {
        (caret.x, caret.width)
    };
    let across = caret_visible_scroll(
        id.scroll_across(window),
        id.shown_across_flow(window),
        id.content_across(window),
        caret_across,
        across_size,
    );
    id.set_scroll_across(window, across);
    let (ime_x, ime_y) = ime_candidate_anchor(&caret, id.vertical(window));
    id.set_ime_anchor(window, ime_x, ime_y, &caret);
}

/// Redraw a pane after it was scrolled.
///
/// No tile is regenerated unless the viewport reached one it does not hold, and
/// the selection is re-cut because its rectangles are clipped to what is on
/// screen. This is a hit test over the visible blocks only.
fn refresh_after_scroll(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    offset: f32,
) {
    // **A shell has no document to cut tiles from** (追加要件 Terminal). This is
    // the one path that draws a pane without going through [`refresh_pane`], so
    // it is also the one place the check had to be repeated — and the cost of
    // leaving it out was a terminal that opened showing the file from the tab
    // beside it, until a click redrew it (書き手の報告, 2026-09-06). Opening a
    // terminal changes the pane's content size, the pane reports a scroll, and
    // this drew the document straight over it.
    if cache.borrow_mut().pane(id).terminal.is_some() {
        refresh_terminal(window, cache, id, TerminalSpot::Front);
        return;
    }
    let started = Instant::now();
    let label = id.label(window);
    // **Where the pane says it is, not where its row says it is.** The row is
    // written by the two-way binding and this is raised by the `changed`
    // handler, and nothing orders those two against each other — reading the
    // row first cuts tiles for where the pane was a moment ago, which leaves
    // the newly uncovered strip with no tile until something else redraws it.
    id.record_scroll(window, offset);
    let mut cache = cache.borrow_mut();
    // **The writer has scrolled, so this is where they want to look now**
    // (要件 8.5). The other way a hold ends is the caret moving, which the
    // layout pass notices for itself (`ViewAnchor`).
    cache.pane(id).view.top_anchor = None;
    let drawn = cache.refresh_pane_tiles(window, id, TILE_PREFETCH_COUNT);
    let placed = match &drawn {
        Ok((count, want, new, _, _)) => {
            if *new > 0 {
                let ms = elapsed_ms(started);
                let line = format!("{label}遅延スクロール: {count}枚{new}新 / {ms:.1}ms");
                window.set_render_status(line.into());
            }
            format!("tiles={count}/{want} new={new}")
        }
        Err(error) => {
            let line = format!("{label}遅延タイル: NG / {error}");
            window.set_render_status(line.into());
            "tiles=-".to_owned()
        }
    };
    if let Err(error) = cache.refresh_pane_selection(window, id) {
        window.set_render_status(format!("{label}選択座標: NG / {error}").into());
    }
    // **Only when something was drawn.** A wheel raises one of these every few
    // pixels, and a line per notch buries the run that mattered.
    if drawn.map(|counts| counts.2).unwrap_or(0) > 0 {
        cache.log_diag(
            &format!("scroll.{}", id.diag_suffix()),
            &format!("at={offset:.0} {placed}"),
        );
    }
}

fn usable_horizontal_width(width: f32) -> u32 {
    if width.is_finite() && width >= MIN_HORIZONTAL_WIDTH as f32 {
        width as u32
    } else {
        HORIZONTAL_WIDTH
    }
}

/// A pane's report of its own size, held to what the tree gave it.
///
/// **No bound at all before the pane has been given anything.** A tree that has
/// not been handed an area yet gives out zeroes, and bounding a report by zero
/// would stop the first refresh from scrolling anywhere (6.19).
fn bounded_extent(reported: f32, given: f32) -> f32 {
    if !given.is_finite() || given <= 0.0 {
        return reported;
    }
    reported.min(given)
}

/// The nearest character boundary at or before `byte`.
///
/// A pane holds its caret and selection as source byte positions, and the *other*
/// pane can edit the document underneath them. A held position can therefore end
/// up inside a character, and every slice taken from it would panic. This is the
/// one place that fact is dealt with. (`str::floor_char_boundary` does exactly
/// this but is still unstable.)
fn floor_char_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// UTF-16 offset of a source byte position. The horizontal pane draws the source
/// itself, so this is the whole of its position mapping.
/// The text one pane lays out, and how positions in it relate to the document.
///
/// The vertical pane lays out the formatted preview and has to map back to the
/// Markdown behind it. The horizontal pane lays out the Markdown itself, where
/// that map is the identity. **3.7 said the difference between the modes would
/// come down to which of these a pane is handed, and this is that.**
///
/// Everything a pane does with positions goes through here, so adding the third
/// mode is a matter of handing the horizontal pane the other one.
#[derive(Clone, Copy)]
enum PaneText<'a> {
    /// The Markdown itself. A position in it is a position in the document.
    Source(&'a str),
    /// The formatted preview, with its map back to the document.
    Preview(&'a PreviewDocument),
}

impl<'a> PaneText<'a> {
    /// What the engine lays out.
    fn text(&self) -> &'a str {
        match self {
            Self::Source(source) => source,
            Self::Preview(preview) => &preview.text,
        }
    }

    /// What each of its lines has marked inside it (要件 7.3.2).
    ///
    /// **Only the preview has any.** A source pane shows the markers
    /// themselves, so nothing has been hidden and there is nothing to mark.
    fn marks(&self) -> &'a [Vec<Emphasis>] {
        match self {
            Self::Source(_) => &[],
            Self::Preview(preview) => preview.marks(),
        }
    }

    /// The marker standing at the head of each of its lines (要件 7.3.2).
    ///
    /// **Only the preview has any**, for a stronger reason than `marks`: a box
    /// hides the glyphs it stands over, and on a source pane those glyphs are
    /// the characters being edited.
    fn markers(&self) -> &'a [Option<LineMarker>] {
        match self {
            Self::Source(_) => &[],
            Self::Preview(preview) => preview.markers(),
        }
    }

    /// Which line is shown as its own source, if one is (要件 7.3.1).
    ///
    /// **A source pane says none.** Every line there is its own source, and
    /// nothing is put over any of them — so there is no one line to name.
    fn source_line(&self) -> Option<usize> {
        match self {
            Self::Source(_) => None,
            Self::Preview(preview) => preview.active_line(),
        }
    }

    /// Where a document position sits in what the pane shows.
    fn utf16_at_source_byte(&self, byte: usize) -> usize {
        match self {
            Self::Source(source) => utf16_at_byte(source, byte),
            Self::Preview(preview) => preview.utf16_at_source_byte(byte),
        }
    }

    /// Where a position in what the pane shows sits **within that text**.
    ///
    /// Not the same question as `source_byte_at_utf16`: this one stays inside
    /// the shown text, and is what splicing the IME's string into it needs.
    fn shown_byte_at_utf16(&self, utf16: usize) -> usize {
        match self {
            Self::Source(source) => byte_at_utf16(source, utf16),
            Self::Preview(preview) => preview.preview_byte_at_utf16(utf16),
        }
    }

    /// Where a position in what the pane shows sits in the document.
    fn source_byte_at_utf16(&self, utf16: usize) -> usize {
        match self {
            Self::Source(source) => byte_at_utf16(source, utf16),
            Self::Preview(preview) => preview.source_byte_at_utf16(utf16),
        }
    }

    /// The document position one grapheme cluster before `byte`, as the pane
    /// sees clusters. What counts as one is decided in the text being shown,
    /// not in the Markdown behind it.
    fn previous_grapheme(&self, byte: usize) -> usize {
        match self {
            Self::Source(source) => previous_grapheme_byte(source, byte),
            Self::Preview(preview) => {
                let at = preview.utf16_at_source_byte(byte);
                preview.source_byte_at_utf16(preview.previous_grapheme_position(at))
            }
        }
    }

    fn next_grapheme(&self, byte: usize) -> usize {
        match self {
            Self::Source(source) => next_grapheme_byte(source, byte),
            Self::Preview(preview) => {
                let at = preview.utf16_at_source_byte(byte);
                preview.source_byte_at_utf16(preview.next_grapheme_position(at))
            }
        }
    }
}

fn utf16_at_byte(text: &str, byte: usize) -> usize {
    text[..floor_char_boundary(text, byte)]
        .encode_utf16()
        .count()
}

/// The source byte a UTF-16 offset lands on, rounded down to a character start.
fn byte_at_utf16(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, character) in text.char_indices() {
        if units >= utf16 {
            return index;
        }
        units += character.len_utf16();
    }
    text.len()
}

fn previous_grapheme_byte(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_grapheme_byte(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..]
        .graphemes(true)
        .next()
        .map(|grapheme| byte + grapheme.len())
        .unwrap_or(byte)
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn scroll_after_content_resize(
    viewport_x: f32,
    visible_width: f32,
    previous_width: f32,
    next_width: f32,
) -> f32 {
    if next_width <= visible_width {
        return 0.0;
    }
    let previous_right = -viewport_x + visible_width;
    let distance_from_right = (previous_width - previous_right).max(0.0);
    let next_right = (next_width - distance_from_right).max(visible_width);
    (visible_width - next_right).clamp(visible_width - next_width, 0.0)
}

/// 要件 10: the caret's place in the file, when the pane that just acted is
/// showing the file.
///
/// **The preview has no column to give.** The caret there sits in text the
/// markup has been taken out of, so a column counted from it would name a place
/// the file does not have — which is why 要件 10 asks for this of source
/// editing and not of the other two modes.
fn source_caret(window: &AppWindow, id: PaneId, caret: Option<usize>) -> Option<usize> {
    (!id.shows_preview(window)).then_some(caret).flatten()
}

fn update_status(
    window: &AppWindow,
    id: PaneId,
    document: &OpenDocument,
    source: &str,
    selection_source_bytes: &[(usize, usize)],
    source_caret: Option<usize>,
) {
    // E1: **探している語の数も、ここで数え直す。**打鍵のたびに通る道なので、
    // ステータスバーの`3 / 12`が古いままになることがない。一致の上に立って
    // いるかは選択で決まる——**矩形選択は走りが複数**なので、1本のときだけが
    // 「一致に立っている」たりうる（要件 7.1）。
    let selected = match selection_source_bytes {
        [only] => Some(*only),
        _ => None,
    };
    tell_find(window, id, source, selected);
    let stats = document.counts.borrow_mut().get(source).stats();
    // Every run, because a rectangle is several (要件 7.1) — and what 要件 10
    // shows is how much text is selected, not how many pieces it is in.
    let selected_characters: usize = selection_source_bytes
        .iter()
        .map(|(start, end)| {
            // Counting characters must never be the thing that brings the app
            // down, so the ends are walked back to character boundaries here
            // too. The paths that *change* the document stay strict.
            let start = floor_char_boundary(source, *start);
            let end = floor_char_boundary(source, *end).max(start);
            source[start..end].graphemes(true).count()
        })
        .sum();
    let long_paragraph = if stats.longest_line_characters > PARAGRAPH_WARNING_CHARACTERS {
        format!(
            "最長段落 {}文字（長すぎます）",
            stats.longest_line_characters
        )
    } else {
        String::new()
    };
    // 要件 10: stated before `source` is shadowed by its own count below.
    let caret = match source_caret {
        Some(byte) => {
            let (line, column) = caret_place(source, byte);
            format!("Ln {line}, Col {column}")
        }
        None => String::new(),
    };
    let lines = format!("{} lines", thousands(stats.logical_lines));
    // 要件 7.8・要件 10: **ルビは既定では数えない。**投稿サイトへ出すための
    // 字数はルビを含まないので、そちらを初期値にしてある。両方を数えてある
    // （`DocumentStats::ruby_characters`）ので、設定を切り替えても数え直しは
    // 起きない。
    let counted = if window.get_count_ruby() {
        stats.body_characters
    } else {
        stats.body_characters.saturating_sub(stats.ruby_characters)
    };
    let body = format!("{} chars", thousands(counted));
    let source = format!("{} source", thousands(stats.source_characters));
    let selected = format!("{} selected", thousands(selected_characters));
    window.set_count_lines(lines.into());
    window.set_count_body(body.into());
    window.set_count_source(source.into());
    window.set_count_selected(selected.into());
    window.set_count_caret(caret.into());
    window.set_count_warning(long_paragraph.into());
}

/// A count with its thousands marked, for the status bar (要件 10).
///
/// **A comma every three digits from the right**, which is what both the
/// Japanese and the English convention do with a plain number. The counts get
/// into the tens of thousands in an ordinary document, and a run of five digits
/// is not something anybody reads at a glance.
fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut marked = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.char_indices() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            marked.push(',');
        }
        marked.push(digit);
    }
    marked
}

/// Where the text being composed sits, which is what the IME places its own
/// windows against.
///
/// **In vertical writing this is simply the caret.** The IME has been told the
/// run is vertical (see [`crate::ime`]), so its candidate list runs vertically
/// too, and it can choose which side of the composition to open on. Reporting
/// the caret and leaving that choice to it is what 一太郎 and Word do.
///
/// Choosing the side here was tried first and was wrong twice over (7.2). A
/// horizontal list needs several hundred pixels of room, and in vertical
/// writing several hundred pixels is a dozen columns — so anchoring away from
/// the text put the list a dozen columns from it, and the caret crossed the
/// threshold while typing, moving the list between one keystroke and the next.
/// **The list was the wrong shape; the position was a symptom.**
///
/// Horizontal writing keeps the offset: there the list is the right shape
/// already, and below-right of the caret is where it belongs.
fn ime_candidate_anchor(caret: &directwrite_render::CaretGeometry, vertical: bool) -> (f32, f32) {
    if vertical {
        return (caret.x, caret.y);
    }
    (
        caret.x + caret.width + IME_CANDIDATE_GAP,
        caret.y + caret.height + IME_CANDIDATE_GAP,
    )
}

/// Scroll offset that keeps the caret inside the viewport, on either axis.
///
/// All of it is one-dimensional arithmetic over the flow axis, so the vertical
/// pane passes its x and the horizontal pane its y. The offset is Slint's, which
/// is negative as content scrolls past the start.
fn caret_visible_scroll(
    viewport: f32,
    visible: f32,
    content: f32,
    caret_start: f32,
    caret_size: f32,
) -> f32 {
    if visible <= 0.0 || content <= visible {
        return 0.0;
    }

    let minimum = visible - content;
    let caret_low = caret_start + viewport;
    let caret_high = caret_low + caret_size;
    let target = if caret_low < CARET_SCROLL_PADDING {
        viewport + CARET_SCROLL_PADDING - caret_low
    } else if caret_high > visible - CARET_SCROLL_PADDING {
        viewport + visible - CARET_SCROLL_PADDING - caret_high
    } else {
        viewport
    };

    target.clamp(minimum, 0.0)
}

/// What a pane has selected, as the runs its last layout cut (要件 7.1).
///
/// **Read back rather than asked again.** Cutting a rectangle into runs is a
/// question about the lines the engine laid out, and the pane has been laid out
/// by the time the writer can press the key that reads this.
fn selected_runs(cache: &Rc<RefCell<RenderCache>>, id: PaneId) -> Vec<(usize, usize)> {
    cache.borrow_mut().pane(id).view.selection_source.clone()
}

/// A rectangle's runs, one per layout line (要件 7.1).
///
/// **The two ends give a coordinate across the line each**, and everything
/// between those two coordinates, on every line between the two ends, is what
/// the rectangle holds. The coordinate is a y where the text runs down the page
/// and an x where it runs across — the same measurement the up and down keys
/// hold on to (`PaneId::line_anchor`), so a rectangle drawn by moving the caret
/// keeps the width the caret was already keeping.
fn rectangular_runs(
    window: &AppWindow,
    id: PaneId,
    engine: &mut TextEngine,
    start: u32,
    end: u32,
) -> Vec<(u32, u32)> {
    let vertical = id.vertical(window);
    let (Ok(from), Ok(to)) = (engine.caret_geometry(start), engine.caret_geometry(end)) else {
        return Vec::new();
    };
    let lo = PaneId::line_anchor(vertical, &from);
    let hi = PaneId::line_anchor(vertical, &to);
    engine
        .rectangle_runs(start, end, lo, hi)
        .unwrap_or_default()
        .into_iter()
        .map(|(start, end)| (start, end - start))
        .collect()
}

/// What a pane has selected, before it is cut into runs (要件 7.1).
///
/// **Two ends and a shape.** A run is the text between them; a rectangle is the
/// part of every layout line between them that lies between the same two
/// coordinates across the line. Which of the two it is cannot be worked out
/// from the ends, and where it is cut into runs the engine has to be asked —
/// so what travels to the layout is this, and the runs come back from there.
#[derive(Clone, Copy, Default)]
struct PaneSelection {
    ends: Option<(usize, usize)>,
    rectangular: bool,
}

fn pane_selection(state: &EditorState) -> PaneSelection {
    PaneSelection {
        ends: selection_source_range(state),
        rectangular: state.rectangular,
    }
}

impl PaneSelection {
    /// One end and the other, whatever shape it is.
    fn run(self) -> Vec<(usize, usize)> {
        self.ends.into_iter().collect()
    }
}

fn selection_source_range(state: &EditorState) -> Option<(usize, usize)> {
    let anchor = state.selection_anchor_source_byte?;
    let focus = state.caret_source_byte?;
    (anchor != focus).then_some((anchor.min(focus), anchor.max(focus)))
}

fn update_selection_after_move(
    state: &mut EditorState,
    source_byte: usize,
    next_source_byte: usize,
    extend_selection: bool,
) -> Option<(usize, usize)> {
    // **The mark is a Shift nobody is holding** (要件 11.4), so it is read in
    // the one place a move decides what the selection becomes.
    if extend_selection || state.mark {
        if state.selection_anchor_source_byte.is_none() {
            state.selection_anchor_source_byte = Some(source_byte);
        }
    } else {
        state.selection_anchor_source_byte = Some(next_source_byte);
    }
    state.caret_source_byte = Some(next_source_byte);
    selection_source_range(state)
}

/// Where in the document a point on a pane is.
///
/// The pane is laid out again first. The text it shows can have changed since
/// the last refresh — the four modes switch which text a pane holds — and a hit
/// test against a stale layout answers about the wrong document.
///
/// `None` means the engine refused; the caller has already been told.
#[allow(clippy::too_many_arguments)]
fn hit_test_pane(
    window: &AppWindow,
    cache: &mut RenderCache,
    document: &OpenDocument,
    id: PaneId,
    source: &str,
    active_line_start: Option<usize>,
    x: f32,
    y: f32,
) -> Option<PaneHit> {
    let mut counts = document.counts.borrow_mut();
    let styles = counts.get(source).line_styles();
    // Split so the text and the engine can be borrowed at once, for the reason
    // `lay_out_for_caret` gives (ペイン分割設計 7.3).
    let Pane { graphics, view, .. } = cache.pane(id);
    let slot = &mut view.preview_slot;
    let shown = pane_text(window, id, slot, source, active_line_start);
    // The same layout the drawing path builds, boxes included: a hit test
    // against a layout without them would answer for text that is not where it
    // is on screen.
    let marked = StyledText::marked(shown.text(), styles, shown.marks());
    let styled = marked
        .with_markers(shown.markers())
        .with_source_line(shown.source_line());
    let typography = pane_typography(window, id);
    let engine = &mut graphics.engine;
    let label = id.label(window);
    if let Err(error) = engine.update(styled, id.line_fit(window, &typography), &typography) {
        window.set_render_status(format!("{label}整形: NG / {error}").into());
        return None;
    }
    // E3: **番号を押したかどうかは、ここでしか分からない。**欄の広さを知って
    // いるのは組版側で、返ってくるバイトは欄の中でも本文の位置（その行の頭）で
    // ある——どちらの問いも同じ点に対する答えなので、一度に訊く。
    let in_numbers = engine.in_number_column(x, y);
    match engine.hit_test(x, y) {
        Ok(hit) => Some(PaneHit {
            byte: shown.source_byte_at_utf16(hit.utf16_position as usize),
            letter: shown.source_byte_at_utf16(hit.utf16_letter as usize),
            in_numbers,
        }),
        Err(error) => {
            window.set_render_status(format!("{label}ヒットテスト: NG / {error}").into());
            None
        }
    }
}

/// 点が当たった場所——本文のバイトと、そこが行番号の欄かどうか（要件 7.1、E3）。
#[derive(Clone, Copy, Debug)]
struct PaneHit {
    /// いちばん近い本文の位置。**欄の中の点でも本文の位置が返る**（その行の頭）。
    ///
    /// カーソルを置く場所なので、**字と字の境目**である——点が字の後ろ半分に
    /// あれば、次の字の頭になる。
    byte: usize,
    /// **押された字そのものの頭**（E3）。語を選ぶのはこちらで訊く：`cat`の`t`の
    /// 右半分を押した書き手は、次の空白ではなく`cat`を指している。
    letter: usize,
    /// 行番号の欄の中か（要件 9 の番号を出している面だけ）。
    in_numbers: bool,
}

/// One pane's text and engine, measured and ready to be asked about a caret.
struct MeasuredPane<'a> {
    shown: PaneText<'a>,
    engine: &'a mut TextEngine,
}

/// Lay a pane out so its engine can be asked where a caret is or where it lands.
///
/// The engine answers about whatever text it measured last, and the text a pane
/// shows can have changed since — the four modes swap it (3.12), and so does
/// revealing another line. So it is measured again first, for the same reason
/// [`hit_test_pane`] does it; the only difference is that the question is asked
/// of the caret rather than of a point.
///
/// `None` means the engine refused the text; the caller has already been told.
fn lay_out_for_caret<'a>(
    window: &AppWindow,
    cache: &'a mut RenderCache,
    document: &OpenDocument,
    id: PaneId,
    source: &'a str,
    active_line_start: Option<usize>,
) -> Option<MeasuredPane<'a>> {
    let mut counts = document.counts.borrow_mut();
    let styles = counts.get(source).line_styles();
    // Split so the text and the engine can be borrowed at once: one comes from
    // the pane's data and the other from its graphics (ペイン分割設計 7.3).
    let Pane { graphics, view, .. } = cache.pane(id);
    let slot = &mut view.preview_slot;
    let shown = pane_text(window, id, slot, source, active_line_start);
    // The same layout the drawing path builds, boxes included, for the reason
    // `hit_test_pane` gives.
    let marked = StyledText::marked(shown.text(), styles, shown.marks());
    let styled = marked
        .with_markers(shown.markers())
        .with_source_line(shown.source_line());
    let typography = pane_typography(window, id);
    let engine = &mut graphics.engine;
    if let Err(error) = engine.update(styled, id.line_fit(window, &typography), &typography) {
        let label = id.label(window);
        window.set_render_status(format!("{label}整形: NG / {error}").into());
        return None;
    }
    Some(MeasuredPane { shown, engine })
}

/// Hit test a pane and move its caret and selection to where the pointer is.
#[allow(clippy::too_many_arguments)]
fn update_pane_selection(
    window: &AppWindow,
    document: &Rc<OpenDocument>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    id: PaneId,
    x: f32,
    y: f32,
    phase: SelectionPhase,
) {
    // **A shell has no document to hit-test.** What a drag over one picks out
    // is cells, and where they are is arithmetic (追加要件 Terminal).
    if cache.borrow_mut().pane(id).terminal.is_some() {
        select_in_terminal(window, cache, id, TerminalSpot::Front, x, y, phase);
        return;
    }
    let drag_started = Instant::now();
    let source = document.text.borrow().clone();
    let active_line_start = PaneId::revealed_line(id.vertical(window), state, &source);

    // A drag reuses the cached preview, the cached block measurements and the
    // cached layouts: nothing here walks the document.
    let hit = {
        let mut borrowed = cache.borrow_mut();
        let cache = &mut *borrowed;
        hit_test_pane(
            window,
            cache,
            document,
            id,
            &source,
            active_line_start,
            x,
            y,
        )
    };
    let Some(hit) = hit else {
        return;
    };
    // E3: **行番号を押したら、その論理行が選ばれる。**番号は本文ではないので、
    // そこへカーソルを置いても書き手の言ったことにならない——押した先が行その
    // ものであるほうが、次にすること（動かす・複製する・消す）に繋がる。
    // **押した瞬間だけ**：そのまま引けば、行の頭から普通の選択が伸びる。
    let from_numbers = match phase {
        SelectionPhase::Begin => hit.in_numbers.then_some(hit.byte),
        // 引いているあいだは、始まりが番号だったかどうかで決まる——途中で
        // ポインタが本文へ入っても、選んでいるのは行のままである。
        _ => state.borrow().line_drag,
    };
    if from_numbers.is_none() && phase == SelectionPhase::Begin {
        // **本文で押し直したら、行の選択は終わり。**離した合図（`End`）は
        // 窓の外へポインタが出ると来ないことがあるので、次に押した回でも畳む。
        state.borrow_mut().line_drag = None;
    }
    // E3: ダブルクリックが選んだ語（書き手の報告 2026-09-10）。
    let chosen_word = match phase {
        SelectionPhase::Begin => None,
        _ => state.borrow().word_drag,
    };
    if let Some((first_start, first_end)) = chosen_word {
        if phase == SelectionPhase::Update {
            // **押したまま動かせば、語ごと伸びる。**押した語は必ず入る。
            let (start, end) = document::word_around(&source, hit.letter);
            let (start, end) = (first_start.min(start), first_end.max(end));
            select_source_range(window, cache, document, state, id, &source, start, end);
            return;
        }
        // **離した合図では、何もしない**（書き手の報告 2026-09-10：「white catは
        // 2語です」「不安定に感じました」）。ここでカーソルを押した点へ置くと、
        // アンカーは語の頭のままなので語の途中までの選択になり、離した点が隣の語に
        // 寄っていれば2語ぶんに広がる——**選んだ語は、選んだそのままでよい。**
        let mut state = state.borrow_mut();
        state.word_drag = None;
        state.mark = false;
        return;
    }
    // 2回目の押下は語を選ぶ（E3）。**数えるのはここ**——窓の`double-clicked`は
    // 離した合図の前後どちらで来るか決まっておらず、3回目・4回目にも来る
    // （それが「契機がわからないのですが、選択がはずれなくなります」であった）。
    // ここで数えれば、2回目で区切って3回目は普通の押下に戻せる。
    if phase == SelectionPhase::Begin {
        let doubled = {
            let mut state = state.borrow_mut();
            state.word_drag = None;
            state.double_click(x, y)
        };
        if doubled && !hit.in_numbers {
            let (start, end) = document::word_around(&source, hit.letter);
            state.borrow_mut().word_drag = Some((start, end));
            cache.borrow_mut().log_diag(
                &format!("pointer.{}", id.diag_suffix()),
                &format!(
                    "word pane={} letter={} span={start}..{end}",
                    id.log_name(),
                    hit.letter,
                ),
            );
            select_source_range(window, cache, document, state, id, &source, start, end);
            return;
        }
    }
    if let Some(anchor) = from_numbers {
        let (first, _) = document::line_span(&source, anchor);
        let (start, end) = document::line_span(&source, hit.byte);
        // 上へ引けば上の行まで、下へ引けば下の行まで。**始めた行は必ず入る。**
        let (start, end) = (first.min(start), first.max(end));
        {
            let mut state = state.borrow_mut();
            state.line_drag = (phase != SelectionPhase::End).then_some(anchor);
        }
        cache.borrow_mut().log_diag(
            &format!("pointer.{}", id.diag_suffix()),
            &format!(
                "numbers pane={} {phase:?} lines={start}..{end}",
                id.log_name()
            ),
        );
        select_source_range(window, cache, document, state, id, &source, start, end);
        return;
    }
    let hit = hit.byte;

    let next_active_line_start = source_line_start(&source, hit);
    cache.borrow_mut().log_diag(
        &format!("pointer.{}", id.diag_suffix()),
        &format!(
            "phase={phase:?} x={x:.1} y={y:.1} scroll={scroll:.0} \
             hit_byte={hit} active={active_line_start:?} preview={preview}",
            scroll = id.scroll(window),
            preview = u8::from(id.shows_preview(window)),
        ),
    );
    let selection = {
        let mut state = state.borrow_mut();
        // **A standing mark answers the mouse too** (書き手の報告 2026-09-07).
        // 要件 11.4's mark is a Shift nobody is holding, and a Shift that only
        // the arrow keys could extend was half a key: the writer who asked for
        // a selection and then pointed at where it should end had said the
        // whole of it. So the press keeps the anchor the mark dropped — the
        // rectangle with it (要件 7.1) — and the release puts the mark down,
        // which is what makes the *next* click plain again and so the way to
        // let a selection go.
        let held = state.mark && phase != SelectionPhase::Extend;
        if !held && (phase == SelectionPhase::Begin || state.selection_anchor_source_byte.is_none())
        {
            state.selection_anchor_source_byte = Some(hit);
            state.rectangular = false;
        }
        if phase == SelectionPhase::End {
            state.mark = false;
        }
        state.caret_source_byte = Some(hit);
        // Only the vertical pane keeps the revealed line: the horizontal one
        // derives it from its caret on every lookup, so writing it there would
        // be storing an answer that is recomputed anyway.
        if id.vertical(window) && phase != SelectionPhase::Update {
            state.active_line_start = Some(next_active_line_start);
        }
        state.preedit.clear();
        state.preferred_line = None;
        pane_selection(&state)
    };
    id.set_ime_buffer(window, "");

    if id.vertical(window) && phase == SelectionPhase::Update {
        drag_caret_only(
            window,
            cache,
            document,
            id,
            &source,
            active_line_start,
            hit,
            &selection.run(),
            drag_started,
        );
        return;
    }
    refresh_pane(
        window,
        cache,
        document,
        id,
        &source,
        Some(next_active_line_start),
        Some(hit),
        selection,
        "",
    );
    if phase == SelectionPhase::Update {
        let ms = elapsed_ms(drag_started);
        let line = format!("{label}ドラッグ選択: {ms:.1}ms", label = id.label(window));
        window.set_render_status(line.into());
    }
}

/// Move the caret and re-cut the selection without laying the document out.
///
/// Mid-drag the text has not changed, so no tile is regenerated: only the caret
/// and the on-screen part of the selection are hit tested again. **Measured on
/// the path the slowness was reported on**, which was the vertical pane. The
/// horizontal pane still takes the whole refresh — giving it this as well is a
/// change to make with a measurement in hand, not inside a rewrite.
#[allow(clippy::too_many_arguments)]
fn drag_caret_only(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    id: PaneId,
    source: &str,
    active_line_start: Option<usize>,
    hit: usize,
    selection: &[(usize, usize)],
    started: Instant,
) {
    let mut borrowed = cache.borrow_mut();
    let cache = &mut *borrowed;
    let (caret_utf16, selection_utf16) = {
        let PaneView {
            preview_slot,
            caret_utf16,
            selection_utf16,
            ..
        } = &mut cache.pane(id).view;
        let shown = pane_text(window, id, preview_slot, source, active_line_start);
        let caret = shown.utf16_at_source_byte(hit) as u32;
        let ranges = selection
            .iter()
            .filter_map(|(start, end)| {
                let start = shown.utf16_at_source_byte(*start);
                let end = shown.utf16_at_source_byte(*end);
                (start < end).then_some((start as u32, (end - start) as u32))
            })
            .collect::<Vec<_>>();
        *caret_utf16 = Some(caret);
        *selection_utf16 = ranges.clone();
        (caret, ranges)
    };

    let content_flow = cache.pane(id).graphics.engine.total_flow_size() as f32;
    let visible = id.flow_range(window, content_flow);
    // Both results are bound to locals first: a call left in a `match` scrutinee
    // keeps its borrow of the cache alive through every arm.
    let caret = {
        let engine = &mut cache.pane(id).graphics.engine;
        engine.caret_geometry(caret_utf16)
    };
    let rects = {
        let engine = &mut cache.pane(id).graphics.engine;
        let mut rects = Vec::new();
        let mut failed = None;
        for run in &selection_utf16 {
            match engine.selection_rects(Some(*run), visible) {
                Ok(found) => rects.extend(found),
                Err(error) => failed = Some(error),
            }
        }
        match failed {
            Some(error) => Err(error),
            None => Ok(rects),
        }
    };
    let label = id.label(window);
    match (caret, rects) {
        (Ok(caret), Ok(rects)) => {
            let rect_count = rects.len();
            apply_pane_geometry(
                window,
                &mut cache.diag,
                id,
                content_flow,
                Some(caret),
                &rects,
                false,
            );
            let place = source_caret(window, id, Some(hit));
            update_status(window, id, document, source, selection, place);
            // Nothing reported the mid-drag cost before, which is exactly the
            // path the slowness was reported on.
            let ms = elapsed_ms(started);
            let line = format!("{label}ドラッグ選択: {rect_count} rects / {ms:.1}ms");
            window.set_render_status(line.into());
        }
        (Err(error), _) | (_, Err(error)) => {
            let line = format!("{label}選択座標: NG / {error}");
            window.set_render_status(line.into());
        }
    }
}

/// The text a pane draws, with any IME pre-edit spliced in at the caret.
///
/// The pre-edit never reaches the document: it exists only in what is drawn.
///
/// One function for both panes. The only thing that differed between them was
/// how a position in the shown text becomes a byte in it, and `PaneText`
/// answers that — the preview by its own index, the source by walking.
fn text_with_preedit<'a>(
    shown: &PaneText<'a>,
    caret_utf16: Option<u32>,
    preedit: &str,
) -> (Cow<'a, str>, Option<u32>, Option<(u32, u32)>) {
    let text = shown.text();
    let Some(caret) = caret_utf16 else {
        return (Cow::Borrowed(text), None, None);
    };
    if preedit.is_empty() {
        return (Cow::Borrowed(text), Some(caret), None);
    }

    let mut rendered = text.to_owned();
    rendered.insert_str(shown.shown_byte_at_utf16(caret as usize), preedit);
    let preedit_length = preedit.encode_utf16().count() as u32;
    (
        Cow::Owned(rendered),
        Some(caret + preedit_length),
        Some((caret, preedit_length)),
    )
}

/// What a pane is showing: the text as it is set, or the Markdown itself.
///
/// **The whole of the four modes is this one function** (要件 7.2、技術検証
/// 3.12). Both panes can be put either way, so which text a pane holds is a
/// property of the pane and not of the writing direction.
///
/// **Every path that turns a screen or caret position into a document position
/// has to come through here.** Reading it out of the other text places the
/// position wherever the two have drifted apart, and that drift grows with
/// every Markdown marker above the point in question — so it looks like the
/// further down the pane, the worse the aim (6.15). It also makes the engine
/// swap texts on every pointer event, which shows up in the log as a full
/// re-measure per event.
fn pane_text<'a>(
    window: &AppWindow,
    id: PaneId,
    preview_slot: &'a mut PreviewSlot,
    source: &'a str,
    active_line_start: Option<usize>,
) -> PaneText<'a> {
    if id.shows_preview(window) {
        PaneText::Preview(preview_slot.get(source, active_line_start))
    } else {
        PaneText::Source(source)
    }
}

/// The line the horizontal pane is currently showing as Markdown, if any.
///
/// Worked out rather than stored, which is what makes it the horizontal half of
/// [`PaneId::revealed_line`]: the pane reveals the caret's line as the caret
/// arrives, and nothing before the first one, so its refreshes pass exactly
/// this.
fn horizontal_active_line(state: &Rc<RefCell<EditorState>>, source: &str) -> Option<usize> {
    let caret = state.borrow().caret_source_byte?;
    let caret = floor_char_boundary(source, caret);
    Some(source_line_start(source, caret))
}

/// The tabs the quick draft may be sent to, and which one the editor is in
/// (要件 12.4).
///
/// **The names the strips show**, read from the documents themselves like
/// `publish_tabs` — there is no second copy to go stale. `resolved` is filled
/// with what each row stands for, so that the row the writer picks is read back
/// rather than worked out a second time: **a list built twice is two opinions
/// about which tab they picked.**
///
/// A pane that is not on screen contributes nothing: its tabs are not in front
/// of anybody, and Split is what puts the second one there.
fn paste_targets(
    window: &AppWindow,
    live: &Live,
    aimed: &str,
    resolved: &mut Vec<(PaneId, usize)>,
) -> quick_draft::TabList {
    resolved.clear();
    let mut rows = Vec::new();
    let mut target = -1;
    // **In the order they are on screen, and numbered by that order.** The
    // layout decides which pane is where — `S v 0.5 P 1 P 0` puts pane 1 above
    // pane 0 — so a number taken from `PaneId` names the panes in an order
    // nobody can see. The writer picks the one they are looking at.
    //
    // **Numbered, not sided**: the two sit side by side today and one above the
    // other tomorrow, and 要件 6.4 divides further than that.
    let arranged = live
        .layout
        .borrow()
        .panes()
        .into_iter()
        .map(|index| PaneId::from_index(index as i32))
        .filter(|id| id.is_shown(window))
        .collect::<Vec<_>>();
    let split = arranged.len() > 1;
    for (place, id) in arranged.into_iter().enumerate() {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        for (index, tab) in strip.tabs.iter().enumerate() {
            let file = tab.document.file.borrow();
            let title = file.title();
            let number = place + 1;
            rows.push(if split {
                format!("{number} · {title}")
            } else {
                title
            });
            if tab_name(id, &file) == aimed {
                target = rows.len() as i32 - 1;
            }
            resolved.push((id, index));
        }
    }
    // 要件 12.4: with nothing remembered, a click sends to the first tab —
    // **but only while nothing is remembered**, which is what tells a first run
    // from a tab that has since been closed.
    if target < 0 && aimed.is_empty() && !rows.is_empty() {
        target = 0;
    }
    let target_name = rows
        .get(target.max(0) as usize)
        .filter(|_| target >= 0)
        .cloned()
        .unwrap_or_else(|| NO_TARGET.to_owned());
    quick_draft::TabList {
        rows,
        target,
        target_name,
    }
}

/// What the send button says when it has nowhere to send to.
const NO_TARGET: &str = "Paste to tab…";

/// How a tab is named in the draft's own file, so that the next run finds it
/// again (要件 12.4).
///
/// **The pane and the document, because a tab is both.** The document alone
/// was not enough: 要件 6.4 opens the second pane on the tab the first one is
/// showing, so the ordinary arrangement has one document in two tabs — and a
/// name that fitted both matched whichever came last. The file is what is still
/// recognisable tomorrow, and the pane is which of its tabs was picked.
///
/// An untitled buffer is named by its number, which is what the session already
/// knows it by (要件 8.4).
fn tab_name(id: PaneId, file: &DocumentFile) -> String {
    let pane = id.index() + 1;
    match file.path() {
        Some(path) => format!("{pane}|file:{}", path.display()),
        None => format!("{pane}|untitled:{}", file.untitled_number()),
    }
}

/// Put the quick draft into one of the editor's tabs (要件 12.4).
///
/// **The tab is brought forward first.** Text put into a tab nobody is looking
/// at is text nobody has been told about — and the caret it lands at is that
/// tab's own, which only the tab in front of the pane has.
fn paste_into_tab(window: &AppWindow, live: &Live, id: PaneId, index: usize, text: &str) -> String {
    // **The pane the tab is in becomes the one the keyboard is in.** The caret
    // the text landed at is that pane's, and a writer sent somewhere is a
    // writer who is now there.
    window.set_focused_pane(id.index());
    switch_to_tab(window, live, id, index);
    let document = live.states.document(id);
    insert_pane_text(
        window,
        id,
        &document,
        &live.states,
        &live.cache,
        text,
        false,
    );
    window.window().set_minimized(false);
    let _ = window.show();
    restore_editor_focus(window);
    // What to call this tab next time, which is what the button will say.
    tab_name(id, &document.file.borrow())
}

/// Insert text at a pane's caret, replacing whatever it has selected.
///
/// `indent_line_start` says the text is an indent rather than something typed,
/// which is the one case that may land somewhere other than the caret. **Both
/// panes pass it for `Tab`**; only the vertical one has anywhere else to put it
/// (below).
#[allow(clippy::too_many_arguments)]
fn insert_pane_text(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    text: &str,
    indent_line_start: bool,
) {
    let input = normalize_typed_input(text);
    if input.is_empty() {
        return;
    }
    // 要件 7.1: **typing over a rectangle takes the rectangle out first**, and
    // what is typed then goes in at its near corner. The selection the writer
    // can see is what goes, which the linear span below would not be. A
    // rectangular *insert* — the same characters put on every line — is a
    // second and larger feature, and no requirement asks for it; doing it here
    // would hide it behind an ordinary keystroke.
    let rectangle = {
        let state = states.of(id);
        let rectangular = state.borrow().rectangular;
        rectangular
    };
    let rectangle = rectangle.then(|| selected_runs(cache, id));
    if let Some(ranges) = rectangle
        && !ranges.is_empty()
    {
        remove_selection(window, id, document, states, cache, &ranges);
    }
    let state = states.of(id);
    id.set_ime_buffer(window, "");
    let started = Instant::now();
    let mut source = document.text.borrow().clone();
    let cloned_ms = elapsed_ms(started);
    if !fits_document_limit(&source, &input) {
        window.set_render_status(over_limit_message(&input).into());
        return;
    }
    let caret = id.caret_byte(&state, &source);
    let selection = selection_source_range(&state.borrow());
    // What the change took out, for the undo that puts it back (要件 7.1).
    // Read before the text moves, because afterwards there is nowhere to read
    // it from.
    let removed = match selection {
        Some((start, end)) => source[start..end].to_owned(),
        None => String::new(),
    };
    let at = if let Some(range) = selection {
        replace_source_range(&mut source, range, &input);
        range.0
    } else if id.vertical(window) && id.shows_preview(window) {
        // A Tab at the start of a heading has to land in front of a marker that
        // is not on screen (要件 7.3.2), so the position comes out of the
        // preview rather than off the caret. **Only the vertical pane does
        // this**: the horizontal one indents at the caret, and widening that is
        // a decision about the editor, not part of putting the panes on one
        // path.
        let line = source_line_start(&source, caret);
        let revealed = PaneId::revealed_line(id.vertical(window), &state, &source).unwrap_or(line);
        let mut borrowed = cache.borrow_mut();
        let slot = &mut borrowed.pane(id).view.preview_slot;
        let preview = slot.get(&source, Some(revealed));
        let shown = preview.utf16_at_source_byte(caret);
        let at = vertical_insertion_source_byte(&source, preview, shown, indent_line_start);
        drop(borrowed);
        source.insert_str(at, &input);
        at
    } else {
        // Nothing is hidden here, so there is no marker to insert in front of
        // and the caret is already a position in the document.
        source.insert_str(caret, &input);
        caret
    };
    let next = at + input.len();
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(next);
        state.selection_anchor_source_byte = Some(next);
        state.active_line_start = Some(source_line_start(&source, next));
        state.preedit.clear();
        state.preferred_line = None;
        // 要件 11.4: the mark was a selection being made, and it has been
        // answered — the text it named is gone or replaced.
        state.mark = false;
    }
    let change = Change {
        at,
        removed: removed.len(),
        inserted: input.len(),
    };
    document.record(at, removed, input);
    let stored = Instant::now();
    *document.text.borrow_mut() = source.clone();
    let stored_ms = elapsed_ms(stored);
    id.draw_edit(window, states, cache, document, &source, next, change);
    let name = id.log_name();
    log_edit(cache, &name, &source, started, cloned_ms, stored_ms);
}

/// 行そのものを動かす・写す・消す（E3の②）。
///
/// **選ばれている行ぜんぶが1つの塊。**カーソルだけならその1行で、選択が3行を
/// またいでいれば3行が一緒に動く（`document::selected_lines`）。
///
/// **普通の編集の道を通る**ので、取り消しは1回で戻り、同じ文書を出している別の面も
/// 付いてくる（要件 7.6）——`draw_edit`が両方をやる。**1操作＝1つの取り消し**：
/// 入れ替えは「消して入れる」の形なので、続けて押しても打鍵のようには繋がらない。
///
/// **先頭の行を前へ、末尾の行を後へは動かせない。**`line_edit`が`None`を返すので、
/// ここは何もしない——動かないことは画面に出ている（行がそこにある）。
fn edit_lines(window: &AppWindow, live: &Live, id: PaneId, what: document::LineEdit) {
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    let (from, to) = {
        let state = state.borrow();
        match selection_source_range(&state) {
            Some((start, end)) => (start, end),
            None => {
                let caret = state.caret_source_byte.unwrap_or(0).min(source.len());
                (caret, caret)
            }
        }
    };
    let span = document::selected_lines(&source, from, to);
    let Some((region, text, chosen)) = document::line_edit(&source, span, what) else {
        return;
    };
    apply_span_edit(
        window,
        live,
        id,
        &source,
        region,
        &text,
        chosen,
        &format!("{what:?}"),
    );
}

/// 本文のひと続きを、別の字で置き換える——1回の編集として（E3）。
///
/// **普通の編集の道**（`draw_edit`）を通るので、取り消しは1回で戻り、同じ文書を
/// 出している別の面も付いてくる（要件 7.6）。行の入れ替えも、箇条書きを終える
/// Enterも、ここを通る——**編集の入口が増えても、編集の道は1本**である。
///
/// `chosen`は編集のあとに選ばれている範囲。長さが無ければ、そこに立つカーソル。
#[allow(clippy::too_many_arguments)]
fn apply_span_edit(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    source: &str,
    region: Range<usize>,
    text: &str,
    chosen: (usize, usize),
    told: &str,
) {
    let document = live.states.document(id);
    let state = live.states.of(id);
    let mut next = source.to_owned();
    next.replace_range(region.clone(), text);
    // **写しは文書を増やす**ので、打鍵や貼り付けと同じ上限を通る（要件 8.2）。
    // 越えるなら何も起きない——保存はできて開き直せないファイルを作らない。
    if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
        window.set_render_status(over_limit_message(text).into());
        return;
    }
    let change = Change {
        at: region.start,
        removed: region.len(),
        inserted: text.len(),
    };
    document.record(
        region.start,
        source[region.clone()].to_owned(),
        text.to_owned(),
    );
    *document.text.borrow_mut() = next.clone();
    {
        let mut state = state.borrow_mut();
        // **動いた行が選ばれたまま**なので、もう一度押せばさらに動く。
        state.selection_anchor_source_byte = Some(chosen.0);
        state.caret_source_byte = Some(chosen.1);
        state.active_line_start = Some(source_line_start(&next, chosen.1));
        state.preferred_line = None;
        state.preedit.clear();
        state.rectangular = false;
        state.mark = false;
        state.search_selection = None;
        state.word_drag = None;
        state.line_drag = None;
    }
    live.cache.borrow_mut().log_diag(
        "lines",
        &format!(
            "pane={} {told} region={}..{} chose={}..{}",
            id.log_name(),
            region.start,
            region.end,
            chosen.0,
            chosen.1,
        ),
    );
    id.draw_edit(
        window,
        &live.states,
        &live.cache,
        &document,
        &next,
        chosen.1,
        change,
    );
}

/// Enterを押した（E3の③）。
///
/// **字下げと、箇条書き・引用の印を継ぐ。**継ぐものが無ければただの改行で、
/// 中身の無い項目なら印を消して継続を終える（`document::enter_continuation`）。
///
/// **行の見方は画面と同じもの**（`DocumentCounts`の`line_styles`）を渡す。ここで
/// 決め直すと、画面が箇条書きとして組んでいる行をEnterが本文として扱うことになる。
///
/// **選んでいるものがあれば、その頭の行で決める**——選択は`insert_pane_text`が
/// 取り除き、入る字はそこへ落ちるからである。
fn enter_in_pane(window: &AppWindow, live: &Live, id: PaneId) {
    // 追加要件 2026-09-07: **打ち始めたら、そのタブは文書になる。**Enterも打鍵で
    // ある——`on_pane_text_input`がこれを呼んでいたので、Enterだけ`New Tab`の上で
    // 何も起きない鍵になっていた。**文書を取り出す前に**呼ぶ：答えたあとのタブは、
    // もう別の文書を持っている。
    answer_new_tab(window, live, id, None);
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    let at = {
        let borrowed = state.borrow();
        match selection_source_range(&borrowed) {
            Some((start, _)) => start,
            None => {
                drop(borrowed);
                id.caret_byte(&state, &source)
            }
        }
    };
    let (line_start, line_end) = document::line_span(&source, at);
    let line = source[line_start..line_end]
        .strip_suffix('\n')
        .unwrap_or(source.get(line_start..line_end).unwrap_or_default());
    let index = source[..line_start].matches('\n').count();
    let style = document
        .counts
        .borrow_mut()
        .get(&source)
        .line_styles()
        .get(index)
        .copied()
        .unwrap_or_default();
    match document::enter_continuation(line, style, at - line_start) {
        document::Continuation::Insert(text) => insert_pane_text(
            window,
            id,
            &document,
            &live.states,
            &live.cache,
            &text,
            false,
        ),
        document::Continuation::Clear { upto, keep } => {
            let region = line_start..line_start + upto;
            let caret = line_start + keep.len();
            apply_span_edit(
                window,
                live,
                id,
                &source,
                region,
                &keep,
                (caret, caret),
                "EndItem",
            );
        }
    }
}

/// Delete the grapheme cluster beside a pane's caret, or its selection.
/// Take back the last change to the document this pane is showing (要件 7.1).
///
/// **The history belongs to the document** (要件 7.6), so it does not matter
/// which pane asks: what comes back is whatever was done last, wherever it was
/// done, and every pane showing that document redraws.
fn undo_in_pane(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    forwards: bool,
) {
    let state = states.of(id);
    let mut source = document.text.borrow().clone();
    let moved = {
        let mut history = document.history.borrow_mut();
        if forwards {
            history.redo_into(&mut source)
        } else {
            history.undo_into(&mut source)
        }
    };
    let Some((caret, change)) = moved else {
        return;
    };
    // A composition that is still open belongs to the text that was there
    // before this took it back.
    id.set_ime_buffer(window, "");
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(caret);
        state.selection_anchor_source_byte = Some(caret);
        state.active_line_start = Some(source_line_start(&source, caret));
        state.preedit.clear();
        state.preferred_line = None;
    }
    *document.text.borrow_mut() = source.clone();
    id.draw_edit(window, states, cache, document, &source, caret, change);
    let name = id.log_name();
    let kind = if forwards { "redo" } else { "undo" };
    cache
        .borrow_mut()
        .log_diag("edit", &format!("{kind} pane={name} caret={caret}"));
}

/// Start selecting from where the caret is, or stop (要件 11.4 の`Ctrl+Space`).
///
/// **It is a Shift the writer does not have to hold.** While the mark is on,
/// every move extends the selection; pressing the key again puts it down and
/// leaves the selection where it stands, so the same key both starts and stops.
/// An edit ends it too, and so does a **click**, which first extends the
/// selection to where it landed (`update_pane_selection`): either one answers
/// the selection that was being made.
///
/// Turning it on drops the anchor at the caret so that the first move already
/// has something to extend from, and turning it off collapses nothing — what
/// was selected is still selected, and `Alt+W` is the next key to reach for.
fn toggle_mark(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    rectangular: bool,
) {
    let state = states.of(id);
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(&state, &source);
    let marking = {
        let mut state = state.borrow_mut();
        // **The other shape restarts it rather than stopping it.** A writer who
        // began a run and then asked for a rectangle asked for a rectangle
        // (要件 7.1), and turning the key into a no-op there would be reading
        // the press as the one before it.
        state.mark = !state.mark || state.rectangular != rectangular;
        state.rectangular = rectangular && state.mark;
        if state.mark {
            state.selection_anchor_source_byte = Some(caret);
            state.caret_source_byte = Some(caret);
        }
        state.mark
    };
    cache.borrow_mut().log_diag(
        "edit",
        &format!(
            "mark pane={} on={marking} rect={rectangular} at={caret}",
            id.log_name()
        ),
    );
    // **The one thing on screen that says the mark is standing.** Nothing else
    // changes when it goes on — the caret sits where it sat — and a writer who
    // cannot tell reads the next arrow key as the feature not working
    // (書き手の報告 2026-09-07).
    let told = match (marking, rectangular) {
        (false, _) => "選択終了",
        (true, false) => "選択開始：矢印かクリックで選ぶ範囲を決めます",
        (true, true) => "矩形選択開始：矢印かクリックで選ぶ範囲を決めます",
    };
    window.set_render_status(told.into());
    refresh_pane_from_state(window, cache, document, id, &state, &source);
}

/// The Kill Ring, and where its last yank landed (要件 11.4・11.6).
///
/// **The yank is remembered as text at a place, not as "the last thing done".**
/// `Alt+Shift+Y` may only replace a yank that is still standing, and asking
/// whether the document still holds that text there answers it without every
/// other edit having to say it happened.
#[derive(Default)]
struct Kills {
    ring: KillRing,
    last_yank: Option<LastYank>,
}

/// What a yank put in, and where.
struct LastYank {
    pane: PaneId,
    at: usize,
    text: String,
}

/// Do what one of 要件 11.4's Kill Ring keys asks.
///
/// **The ring is not the clipboard** (要件 11.6). `Ctrl+C` and `Ctrl+X` reach
/// Windows'; `Ctrl+K`, `Alt+W` and `Alt+X` reach this one. A writer can carry
/// one thing between programs and another between paragraphs, which is the
/// whole of why the requirement asks for two.
///
/// Every taking and every putting goes through the ordinary edit paths
/// (`remove_source_range`, `insert_pane_text`), so each of these is one press
/// of `Ctrl+Z` away from being undone (要件 7.1) — **`Alt+Shift+Y` included**,
/// because replacing the yanked run is one change and not a delete followed by
/// an insert.
#[allow(clippy::too_many_arguments)]
fn kill_ring_action(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    kills: &Rc<RefCell<Kills>>,
    what: i32,
) {
    let Some(what) = KillAction::from_index(what) else {
        return;
    };
    let state = states.of(id);
    match what {
        KillAction::ToLineEnd => {
            let source = document.text.borrow().clone();
            let caret = id.caret_byte(&state, &source);
            let end = line_end_to_kill(&source, caret);
            if end <= caret {
                return;
            }
            kills.borrow_mut().ring.add(source[caret..end].to_owned());
            remove_source_range(window, id, document, states, cache, caret, end);
        }
        KillAction::Copy | KillAction::Cut => {
            let source = document.text.borrow().clone();
            let ranges = selected_runs(cache, id);
            if ranges.is_empty() {
                return;
            }
            kills.borrow_mut().ring.add(selected_text(&source, &ranges));
            if what == KillAction::Cut {
                remove_selection(window, id, document, states, cache, &ranges);
            }
        }
        KillAction::Yank => {
            let Some(text) = kills.borrow_mut().ring.newest().map(str::to_owned) else {
                return;
            };
            yank_into_pane(window, id, document, states, cache, kills, text);
        }
        KillAction::YankOlder => {
            // **The yank has to still be there.** The writer may have typed,
            // undone or clicked since, and putting the older kill over whatever
            // is at those bytes now would take out text nobody killed.
            let Some(end) = standing_yank(id, states, document, kills) else {
                return;
            };
            let Some(text) = kills.borrow_mut().ring.older().map(str::to_owned) else {
                return;
            };
            {
                let mut state = state.borrow_mut();
                state.selection_anchor_source_byte = Some(end.0);
                state.caret_source_byte = Some(end.1);
            }
            yank_into_pane(window, id, document, states, cache, kills, text);
        }
    }
}

/// How far a `Ctrl+K` reaches from the caret (要件 11.4).
///
/// To the end of the line — and **when the caret is already there, the line
/// break itself**. Without that, the key does nothing at every line end and a
/// writer clearing a passage has to reach for another one; with it, pressing it
/// twice takes the line and then closes the gap, which is what the key is for.
fn line_end_to_kill(source: &str, caret: usize) -> usize {
    let line_end = source[caret..]
        .find('\n')
        .map_or(source.len(), |offset| caret + offset);
    if line_end > caret {
        line_end
    } else {
        // A line break is one byte, so this is a character boundary wherever it
        // lands, and `min` covers the caret sitting at the very end.
        (caret + 1).min(source.len())
    }
}

/// Where the last yank still stands, as the run it put in.
///
/// `None` if it was in another pane, or if the document no longer holds that
/// text there, or if the caret has left its end — each of those means the
/// writer has done something since, and `Alt+Shift+Y` is about the yank that is
/// still in front of them.
fn standing_yank(
    id: PaneId,
    states: &PaneStates,
    document: &Rc<OpenDocument>,
    kills: &Rc<RefCell<Kills>>,
) -> Option<(usize, usize)> {
    let borrowed = kills.borrow();
    let last = borrowed.last_yank.as_ref()?;
    if last.pane != id {
        return None;
    }
    let source = document.text.borrow();
    let end = last.at + last.text.len();
    if source.get(last.at..end) != Some(last.text.as_str()) {
        return None;
    }
    let state = states.of(id);
    let caret = id.caret_byte(&state, &source);
    (caret == end).then_some((last.at, end))
}

/// Put a kill into the pane, and write down where it went.
fn yank_into_pane(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    kills: &Rc<RefCell<Kills>>,
    text: String,
) {
    // Normalised here as well as inside the insert, so that the run written
    // down is the run the document ends up holding.
    let text = normalize_typed_input(&text);
    insert_pane_text(window, id, document, states, cache, &text, false);
    let caret = states.of(id).borrow().caret_source_byte;
    let at = caret.and_then(|caret| caret.checked_sub(text.len()));
    kills.borrow_mut().last_yank = at.map(|at| LastYank { pane: id, at, text });
}

/// Bring out the tab after the one a pane is showing, or the one before
/// (要件 11.3).
///
/// **The strip's order**, so the key walks the tabs in the order they are
/// drawn — the same numbering the overflow list gives them. **It wraps**: the
/// strip has two ends and the key has one direction, and stopping at an end
/// would leave the tab across the join reachable only by turning round.
///
/// One tab is not a strip to walk.
fn step_tab(window: &AppWindow, live: &Live, id: PaneId, backwards: bool) {
    let (count, active) = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        (strip.tabs.len(), strip.active)
    };
    let landed = stepped_tab(count, active, backwards);
    live.cache.borrow_mut().log_diag(
        "tab",
        &format!(
            "step pane={} count={count} at={active} to={landed:?}",
            id.log_name()
        ),
    );
    let Some(index) = landed else {
        return;
    };
    switch_to_tab(window, live, id, index);
}

/// Where a step round the strip lands, or `None` if there is nowhere to go.
fn stepped_tab(count: usize, active: usize, backwards: bool) -> Option<usize> {
    if count < 2 {
        return None;
    }
    // Counted forwards either way: `count - 1` steps on is one step back, and
    // never asks a `usize` to go below zero.
    let step = if backwards { count - 1 } else { 1 };
    Some((active + step) % count)
}

/// Give the keyboard to the pane in a direction (要件 11.3).
///
/// **The rectangles decide, and they are the ones on screen**: each pane's row
/// carries the area `place_panes` handed it, and a pane the tree does not name
/// has none — so a pane that is not showing cannot be moved into. Nothing
/// happens when there is no pane that way. The key does not wrap round the
/// arrangement the way the tab key wraps round a strip: the strip's ends are
/// a join the writer cannot see, and the editing area's edges are the window.
///
/// Setting the pane and asking for the keyboard is all this does. The pane that
/// takes it says so itself (`focus-taken`), which is what turns the IME the
/// right way round for the writing it is about to be used for.
fn move_focus(window: &AppWindow, cache: &Rc<RefCell<RenderCache>>, from: PaneId, towards: i32) {
    let Some(towards) = Towards::from_index(towards) else {
        return;
    };
    let placed = placed_panes(window);
    let landed = neighbour(&placed, from.index() as usize, towards)
        .map(|pane| PaneId::from_index(pane as i32));
    // **Logged whether or not it moved.** "The key did nothing" and "the key
    // never arrived" look the same on screen, and only one of them is a bug.
    let arriving = landed.map_or_else(|| "-".to_owned(), PaneId::log_name);
    cache.borrow_mut().log_diag(
        "layout",
        &format!(
            "focus {}->{arriving} {towards:?} shown={}",
            from.log_name(),
            placed.len()
        ),
    );
    let Some(landed) = landed else {
        return;
    };
    window.set_focused_pane(landed.index());
    restore_editor_focus(window);
}

/// Put a pane's selection on the clipboard, and take it out if this is a cut
/// (要件 11.2).
///
/// **What goes over is the source between the two ends.** Both panes hold the
/// selection as source bytes even in live preview, where the caret is placed by
/// the shown text and remembered by the byte behind it. So a copy made in the
/// preview carries whatever markers lie between its ends, and pasting it back
/// gives the document what it had.
///
/// **The cut waits on the copy.** Another program can be holding the clipboard,
/// and taking the text out after the hand-over was turned away would lose it
/// for a reason the writer never saw. When it does go, it goes through the
/// ordinary delete, so one Undo puts it back (要件 7.1).
fn copy_selection(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    cut: bool,
) {
    let source = document.text.borrow().clone();
    let ranges = selected_runs(cache, id);
    if ranges.is_empty() {
        return;
    }
    if !clipboard::put_text(ime::window_handle(window), &selected_text(&source, &ranges)) {
        window.set_render_status("クリップボードへ渡せませんでした".into());
        return;
    }
    if cut {
        remove_selection(window, id, document, states, cache, &ranges);
    }
}

/// What a selection holds, its runs joined by line breaks (要件 7.1).
///
/// **A rectangle comes out as lines**, one per line it covers, because that is
/// the shape it was: a run that another program pastes back as a block. An
/// ordinary selection is one run and comes out unchanged.
fn selected_text(source: &str, ranges: &[(usize, usize)]) -> String {
    ranges
        .iter()
        .map(|(start, end)| &source[*start..*end])
        .collect::<Vec<_>>()
        .join("\n")
}

fn delete_adjacent_grapheme(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    backward: bool,
) {
    let state = states.of(id);
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(&state, &source);
    let revealed = PaneId::revealed_line(id.vertical(window), &state, &source);
    // What one character is belongs to the text being shown, not to the
    // Markdown behind it (技術検証 3.12), so this asks the same text the pane
    // laid out.
    // A selection goes out as a selection, whatever shape it is (要件 7.1).
    let selected = selected_runs(cache, id);
    if !selected.is_empty() {
        remove_selection(window, id, document, states, cache, &selected);
        return;
    }
    let (start, end) = {
        let mut borrowed = cache.borrow_mut();
        let slot = &mut borrowed.pane(id).view.preview_slot;
        let shown = pane_text(window, id, slot, &source, revealed);
        if backward {
            (shown.previous_grapheme(caret), caret)
        } else {
            (caret, shown.next_grapheme(caret))
        }
    };
    remove_source_range(window, id, document, states, cache, start, end);
}

/// Take a run of source out of the document, and redraw.
///
/// **The one way text leaves a document by being deleted.** Backspace, Delete
/// and 要件 11.4's `Ctrl+K` and `Alt+X` all end here, so all of them record
/// their undo the same way and one press of `Ctrl+Z` puts any of them back
/// (要件 7.1). What differs between them is only which run they name.
fn remove_source_range(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    start: usize,
    end: usize,
) {
    splice_source(window, id, document, states, cache, start, end, "", start);
}

/// Take a selection out, however many runs it is in (要件 7.1).
///
/// **One recorded change, whatever shape the selection was.** The span from the
/// first run's start to the last one's end is rebuilt without the runs and put
/// back in its place, so a rectangle over twenty lines comes back with one
/// press of `Ctrl+Z` (要件 7.1) rather than twenty.
///
/// The caret lands at the rectangle's near corner, which is where the writer
/// was looking and where the next thing they type belongs.
fn remove_selection(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    ranges: &[(usize, usize)],
) {
    let spliced = spliced_out(&document.text.borrow(), ranges);
    let Some((start, end, kept)) = spliced else {
        return;
    };
    splice_source(
        window, id, document, states, cache, start, end, &kept, start,
    );
}

/// The span a selection covers, and what is left of it once the runs are gone.
///
/// **One string for the whole span**, so that however many runs a rectangle is
/// in, the document changes once and comes back with one press of `Ctrl+Z`.
/// `None` when there is nothing to take out.
fn spliced_out(source: &str, ranges: &[(usize, usize)]) -> Option<(usize, usize, String)> {
    let (start, end) = (ranges.first()?.0, ranges.last()?.1);
    // **A rectangle of no width covers lines but holds nothing.** Its span is
    // not empty — it reaches from one line to another — so the ends alone
    // cannot say that there is nothing to take out.
    if start >= end || ranges.iter().all(|(start, end)| start >= end) {
        return None;
    }
    let mut kept = String::new();
    let mut at = start;
    for (run_start, run_end) in ranges {
        kept.push_str(&source[at..*run_start]);
        at = *run_end;
    }
    kept.push_str(&source[at..end]);
    Some((start, end, kept))
}

/// Put `text` in the place of the source between `start` and `end`, and redraw.
///
/// **The one way a document changes by having something taken out.** Backspace,
/// Delete, 要件 11.4's `Ctrl+K` and `Alt+X`, and 要件 7.1's rectangle all end
/// here, so every one of them records its undo the same way and one press of
/// `Ctrl+Z` puts any of them back (要件 7.1). What differs between them is only
/// which span they name and what they leave in it.
#[allow(clippy::too_many_arguments)]
fn splice_source(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    start: usize,
    end: usize,
    text: &str,
    caret: usize,
) {
    if start >= end {
        return;
    }
    let state = states.of(id);
    let mut source = document.text.borrow().clone();
    let removed = source[start..end].to_owned();
    replace_source_range(&mut source, (start, end), text);
    let caret = caret.min(source.len());
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(caret);
        state.selection_anchor_source_byte = Some(caret);
        state.active_line_start = Some(source_line_start(&source, caret));
        state.preferred_line = None;
        state.mark = false;
        state.rectangular = false;
    }
    let change = Change {
        at: start,
        removed: removed.len(),
        inserted: text.len(),
    };
    document.record(start, removed, text.to_owned());
    *document.text.borrow_mut() = source.clone();
    id.draw_edit(window, states, cache, document, &source, caret, change);
}

/// Move a pane's caret: one grapheme along the line, or one line across.
///
/// `direction` is ±1 for a grapheme and ±2 for a line and means the same in
/// both panes. Each engine decides what "the next line" is for its own writing
/// direction, so the two panes hand it the same number.
#[allow(clippy::too_many_arguments)]
fn move_pane_caret(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    reveal: &Rc<Timer>,
    direction: i32,
    extend_selection: bool,
) {
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(state, &source);
    let revealed = PaneId::revealed_line(id.vertical(window), state, &source);
    let (preferred_line, rectangular) = {
        let state = state.borrow();
        (state.preferred_line, state.rectangular)
    };

    let moved = {
        let mut borrowed = cache.borrow_mut();
        let cache = &mut *borrowed;
        let measured = lay_out_for_caret(window, cache, document, id, &source, revealed);
        let Some(measured) = measured else {
            return;
        };
        let MeasuredPane { shown, engine } = measured;
        let at = shown.utf16_at_source_byte(caret) as u32;
        match direction {
            -1 => Ok((shown.previous_grapheme(caret), None)),
            1 => Ok((shown.next_grapheme(caret), None)),
            // 要件 11.4's `Alt+B` and `Alt+F`. **Asked of the text the pane
            // laid out**, like the grapheme steps beside them (技術検証 3.12):
            // a word is a run of the characters the writer can see, and in the
            // preview the markers are not among them.
            -3 | 3 => {
                let text = shown.text();
                let at = shown.shown_byte_at_utf16(at as usize);
                let moved = if direction < 0 {
                    document::previous_word_boundary(text, at)
                } else {
                    document::next_word_boundary(text, at)
                };
                let landed = shown.source_byte_at_utf16(utf16_at_byte(text, moved));
                Ok((landed, None))
            }
            -2 | 2 => {
                let anchor = match preferred_line {
                    Some(anchor) => Ok(anchor),
                    None => {
                        let geometry = engine.caret_geometry(at);
                        geometry.map(|caret| PaneId::line_anchor(id.vertical(window), &caret))
                    }
                };
                anchor.and_then(|anchor| {
                    engine
                        .move_caret_by_line(at, direction, Some(anchor))
                        .map(|hit| {
                            let position = hit.utf16_position as usize;
                            (shown.source_byte_at_utf16(position), Some(anchor))
                        })
                })
            }
            _ => Ok((caret, None)),
        }
    };

    let (next, next_preferred) = match moved {
        Ok(moved) => moved,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}キャレット移動: NG / {error}").into());
            return;
        }
    };
    let selection = {
        let mut state = state.borrow_mut();
        update_selection_after_move(&mut state, caret, next, extend_selection);
        state.preferred_line = next_preferred;
        pane_selection(&state)
    };
    // **Where a vertical step landed, in the file's own lines** (要件 7.1). The
    // rectangle is drawn over the lines on screen, and a screen line says
    // nothing about which line of the file the writer is on — that difference
    // is the whole of why the rectangle was built the other way first.
    if direction.abs() == 2 {
        let from = document::caret_place(&source, caret);
        let to = document::caret_place(&source, next);
        cache.borrow_mut().log_diag(
            "edit",
            &format!(
                "step pane={} dir={direction} rect={rectangular} {}:{}->{}:{}",
                id.log_name(),
                from.0,
                from.1,
                to.0,
                to.1
            ),
        );
    }
    // Whether the line the caret arrived on shows its Markdown now or once the
    // caret settles is the pane's to decide, not this function's.
    let shown_line = if PaneId::reveals_while_moving(id.vertical(window)) {
        Some(source_line_start(&source, next))
    } else {
        revealed
    };
    refresh_pane(
        window,
        cache,
        document,
        id,
        &source,
        shown_line,
        Some(next),
        selection,
        "",
    );
    if !PaneId::reveals_while_moving(id.vertical(window)) {
        schedule_active_line_reveal(reveal, window, id, state, cache, document);
    }
}

/// `Home` and `End`, and their `Ctrl` forms, in either pane.
#[allow(clippy::too_many_arguments)]
fn move_pane_to_line_edge(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    reveal: &Rc<Timer>,
    to_end: bool,
    document_edge: bool,
    extend_selection: bool,
) {
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(state, &source);
    let revealed = PaneId::revealed_line(id.vertical(window), state, &source);

    let next = if document_edge {
        if to_end { source.len() } else { 0 }
    } else {
        let mut borrowed = cache.borrow_mut();
        let cache = &mut *borrowed;
        let measured = lay_out_for_caret(window, cache, document, id, &source, revealed);
        let Some(measured) = measured else {
            return;
        };
        let MeasuredPane { shown, engine } = measured;
        // Line edges come from the cached line metrics, so this costs nothing.
        let at = shown.utf16_at_source_byte(caret) as u32;
        let edge = engine.move_caret_to_line_edge(at, to_end);
        shown.source_byte_at_utf16(edge as usize)
    };

    let selection = {
        let mut state = state.borrow_mut();
        update_selection_after_move(&mut state, caret, next, extend_selection);
        state.preferred_line = None;
        pane_selection(&state)
    };
    let shown_line = if PaneId::reveals_while_moving(id.vertical(window)) {
        Some(source_line_start(&source, next))
    } else {
        revealed
    };
    refresh_pane(
        window,
        cache,
        document,
        id,
        &source,
        shown_line,
        Some(next),
        selection,
        "",
    );
    if !PaneId::reveals_while_moving(id.vertical(window)) {
        schedule_active_line_reveal(reveal, window, id, state, cache, document);
    }
}

/// Show what the IME is composing in a pane, without committing it.
///
/// The string is not in the document and must not reach it: it goes to the
/// layout only, spliced in at the caret by `text_with_preedit`.
fn set_pane_preedit(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    text: &str,
) {
    let source = document.text.borrow().clone();
    let caret = id.caret_byte(state, &source);
    let line = source_line_start(&source, caret);
    let revealed = PaneId::revealed_line(id.vertical(window), state, &source).unwrap_or(line);
    let selection = pane_selection(&state.borrow());
    let preedit = text.to_string();
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(caret);
        state.active_line_start = Some(revealed);
        state.preedit = preedit.clone();
        state.preferred_line = None;
    }
    refresh_pane(
        window,
        cache,
        document,
        id,
        &source,
        Some(revealed),
        Some(caret),
        selection,
        &preedit,
    );
}

/// Keep a pane's stored positions inside a document the other pane changed.
///
/// Both the length and the character boundaries can have moved under them, and
/// an unclamped position is what every slice downstream panics on.
/// Carry the caret from the pane being left to the one being shown.
///
/// 要件 7.2 keeps the cursor position across a mode change. The two panes hold
/// their own carets because in Split they are two views of one document and
/// each has its own (要件 7.6) — but **a mode change is not two views**. It is
/// one view changing how it draws, so the position has to come with it.
/// Without this the pane that appears shows wherever it was last left, which
/// after editing on the other side is a position with no relation to what the
/// writer was doing.
///
/// `preferred_line` is deliberately dropped. It is a coordinate on the *line*
/// axis — a y in the vertical pane and an x in the horizontal one — so it does
/// not name the same thing on the other side. The preedit goes for the same
/// reason: it belongs to a composition in the pane being left.
fn carry_caret_between_panes(
    from: &Rc<RefCell<EditorState>>,
    to: &Rc<RefCell<EditorState>>,
    source: &str,
) {
    let from = from.borrow();
    let clamp = |byte: usize| floor_char_boundary(source, byte);
    let caret = from.caret_source_byte.map(clamp);
    let anchor = from.selection_anchor_source_byte.map(clamp);
    let mut to = to.borrow_mut();
    to.caret_source_byte = caret;
    to.selection_anchor_source_byte = anchor;
    to.active_line_start = caret.map(|caret| source_line_start(source, caret));
    to.preferred_line = None;
    to.preedit.clear();
}

fn clamp_state_into(state: &Rc<RefCell<EditorState>>, source: &str) {
    let mut state = state.borrow_mut();
    let clamp = |byte: usize| floor_char_boundary(source, byte);
    state.caret_source_byte = state.caret_source_byte.map(clamp);
    state.selection_anchor_source_byte = state.selection_anchor_source_byte.map(clamp);
    state.active_line_start = state
        .caret_source_byte
        .map(|caret| source_line_start(source, caret));
}

/// Bring one pane's stored positions across an edit made in another.
fn carry_state_across(state: &Rc<RefCell<EditorState>>, change: Change, source: &str) {
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = state.caret_source_byte.map(|byte| change.moved(byte));
        state.selection_anchor_source_byte = state
            .selection_anchor_source_byte
            .map(|byte| change.moved(byte));
    }
    // Rounded to a character boundary afterwards, because the arithmetic above
    // is in bytes and the position it lands on has to be one a slice can start
    // at (6.7).
    clamp_state_into(state, source);
}

fn source_line_start(source: &str, source_byte: usize) -> usize {
    let source_byte = source_byte.min(source.len());
    source[..source_byte]
        .rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(0)
}

/// Drop what a text field hands over that a document should not hold: control
/// characters and the private-use codepoints some IMEs emit. Both panes type
/// through this.
fn normalize_typed_input(input: &str) -> String {
    // A Windows clipboard hands over CRLF, and mapping each half of it to a line
    // break would double every one. Typed input never contains a carriage
    // return, so the common path allocates nothing.
    let input = if input.contains('\r') {
        Cow::Owned(input.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(input)
    };
    input
        .chars()
        .filter_map(|character| match character {
            '\r' => Some('\n'),
            '\n' | '\t' => Some(character),
            character
                if !character.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&character) =>
            {
                Some(character)
            }
            _ => None,
        })
        .collect()
}

/// What to say when text is turned away for being over the limit.
///
/// Said rather than done quietly: refusing without a word looks like a dropped
/// keystroke, and the writer has no way to tell the two apart.
fn over_limit_message(input: &str) -> String {
    format!(
        "文書の上限{}文字を超えるため、{}文字の入力を取り消しました",
        MAX_DOCUMENT_CHARACTERS,
        input.chars().count()
    )
}

/// Whether `source` can hold `addition` and stay inside the document limit.
///
/// Counted in characters, so the answer does not depend on the encoding of what
/// is being added. The existing document is measured too rather than tracked,
/// because it is the thing being protected and it is cheap next to the paste
/// that prompted the question.
fn fits_document_limit(source: &str, addition: &str) -> bool {
    let existing = source.chars().count();
    let added = addition.chars().count();
    existing.saturating_add(added) <= MAX_DOCUMENT_CHARACTERS
}

/// One line for the whole of an edit, from the callback to the last pixel.
///
/// **The refresh lines do not cover this.** They begin once the new text is in
/// hand, and by then the document has been copied twice and moved once — work
/// that follows the size of the *document* rather than the paragraph being
/// edited, and that was never in any figure the log carried (技術検証 7.4).
/// Whether this editor could hold a much larger document is decided here.
fn log_edit(
    cache: &Rc<RefCell<RenderCache>>,
    pane: &str,
    source: &str,
    started: Instant,
    cloned_ms: f64,
    stored_ms: f64,
) {
    let total = elapsed_ms(started);
    cache.borrow_mut().log_perf(&format!(
        "edit pane={pane} total={total:.2} clone={cloned_ms:.2} store={stored_ms:.2} \
         rest={rest:.2} bytes={bytes}",
        rest = total - cloned_ms - stored_ms,
        bytes = source.len(),
    ));
    // Bytes only. Counting characters or lines here would be a pass over the
    // whole document on every keystroke, which is exactly what 6.12 took out.
    let bytes = source.len();
    cache.borrow_mut().log_diag(
        "edit",
        &format!("pane={pane} bytes={bytes} total_ms={total:.2}"),
    );
}

fn vertical_insertion_source_byte(
    source: &str,
    preview: &PreviewDocument,
    caret: usize,
    indent_line_start: bool,
) -> usize {
    let mapped = preview.source_byte_at_utf16(caret);
    let preview_byte = preview.preview_byte_at_utf16(caret);
    let is_preview_line_start = preview_byte == 0
        || preview.text.as_bytes().get(preview_byte.saturating_sub(1)) == Some(&b'\n');
    let line_start = source_line_start(source, mapped);
    let line_end = source[line_start..]
        .find('\n')
        .map(|relative| line_start + relative)
        .unwrap_or(source.len());
    let line = &source[line_start..line_end];
    let marker_content_start = markdown_block_content_start(line).map(|offset| line_start + offset);

    if indent_line_start
        && (is_preview_line_start || mapped == line_start || marker_content_start == Some(mapped))
    {
        line_start
    } else {
        mapped
    }
}

fn markdown_block_content_start(line: &str) -> Option<usize> {
    if let Some(rest) = line.strip_prefix("> ") {
        return Some(line.len() - rest.len());
    }
    if let Some(rest) = line.strip_prefix('>') {
        return Some(line.len() - rest.len());
    }

    let marker_length = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&marker_length)
        && line
            .get(marker_length..)
            .is_some_and(|rest| rest.starts_with(' '))
    {
        Some(marker_length + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 取り消しの部品は`open_document`のもので、本体はもう名前で呼んでいない。
    use crate::open_document::{Edit, History, UNDO_JOIN_IDLE};
    // 退避の刻みの規則（要件 8.1）は`saving`のもの。
    use crate::saving::work_copy_due;

    /// E3（書き手の報告 2026-09-10）: **2回目で区切る。3回目は普通の押下。**
    ///
    /// 数え続けると、押すたびに語が選び直されて選択が外れなくなる
    /// （「契機がわからないのですが、選択がはずれなくなります」）。
    #[test]
    fn the_third_click_is_a_first_click_again() {
        let mut state = EditorState::default();

        assert!(!state.double_click(100.0, 40.0), "1回目は2回目ではない");
        assert!(state.double_click(101.0, 41.0), "手のぶれの内側なら2回目");
        assert!(!state.double_click(101.0, 41.0), "3回目は次の1回目");
        assert!(state.double_click(101.0, 41.0), "その次が2回目");
    }

    /// E3: **離れたところを2回押したのは、同じものを2回押したのではない。**
    #[test]
    fn a_click_somewhere_else_is_not_the_second_of_a_pair() {
        let mut state = EditorState::default();

        assert!(!state.double_click(100.0, 40.0));
        assert!(!state.double_click(400.0, 40.0));
        // 直前の押下として覚えているのは、いま押されたほうである。
        assert!(state.double_click(400.0, 41.0));
    }

    /// 要件 5.1, 7.7: a history is newest first, holds each path once, and
    /// never grows past what it is allowed to keep.
    #[test]
    fn a_history_moves_what_is_opened_again_to_the_top() {
        let mut history = Vec::new();
        let a = PathBuf::from("D:\\甲");
        let b = PathBuf::from("D:\\乙");
        let c = PathBuf::from("D:\\丙");
        let d = PathBuf::from("D:\\丁");
        remember_path(&mut history, &a, 3);
        remember_path(&mut history, &b, 3);
        remember_path(&mut history, &a, 3);

        // Opened again, so it moves up rather than appearing twice.
        assert_eq!(history, [a.clone(), b.clone()]);

        remember_path(&mut history, &c, 3);
        remember_path(&mut history, &d, 3);

        // The oldest falls off the end; the newest is never refused.
        assert_eq!(history, [d, c, a]);
    }

    fn a_group(id: u32, name: &str, words: &[&str]) -> word_marks::WordGroup {
        word_marks::WordGroup {
            id,
            name: name.to_owned(),
            colour: Some([0.8, 0.2, 0.2]),
            words: words.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    fn a_mode(id: u32, name: &str, groups: Vec<word_marks::WordGroup>) -> word_marks::WordMode {
        word_marks::WordMode {
            id,
            name: name.to_owned(),
            groups,
        }
    }

    fn stored_of(modes: &[word_marks::WordMode]) -> app_data::StoredWords {
        app_data::StoredWords {
            modes: modes.iter().map(stored_from_mode).collect(),
            next_id: 99,
            notes: Vec::new(),
        }
    }

    /// 要件 7.9（2026-09-08）: モードは、編集器の中の形と往復する。
    ///
    /// **書き出せて読み戻せなければ、次の起動で書き手の一覧が消える。**
    #[test]
    fn a_word_mode_goes_out_and_comes_back() {
        let mode = a_mode(
            3,
            "作品A",
            vec![
                a_group(4, "人物", &["田中", "佐藤"]),
                a_group(5, "地名", &["京都"]),
            ],
        );
        let written = app_data::encode_words(&stored_of(&[mode.clone()]));
        let (read, damaged) = app_data::decode_words(&written).expect("decodes");

        assert_eq!(damaged, 0);
        assert_eq!(read.next_id, 99, "次に配る番号も覚えている");
        assert_eq!(read.modes.len(), 1);
        let back = mode_from_stored(&read.modes[0]);
        assert_eq!(back.id, 3, "番号は変わらない");
        assert_eq!(back.name, mode.name);
        assert_eq!(back.groups.len(), 2);
        assert_eq!(back.groups[0].id, 4);
        assert_eq!(back.groups[0].words, ["田中", "佐藤"]);
        assert_eq!(back.groups[1].words, ["京都"]);
        let was = mode.groups[0].colour.expect("色がある");
        let now = back.groups[0].colour.expect("色は往復する");
        for (was, now) in was.iter().zip(now.iter()) {
            assert!((was - now).abs() < 0.005, "{was} と {now}");
        }
    }

    /// 除外語群（書き手と決めた 2026-09-08）: **色を持たない語群は往復する。**
    ///
    /// 色が戻らなければ、次の起動で除外語群がただの語群になり、**止めていた
    /// 短い語が一斉に光りだす。**
    #[test]
    fn a_group_with_no_colour_goes_out_and_comes_back() {
        let mut group = a_group(4, "除外", &["カリオン"]);
        group.colour = None;
        let mode = a_mode(3, "作品A", vec![group]);
        let written = app_data::encode_words(&stored_of(&[mode]));
        assert!(
            written.contains("| 除外 | none"),
            "綴りは`none`:\n{written}"
        );

        let (read, damaged) = app_data::decode_words(&written).expect("decodes");
        assert_eq!(damaged, 0);
        let back = mode_from_stored(&read.modes[0]);
        assert_eq!(back.groups[0].colour, None);
        assert_eq!(back.groups[0].words, ["カリオン"]);
    }

    /// 版3（書き手の求め 2026-09-08）: **語は前置き無しの1行**、`#`は覚え書き。
    ///
    /// 「毎行Word半角スペースを入れるのは、作業量が多いです」——**手で足すのが
    /// 普通の道になった以上、打鍵の少ないほうが正しい形である。**
    #[test]
    fn a_word_is_a_bare_line_and_a_hash_is_a_note() {
        let raw = concat!(
            "RFN-EDIT-WORDS 3\n",
            "next: 9\n",
            "# この辞書について\n",
            "mode: 1 | 作品A\n",
            "group: 2 | 人物 | #cc3333\n",
            "# 主要人物\n",
            "田中\n",
            "\n",
            "佐藤\n",
            "word: mode: 名前が鍵と紛れる語\n",
        );
        let (read, damaged) = app_data::decode_words(raw).expect("decodes");

        assert_eq!(damaged, 0);
        assert_eq!(read.notes, ["# この辞書について"], "語群の外の覚え書き");
        assert_eq!(
            read.modes[0].groups[0].words,
            ["# 主要人物", "田中", "佐藤", "mode: 名前が鍵と紛れる語"],
            "覚え書きは並びのまま残り、紛れる語は前置きで守られる"
        );

        // **書き直しても同じものが読める**——前置きが要るのは紛れる語だけ。
        let again = app_data::encode_words(&read);
        assert!(again.contains("\n田中\n"), "語は素の行で出る:\n{again}");
        assert!(
            again.contains("word: mode: 名前が紛れる語") || again.contains("word: mode: "),
            "紛れる語にだけ前置きが付く:\n{again}"
        );
        let (round, _) = app_data::decode_words(&again).expect("decodes again");
        assert_eq!(round, read, "往復して同じ");
    }

    /// **古い版は読まない**（2026-09-08、書き手の指示）。まだ誰の辞書も世に
    /// 出ていない段階で、形式は文書に書いてあれば足りる。**読めない表は
    /// 上書きしない**（要件 4.4）ので、間違って古い版を指しても消えはしない。
    #[test]
    fn another_version_is_not_this_table() {
        let raw = concat!("RFN-EDIT-WORDS 2\n", "next: 5\n", "mode: 1 | 作品A\n",);

        assert!(app_data::decode_words(raw).is_none());
    }

    /// 二つのモードがあっても、語がどちらのものかを取り違えない。
    #[test]
    fn two_modes_keep_their_own_groups() {
        let one = a_mode(1, "作品A", vec![a_group(2, "人物", &["田中"])]);
        let other = a_mode(3, "作品B", vec![a_group(4, "人物", &["山田", "伊藤"])]);
        let written = app_data::encode_words(&stored_of(&[one, other]));
        let (read, _) = app_data::decode_words(&written).expect("decodes");

        assert_eq!(read.modes.len(), 2);
        assert_eq!(read.modes[0].groups[0].words, ["田中"]);
        assert_eq!(read.modes[1].groups[0].words, ["山田", "伊藤"]);
    }

    /// **読めなかった行は数える**（2026-09-08）。黙って半分になった辞書は、
    /// いちばん気づきにくい失い方である。
    #[test]
    fn damaged_lines_are_counted_not_hidden() {
        let raw = concat!(
            "RFN-EDIT-WORDS 3\n",
            "next: 9\n",
            "これは行ではない\n",
            "word: 迷子\n",
            "  mode: 1 | 作品A\n",
            "group: 2 | 人物 | #cc3333\n",
            "word: 田中\n",
        );
        let (read, damaged) = app_data::decode_words(raw).expect("decodes");

        assert_eq!(damaged, 2, "行ではない1行と、モードの外の語1つ");
        // 頭に空白のある`mode:`も読める——**手で開いて直せるファイル**なので、
        // 字下げくらいで語が消えては困る。
        assert_eq!(read.modes.len(), 1);
        assert_eq!(read.modes[0].groups[0].words, ["田中"], "読めたぶんは残る");
    }

    /// **番号を書かずに手で足された表には、読むときに配る**（2026-09-08）。
    ///
    /// 辞書は書き手が開いて直せるもの（要件 5.3）なので、`mode: 作品A`と1行
    /// 書いただけのものが来る。**番号は編集器の都合であって、書き手が覚えて
    /// おくものではない。**
    #[test]
    fn a_hand_written_table_is_given_ids_when_it_is_read() {
        let raw = "RFN-EDIT-WORDS 3\nmode: 作品A\ngroup: 人物 | #cc3333\n田中\n";
        let (read, _) = app_data::decode_words(raw).expect("decodes");

        assert!(read.modes[0].id > 0);
        assert!(read.modes[0].groups[0].id > 0);
        assert_ne!(read.modes[0].id, read.modes[0].groups[0].id);
        assert!(read.next_id > read.modes[0].groups[0].id);
    }

    /// **この編集器の表でないものは`None`。**呼ぶ側はそれを上書きしない。
    #[test]
    fn a_table_from_something_else_is_refused() {
        assert!(app_data::decode_words("hello\nmode: 1 | 作品A\n").is_none());
    }

    /// 追加要件 2026-09-08: 接続先の一覧は設定ファイルの行になり、行から
    /// 戻ってくる。**名前にも引数にも空白がある**ので、切るのは`|`であって
    /// 空白ではない。
    #[test]
    fn a_shell_line_goes_out_and_comes_back_the_same() {
        let shell = TerminalShell {
            name: "PowerShell 7".to_owned(),
            command: "pwsh.exe -NoLogo -WorkingDirectory .".to_owned(),
        };

        assert_eq!(
            shell.written(),
            "PowerShell 7 | pwsh.exe -NoLogo -WorkingDirectory ."
        );
        assert_eq!(TerminalShell::read(&shell.written()), Some(shell));
    }

    /// **`|`の無い行は、丸ごとコマンド行。**手で足すとき、名前を考えるまでも
    /// ない接続先がある（`cmd.exe`）——そのときは道具の名前がそのまま名前に
    /// なる。
    #[test]
    fn a_shell_line_with_no_name_is_named_after_its_program() {
        let read = TerminalShell::read("  cmd.exe /k chcp 65001  ").expect("reads");

        assert_eq!(read.name, "cmd.exe");
        assert_eq!(read.command, "cmd.exe /k chcp 65001");
        // PATHを探すのは最初の語だけ。引数はその道具のもので、ファイルの名前
        // ではない。
        assert_eq!(read.program(), "cmd.exe");
    }

    /// 片側しか無い行は行ではない。**断るのは`decode_settings`と同じ構え**
    /// ——読めない1行は、設定を全部初期値へ戻す理由にはならない。
    #[test]
    fn half_a_shell_line_is_refused() {
        assert_eq!(TerminalShell::read(""), None);
        assert_eq!(TerminalShell::read("   "), None);
        assert_eq!(TerminalShell::read("WSL |"), None);
        assert_eq!(TerminalShell::read("| wsl.exe"), None);
    }

    /// 組み込みの三つは、そのまま設定ファイルの行として往復できる——初回に
    /// 書き出されるのはこれで、書き手が手を入れる出発点になる。
    #[test]
    fn the_built_in_shells_survive_the_settings_file() {
        for shell in TerminalShell::built_in() {
            let line = shell.written();
            assert_eq!(TerminalShell::read(&line), Some(shell.clone()));
            let values = vec![(format!("{SHELL_SETTING}.0"), line)];
            let file = app_data::encode_settings(&values);
            let read = app_data::decode_settings(&file).expect("reads");

            assert_eq!(read, values, "{} は行のまま戻る", shell.name);
        }
    }

    fn engine_for(text: &str, zoom: i32) -> TextEngine {
        let mut engine = TextEngine::default();
        engine
            .update(
                StyledText::plain(text),
                LineFit::Extent(PREVIEW_HEIGHT),
                &Typography::new(font_size_for(BASE_FONT_SIZE, zoom)),
            )
            .expect("vertical layout");
        engine
    }

    fn engine_for_height(text: &str, height: u32) -> TextEngine {
        let mut engine = TextEngine::default();
        engine
            .update(
                StyledText::plain(text),
                LineFit::Extent(height),
                &Typography::new(font_size_for(BASE_FONT_SIZE, 100)),
            )
            .expect("vertical layout");
        engine
    }

    /// A pane's state with just the two positions the caret helpers read.
    fn editor_state(caret: Option<usize>, line: Option<usize>) -> Rc<RefCell<EditorState>> {
        Rc::new(RefCell::new(EditorState {
            caret_source_byte: caret,
            active_line_start: line,
            ..EditorState::default()
        }))
    }

    /// Tiles keyed by a stand-in fingerprint, laid out left to right for the
    /// eviction tests. Key `n` was last placed at `n * width`.
    fn tile_map(count: u32, width: u32) -> BTreeMap<u64, CachedTile> {
        (0..count)
            .map(|index| {
                (
                    index as u64,
                    CachedTile {
                        image: Image::default(),
                        pixels: SharedPixelBuffer::new(1, 1),
                        last_flow: (index * width) as i32,
                    },
                )
            })
            .collect()
    }

    fn long_document(characters: usize) -> String {
        let paragraph = "## 長文性能検証\n\nこれは二万文字から三万文字のMarkdown文書を想定した性能確認用の段落です。縦書きの日本語、句読点、全角英数字ＡＢＣ１２３、半角英数字ABC123を含め、表示タイルの遅延生成とスクロール応答を確認します。\n\n";
        paragraph
            .repeat(characters.div_ceil(paragraph.chars().count()))
            .chars()
            .take(characters)
            .collect()
    }

    /// A pane the tree does not name gets no area, and having no area is what
    /// being off screen means (`PaneId::is_shown`).
    #[test]
    fn renders_only_the_panes_the_tree_names() {
        let area = pane_layout::Rect::new(0.0, 0.0, 800.0, 600.0);
        let alone = Layout::single(PaneId(2).index() as usize);
        let (placed, boundaries) = alone.place(area);

        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].0, PaneId(2).index() as usize);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn inserts_preedit_only_into_the_rendered_preview() {
        let preview = PreviewDocument::from_source("# 見出し");
        let shown = PaneText::Preview(&preview);

        let (text, caret, range) = text_with_preedit(&shown, Some(2), "変換");

        assert_eq!(preview.text, "見出し");
        assert_eq!(text, "見出変換し");
        assert_eq!(caret, Some(4));
        assert_eq!(range, Some((2, 2)));
    }

    #[test]
    fn borrows_the_preview_when_there_is_no_preedit() {
        let preview = PreviewDocument::from_source("# 見出し\n本文");
        let shown = PaneText::Preview(&preview);

        let (text, _, range) = text_with_preedit(&shown, Some(1), "");

        assert!(
            matches!(text, Cow::Borrowed(_)),
            "a keystroke without IME must not copy the document"
        );
        assert_eq!(range, None);
    }

    /// Tab in the horizontal pane replaces the selection, like any other input.
    /// It used to go through Slint's `TextInput` byte offsets; now it is the same
    /// path every horizontal keystroke takes.
    #[test]
    fn inserts_a_four_space_indent_at_the_horizontal_selection() {
        let mut text = "前方後方".to_owned();
        let selection = ("前方".len(), "前方後".len());

        let caret = replace_source_range(&mut text, selection, TAB_INDENT);

        assert_eq!(text, "前方    方");
        assert_eq!(caret, "前方    ".len());
    }

    /// 要件 4.2（書き手の報告 2026-09-08）: **Markdownでないものは横書きの
    /// ソースで開く。**縦書きのペインで設定ファイルを開いたら、設定ファイルまで
    /// 縦書きになっていた。
    ///
    /// **拡張子で判じる**ので、大小の別は無く、`.`で始まる名前は拡張子ではない。
    #[test]
    fn only_markdown_opens_the_way_the_pane_is_set() {
        for path in ["原稿.md", "原稿.MD", "note.markdown", "a/b/章1.mdown"] {
            assert!(is_markdown_path(Path::new(path)), "{path}");
        }
        for path in [
            "words.rfnwords",
            "settings.rfnsettings",
            "memo.txt",
            "README",
            ".md",
        ] {
            assert!(!is_markdown_path(Path::new(path)), "{path}");
        }
    }

    /// 追加要件 2026-09-06: renaming a tab selects the name without its
    /// extension, so that typing replaces the name and leaves the `.md`.
    ///
    /// **The last dot, and never the first character**: `.gitignore` is a name
    /// that begins with a dot, not an extension with nothing in front of it.
    ///
    /// **数えるのはバイト**（2026-09-09）。渡す先の`set-selection-offsets`が
    /// Slintの言葉で「2つのUTF-8の位置のあいだを選ぶ」ものだからで、文字で
    /// 数えていたあいだ**日本語の名前は頭の1文字しか選ばれていなかった**。
    #[test]
    fn a_rename_selects_the_name_without_its_extension() {
        assert_eq!(stem_length("Part01設計.md"), "Part01設計".len() as i32);
        assert_eq!(stem_length("notes.tar.gz"), "notes.tar".len() as i32);
        // No extension: all of it is the name.
        assert_eq!(stem_length("README"), 6);
        assert_eq!(stem_length("無題1"), "無題1".len() as i32);
        // A dotfile is a name, not an empty one with an extension.
        assert_eq!(stem_length(".gitignore"), 10);
        assert_eq!(stem_length(""), 0);
        // **バイトであって文字ではない**——`あいう`は9バイト。ここを3にして
        // いたので、`あいう.md`の名前変更は`あ`だけを選んでいた。
        assert_eq!(stem_length("あいう.md"), 9);
    }

    #[test]
    fn tab_indent_has_a_stable_four_character_width() {
        assert_eq!(TAB_INDENT, "    ");
        assert_eq!(TAB_INDENT.chars().count(), 4);
    }

    /// The horizontal pane draws the source itself, so its whole position
    /// mapping is these two functions. If they ever disagree, the caret lands
    /// somewhere other than where it was drawn.
    #[test]
    fn maps_every_source_position_to_utf16_and_back() {
        let source = "# 見出し\n\n本文ABC123と絵文字🇯🇵と結合文字がある行。\n";

        for (byte, _) in source
            .char_indices()
            .chain(std::iter::once((source.len(), ' ')))
        {
            let utf16 = utf16_at_byte(source, byte);
            assert_eq!(
                byte_at_utf16(source, utf16),
                byte,
                "byte {byte} became UTF-16 {utf16} and came back elsewhere"
            );
        }
        assert_eq!(
            utf16_at_byte(source, source.len()),
            source.encode_utf16().count()
        );
    }

    /// 要件 7.1: a rectangle comes out as lines, because that is the shape it
    /// had — another program pastes it back as a block.
    #[test]
    fn a_rectangle_is_copied_as_one_line_per_line_it_covered() {
        // The runs a rectangle three lines tall cuts: the same columns out of
        // each of the three lines the engine laid out.
        let source = "abcdef\nabcdef\nabcdef";
        let ranges = [(1, 4), (8, 11), (15, 18)];

        assert_eq!(selected_text(source, &ranges), "bcd\nbcd\nbcd");
    }

    /// An ordinary selection is one run and comes out as it stands.
    #[test]
    fn an_ordinary_selection_is_copied_unchanged() {
        let source = "ひとつ目\nふたつ目";

        assert_eq!(selected_text(source, &[(3, 16)]), "とつ目\nふ");
    }

    /// **One change for the whole rectangle** (要件 7.1): the span it covers,
    /// with its runs gone and everything between them still there.
    #[test]
    fn taking_a_rectangle_out_leaves_one_span_to_record() {
        let source = "abcdef\nabcdef\nabcdef";
        let ranges = [(1, 4), (8, 11), (15, 18)];

        let (start, end, kept) = spliced_out(source, &ranges).expect("three runs");
        assert_eq!((start, end), (1, 18));
        assert_eq!(kept, "ef\naef\na");
        // What the document would become, put back together.
        let mut after = source.to_owned();
        after.replace_range(start..end, &kept);
        assert_eq!(after, "aef\naef\naef");
    }

    /// A rectangle of no width takes nothing out, however many lines it covers.
    #[test]
    fn a_rectangle_of_no_width_takes_nothing() {
        // Two lines, and nothing under the columns on either.
        let source = "abcdef\nabcdef";
        let ranges = [(2, 2), (9, 9)];

        assert_eq!(selected_text(source, &ranges), "\n");
        assert_eq!(spliced_out(source, &ranges), None);
    }

    /// `Ctrl+K` reaches the end of the line (要件 11.4).
    #[test]
    fn a_kill_reaches_the_end_of_the_line() {
        let source = "ひとつ目\nふたつ目\n";

        // Between the second and third character of the first line.
        assert_eq!(line_end_to_kill(source, 6), 12);
    }

    /// At the end of a line there is nothing left of it, so the break goes.
    /// **Two presses take the line and then close the gap**, which is what
    /// makes the key usable at all.
    #[test]
    fn a_kill_at_the_end_of_a_line_takes_the_break() {
        let source = "ひとつ目\nふたつ目\n";

        assert_eq!(line_end_to_kill(source, 12), 13);
    }

    /// The very end of the document has neither line nor break left.
    #[test]
    fn a_kill_at_the_end_of_the_document_reaches_nothing() {
        let source = "ひとつ目";

        assert_eq!(line_end_to_kill(source, source.len()), source.len());
    }

    /// A document that does not end in a break still has a line to take.
    #[test]
    fn a_kill_on_the_last_line_reaches_the_end_of_the_text() {
        let source = "ひとつ目\nふたつ目";

        assert_eq!(line_end_to_kill(source, 13), source.len());
    }

    /// An empty line is a break and nothing else.
    #[test]
    fn a_kill_on_an_empty_line_takes_its_break() {
        let source = "\n\nあと";

        assert_eq!(line_end_to_kill(source, 0), 1);
    }

    /// The strip has two ends and the key has one direction (要件 11.3).
    #[test]
    fn stepping_past_either_end_of_the_strip_comes_round() {
        assert_eq!(stepped_tab(3, 2, false), Some(0));
        assert_eq!(stepped_tab(3, 0, true), Some(2));
    }

    #[test]
    fn stepping_walks_the_strip_in_the_order_it_is_drawn() {
        assert_eq!(stepped_tab(3, 0, false), Some(1));
        assert_eq!(stepped_tab(3, 1, false), Some(2));
        assert_eq!(stepped_tab(3, 2, true), Some(1));
    }

    /// One tab is not a strip to walk, and a pane with none is not either.
    #[test]
    fn a_strip_of_one_has_nowhere_to_step() {
        assert_eq!(stepped_tab(1, 0, false), None);
        assert_eq!(stepped_tab(1, 0, true), None);
        assert_eq!(stepped_tab(0, 0, false), None);
    }

    /// Pasting is the only way a line break reaches this function in quantity,
    /// and the clipboard on Windows uses CRLF. Treating each half as a break of
    /// its own turned every pasted line break into two.
    /// The limit is a rule about documents, so it counts characters — a paste
    /// of the same text is accepted or refused the same way whatever its bytes
    /// weigh.
    #[test]
    fn refuses_an_addition_that_would_pass_the_document_limit() {
        let nearly_full = "あ".repeat(MAX_DOCUMENT_CHARACTERS - 2);

        assert!(fits_document_limit(&nearly_full, "あい"));
        assert!(!fits_document_limit(&nearly_full, "あいう"));
        // Counted in characters, not bytes: three ASCII characters weigh three
        // bytes and three ideographs weigh nine, and both are three characters.
        assert!(!fits_document_limit(&nearly_full, "abc"));
        assert!(fits_document_limit(
            "",
            &"あ".repeat(MAX_DOCUMENT_CHARACTERS)
        ));
    }

    #[test]
    fn keeps_pasted_line_breaks_and_folds_crlf_into_one() {
        assert_eq!(
            normalize_typed_input("一行目\r\n二行目\r\n"),
            "一行目\n二行目\n"
        );
        assert_eq!(normalize_typed_input("古い\rMac"), "古い\nMac");
        assert_eq!(normalize_typed_input("段落\n\n次"), "段落\n\n次");
        assert_eq!(
            normalize_typed_input("制御\u{7}文字\u{e000}は落とす\tタブは残す"),
            "制御文字は落とす\tタブは残す"
        );
    }

    /// The crash this pane shipped with: one pane holds its caret as a source
    /// byte while the other edits the document, so a held position can end up
    /// inside a character. Every slice taken from such a position panics, and
    /// the guard that was supposed to catch it called back into the very
    /// function that panicked.
    #[test]
    fn survives_a_caret_left_inside_a_character_by_the_other_pane() {
        let source = "あいうえお";
        let inside = 4; // The middle of 'い', which occupies bytes 3..6.
        assert!(!source.is_char_boundary(inside));

        assert_eq!(floor_char_boundary(source, inside), 3);
        assert_eq!(utf16_at_byte(source, inside), 1, "'あ' is one UTF-16 unit");
        assert_eq!(previous_grapheme_byte(source, inside), 0);
        assert_eq!(next_grapheme_byte(source, inside), 6);
        assert_eq!(
            floor_char_boundary(source, 9_999),
            source.len(),
            "past the end clamps to the end"
        );
        assert_eq!(floor_char_boundary("", 4), 0);
    }

    #[test]
    fn a_pane_rounds_its_caret_down_to_a_character_start() {
        let source = "あいうえお";
        let inside = 4; // The middle of 'い', which occupies bytes 3..6.
        for id in [PaneId::FIRST, PaneId(1), PaneId(7)] {
            let state = editor_state(Some(inside), None);
            assert_eq!(id.caret_byte(&state, source), 3, "{id:?}");
            let state = editor_state(Some(9_999), None);
            assert_eq!(id.caret_byte(&state, source), source.len(), "{id:?}");
            let state = editor_state(None, None);
            assert_eq!(
                id.caret_byte(&state, source),
                source.len(),
                "{id:?}: no caret yet means the end of the document"
            );
        }
    }

    /// 要件 9: **zero is not a zoom.** A pane row nothing has written yet and a
    /// session from before the zoom belonged to a pane both say 0, and a pane
    /// set to 0% would be a pane with nothing on it.
    #[test]
    fn a_stored_zoom_of_zero_means_the_default() {
        assert_eq!(zoom_from(0), ZOOM_DEFAULT);
        assert_eq!(zoom_from(125), 125);
        // Held inside the bounds the keys hold it to, whichever way it came in.
        assert_eq!(zoom_from(4000), ZOOM_MAX);
        assert_eq!(zoom_from(1), ZOOM_MIN);
        assert_eq!(zoom_from(-30), ZOOM_MIN);
        // A pane at either end stays there rather than wrapping when the next
        // notch arrives, which is what `on_pane_zoom` adds to.
        assert_eq!(zoom_from(ZOOM_MAX + ZOOM_STEP), ZOOM_MAX);
        assert_eq!(zoom_from(ZOOM_MIN - ZOOM_STEP), ZOOM_MIN);
    }

    /// A new pane opens at the size the writer left nothing about, and every
    /// pane opens the same — the zoom is per pane, not per direction.
    #[test]
    fn a_pane_opens_at_the_default_zoom() {
        for pane in 0..4_u32 {
            let row = PaneId(pane).initial_screen(false, false);
            assert_eq!(zoom_from(row.zoom), ZOOM_DEFAULT, "{row:?}");
        }
    }

    /// The pane model is indexed by [`PaneId::index`], so a row has to sit at
    /// the position its own number names. A row in the wrong place would send
    /// a keystroke to the pane beside the one it was typed in.
    ///
    /// [`PaneId::index`]: PaneId::index
    #[test]
    fn a_pane_row_sits_at_the_position_its_number_names() {
        let rows = (0..5_u32)
            .map(|pane| PaneId(pane).initial_screen(false, false))
            .collect::<Vec<PaneScreen>>();

        for (position, row) in rows.iter().enumerate() {
            assert_eq!(row.id as usize, position, "{row:?}");
        }
        // **A row's number says nothing about how it draws** (要件 7.2): that
        // comes from the tab in front of it, and a new pane is told what to
        // show by the pane it was split from.
        assert!(rows.iter().all(|row| !row.vertical));
    }

    #[test]
    fn only_the_vertical_pane_keeps_the_line_it_reveals() {
        let source = "# 見出し\n本文\n";
        let second = "# 見出し\n".len();
        let state = editor_state(Some(second), Some(0));

        assert_eq!(
            PaneId::revealed_line(true, &state, source),
            Some(0),
            "the stored line lags the caret on purpose"
        );
        assert_eq!(
            PaneId::revealed_line(false, &state, source),
            Some(second),
            "the caret's own line, whatever the other pane stored"
        );

        let empty = editor_state(None, None);
        assert_eq!(PaneId::revealed_line(true, &empty, source), None);
        assert_eq!(PaneId::revealed_line(false, &empty, source), None);
    }

    #[test]
    fn the_line_anchor_is_the_coordinate_across_the_flow() {
        let caret = CaretGeometry {
            x: 12.0,
            y: 34.0,
            width: 2.0,
            height: 20.0,
        };

        assert_eq!(PaneId::line_anchor(true, &caret), 34.0);
        assert_eq!(PaneId::line_anchor(false, &caret), 12.0);
    }

    #[test]
    fn steps_the_horizontal_caret_by_grapheme_cluster() {
        let source = "あ🇯🇵い";
        let flag = "あ".len() + "🇯🇵".len();

        assert_eq!(next_grapheme_byte(source, "あ".len()), flag);
        assert_eq!(previous_grapheme_byte(source, flag), "あ".len());
        assert_eq!(previous_grapheme_byte(source, 0), 0, "clamps at the start");
        assert_eq!(
            next_grapheme_byte(source, source.len()),
            source.len(),
            "clamps at the end"
        );
    }

    #[test]
    fn splices_the_horizontal_preedit_into_the_rendered_text_only() {
        let source = "本文です";
        let caret = utf16_at_byte(source, "本文".len()) as u32;

        let shown = PaneText::Source(source);
        let (rendered, render_caret, range) = text_with_preedit(&shown, Some(caret), "かん");

        assert_eq!(rendered, "本文かんです");
        assert_eq!(
            render_caret,
            Some(caret + 2),
            "the caret follows the preedit"
        );
        assert_eq!(range, Some((caret, 2)));
        assert_eq!(source, "本文です", "the document itself is untouched");

        let (borrowed, _, none) = text_with_preedit(&shown, Some(caret), "");
        assert!(
            matches!(borrowed, Cow::Borrowed(_)),
            "a keystroke without IME must not copy the document"
        );
        assert_eq!(none, None);
    }

    #[test]
    fn falls_back_to_the_default_line_width_before_the_horizontal_pane_reports_one() {
        assert_eq!(usable_horizontal_width(f32::NAN), HORIZONTAL_WIDTH);
        assert_eq!(usable_horizontal_width(0.0), HORIZONTAL_WIDTH);
        assert_eq!(
            usable_horizontal_width(MIN_HORIZONTAL_WIDTH as f32 - 1.0),
            HORIZONTAL_WIDTH
        );
        assert_eq!(usable_horizontal_width(880.0), 880);
    }

    /// The horizontal document is anchored at the top, so a line added anywhere
    /// leaves everything above it where it was and the viewport must not move.
    /// The vertical pane is the one that has to compensate.
    #[test]
    fn keeps_the_horizontal_caret_inside_its_viewport() {
        // A caret at the bottom edge pulls the viewport down.
        assert_eq!(
            caret_visible_scroll(0.0, 600.0, 2400.0, 590.0, 22.0),
            -36.0,
            "a caret below the fold scrolls the pane"
        );
        // One already in view moves nothing.
        assert_eq!(
            caret_visible_scroll(-100.0, 600.0, 2400.0, 400.0, 22.0),
            -100.0
        );
        // A document shorter than the pane never scrolls.
        assert_eq!(caret_visible_scroll(0.0, 600.0, 400.0, 380.0, 22.0), 0.0);
    }

    #[test]
    fn reports_the_caret_itself_as_the_vertical_composition() {
        let caret = directwrite_render::CaretGeometry {
            x: 100.0,
            y: 64.0,
            width: 22.0,
            height: 22.0,
        };

        // Vertical: the caret itself, so the IME picks the side.
        assert_eq!(ime_candidate_anchor(&caret, true), (100.0, 64.0));
        // Horizontal: below and to the right, where a horizontal list belongs.
        assert_eq!(ime_candidate_anchor(&caret, false), (130.0, 94.0));
    }

    #[test]
    fn keeps_the_vertical_caret_inside_the_horizontal_viewport() {
        assert_eq!(
            caret_visible_scroll(0.0, 600.0, 1200.0, 1140.0, 24.0),
            -588.0
        );
        assert_eq!(
            caret_visible_scroll(-588.0, 600.0, 1200.0, 540.0, 24.0),
            -516.0
        );
        assert_eq!(
            caret_visible_scroll(-516.0, 600.0, 1200.0, 700.0, 24.0),
            -516.0
        );
        assert_eq!(caret_visible_scroll(-80.0, 600.0, 500.0, 450.0, 24.0), 0.0);
    }

    /// Closing 無題2 and asking for a new document gives 無題2 back, rather
    /// than counting away from the numbers actually on screen (要件 8.4).
    #[test]
    fn a_colour_is_read_when_it_is_complete_and_not_before() {
        assert_eq!(parse_hex_colour("#ffffff"), Some([1.0, 1.0, 1.0]));
        assert_eq!(parse_hex_colour("000000"), Some([0.0, 0.0, 0.0]));
        // Either case, and the spaces a paste can bring with it.
        assert_eq!(parse_hex_colour(" #FF0000 "), Some([1.0, 0.0, 0.0]));
        // **Half-written is not wrong, it is not finished.** Every keystroke
        // is offered while the writer types, and only a whole colour is taken.
        assert_eq!(parse_hex_colour("#ff00"), None);
        assert_eq!(parse_hex_colour("#gggggg"), None);
        assert_eq!(parse_hex_colour(""), None);
    }

    #[test]
    fn a_colour_survives_being_written_down_and_read_back() {
        let written = hex_colour(slint_colour([36.0 / 255.0, 33.0 / 255.0, 30.0 / 255.0]));
        assert_eq!(written, "#24211e");
        let read = parse_hex_colour(&written).expect("what was just written");
        assert_eq!(hex_colour(slint_colour(read)), written);
    }

    #[test]
    fn the_two_sheets_open_on_two_papers() {
        // **Different by default, the same if the writer says so.** The pair
        // used to be one colour and a rule that deepened it for vertical text;
        // now each sheet has a paper of its own and the rule is only what they
        // start at (要件 9).
        assert_eq!(default_colour(0, PAPER_SLOT), DEFAULT_PAPER);
        assert_ne!(default_colour(1, PAPER_SLOT), DEFAULT_PAPER);
        // Every other slot is ink, and ink starts the same either way.
        assert_eq!(default_colour(0, 0), DEFAULT_INK);
        assert_eq!(default_colour(1, 3), DEFAULT_INK);
    }

    #[test]
    fn a_settings_name_says_which_sheet_it_is_for() {
        assert_eq!(sheet_prefix("h.body-size"), (Some(0), "body-size"));
        assert_eq!(sheet_prefix("v.ink-h1"), (Some(1), "ink-h1"));
        // **No prefix is a name from before the sheets were split**, and it
        // meant one value for both.
        assert_eq!(sheet_prefix("font-code"), (None, "font-code"));
    }

    #[test]
    fn a_count_is_marked_off_in_threes() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(12345), "12,345");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn a_new_document_takes_the_smallest_free_number() {
        assert_eq!(next_untitled_number(&[]), 1);
        assert_eq!(next_untitled_number(&[1]), 2);
        assert_eq!(next_untitled_number(&[1, 2, 3]), 4);
        assert_eq!(next_untitled_number(&[1, 3]), 2);
        // A saved document reports 0, which must never be handed out.
        assert_eq!(next_untitled_number(&[0, 1]), 2);
    }

    #[test]
    fn closing_a_tab_keeps_the_same_one_active() {
        // Three tabs, the last active, the first closed: still the last.
        assert_eq!(active_after_close(3, 2, 0), 1);
        // Closing after the active one leaves it where it is.
        assert_eq!(active_after_close(3, 0, 2), 0);
    }

    #[test]
    fn closing_the_active_tab_moves_to_the_one_that_took_its_place() {
        assert_eq!(active_after_close(3, 1, 1), 1);
        // Closing the last tab moves to the new last.
        assert_eq!(active_after_close(3, 2, 2), 1);
    }

    /// 要件 6.5: carrying a tab does not change which document is in front.
    #[test]
    fn carrying_a_tab_keeps_the_same_one_in_front() {
        // The tab in front is the one being carried: the front goes with it.
        assert_eq!(active_after_move(1, 1, 3), 3);
        assert_eq!(active_after_move(2, 2, 0), 0);
        // Carried from before it to after it: everything shuffles down one.
        assert_eq!(active_after_move(2, 0, 3), 1);
        // Carried from after it to before it: up one.
        assert_eq!(active_after_move(2, 4, 1), 3);
        // Carried past it on the far side, both ways: it does not move.
        assert_eq!(active_after_move(2, 3, 4), 2);
        assert_eq!(active_after_move(2, 1, 0), 2);
        // Landing on the tab in front counts as passing it.
        assert_eq!(active_after_move(2, 0, 2), 1);
        assert_eq!(active_after_move(2, 4, 2), 3);
    }

    /// The pair the arithmetic above stands for: a strip in a known order,
    /// carried, comes out in the order the writer put it in.
    #[test]
    fn carrying_a_tab_puts_it_where_it_was_let_go() {
        let mut strip = vec!["一", "二", "三", "四"];
        let carried = strip.remove(3);
        strip.insert(1, carried);
        assert_eq!(strip, ["一", "四", "二", "三"]);

        let mut strip = vec!["一", "二", "三", "四"];
        let carried = strip.remove(0);
        strip.insert(3, carried);
        assert_eq!(strip, ["二", "三", "四", "一"]);
    }

    #[test]
    fn closing_the_only_tab_leaves_the_index_at_zero() {
        assert_eq!(active_after_close(1, 0, 0), 0);
    }

    #[test]
    fn closing_the_other_tabs_leaves_out_the_one_in_front() {
        assert_eq!(close_run_positions(4, 2, true), vec![0, 1, 3]);
        // The first and the last are nothing special.
        assert_eq!(close_run_positions(3, 0, true), vec![1, 2]);
        assert_eq!(close_run_positions(3, 2, true), vec![0, 1]);
    }

    #[test]
    fn closing_all_the_tabs_leaves_none_of_them_out() {
        assert_eq!(close_run_positions(3, 1, false), vec![0, 1, 2]);
        // 他のタブを閉じる with nothing beside it closes nothing at all, which
        // is not the same as closing the one tab there is.
        assert_eq!(close_run_positions(1, 0, true), Vec::<usize>::new());
        assert_eq!(close_run_positions(1, 0, false), vec![0]);
    }

    #[test]
    fn an_active_position_past_the_end_still_keeps_a_tab() {
        // What a strip carries for the moment between losing its last tab and
        // being told which one is in front now.
        assert_eq!(close_run_positions(2, 5, true), vec![0]);
        assert_eq!(close_run_positions(0, 3, true), Vec::<usize>::new());
    }

    /// Stopping puts the work copy two seconds later (要件 8.1).
    #[test]
    fn a_pause_in_typing_brings_the_work_copy_forward() {
        let just_stopped = work_copy_due(Duration::from_millis(1900), Duration::from_secs(3));
        let stopped = work_copy_due(Duration::from_millis(2100), Duration::from_secs(3));
        assert!(!just_stopped);
        assert!(stopped);
    }

    /// The rule that matters most: typing that never pauses is exactly when
    /// there is the most to lose, and the idle rule alone would never fire.
    #[test]
    fn continuous_typing_still_gets_a_work_copy_within_five_seconds() {
        let typing = Duration::from_millis(100);
        assert!(!work_copy_due(typing, Duration::from_millis(4900)));
        assert!(work_copy_due(typing, Duration::from_millis(5100)));
    }

    /// A viewport reported taller than the pane really shows cannot reach the
    /// end of the document (6.16).
    ///
    /// The scroll is clamped to `visible - content`, so the over-estimate that
    /// is safe for choosing tiles stops the document short here and leaves the
    /// last lines beyond every caret. The numbers are the ones the log carried
    /// when this was found: 585 shown, 760 estimated, 861 of content.
    #[test]
    fn an_over_estimated_viewport_cannot_reach_the_last_line() {
        let real = caret_visible_scroll(0.0, 585.0, 861.0, 840.0, 24.0);
        let over = caret_visible_scroll(0.0, 760.0, 861.0, 840.0, 24.0);
        assert_eq!(real, 585.0 - 861.0, "the true viewport reaches the end");
        assert_eq!(over, 760.0 - 861.0);
        assert!(
            over > real,
            "the over-estimate stops {} short of the end",
            over - real
        );
    }

    fn undone(history: &mut History, source: &mut String) -> Option<usize> {
        history.undo_into(source).map(|(caret, _)| caret)
    }

    fn redone(history: &mut History, source: &mut String) -> Option<usize> {
        history.redo_into(source).map(|(caret, _)| caret)
    }

    fn typed(at: usize, text: &str, made_at: Instant) -> Edit {
        Edit {
            at,
            removed: String::new(),
            inserted: text.to_owned(),
            made_at,
        }
    }

    fn deleted(at: usize, text: &str, made_at: Instant) -> Edit {
        Edit {
            at,
            removed: text.to_owned(),
            inserted: String::new(),
            made_at,
        }
    }

    /// **The other pane's caret is carried, not left where it was** (要件 7.6).
    ///
    /// A position before the change does not move; one after it moves by what
    /// the change added or took away; one inside what was removed has nothing
    /// left to point at. Leaving them all where they were is what made a caret
    /// slide along the line while somebody typed in the pane beside it.
    #[test]
    fn a_position_in_another_pane_moves_with_the_edit() {
        // Three characters typed at byte 10.
        let typed = Change {
            at: 10,
            removed: 0,
            inserted: 3,
        };
        assert_eq!(typed.moved(4), 4, "before it, nothing moves");
        assert_eq!(typed.moved(10), 10, "at it, nothing moves");
        assert_eq!(typed.moved(20), 23, "after it, everything shifts");

        // Five bytes taken out at byte 10.
        let deleted = Change {
            at: 10,
            removed: 5,
            inserted: 0,
        };
        assert_eq!(deleted.moved(4), 4);
        assert_eq!(deleted.moved(20), 15);
        assert_eq!(deleted.moved(12), 10, "inside what went, to the cut");
        assert_eq!(deleted.moved(15), 10, "and the far edge lands there too");

        // A selection replaced: five out, two in.
        let replaced = Change {
            at: 10,
            removed: 5,
            inserted: 2,
        };
        assert_eq!(replaced.moved(20), 17);
        assert_eq!(replaced.moved(13), 10);
    }

    /// A run of typing is one step, not one per keystroke (要件 7.1).
    #[test]
    fn typing_that_carries_on_is_one_undo() {
        let start = Instant::now();
        let mut history = History::default();
        let mut source = String::new();

        for (index, letter) in ["あ", "い", "う"].iter().enumerate() {
            source.push_str(letter);
            history.record(typed(index * 3, letter, start));
        }

        assert_eq!(undone(&mut history, &mut source), Some(0));
        assert_eq!(source, "");
        assert_eq!(history.undo_into(&mut source), None, "one step, not three");
    }

    /// A pause ends the run. Where the writer stopped to think is where they
    /// expect the step to end.
    #[test]
    fn typing_after_a_pause_is_a_step_of_its_own() {
        let start = Instant::now();
        let later = start + UNDO_JOIN_IDLE + Duration::from_millis(1);
        let mut history = History::default();
        let mut source = String::from("ab");
        history.record(typed(0, "a", start));
        history.record(typed(1, "b", later));

        assert_eq!(undone(&mut history, &mut source), Some(1));
        assert_eq!(source, "a");
        assert_eq!(undone(&mut history, &mut source), Some(0));
        assert_eq!(source, "");
    }

    /// Backspace takes the character before the last one taken, so the run
    /// grows backwards; Delete takes the one after, and the position stays.
    #[test]
    fn a_run_of_deletions_joins_from_either_side() {
        let start = Instant::now();
        let mut backspace = History::default();
        let mut source = String::from("abc");
        source.truncate(2);
        backspace.record(deleted(2, "c", start));
        source.truncate(1);
        backspace.record(deleted(1, "b", start));

        assert_eq!(undone(&mut backspace, &mut source), Some(3));
        assert_eq!(source, "abc");

        let mut forward = History::default();
        let mut source = String::from("abc");
        source.remove(0);
        forward.record(deleted(0, "a", start));
        source.remove(0);
        forward.record(deleted(0, "b", start));

        assert_eq!(undone(&mut forward, &mut source), Some(2));
        assert_eq!(source, "abc");
    }

    /// A line break ends the step. Undoing a paragraph is one Ctrl+Z per line,
    /// which is where a writer looks for the boundary.
    #[test]
    fn a_line_break_ends_the_step() {
        let start = Instant::now();
        let mut history = History::default();
        let mut source = String::from("a\n");
        history.record(typed(0, "a", start));
        history.record(typed(1, "\n", start));

        assert_eq!(undone(&mut history, &mut source), Some(1));
        assert_eq!(source, "a");
    }

    /// Redo goes forwards again, and anything typed after an undo throws the
    /// way forwards away — it leads to a text that no longer exists.
    #[test]
    fn typing_after_an_undo_ends_the_way_forwards() {
        let start = Instant::now();
        let mut history = History::default();
        let mut source = String::from("ab");
        history.record(typed(0, "ab", start));

        assert_eq!(undone(&mut history, &mut source), Some(0));
        assert_eq!(source, "");
        assert_eq!(redone(&mut history, &mut source), Some(2));
        assert_eq!(source, "ab");

        assert_eq!(undone(&mut history, &mut source), Some(0));
        source.push('c');
        history.record(typed(0, "c", start));
        assert_eq!(history.redo_into(&mut source), None);
    }

    /// A history that no longer describes the text is dropped rather than
    /// applied. A document can be replaced wholesale, and a position remembered
    /// from before that names something else now.
    #[test]
    fn a_history_that_does_not_fit_the_text_is_forgotten() {
        let start = Instant::now();
        let mut history = History::default();
        history.record(typed(4, "いろは", start));

        let mut replaced = String::from("ab");
        assert_eq!(history.undo_into(&mut replaced), None);
        assert_eq!(replaced, "ab", "the text is left alone");
        assert_eq!(history.redo_into(&mut replaced), None, "and so is the rest");
    }

    /// A pane's number and its row are the same thing, both ways round
    /// (ペイン分割設計 5). A number that did not survive the round trip would
    /// send a keystroke to the pane beside the one it was typed in.
    #[test]
    fn a_pane_number_is_its_row() {
        for position in 0..8_i32 {
            let id = PaneId::from_index(position);
            assert_eq!(id.index(), position, "{id:?}");
        }
        // A number from the window that names nothing sensible is the first
        // pane, which always exists (要件 6.3) — never a panic, because these
        // arrive with keystrokes.
        assert_eq!(PaneId::from_index(-1), PaneId::FIRST);
    }

    /// The bound that stops 6.16 from happening again when the area is divided.
    ///
    /// Dividing does not re-create a pane that is already on screen, so the
    /// width it reports is the width it had until the layout runs — which is
    /// after the refresh the division itself does (6.19). The report is
    /// therefore held to what the tree gave the pane.
    #[test]
    fn a_pane_is_never_wider_than_the_area_it_was_given() {
        // What a pane reported while it had the window to itself, still in its
        // row when the area is divided.
        let stale: f32 = 1000.0;

        assert_eq!(bounded_extent(stale, 500.0), 500.0);
        assert_eq!(bounded_extent(stale, 1200.0), stale, "never widened");
        assert_eq!(
            bounded_extent(stale, 0.0),
            stale,
            "a pane given nothing yet is not bounded to nothing"
        );
    }

    /// Inserting a line break must push the text after it leftwards, not drag
    /// the text before it rightwards.
    ///
    /// Vertical text starts at the right, and a new column widens the content at
    /// the right edge. The viewport has to follow that edge by the same amount,
    /// or everything already written appears to slide sideways.
    #[test]
    fn a_new_column_moves_the_later_text_left_and_leaves_the_earlier_text_put() {
        let visible = 640.0;
        let before = 5000.0;
        let column = 36.0;
        let after = before + column;
        // Parked in the middle of the document.
        let viewport = -2000.0;

        let next = scroll_after_content_resize(viewport, visible, before, after);

        assert_eq!(
            next,
            viewport - column,
            "the viewport must follow the right edge so earlier text stays put"
        );
        let earlier_text_on_screen = |scroll: f32, content_width: f32| content_width + scroll;
        assert_eq!(
            earlier_text_on_screen(next, after),
            earlier_text_on_screen(viewport, before),
            "the document start must land in the same place on screen"
        );
    }

    #[test]
    fn keeps_the_same_distance_from_the_vertical_document_start_after_resize() {
        assert_eq!(
            scroll_after_content_resize(0.0, 640.0, 665.0, 86_386.0),
            -85_721.0
        );
        assert_eq!(
            scroll_after_content_resize(-2000.0, 640.0, 5000.0, 6000.0),
            -3000.0
        );
    }

    /// The bitmap holds BGRA and Slint wants RGBA, and the swap happens in the
    /// buffer the pixels were drawn into.
    ///
    /// **Nothing else would say if it stopped happening.** The engine's own
    /// tests read the bitmap's order, and body ink is grey — red and blue swapped
    /// in it look the same. What would change is the paper and the headings, and
    /// only on screen.
    #[test]
    fn a_drawn_tile_reaches_slint_in_rgba() {
        let mut spare = Vec::new();
        let mut drawn = TileImages {
            spare: &mut spare,
            drawing: None,
            produced: Vec::new(),
            uploaded: 0,
            reused: 0,
        };
        let span = TileSpan {
            block_index: 0,
            sub_index: 0,
            cross_index: 0,
            flow_start: 0,
            flow_size: 2,
            cross_start: 0,
            cross_size: 1,
        };
        let room = drawn.buffer(span, 2, 1);
        // Two pixels, blue-ish and red-ish, as the bitmap would hold them.
        room.copy_from_slice(&[200, 20, 10, 255, 10, 20, 200, 255]);
        drawn.filled(span);

        let (_, _, pixels) = drawn.produced.first().expect("one tile");
        let shown = pixels.as_slice();
        assert_eq!(
            (shown[0].r, shown[0].g, shown[0].b, shown[0].a),
            (10, 20, 200, 255),
            "the first pixel is the blue one"
        );
        assert_eq!(
            (shown[1].r, shown[1].g, shown[1].b, shown[1].a),
            (200, 20, 10, 255),
            "and the second is the red one"
        );
        assert_eq!(drawn.uploaded, 8);
    }

    /// 技術検証 7.8: an evicted tile's buffer is drawn into again rather than
    /// allocated, because allocating two megabytes costs more than drawing them.
    #[test]
    fn a_spare_buffer_is_the_same_memory() {
        let mut spare = vec![SharedPixelBuffer::<Rgba8Pixel>::new(4, 4)];
        let was = spare[0].as_bytes().as_ptr();

        let taken = take_spare(&mut spare, 4, 4).expect("a buffer of that size");

        assert_eq!(
            taken.as_bytes().as_ptr(),
            was,
            "the pages are the ones already touched"
        );
        assert!(spare.is_empty(), "and it is not handed out twice");
    }

    /// **A miss lets one go, and only one.** Tiles are not all one size — the
    /// last slice of a block is short — so a buffer that does not fit this tile
    /// may fit the next. What must not happen is a pool that fills with a size
    /// nobody asks for any more, which is where a resize leaves it.
    #[test]
    fn a_miss_lets_the_oldest_spare_go() {
        let mut spare = vec![
            SharedPixelBuffer::<Rgba8Pixel>::new(4, 4),
            SharedPixelBuffer::<Rgba8Pixel>::new(8, 4),
        ];

        assert!(take_spare(&mut spare, 16, 4).is_none());
        assert_eq!(spare.len(), 1, "one goes, not all of them");
        assert_eq!(spare[0].width(), 8, "and it is the oldest that goes");

        // The short tile of a block still finds its own.
        assert!(take_spare(&mut spare, 8, 4).is_some());
    }

    /// 要件 8.5: the view stays where the writer left it until they look
    /// somewhere else, and **the caret moving is them looking somewhere else**.
    #[test]
    fn a_held_view_lasts_until_the_caret_moves() {
        let anchor = Some(ViewAnchor {
            byte: 1200,
            caret: Some(40),
        });

        assert_eq!(held_view(anchor, Some(40)), Some(1200));
        assert_eq!(held_view(anchor, Some(41)), None);
        assert_eq!(held_view(anchor, None), None);
        assert_eq!(held_view(None, Some(40)), None);

        // A tab restored with no caret is held all the same: it is where the
        // writer left it, and nothing has said otherwise.
        let unmoved = Some(ViewAnchor {
            byte: 8,
            caret: None,
        });
        assert_eq!(held_view(unmoved, None), Some(8));
    }

    /// 要件 2: **a cheap draw is never held back.** Every ordinary document
    /// draws a keystroke in a couple of milliseconds, and a wait there would
    /// only make the editor late for nothing.
    #[test]
    fn a_cheap_draw_owes_the_window_nothing() {
        let mut pace = EditPace {
            took: PACE_FREE_MS,
            drawn: Some(Instant::now()),
            ..EditPace::default()
        };
        assert_eq!(pace.owed(), Duration::ZERO);

        // And one that cost real time owes about what it took.
        pace.drew(80.0);
        let owed = pace.owed();
        assert!(
            owed > Duration::from_millis(60) && owed <= Duration::from_millis(80),
            "owed {owed:?}"
        );
    }

    /// **And the wait is against the clock, not the keystroke.** A writer who
    /// stopped and started again is not in a burst, so nothing is owed however
    /// heavy the last draw was — otherwise every first keystroke after a pause
    /// would arrive late.
    #[test]
    fn a_pause_pays_the_wait_off() {
        let pace = EditPace {
            took: 80.0,
            drawn: Some(Instant::now() - Duration::from_millis(300)),
            ..EditPace::default()
        };

        assert_eq!(pace.owed(), Duration::ZERO);
    }

    /// However heavy a document is, the next draw comes within `PACE_MAX`.
    #[test]
    fn a_very_heavy_draw_still_comes_back() {
        let pace = EditPace {
            took: 4_000.0,
            drawn: Some(Instant::now()),
            ..EditPace::default()
        };

        assert!(pace.owed() <= PACE_MAX);
    }

    #[test]
    fn evicts_the_tiles_furthest_from_the_viewport() {
        let mut tiles = tile_map(10, 1024);

        evict_distant_tiles(&mut tiles, &mut Vec::new(), &[5, 6], 5600.0);

        assert_eq!(tiles.len(), TILE_CACHE_LIMIT);
        assert!(tiles.contains_key(&5), "the viewport tiles must survive");
        assert!(tiles.contains_key(&6), "the viewport tiles must survive");
        assert!(!tiles.contains_key(&0), "the furthest tile must be dropped");
    }

    #[test]
    fn falls_back_to_the_default_column_height_before_the_pane_reports_one() {
        assert_eq!(usable_preview_height(900.0), 900);
        assert_eq!(
            usable_preview_height(MIN_PREVIEW_HEIGHT as f32),
            MIN_PREVIEW_HEIGHT
        );
        assert_eq!(
            usable_preview_height(40.0),
            PREVIEW_HEIGHT,
            "a pane too short to lay out falls back rather than producing slivers"
        );
        assert_eq!(usable_preview_height(0.0), PREVIEW_HEIGHT);
        assert_eq!(usable_preview_height(f32::NAN), PREVIEW_HEIGHT);
    }

    /// A tall window must not simply make every tile more expensive.
    #[test]
    fn a_taller_pane_makes_tiles_narrower_rather_than_costlier() {
        let text = long_document(30_000);
        let short = engine_for_height(&text, 520);
        let tall = engine_for_height(&text, 1560);

        assert!(
            tall.tile_flow_size() < short.tile_flow_size(),
            "a three times taller pane should not keep the full tile width"
        );
        let short_pixels = short.tile_flow_size() * short.line_extent();
        let tall_pixels = tall.tile_flow_size() * tall.line_extent();
        assert!(
            tall_pixels <= short_pixels * 12 / 10,
            "pixels per tile should stay roughly constant: {short_pixels} then {tall_pixels}"
        );
    }

    /// A narrower tile means more tiles on screen, so the cache must not evict
    /// the ones it is about to be asked for.
    #[test]
    fn keeps_every_wanted_tile_even_past_the_nominal_cap() {
        let mut tiles = tile_map(14, 256);
        let wanted: Vec<u64> = (0..9).collect();

        evict_distant_tiles(&mut tiles, &mut Vec::new(), &wanted, 1024.0);

        for key in &wanted {
            assert!(tiles.contains_key(key), "dropped a wanted tile {key}");
        }
    }

    #[test]
    fn keeps_a_tile_that_is_still_wanted_even_when_far_from_the_centre() {
        let mut tiles = tile_map(10, 1024);

        evict_distant_tiles(&mut tiles, &mut Vec::new(), &[0], 8000.0);

        assert!(tiles.contains_key(&0));
    }

    #[test]
    fn normalizes_a_backward_vertical_selection() {
        let state = EditorState {
            caret_source_byte: Some(3),
            selection_anchor_source_byte: Some(9),
            ..Default::default()
        };

        assert_eq!(selection_source_range(&state), Some((3, 9)));
    }

    #[test]
    fn shift_move_extends_and_plain_move_clears_the_selection() {
        let mut state = EditorState {
            caret_source_byte: Some(4),
            selection_anchor_source_byte: Some(4),
            ..Default::default()
        };

        let extended = update_selection_after_move(&mut state, 4, 7, true);
        assert_eq!(extended, Some((4, 7)));

        let cleared = update_selection_after_move(&mut state, 7, 8, false);
        assert_eq!(cleared, None);
        assert_eq!(state.selection_anchor_source_byte, Some(8));
        assert_eq!(state.caret_source_byte, Some(8));
    }

    #[test]
    fn replaces_the_selected_source_range() {
        let mut source = "選択した本文".to_owned();
        let start = "選択".len();
        let end = "選択した".len();

        let caret = replace_source_range(&mut source, (start, end), "する");

        assert_eq!(source, "選択する本文");
        assert_eq!(caret, "選択する".len());
    }

    #[test]
    fn vertical_tab_at_a_heading_start_indents_before_the_markdown_marker() {
        let source = "# 見出し\n本文";
        let preview = PreviewDocument::from_source(source);

        let insertion = vertical_insertion_source_byte(source, &preview, 0, true);
        let mut indented = source.to_owned();
        indented.insert_str(insertion, TAB_INDENT);

        assert_eq!(insertion, 0);
        assert_eq!(indented, "    # 見出し\n本文");
        assert_eq!(
            PreviewDocument::from_source(&indented).text,
            "    # 見出し\n本文"
        );

        let blank_line_source = "前\n\n後";
        let blank_line_preview = PreviewDocument::from_source(blank_line_source);
        let blank_line_caret = "前\n".encode_utf16().count();
        let blank_line_insertion = vertical_insertion_source_byte(
            blank_line_source,
            &blank_line_preview,
            blank_line_caret,
            true,
        );
        let mut indented_blank_line = blank_line_source.to_owned();
        indented_blank_line.insert_str(blank_line_insertion, TAB_INDENT);

        assert_eq!(blank_line_insertion, "前\n".len());
        assert_eq!(indented_blank_line, "前\n    \n後");
        assert_eq!(
            PreviewDocument::from_source(&indented_blank_line).text,
            "前\n    \n後"
        );
    }

    #[test]
    fn does_not_truncate_the_technical_validation_document() {
        let preview = PreviewDocument::from_source(include_str!("../技術検証.md"));
        let engine = engine_for(&preview.text, 100);

        assert!(engine.total_flow_size() > 4096);
        assert!(engine.block_count() > 1);
    }

    #[test]
    fn moves_left_across_a_wrapped_sample_column() {
        let preview = PreviewDocument::from_source(SAMPLE_MARKDOWN);
        let mut engine = engine_for(&preview.text, 100);

        let moved = engine
            .move_caret_by_line(104, -1, None)
            .expect("move to the visual left column");

        assert_ne!(moved.utf16_position, 104);
    }

    #[test]
    fn keeps_visual_height_when_moving_left_between_rotated_latin_runs() {
        let preview = PreviewDocument::from_source(SAMPLE_MARKDOWN);
        let mut engine = engine_for(&preview.text, 100);
        let markdown_start = preview.text.find("Markdown").expect("Markdown run");
        let after_mark = preview.text[..markdown_start + "Mark".len()]
            .encode_utf16()
            .count() as u32;
        let directwrite_start = preview.text.find("DirectWrite").expect("DirectWrite run");
        let after_write = preview.text[..directwrite_start + "DirectWrite".len()]
            .encode_utf16()
            .count() as u32;

        let before = engine
            .caret_geometry(after_mark)
            .expect("source caret geometry");
        let moved = engine
            .move_caret_by_line(after_mark, -1, None)
            .expect("move left between rotated Latin runs");
        let after = engine
            .caret_geometry(moved.utf16_position)
            .expect("target caret geometry");

        assert_ne!(moved.utf16_position, after_write);
        assert!(after.x < before.x);
        assert!(
            (after.y - before.y).abs() <= font_size_for(BASE_FONT_SIZE, 100),
            "horizontal movement should preserve the visual height"
        );
    }

    /// The long-document budget: a viewport still needs a couple of tiles, and
    /// the blocks behind them stay a small fraction of the document.
    ///
    /// Tiles are slices of blocks now, so a 640px viewport can straddle the
    /// short slice at one block's left edge and the next block's slice as well
    /// as the slice it sits in. The count is bounded by the viewport, which is
    /// the property that matters; the exact number is not.
    #[test]
    fn thirty_thousand_characters_need_at_most_three_resident_tiles() {
        let text = long_document(30_000);
        let engine = engine_for(&text, 100);
        let width = engine.total_flow_size();
        let middle = -((width / 2) as f32);

        assert!(
            width > 65_536,
            "the sample must be a genuinely wide document"
        );
        let tiles = engine.visible_tiles(middle, 640.0, 0, 0.0, PREVIEW_HEIGHT as f32);
        assert!(
            tiles.len() <= 3,
            "a 640px viewport needed {} tiles",
            tiles.len()
        );
        assert!(
            tiles
                .iter()
                .all(|tile| tile.flow_size <= engine.tile_flow_size()),
            "no slice may exceed the tile width"
        );
    }

    /// Panning must not re-measure anything: the text has not changed, so every
    /// block keeps the measurement and the layout it already had.
    #[test]
    fn scrolling_a_long_document_measures_nothing() {
        let text = long_document(30_000);
        let mut engine = engine_for(&text, 100);

        let measured = engine
            .update(
                StyledText::plain(&text),
                LineFit::Extent(PREVIEW_HEIGHT),
                &Typography::new(font_size_for(BASE_FONT_SIZE, 100)),
            )
            .expect("repeat update");

        assert_eq!(measured, directwrite_render::UpdateCost::default());
    }

    /// A drag only hit tests the blocks on screen, so a selection spanning the
    /// whole document costs the same as one spanning the viewport.
    #[test]
    fn a_document_wide_selection_only_measures_the_visible_blocks() {
        let text = long_document(30_000);
        let mut engine = engine_for(&text, 100);
        let width = engine.total_flow_size() as f32;
        let visible = visible_flow_range(0.0, 640.0, width);

        let rects = engine
            .selection_rects(Some((0, engine.utf16_len())), visible)
            .expect("selection rectangles");

        assert!(!rects.is_empty());
        assert!(
            rects.len() < 200,
            "clipping to the viewport should keep the rectangle count small, got {}",
            rects.len()
        );
    }

    /// 追加要件 Terminal: **修飾キーそのものは打鍵ではない。**
    ///
    /// これを取りこぼすと、Shiftを押しただけで`Ctrl+P`（履歴をひとつ戻る）が
    /// シェルへ飛ぶ——最初の端末で実際にそうなった。
    #[test]
    fn a_modifier_key_is_not_a_keystroke() {
        // 窓が名前を付けられなかったと言ってきた場合。
        assert_eq!(named_key(-1, "\u{10}", false), None);
        // 番号が付かずに文字として来た場合も、同じ答えでなければならない。
        assert_eq!(named_key(0, "\u{10}", false), None, "Shift");
        assert_eq!(named_key(0, "\u{11}", true), None, "Control");
        assert_eq!(named_key(0, "\u{12}", false), None, "Alt");
        assert_eq!(named_key(0, "\u{17}", false), None, "Windows key");
        assert_eq!(named_key(0, "\u{f710}", false), None, "F13 と、その先");
        assert_eq!(named_key(0, "", false), None, "文字を持たないキー");
    }

    #[test]
    fn ctrl_and_a_letter_is_that_letter() {
        // winitは論理キーを渡すので、Ctrlを押していても文字は`c`のまま来る。
        assert_eq!(named_key(0, "c", true), Some(TerminalKey::Char('c')));
        // 制御文字の形で来ても同じところへ着く（バイトにするのは`terminal.rs`）。
        assert_eq!(named_key(0, "\u{3}", true), Some(TerminalKey::Char('c')));
    }

    #[test]
    fn the_keys_with_names_keep_them() {
        assert_eq!(named_key(13, "\n", false), Some(TerminalKey::Enter));
        assert_eq!(named_key(1, "\u{f700}", false), Some(TerminalKey::Up));
        assert_eq!(named_key(12, "\t", false), Some(TerminalKey::Tab));
        assert_eq!(
            named_key(24, "\u{f708}", false),
            Some(TerminalKey::Function(5))
        );
        assert_eq!(named_key(0, "あ", false), Some(TerminalKey::Char('あ')));
    }

    /// 追加要件 Terminal: **選択は矩形ではなく文の形**をしている。
    #[test]
    fn a_selection_takes_the_end_of_one_row_and_the_start_of_another() {
        let across = TerminalSelection {
            anchor: (1, 5),
            head: (3, 2),
        };
        assert_eq!(across.columns_in(0, 80), None);
        assert_eq!(
            across.columns_in(1, 80),
            Some((5, 80)),
            "最初の行は途中から末尾まで"
        );
        assert_eq!(across.columns_in(2, 80), Some((0, 80)), "間の行は丸ごと");
        assert_eq!(
            across.columns_in(3, 80),
            Some((0, 2)),
            "最後の行は頭から途中まで"
        );
        assert_eq!(across.columns_in(4, 80), None);

        // **上へ引いても下へ引いても同じ選択である。**
        let upwards = TerminalSelection {
            anchor: (3, 2),
            head: (1, 5),
        };
        for row in 0..5 {
            assert_eq!(upwards.columns_in(row, 80), across.columns_in(row, 80));
        }
    }

    #[test]
    fn a_selection_inside_one_row_is_just_those_columns() {
        let one = TerminalSelection {
            anchor: (2, 7),
            head: (2, 3),
        };
        assert_eq!(one.columns_in(2, 80), Some((3, 7)));
        assert!(!one.is_empty());
        let none = TerminalSelection {
            anchor: (2, 3),
            head: (2, 3),
        };
        assert_eq!(
            none.columns_in(2, 80),
            None,
            "掴んだだけでは何も選ばれていない"
        );
        assert!(none.is_empty());
    }

    /// 書き手の報告 2026-09-07: `ls`と`ls`＋改行は、色の付く行数で見分ける。
    #[test]
    fn the_draft_marks_the_lines_that_would_be_sent() {
        let marked = |draft: &str| -> Vec<(String, bool, bool)> {
            draft_lines(draft)
                .iter()
                .map(|line| (line.text.to_string(), line.sent, line.breaks))
                .collect()
        };
        assert_eq!(marked("ls"), vec![("ls".to_owned(), true, false)]);
        assert_eq!(
            marked("ls\n"),
            vec![("ls".to_owned(), true, true), (String::new(), true, false),],
            "末尾の改行は、その下の行に色が付くことで見える"
        );
    }

    /// 書き手の報告 2026-09-07: 端では止まる。
    #[test]
    fn a_walk_stops_at_both_ends_of_what_it_remembers() {
        assert_eq!(stepped_place(3, 1, false), Some(0));
        assert_eq!(stepped_place(3, 1, true), Some(2));
        assert_eq!(stepped_place(3, 0, false), None, "これより前は無い");
        assert_eq!(stepped_place(3, 2, true), None, "これより先は無い");
        assert_eq!(stepped_place(0, 0, false), None);
        assert_eq!(stepped_place(0, 0, true), None, "まだどこにも行っていない");
    }

    #[test]
    fn an_empty_draft_has_one_line_and_sends_none_of_it() {
        let lines = draft_lines("");
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].sent, "送るものが無いのだから色も付かない");
        assert!(!lines[0].breaks);
    }
}
