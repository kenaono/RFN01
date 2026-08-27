mod app_data;
mod buffer;
mod diag;
mod directwrite_probe;
mod directwrite_render;
mod document;
mod file_dialog;
mod file_io;
mod file_tree;
mod find;
mod ime;
mod pane_layout;
mod quick_draft;
mod searcher;
mod shell;
mod text_blocks;
#[cfg(test)]
mod vertical_layout;
mod writer;

use std::{
    borrow::Cow,
    cell::{Cell, Ref, RefCell, RefMut},
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use buffer::{DocumentFile, ExternalChange};
use diag::DiagLog;
use directwrite_render::{CaretGeometry, SelectionRect, TextEngine, TileSink, WritingMode};
use document::{DocumentCounts, PreviewDocument, caret_place};
use pane_layout::{Layout, Rect, Split};
use searcher::{NeverSuperseded, SearchJob, SearchOutcome, Searcher};
use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, RenderingState, Rgba8Pixel, SharedPixelBuffer,
    SharedString, Timer, TimerMode, VecModel, Weak,
};
use std::collections::{BTreeSet, VecDeque};
use text_blocks::{
    DEFAULT_BODY_FONT, DEFAULT_CODE_FONT, DEFAULT_HEADING_FONT, DEFAULT_INK, DEFAULT_PAPER,
    Emphasis, LineMarker, MAX_HEADING_LEVEL, StyledText, TileSpan, Typography, visible_flow_range,
};
use unicode_segmentation::UnicodeSegmentation;
use writer::FileWriter;

slint::include_modules!();

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
const HORIZONTAL_MODE: i32 = 0;
const VERTICAL_MODE: i32 = 1;
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

/// The document's text, and whether it has changed since it last agreed with
/// its file.
///
/// The flag lives with the text because **every writer goes through
/// `borrow_mut`**. Threading it through the four editing functions instead
/// would hold until the fifth one was written; this cannot be forgotten, which
/// is the same reason a block decides its size from its own measurements
/// rather than from a running total (技術検証 3.4).
struct SharedText {
    text: RefCell<String>,
    edited: Cell<bool>,
    /// When the text last changed, and when the run of changes now waiting for
    /// a work copy began. 要件 8.1 states two rules and they are measured from
    /// these two moments; see [`work_copy_due`].
    changed_at: Cell<Instant>,
    pending_since: Cell<Option<Instant>>,
    /// Told as soon as the flag moves, so the title marker does not depend on
    /// remembering to refresh it at each of the places an edit can begin.
    window: Weak<AppWindow>,
}

impl SharedText {
    fn new(text: String, window: Weak<AppWindow>) -> Self {
        Self {
            text: RefCell::new(text),
            edited: Cell::new(false),
            changed_at: Cell::new(Instant::now()),
            pending_since: Cell::new(None),
            window,
        }
    }

    fn borrow(&self) -> Ref<'_, String> {
        self.text.borrow()
    }

    /// Borrowing the text to write *is* the edit.
    fn borrow_mut(&self) -> RefMut<'_, String> {
        let now = Instant::now();
        self.changed_at.set(now);
        if self.pending_since.get().is_none() {
            self.pending_since.set(Some(now));
        }
        self.set_edited(true);
        self.text.borrow_mut()
    }

    /// When the changes now waiting for a work copy began, if any are.
    fn pending_since(&self) -> Option<Instant> {
        self.pending_since.get()
    }

    fn changed_at(&self) -> Instant {
        self.changed_at.get()
    }

    /// A work copy has caught up with the text.
    fn work_copy_written(&self) {
        self.pending_since.set(None);
    }

    /// The copy that holds this text is filed under a name the document no
    /// longer has: its file was renamed (要件 5.2), so one has to be written
    /// again under the new one. A document that agrees with its file has
    /// nothing waiting either way.
    fn mark_pending(&self) {
        if self.edited.get() && self.pending_since.get().is_none() {
            self.pending_since.set(Some(Instant::now()));
        }
    }

    fn edited(&self) -> bool {
        self.edited.get()
    }

    /// Restored from a work copy: the text does not agree with its file, but
    /// the copy on disk already holds it, so nothing is waiting to be written.
    fn mark_restored(&self) {
        self.pending_since.set(None);
        self.set_edited(true);
    }

    /// The text now agrees with its file: it was just opened, or just saved.
    fn mark_saved(&self) {
        self.pending_since.set(None);
        self.set_edited(false);
    }

    /// Only the moves are reported. A keystroke in an already-edited document
    /// changes nothing the window shows.
    ///
    /// **The strips are published again as well.** A tab's unsaved marker is
    /// this flag, and this is the one time it changes without the writer having
    /// touched the strip — which is why the window carries a callback back into
    /// the tab code for it.
    fn set_edited(&self, edited: bool) {
        if self.edited.replace(edited) == edited {
            return;
        }
        if let Some(window) = self.window.upgrade() {
            window.set_document_edited(edited);
            window.invoke_republish_tabs();
        }
    }
}

/// One open document: its text, and the file it came from.
///
/// **A document is shared; a tab is one pane's view of it** (要件 7.6). Two
/// panes showing the same file hold the same `Rc`, so an edit made through one
/// *is* the text the other draws, and `Rc::ptr_eq` answers "the same document?"
/// — the question that decides whether an edit in one pane has to redraw
/// another.
///
/// The design asked for a registry of documents with ids (ペイン分割設計 3).
/// **The `Rc` is that registry**: a document lives while a tab holds it and
/// goes when the last one lets go, with no table to keep in step and no id that
/// can name something that is no longer there.
struct OpenDocument {
    file: RefCell<DocumentFile>,
    text: SharedText,
    /// What has been done to this text and can be taken back (要件 7.1). It
    /// belongs to the document because 要件 7.6 says it does: one file, one
    /// history, however many panes are showing it.
    history: RefCell<History>,
    /// The per-line counts: the statistics behind the status bar and the
    /// heading level of every line.
    ///
    /// **A property of the text, not of any view of it** (ペイン分割設計 2), so
    /// panes share one. It used to sit in `RenderCache`, where one slot served
    /// every pane — right, because it is keyed by the text and rebuilds when it
    /// does not match, but a full re-scan every time two panes showing
    /// different documents took turns.
    counts: RefCell<CountsSlot>,
}

impl OpenDocument {
    fn new(file: DocumentFile, text: String, window: Weak<AppWindow>) -> Rc<Self> {
        Rc::new(Self {
            file: RefCell::new(file),
            text: SharedText::new(text, window),
            history: RefCell::new(History::default()),
            counts: RefCell::new(CountsSlot::default()),
        })
    }

    /// Remember a change that has just gone into the text.
    fn record(&self, at: usize, removed: String, inserted: String) {
        self.history.borrow_mut().record(Edit {
            at,
            removed,
            inserted,
            made_at: Instant::now(),
        });
    }

    /// A document that has never been saved (要件 8.4).
    fn untitled(number: u32, window: Weak<AppWindow>) -> Rc<Self> {
        Self::new(DocumentFile::untitled(number), String::new(), window)
    }
}

/// One change to a document's text, and everything it takes to undo it.
///
/// A range of the old text, and what went in its place. Both halves are kept
/// because an undo has to put back exactly what was there, and a redo exactly
/// what replaced it — **the text itself, not a copy of the document**: a
/// snapshot per keystroke of a document this editor will open is megabytes.
#[derive(Clone, Debug)]
struct Edit {
    /// Where the change begins, in source bytes.
    at: usize,
    /// What was in `at..at + removed.len()` before.
    removed: String,
    /// What is in `at..at + inserted.len()` now.
    inserted: String,
    /// When it was made. A run of typing joins into one entry, and this is
    /// what says the run has stopped.
    made_at: Instant,
}

impl Edit {
    /// Whether this is a run of typing rather than a deletion or a
    /// replacement, which is the only kind that joins with the one before.
    fn is_typing(&self) -> bool {
        self.removed.is_empty() && !self.inserted.contains('\n')
    }

    /// Whether this is one backspace or one delete: text taken out and
    /// nothing put in.
    fn is_deletion(&self) -> bool {
        self.inserted.is_empty() && !self.removed.contains('\n')
    }
}

/// How long a run of typing may pause and still be one undo (要件 7.1).
///
/// Undoing a paragraph one character at a time is not what Ctrl+Z is for, and
/// undoing a whole session at once is worse. A pause is the writer stopping to
/// think, which is where they would expect the step to end.
const UNDO_JOIN_IDLE: Duration = Duration::from_millis(1200);
/// The most changes a document remembers.
///
/// Each holds only the text that changed, so the list is small until it is very
/// long. This is a bound on the pathological case, not a budget anybody is
/// meant to notice.
const UNDO_DEPTH: usize = 500;

/// A document's undo history.
///
/// **One per document, not per pane** (要件 7.6): the same file open twice is
/// one text with one history, so an undo in either pane takes back whatever was
/// done last, in whichever pane it was done.
#[derive(Default)]
struct History {
    done: Vec<Edit>,
    undone: Vec<Edit>,
}

impl History {
    /// Remember a change that has just been made.
    ///
    /// A change that continues the one before it joins it, so that a run of
    /// typing is one step. **Anything recorded throws away what was undone**:
    /// the redo list is a path back to a text that no longer exists once the
    /// writer has gone somewhere else.
    fn record(&mut self, edit: Edit) {
        self.undone.clear();
        if let Some(last) = self.done.last_mut()
            && edit.made_at.duration_since(last.made_at) <= UNDO_JOIN_IDLE
        {
            // Typing that carries on where the last left off.
            if last.is_typing() && edit.is_typing() && edit.at == last.at + last.inserted.len() {
                last.inserted.push_str(&edit.inserted);
                last.made_at = edit.made_at;
                return;
            }
            // Backspace, which takes the character before the last one taken.
            if last.is_deletion() && edit.is_deletion() && edit.at + edit.removed.len() == last.at {
                last.removed.insert_str(0, &edit.removed);
                last.at = edit.at;
                last.made_at = edit.made_at;
                return;
            }
            // Delete, which takes the character after, leaving the position
            // where it was.
            if last.is_deletion() && edit.is_deletion() && edit.at == last.at {
                last.removed.push_str(&edit.removed);
                last.made_at = edit.made_at;
                return;
            }
        }
        self.done.push(edit);
        if self.done.len() > UNDO_DEPTH {
            self.done.remove(0);
        }
    }

    /// Take the last change back out of `source`, and say where the caret goes.
    ///
    /// **Checked against the text before anything is moved.** A document can be
    /// replaced wholesale — a reload, the measurement sample — and a position
    /// remembered from before that names something else now. Rather than trust
    /// it, the whole history goes: nothing here is worth a panic in front of
    /// somebody's manuscript.
    fn undo_into(&mut self, source: &mut String) -> Option<(usize, Change)> {
        let edit = self.done.pop()?;
        let end = edit.at + edit.inserted.len();
        if source.get(edit.at..end) != Some(edit.inserted.as_str()) {
            self.forget();
            return None;
        }
        let change = Change {
            at: edit.at,
            removed: edit.inserted.len(),
            inserted: edit.removed.len(),
        };
        let caret = replace_source_range(source, (edit.at, end), &edit.removed);
        self.undone.push(edit);
        Some((caret, change))
    }

    /// Put back the last change that was taken out.
    fn redo_into(&mut self, source: &mut String) -> Option<(usize, Change)> {
        let edit = self.undone.pop()?;
        let end = edit.at + edit.removed.len();
        if source.get(edit.at..end) != Some(edit.removed.as_str()) {
            self.forget();
            return None;
        }
        let change = Change {
            at: edit.at,
            removed: edit.removed.len(),
            inserted: edit.inserted.len(),
        };
        let caret = replace_source_range(source, (edit.at, end), &edit.inserted);
        self.done.push(edit);
        Some((caret, change))
    }

    /// Nothing that was done can be taken back any more.
    fn forget(&mut self) {
        self.done.clear();
        self.undone.clear();
    }
}

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
}

/// Both panes' states, so that a callback carrying a pane number can reach the
/// one it names.
///
/// It hands out **the named pane's state and the other's**, because an edit
/// needs both: one to move the caret in, one to keep in step over the shared
/// document (要件 7.6). Cloned into every editing callback, which is why the
/// panes each hold an `Rc` rather than the state itself.
#[derive(Clone)]
struct PaneStates {
    /// Indexed by [`PaneId::index`], which is the order of [`PaneId::ALL`].
    states: [Rc<RefCell<EditorState>>; 2],
    /// The document each pane has in front of it. **Asked at the moment it is
    /// needed, never held**: a pane can be showing a different document by the
    /// time a timer fires.
    showing: [Rc<RefCell<Rc<OpenDocument>>>; 2],
}

impl PaneStates {
    /// Every pane starts on the same document, which is what 要件 6.4 says a
    /// new pane opens with.
    fn new(document: &Rc<OpenDocument>) -> Self {
        Self {
            states: Default::default(),
            showing: PaneId::ALL.map(|_| Rc::new(RefCell::new(document.clone()))),
        }
    }

    fn of(&self, id: PaneId) -> &Rc<RefCell<EditorState>> {
        &self.states[id.index() as usize]
    }

    /// What this pane is showing.
    fn document(&self, id: PaneId) -> Rc<OpenDocument> {
        self.showing[id.index() as usize].borrow().clone()
    }

    /// Put a document in front of this pane.
    fn show(&self, id: PaneId, document: &Rc<OpenDocument>) {
        *self.showing[id.index() as usize].borrow_mut() = document.clone();
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

/// The document's per-line counts, kept up to date rather than rebuilt.
///
/// Replaces two caches that each walked the whole document whenever any of it
/// changed: the statistics behind the status bar, and the heading level of
/// every logical line. Both are sums or maxima over lines, and a keystroke
/// changes one line (技術検証 7.1).
#[derive(Default)]
struct CountsSlot {
    source: String,
    counts: DocumentCounts,
    started: bool,
}

impl CountsSlot {
    fn get(&mut self, source: &str) -> &DocumentCounts {
        if !self.started || self.source != source {
            self.counts.refresh(source);
            self.source.clear();
            self.source.push_str(source);
            self.started = true;
        }
        &self.counts
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
    selection_utf16: Option<(u32, u32)>,
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
    /// The direction this pane's engine was built for.
    ///
    /// **A pane no longer *is* a direction.** The tab in front of it decides
    /// (要件 7.2's four modes belong to the tab), so a pane has to be able to
    /// change, and this is what it is changing from.
    mode: WritingMode,
}

impl Pane {
    fn new(mode: WritingMode) -> Self {
        Self {
            graphics: PaneGraphics::new(mode),
            view: PaneView::default(),
            mode,
        }
    }

    /// Set the direction this pane draws in, and say whether it moved.
    ///
    /// **The engine is rebuilt when it moves.** An engine is built for one
    /// writing mode and keeps it (`directwrite_render`'s module doc), and every
    /// block measurement and tile it holds is in that direction — so a change
    /// costs the whole document being measured again, about what a zoom costs
    /// (6.8). That is a price for a deliberate switch, never for a keystroke.
    fn set_mode(&mut self, mode: WritingMode) -> bool {
        if self.mode == mode {
            return false;
        }
        *self = Pane::new(mode);
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
    panes: [Pane; 2],
    /// Shared with the rendering notifier, which runs between refreshes.
    frames: Rc<RefCell<FrameProbe>>,
    /// How long the last push of the source text into the horizontal pane took.
    /// Only Split keeps that pane alive, so this isolates what Split adds.
    source_push_ms: Option<f64>,
    perf_log: PerfLog,
    /// This run's trace. Kept beside the perf log rather than inside it: one is
    /// about cost and the other about what happened (see `diag.rs`).
    diag: DiagLog,
    /// How each pane is keeping up with a run of keystrokes.
    pace: [EditPace; 2],
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
            // The arrangement the editor opens in: the left pane horizontal,
            // the right one vertical. Neither is fixed that way any more.
            panes: [
                Pane::new(WritingMode::Horizontal),
                Pane::new(WritingMode::Vertical),
            ],
            frames: Rc::default(),
            source_push_ms: None,
            perf_log: PerfLog::default(),
            diag: DiagLog::default(),
            pace: Default::default(),
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
    // The horizontal spec, because the header is one line and the two panes
    // may be set differently now (要件 9). What it is for is the font size and
    // the build profile, and the size is the same either way.
    let typography = typography_for(window, window.get_zoom_percent(), false, true);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "session started={} profile={profile} zoom={zoom} font={font:.1} \
         space={space:.2} lead={lead:.2} head={head:.2}",
        epoch_seconds(),
        zoom = window.get_zoom_percent(),
        font = typography.font_size,
        space = typography.character_spacing,
        lead = typography.line_spacing,
        head = typography.size_scale(1),
    )
}

fn main() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    // The panes, published before anything can read one. Each row carries its
    // own number and direction; everything else in it is either what a refresh
    // has put there or what the pane itself reports once it exists.
    let rows = PaneId::ALL.map(PaneId::initial_screen);
    window.set_panes(ModelRc::new(VecModel::from(rows.to_vec())));
    // 要件 8.1: whatever was being edited when the last run ended comes back
    // before anything is drawn, so the first thing on screen is the writer's
    // own text rather than something they have to clear away first.
    let restored = restore_tabs(&window);
    let was_restored = !restored.is_empty();
    // 要件 8.5: the arrangement comes back too — which pane held which tabs, in
    // what order, in which of the four modes, and how the area was divided.
    // Nothing there is a first run, and something unreadable is a session from
    // another build. Neither is worth a word on screen: the editor opens the
    // way it would have anyway.
    let session = app_data::app_directory()
        .as_deref()
        .and_then(app_data::read_session);
    // 要件 7.7: what was opened most recently, carried across runs with the
    // rest of the arrangement.
    let remembered = match &session {
        Some(session) => session.recent.clone(),
        None => Vec::new(),
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
        },
        None => WorkFolder::default(),
    };
    window.set_tree_open(session.as_ref().is_none_or(|session| session.tree_shown));
    // 要件 9: the zoom the writer left. Held within the same bounds the buttons
    // hold it to, so a hand-edited session cannot open at 4000%. A session from
    // a build that did not write one says 0, which is not a zoom and is left at
    // the default.
    if let Some(zoom) = session.as_ref().map(|session| session.zoom)
        && zoom > 0
    {
        window.set_zoom_percent(zoom.clamp(50, 240));
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
    // One caret per pane, and what each pane has in front of it. Neither pane
    // follows the other's caret, and every callback a pane raises reaches its
    // own state and its own document through here.
    let pane_states = PaneStates::new(&opening);
    let render_cache = Rc::new(RefCell::new(RenderCache::default()));
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
    for id in PaneId::ALL {
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
        // 要件 8.5: and the passage it was looking at, which is what actually
        // puts the view back — the scroll above is pixels, and the zoom, the
        // split and the extent all change what those mean before the window
        // stands still (`ViewAnchor`).
        hold_view(&render_cache, id, tab.view.top, caret);
        id.set_shows_preview(&window, tab.view.preview);
        set_pane_direction(&window, &render_cache, id, tab.view.vertical);
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
    let draft = Rc::new(RefCell::new(quick_draft::QuickDraftWindow::default()));
    let live = Live {
        states: pane_states.clone(),
        folder: Rc::new(RefCell::new(work_folder)),
        tree_paths: Rc::new(RefCell::new(Vec::new())),
        results: Rc::new(RefCell::new(Vec::new())),
        recent: Rc::new(RefCell::new(remembered)),
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
            mode = window.get_editor_mode(),
            split = u8::from(window.get_split_view()),
            zoom = window.get_zoom_percent(),
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
    // Nothing is on screen until the tree has handed out an area. The window
    // reports its own the moment the editing area exists, and this is the state
    // until then.
    place_panes(&window, &layout.borrow());
    publish_left(&window, &live);

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

    for id in [PaneId::Vertical, PaneId::Horizontal] {
        refresh_pane(
            &window,
            &render_cache,
            &opening,
            id,
            &initial,
            100,
            None,
            None,
            None,
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
            collect_write_results(&timer_live);
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
            for id in PaneId::ALL {
                if !id.is_shown(&window) {
                    continue;
                }
                let showing = area_states.document(id);
                let source = showing.text.borrow().clone();
                let state = area_states.of(id);
                refresh_pane_from_state(&window, &area_cache, &showing, id, state, &source);
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
    let weak = window.as_weak();
    let undivide_live = live.clone();
    window.on_undivide_requested(move || {
        if let Some(window) = weak.upgrade() {
            let here = focused_pane(&window);
            undivide_away(&window, &undivide_live, here.other());
        }
    });

    // 要件 6.4: 編集ペインの入れ替え。The arrangement stays and the panes move,
    // so everything each pane holds — its tabs, its carets — goes with it.
    let weak = window.as_weak();
    let swap_live = live.clone();
    window.on_swap_requested(move || {
        if let Some(window) = weak.upgrade() {
            let here = focused_pane(&window);
            swap_live
                .layout
                .borrow_mut()
                .swap(here.index() as usize, here.other().index() as usize);
            after_layout_change(&window, &swap_live);
        }
    });

    // 要件 5.1, 5.2: the work folder and its tree.
    let weak = window.as_weak();
    let folder_live = live.clone();
    window.on_work_folder_requested(move || {
        if let Some(window) = weak.upgrade() {
            let owner = ime::window_handle(&window);
            let Some(chosen) = file_dialog::open_folder(owner) else {
                return;
            };
            {
                let mut folder = folder_live.folder.borrow_mut();
                // 要件 5.1: one folder to a window, so the one that was open
                // goes — and with it every folder that was open inside it.
                folder.root = Some(chosen.clone());
                folder.expanded.clear();
            }
            publish_left(&window, &folder_live);
            write_session(&window, &folder_live);
            folder_live
                .cache
                .borrow_mut()
                .log_diag("folder", &format!("opened path={}", chosen.display()));
        }
    });

    let weak = window.as_weak();
    let tree_live = live.clone();
    window.on_left_row_activated(move |index| {
        let index = index.max(0) as usize;
        let weak = weak.clone();
        let live = tree_live.clone();
        // Opening a file replaces the model this row is drawn from. 6.18 again:
        // not from inside the click that is on it.
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                activate_left_row(&window, &live, index);
            }
        });
    });

    // 要件 6.2: which of the left pane's three things is showing. Rust holds
    // it, because the rows it puts there have to agree with it.
    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_left_tab_chosen(move |_tab| {
        let weak = weak.clone();
        let live = tab_live.clone();
        // The tab itself is set in the window, so the highlight moves at once.
        // Filling the panel replaces the model the rows are drawn from, and
        // this is a click inside a repeater — 6.18's rule, from the other side:
        // a different repeater, but not worth being clever about.
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                publish_left(&window, &live);
            }
        });
    });

    // 要件 7.7: the whole work folder, not the document in front.
    let weak = window.as_weak();
    let search_live = live.clone();
    window.on_folder_search_requested(move || {
        if let Some(window) = weak.upgrade() {
            search_work_folder(&window, &search_live);
        }
    });

    // 要件 2: and the answer, whenever the searching thread has one.
    let weak = window.as_weak();
    let found_live = live.clone();
    window.on_folder_search_finished(move || {
        if let Some(window) = weak.upgrade() {
            collect_search(&window, &found_live);
        }
    });

    let weak = window.as_weak();
    let command_live = live.clone();
    // From the event loop rather than from the callback, for 6.18's reason: the
    // commands are asked for from a menu that hangs off a row of the tree, and
    // half of them draw the tree again — which takes that row, and the menu on
    // it, away while the click is still being handled.
    window.on_tree_command(move |command| {
        let Some(command) = TreeCommand::from_index(command) else {
            return;
        };
        let weak = weak.clone();
        let live = command_live.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                tree_command(&window, &live, command);
            }
        });
    });

    let picked_live = live.clone();
    window.on_left_row_picked(move |index| {
        pick_tree_row(&picked_live, index.max(0) as usize);
    });

    // 要件 7.7: finding and replacing inside the document in front of the
    // writer. All three act on the focused pane, and all three go through the
    // ordinary editing path so that undo and the other panes follow.
    let weak = window.as_weak();
    let find_live = live.clone();
    window.on_find_requested(move |forwards| {
        if let Some(window) = weak.upgrade() {
            find_in_pane(&window, &find_live, forwards);
        }
    });

    let weak = window.as_weak();
    let replace_live = live.clone();
    window.on_replace_requested(move || {
        if let Some(window) = weak.upgrade() {
            replace_in_pane(&window, &replace_live);
        }
    });

    let weak = window.as_weak();
    let replace_all_live = live.clone();
    window.on_replace_all_requested(move || {
        if let Some(window) = weak.upgrade() {
            replace_all_in_pane(&window, &replace_all_live);
        }
    });

    let weak = window.as_weak();
    let tab_live = live.clone();
    window.on_pane_new_tab(move |pane| {
        if let Some(window) = weak.upgrade() {
            new_tab(&window, &tab_live, PaneId::from_index(pane));
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
    window.on_pane_scroll_changed(move |pane, _| {
        if let Some(window) = weak.upgrade() {
            refresh_after_scroll(&window, &cache, PaneId::from_index(pane));
        }
    });

    // Resizing changes how long a line may be — a column's height on one side,
    // a line's width on the other — and with it every block measurement.
    // Dragging a window edge produces a change per frame, so the relayout waits
    // for the drag to stop. **One timer per pane**, because the split divider
    // resizes both at once and a shared timer would let the second cancel the
    // first.
    let timers = [Rc::new(Timer::default()), Rc::new(Timer::default())];
    let reveal_timer = Rc::new(Timer::default());
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_resized(move |pane| {
        let id = PaneId::from_index(pane);
        let weak = weak.clone();
        let states = states.clone();
        let cache = cache.clone();
        let timer = &timers[id.index() as usize];
        timer.start(TimerMode::SingleShot, RESIZE_SETTLE, move || {
            if let Some(window) = weak.upgrade() {
                let started = Instant::now();
                let showing = states.document(id);
                let source = showing.text.borrow().clone();
                refresh_pane_from_state(&window, &cache, &showing, id, states.of(id), &source);
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
            update_pane_selection(&window, &document, state, &cache, id, x, y, phase);
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
            update_pane_selection(&window, &document, state, &cache, id, x, y, phase);
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
            update_pane_selection(&window, &document, state, &cache, id, x, y, phase);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_text_input(move |pane, text| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let text = text.as_str();
            insert_pane_text(&window, id, &document, &states, &cache, text, false);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_tab(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let indent = TAB_INDENT;
            insert_pane_text(&window, id, &document, &states, &cache, indent, true);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_preedit_changed(move |pane, text| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            let state = states.of(id);
            set_pane_preedit(&window, id, &document, state, &cache, text.as_str());
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
                state,
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
                state,
                &cache,
                &reveal,
                to_end,
                document_edge,
                extend,
            );
        }
    });

    let weak = window.as_weak();
    let mode_live = live.clone();
    window.on_view_mode_requested(move |requested_mode| {
        if let Some(window) = weak.upgrade() {
            let shown = PaneId::from_index(requested_mode);
            let panes = mode_live.layout.borrow().panes();
            if panes.len() == 1 && panes[0] == shown.index() as usize {
                return;
            }
            // Which pane the caret comes from, decided before anything moves:
            // whichever had the keyboard.
            let from = focused_pane(&window);
            // Only from a pane that was looking at the same document. A byte
            // offset means nothing in another text (6.7), and carrying one
            // across would put the caret inside a character of a document
            // nobody was reading.
            if from != shown && mode_live.states.same_document(from, shown) {
                let showing = mode_live.states.document(shown);
                let source = showing.text.borrow().clone();
                let states = &mode_live.states;
                carry_caret_between_panes(states.of(from), states.of(shown), &source);
            }
            mode_live.cache.borrow_mut().log_diag(
                "mode",
                &format!("to={} from={}", shown.log_name(), from.log_name()),
            );
            show_only(&window, &mode_live, shown);
        }
    });

    let weak = window.as_weak();
    let divide_live = live.clone();
    window.on_divide_requested(move |side_by_side| {
        if let Some(window) = weak.upgrade() {
            let split = if side_by_side {
                Split::SideBySide
            } else {
                Split::Stacked
            };
            let here = focused_pane(&window);
            let other = here.other();
            {
                let mut layout = divide_live.layout.borrow_mut();
                let this = here.index() as usize;
                let new = other.index() as usize;
                if layout.panes().len() > 1 {
                    // Both panes are already on screen, so this turns the
                    // arrangement rather than adding a pane. **Two is all there
                    // are** until the panes themselves are a list; the tree is
                    // ready for more, nothing else is yet.
                    *layout = Layout::divided(split, this, new);
                } else {
                    layout.divide(this, split, new);
                }
            }
            // Placed before the pane is filled, so that the tab it opens is laid
            // out into the area it will have rather than the one it had.
            place_panes(&window, &divide_live.layout.borrow());
            open_same_file_in(&window, &divide_live, other, here);
            after_layout_change(&window, &divide_live);
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
    window.on_zoom_in(move || {
        if let Some(window) = weak.upgrade() {
            let zoom = (window.get_zoom_percent() + 10).min(240);
            window.set_zoom_percent(zoom);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_zoom_out(move || {
        if let Some(window) = weak.upgrade() {
            let zoom = (window.get_zoom_percent() - 10).max(50);
            window.set_zoom_percent(zoom);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    window.on_zoom_reset(move || {
        if let Some(window) = weak.upgrade() {
            window.set_zoom_percent(100);
            schedule_relayout(&window, &states, &cache, &timer);
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
    window.set_sheet_numbers(ModelRc::from(numbers.clone()));
    window.set_palette(ModelRc::from(palette.clone()));
    window.set_sheet_fonts(ModelRc::from(sheet_fonts.clone()));
    // The families installed on this machine, asked for the first time the
    // picker is opened. **Not at startup**: it is a few hundred families read
    // out of the system collection, and a writer who never opens the picker
    // should not wait for it.
    let font_names: Rc<VecModel<SharedString>> = Rc::new(VecModel::default());
    window.set_font_names(ModelRc::from(font_names.clone()));
    // 要件 9: what the last run was set to, before anything is drawn with it.
    if let Some(directory) = app_data::app_directory()
        && let Some(values) = app_data::read_settings(&directory)
    {
        apply_settings(&numbers, &palette, &sheet_fonts, &values);
    }

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let steps = numbers.clone();
    window.on_typography_step(move |setting, by| {
        let Some(setting) = Setting::from_index(setting) else {
            return;
        };
        if let Some(window) = weak.upgrade() {
            step_setting(&window, &steps, setting, by);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // 要件 9: the colour the writer picks in the window Windows draws.
    //
    // **From the event loop, not from the click.** The dialog runs a message
    // loop of its own while it is open (6.18), and the swatch that asked for it
    // is inside a popup that may be taken down while it stands.
    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let colours = palette.clone();
    window.on_color_picked(move |slot| {
        let slot = slot.max(0) as usize;
        let weak = weak.clone();
        let states = states.clone();
        let cache = cache.clone();
        let timer = timer.clone();
        let colours = colours.clone();
        Timer::single_shot(Duration::ZERO, move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let sheet = shown_sheet(&window);
            let row = colour_row(sheet, slot);
            let now = window.get_palette().row_data(row).unwrap_or_default();
            let owner = ime::window_handle(&window);
            let standing = [now.red(), now.green(), now.blue()];
            let Some(picked) = shell::choose_colour(owner, standing) else {
                return;
            };
            let rgb = [
                picked[0] as f32 / 255.0,
                picked[1] as f32 / 255.0,
                picked[2] as f32 / 255.0,
            ];
            set_colour(&colours, sheet, slot, rgb);
            schedule_relayout(&window, &states, &cache, &timer);
        });
    });

    // 要件 9: which families this machine has, and which one was chosen.
    let weak = window.as_weak();
    let names = font_names;
    window.on_font_picked(move |slot| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // Read once and kept: the collection does not change while the editor
        // runs, and the picker is opened several times in a row when somebody
        // is settling on a set of families.
        if names.row_count() == 0 {
            for family in directwrite_render::font_families() {
                names.push(SharedString::from(family));
            }
        }
        let sheet = shown_sheet(&window);
        let row = font_row(sheet, slot.max(0) as usize);
        let standing = window.get_sheet_fonts().row_data(row).unwrap_or_default();
        window.set_font_slot(slot);
        window.set_font_current(standing);
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer.clone();
    let families = sheet_fonts.clone();
    window.on_font_chosen(move |slot, family| {
        if let Some(window) = weak.upgrade() {
            let sheet = shown_sheet(&window);
            let slot = slot.max(0) as usize;
            families.set_row_data(font_row(sheet, slot), family.clone());
            cache.borrow_mut().log_diag(
                "spec",
                &format!("font sheet={sheet} {}={family}", font_name(slot)),
            );
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    let timer = spec_timer;
    let steps = numbers;
    let colours = palette;
    let families = sheet_fonts;
    window.on_typography_reset(move || {
        if let Some(window) = weak.upgrade() {
            // **Both sheets.** 「初期値へ戻す」 is about the settings, and the
            // settings are two sheets of them; putting back only the one on
            // screen would leave the other holding whatever it held.
            reset_settings(&steps, &colours, &families);
            schedule_relayout(&window, &states, &cache, &timer);
        }
    });

    // The IME lays its candidate list out from the composition font, so it has
    // to be told which pane took the input (技術検証 7.2).
    let weak = window.as_weak();
    window.on_ime_vertical_requested(move |vertical| {
        if let Some(window) = weak.upgrade() {
            ime::set_vertical(&window, vertical);
        }
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
            refresh_pane_from_state(&window, &cache, &document, id, states.of(id), &source);
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
            let document = states.document(id);
            set_pane_direction(&window, &cache, id, !id.vertical(&window));
            // The anchor the up and down keys hold on to is a coordinate in the
            // layout that has just stopped existing.
            states.of(id).borrow_mut().preferred_line = None;
            let source = document.text.borrow().clone();
            refresh_pane_from_state(&window, &cache, &document, id, states.of(id), &source);
        }
    });

    let weak = window.as_weak();
    let states = pane_states.clone();
    let cache = render_cache.clone();
    window.on_pane_select_all(move |pane| {
        if let Some(window) = weak.upgrade() {
            let id = PaneId::from_index(pane);
            let document = states.document(id);
            select_whole_document(&window, &document, states.of(id), &cache, id);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_open_file_requested(move || {
        if let Some(window) = weak.upgrade() {
            open_document(&window, &file_live);
            restore_editor_focus(&window);
        }
    });

    // 要件 12: the quick draft. **The menu asks for it; it does not open it** —
    // what 要件 12.2 asks of this is that a global shortcut be able to ask the
    // same way later.
    let weak = window.as_weak();
    let held_draft = draft.clone();
    let draft_live = live.clone();
    window.on_quick_draft_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        // **What the draft window is given is a list of tabs and a way to put
        // text in one**, not the editor. It knows no more about this side than
        // `searcher.rs` knows about the window it wakes.
        //
        // The two share what the list resolved to, so the place a row stands
        // for is decided once (`paste_targets`).
        let resolved: Rc<RefCell<Vec<(PaneId, usize)>>> = Rc::default();
        let editor = quick_draft::Editor {
            tabs: {
                let weak = window.as_weak();
                let live = draft_live.clone();
                let resolved = resolved.clone();
                Box::new(move |aimed| {
                    let Some(window) = weak.upgrade() else {
                        return quick_draft::TabList {
                            rows: Vec::new(),
                            current: -1,
                            target: -1,
                            target_name: NO_TARGET.to_owned(),
                        };
                    };
                    paste_targets(&window, &live, aimed, &mut resolved.borrow_mut())
                })
            },
            paste: {
                let weak = window.as_weak();
                let live = draft_live.clone();
                let resolved = resolved.clone();
                Box::new(move |at, text| {
                    let Some(&(id, index)) = resolved.borrow().get(at) else {
                        return String::new();
                    };
                    let Some(window) = weak.upgrade() else {
                        return String::new();
                    };
                    paste_into_tab(&window, &live, id, index, text)
                })
            },
        };
        quick_draft::QuickDraftWindow::open(&held_draft, &window, editor);
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_document(&window, &file_live, false);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_as_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_document(&window, &file_live, true);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_save_all_requested(move || {
        if let Some(window) = weak.upgrade() {
            save_all(&window, &file_live);
            restore_editor_focus(&window);
        }
    });

    let weak = window.as_weak();
    let file_live = live.clone();
    window.on_reveal_requested(move || {
        if let Some(window) = weak.upgrade() {
            reveal_active_document(&window, &file_live);
            restore_editor_focus(&window);
        }
    });

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
        open_path_in_pane(&window, &live, opening_pane, &path);
    }

    let outcome = window.run();
    // 要件 8.5: the arrangement as the writer left it, including a boundary
    // moved without anything else happening. The views are taken out of the
    // panes first, because a caret and a scroll live there until they are.
    sync_active_tab(&window, &live);
    write_session(&window, &live);
    // 要件 12.4: and the draft, which is otherwise waiting on a two-second
    // timer that the end of the process will not let run.
    draft.borrow().store_now();
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
    for id in PaneId::ALL {
        if Rc::ptr_eq(&states.document(id), document) {
            *states.of(id).borrow_mut() = EditorState::default();
            id.set_scroll(window, 0.0);
        }
    }
    relayout_panes(window, states, cache);
}

/// Put the document's name where the writer can see it.
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
    /// The vertical pane opens on the formatted text and the horizontal one on
    /// the source. That pairing is what the four modes start from, and it is
    /// the only thing about a fresh view that depends on which pane it is.
    fn for_pane(id: PaneId) -> Self {
        Self {
            vertical: id.is_right(),
            preview: id.is_right(),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct PaneTab {
    /// **The document, not a copy of it.** A tab is a view; switching away no
    /// longer takes the text out of the pane and switching back no longer puts
    /// it in, so a switch costs the relayout and nothing else.
    document: Rc<OpenDocument>,
    /// How *this pane* is looking at that document. Another pane showing the
    /// same file has a tab of its own, with a caret and a scroll of its own
    /// (要件 7.6).
    view: TabView,
}

impl PaneTab {
    fn document(&self) -> Rc<OpenDocument> {
        self.document.clone()
    }

    /// A tab showing a document this pane has not looked at yet.
    fn showing(id: PaneId, document: Rc<OpenDocument>) -> Self {
        Self {
            document,
            view: TabView::for_pane(id),
        }
    }
}

/// One pane's strip (要件 6.3: 各編集ペインは独立したタブ列を持つ).
#[derive(Clone)]
struct PaneTabs {
    tabs: Vec<PaneTab>,
    /// Which of them the pane is showing. Always a position in `tabs`: the
    /// strip is never empty, because a pane with no tab has nothing to show and
    /// nowhere to type.
    active: usize,
}

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
    /// something per pane.
    panes: [PaneTabs; 2],
}

impl Tabs {
    fn of(&self, id: PaneId) -> &PaneTabs {
        &self.panes[id.index() as usize]
    }

    fn of_mut(&mut self, id: PaneId) -> &mut PaneTabs {
        &mut self.panes[id.index() as usize]
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
        let document = self.states.document(id);
        let source = document.text.borrow().clone();
        let zoom = window.get_zoom_percent();
        let state = self.states.of(id);
        let active_line_start = PaneId::revealed_line(id.vertical(window), state, &source);
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
        let mut borrowed = self.cache.borrow_mut();
        let cache = &mut *borrowed;
        hit_test_pane(
            window,
            cache,
            &document,
            id,
            &source,
            zoom,
            active_line_start,
            x,
            y,
        )
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
        refresh_pane_from_state(window, &self.cache, document, id, state, &source);
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

/// Put back what the last run left (要件 8.5).
///
/// **The two halves come from different places and are matched up here.** The
/// text comes from the work copies (要件 8.1), because that is where anything
/// unsaved lives; the arrangement — which pane held which tab, in what order,
/// in which of the four modes, and how the area was divided — comes from the
/// session. A file the session names that no work copy holds had nothing
/// unsaved in it, so it is opened from disk.
///
/// Anything that does not add up is dropped rather than argued with: a file
/// that has been deleted, a pane number from a later build, a session with no
/// panes at all. **Starting is worth more than any of it.**
fn open_session(
    window: &AppWindow,
    session: Option<app_data::Session>,
    restored: Vec<(Rc<OpenDocument>, EditorState)>,
) -> (Tabs, Layout) {
    let Some(session) = session else {
        return open_without_session(window, restored);
    };
    let mut placed: Vec<Rc<OpenDocument>> = Vec::new();
    let mut strips = PaneId::ALL.map(|_| PaneTabs {
        tabs: Vec::new(),
        active: 0,
    });
    for (id, stored) in PaneId::ALL.into_iter().zip(session.panes.iter()) {
        for tab in &stored.tabs {
            let Some(document) = document_for(window, tab, &restored, &placed) else {
                continue;
            };
            if !placed.iter().any(|held| Rc::ptr_eq(held, &document)) {
                placed.push(document.clone());
            }
            let caret = tab.caret;
            strips[id.index() as usize].tabs.push(PaneTab {
                document,
                view: TabView {
                    state: EditorState {
                        caret_source_byte: caret,
                        selection_anchor_source_byte: tab.anchor.or(caret),
                        ..EditorState::default()
                    },
                    scroll: tab.scroll as f32,
                    top: tab.top,
                    vertical: tab.vertical,
                    preview: tab.preview,
                },
            });
        }
        let strip = &mut strips[id.index() as usize];
        strip.active = stored.active.min(strip.tabs.len().saturating_sub(1));
    }
    // A work copy the session does not name is still somebody's unsaved work.
    // It goes in front of the writer rather than being left on disk unopened.
    let focused = PaneId::from_index(session.focused);
    for (document, state) in &restored {
        if placed.iter().any(|held| Rc::ptr_eq(held, document)) {
            continue;
        }
        let strip = &mut strips[focused.index() as usize];
        strip.tabs.push(PaneTab {
            document: document.clone(),
            view: TabView {
                state: state.clone(),
                ..TabView::for_pane(focused)
            },
        });
    }
    for id in PaneId::ALL {
        if strips[id.index() as usize].tabs.is_empty() {
            let number = next_untitled_number(&taken_numbers(&strips));
            let empty = OpenDocument::untitled(number, window.as_weak());
            strips[id.index() as usize]
                .tabs
                .push(PaneTab::showing(id, empty));
        }
    }
    let layout = Layout::decode(&session.layout)
        .filter(|layout| layout.panes().iter().all(|pane| *pane < PaneId::ALL.len()))
        .unwrap_or_else(|| Layout::single(focused.index() as usize));
    window.set_focused_pane(focused.index());
    window.set_editor_mode(focused.index());
    (Tabs { panes: strips }, layout)
}

/// The document one session tab names, from the work copies or from disk.
fn document_for(
    window: &AppWindow,
    tab: &app_data::SessionTab,
    restored: &[(Rc<OpenDocument>, EditorState)],
    placed: &[Rc<OpenDocument>],
) -> Option<Rc<OpenDocument>> {
    let names = |document: &Rc<OpenDocument>| {
        let file = document.file.borrow();
        match (&tab.origin, file.path()) {
            (Some(origin), Some(path)) => origin == path,
            (None, None) => file.untitled_number() == tab.untitled,
            _ => false,
        }
    };
    // A file open in two panes is one document, so a tab that names one already
    // put somewhere shares it (要件 7.6).
    if let Some(document) = placed.iter().find(|held| names(held)) {
        return Some(document.clone());
    }
    if let Some((document, _)) = restored.iter().find(|(document, _)| names(document)) {
        return Some(document.clone());
    }
    // Nothing unsaved, so the file itself is what it was.
    let origin = tab.origin.as_ref()?;
    match DocumentFile::open(origin, MAX_DOCUMENT_CHARACTERS) {
        Ok((file, text)) => Some(OpenDocument::new(file, text, window.as_weak())),
        Err(_) => None,
    }
}

/// Every untitled number the strips are holding.
fn taken_numbers(strips: &[PaneTabs; 2]) -> Vec<u32> {
    strips
        .iter()
        .flat_map(|strip| strip.tabs.iter())
        .map(|tab| tab.document.file.borrow().untitled_number())
        .collect()
}

/// The first run, or one whose session could not be read.
///
/// The work copies go to the pane on screen, and the other pane opens on the
/// same document as its active tab — 要件 6.4's rule for a pane that is new.
fn open_without_session(
    window: &AppWindow,
    restored: Vec<(Rc<OpenDocument>, EditorState)>,
) -> (Tabs, Layout) {
    let documents = if restored.is_empty() {
        let first = OpenDocument::new(
            DocumentFile::untitled(1),
            SAMPLE_MARKDOWN.to_owned(),
            window.as_weak(),
        );
        vec![(first, EditorState::default())]
    } else {
        restored
    };
    let here = focused_pane(window);
    let strips = PaneId::ALL.map(|id| {
        let tabs = if id == here {
            documents
                .iter()
                .map(|(document, state)| PaneTab {
                    document: document.clone(),
                    view: TabView {
                        state: state.clone(),
                        ..TabView::for_pane(id)
                    },
                })
                .collect()
        } else {
            vec![PaneTab {
                document: documents[0].0.clone(),
                view: TabView {
                    state: documents[0].1.clone(),
                    ..TabView::for_pane(id)
                },
            }]
        };
        PaneTabs { tabs, active: 0 }
    });
    (
        Tabs { panes: strips },
        Layout::single(here.index() as usize),
    )
}

/// What is on screen, in the form the session keeps it (要件 8.5).
fn capture_session(window: &AppWindow, live: &Live) -> app_data::Session {
    let tabs = live.tabs.borrow();
    let panes = PaneId::ALL
        .iter()
        .map(|id| {
            let strip = tabs.of(*id);
            app_data::SessionPane {
                active: strip.active,
                tabs: strip.tabs.iter().map(session_tab).collect(),
            }
        })
        .collect();
    let folder = live.folder.borrow();
    app_data::Session {
        layout: live.layout.borrow().encode(),
        focused: focused_pane(window).index(),
        panes,
        folder: folder.root.clone(),
        expanded: folder.expanded.iter().cloned().collect(),
        tree_shown: window.get_tree_open(),
        recent: live.recent.borrow().clone(),
        zoom: window.get_zoom_percent(),
    }
}

/// One tab, as the session keeps it.
///
/// The document is named the way a work copy names one — by its file, or by its
/// untitled number — so that the two are matched up when they come back.
fn session_tab(tab: &PaneTab) -> app_data::SessionTab {
    let file = tab.document.file.borrow();
    app_data::SessionTab {
        origin: file.path().map(Path::to_path_buf),
        untitled: file.untitled_number(),
        vertical: tab.view.vertical,
        preview: tab.view.preview,
        // Whole pixels: the scroll is a position on a page, and a session that
        // remembered a fraction of one would be keeping precision nobody can
        // see.
        scroll: tab.view.scroll as i32,
        top: tab.view.top,
        caret: tab.view.state.caret_source_byte,
        anchor: tab.view.state.selection_anchor_source_byte,
    }
}

/// Put the session away.
///
/// **Written whole, every time.** It is a few hundred bytes about an
/// arrangement, not a document, so there is nothing to be gained by working out
/// what changed.
fn write_session(window: &AppWindow, live: &Live) {
    let Some(directory) = app_data::app_directory() else {
        return;
    };
    let session = capture_session(window, live);
    if let Err(error) = app_data::write_session(&directory, &session) {
        live.cache
            .borrow_mut()
            .log_diag("session", &format!("save failed error={error}"));
    }
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
                    document,
                    view: TabView {
                        vertical: like.vertical(window),
                        preview: like.shows_preview(window),
                        ..TabView::default()
                    },
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
    // **A pane that has just come on screen brings its strip with it.** Its
    // tabs are in its own list and nowhere else until they are published, so a
    // pane could otherwise appear with a document and no tabs above it.
    publish_tabs(window, live);
    for id in PaneId::ALL {
        if !id.is_shown(window) {
            continue;
        }
        let showing = live.states.document(id);
        let source = showing.text.borrow().clone();
        let state = live.states.of(id);
        refresh_pane_from_state(window, &live.cache, &showing, id, state, &source);
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

/// Find the next match and put the selection on it (要件 7.7).
///
/// **The search runs in the pane the writer is in**, over the document that
/// pane is showing, from its caret. Selecting the match rather than only
/// scrolling to it means the next keystroke replaces it and 置換 has something
/// to work with — and it costs nothing, because a selection is what this editor
/// already knows how to show.
fn find_in_pane(window: &AppWindow, live: &Live, forwards: bool) {
    let id = focused_pane(window);
    let needle = window.get_find_needle().to_string();
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    let caret = id.caret_byte(state, &source);
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
    let Some((start, end)) = find::next_match(&source, &needle, from, forwards) else {
        let found = if needle.is_empty() {
            String::new()
        } else {
            format!("「{needle}」は見つかりません")
        };
        window.set_find_status(found.into());
        return;
    };
    let total = find::count(&source, &needle);
    window.set_find_status(format!("{total}件").into());
    show_source_range(window, live, id, &source, start, end);
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
    {
        let mut state = state.borrow_mut();
        state.selection_anchor_source_byte = Some(start);
        state.caret_source_byte = Some(end);
        state.active_line_start = Some(source_line_start(source, end));
        state.preferred_line = None;
    }
    refresh_pane_from_state(window, &live.cache, &document, id, state, source);
}

/// Replace what a search found, and go to the next one (要件 7.7).
///
/// **Through the ordinary editing path**, so the replacement is one undo step,
/// reaches every pane showing the document, and is written to the work copy
/// like anything else typed.
fn replace_in_pane(window: &AppWindow, live: &Live) {
    let id = focused_pane(window);
    let needle = window.get_find_needle().to_string();
    if needle.is_empty() {
        return;
    }
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let state = live.states.of(id);
    let selected = selection_source_range(&state.borrow());
    // Only what the search found. A selection that is not the needle means the
    // writer has moved on, and replacing it would take out something they chose
    // themselves.
    let on_a_match = selected
        .filter(|(start, end)| source.get(*start..*end) == Some(needle.as_str()))
        .is_some();
    if !on_a_match {
        find_in_pane(window, live, true);
        return;
    }
    let replacement = window.get_find_replacement().to_string();
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
    let id = focused_pane(window);
    let needle = window.get_find_needle().to_string();
    if needle.is_empty() {
        return;
    }
    let replacement = window.get_find_replacement().to_string();
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let (next, replaced) = find::replace_all(&source, &needle, &replacement);
    if replaced == 0 {
        window.set_find_status(format!("「{needle}」は見つかりません").into());
        return;
    }
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
        let mut state = live.states.of(id).borrow_mut();
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
    window.set_find_status(format!("{replaced}件置換しました").into());
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
        update_status(window, document, source, None, None);
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
    match LeftTab::from_index(window.get_left_tab()) {
        LeftTab::Explorer => publish_tree(window, live),
        LeftTab::Search => publish_results(window, live),
        LeftTab::Recent => publish_recent(window, live),
        LeftTab::Outline => publish_outline(window, live),
    }
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
    open_path_in_focused_pane(window, live, &found.path);
    let id = focused_pane(window);
    let document = live.states.document(id);
    let showing = document.file.borrow().path() == Some(found.path.as_path());
    if !showing {
        return;
    }
    let needle = window.get_folder_needle().to_string();
    let source = document.text.borrow().clone();
    let Some((start, end)) = find::next_match(&source, &needle, found.at, true) else {
        return;
    };
    show_source_range(window, live, id, &source, start, end);
}

/// Open something from the history (要件 7.7).
fn open_remembered(window: &AppWindow, live: &Live, index: usize) {
    let Some(path) = live.recent.borrow().get(index).cloned() else {
        return;
    };
    open_path_in_focused_pane(window, live, &path);
    // Opening it moves it to the top, so the list under the writer's hand has
    // changed and has to be drawn again.
    publish_left(window, live);
}

/// What a file is called in the history (要件 7.7).
///
/// **With the folder holding it**, because a work folder full of chapters has
/// several files called the same thing, and a list of identical names is not a
/// list anybody can choose from.
fn remembered_name(path: &Path) -> String {
    let name = entry_name(path);
    match path.parent().map(entry_name) {
        Some(folder) if !folder.is_empty() => format!("{name} — {folder}"),
        _ => name,
    }
}

/// How many files the history keeps.
const REMEMBERED_FILES: usize = 30;

/// Put a file at the top of the history (要件 7.7).
///
/// **Newest first, each file once.** Opening something already in the list
/// moves it up rather than repeating it, which is what makes a short list worth
/// reading.
fn remember_recent(live: &Live, path: &Path) {
    let mut recent = live.recent.borrow_mut();
    recent.retain(|held| held != path);
    recent.insert(0, path.to_path_buf());
    recent.truncate(REMEMBERED_FILES);
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
    let Some(root) = live.folder.borrow().root.clone() else {
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
    let drawn = rows
        .iter()
        .map(|row| LeftRow {
            name: row.name.clone().into(),
            depth: row.depth as i32,
            folder: row.folder,
            open: row.open,
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
    open_path_in_focused_pane(window, live, &path);
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
fn open_path_in_focused_pane(window: &AppWindow, live: &Live, path: &Path) {
    open_path_in_pane(window, live, focused_pane(window), path);
}

/// The same, into a pane the caller names.
///
/// Startup is why this is separate: [`focused_pane`] asks whether a pane is on
/// screen, and before the window has been shown nothing has a width, so it
/// answers for the pane that is *not* about to be in front. A file named on the
/// command line went to the hidden pane and looked like it had not opened at
/// all.
fn open_path_in_pane(window: &AppWindow, live: &Live, id: PaneId, path: &Path) {
    let held = {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        strip
            .tabs
            .iter()
            .position(|tab| tab.document.file.borrow().path() == Some(path))
    };
    if let Some(index) = held {
        // 要件 7.7: bringing a file forward is opening it, tab or no tab.
        remember_recent(live, path);
        switch_to_tab(window, live, id, index);
        return;
    }
    // Open somewhere else in the window is still the same document, and 要件 7.6
    // wants one text however many panes are showing it.
    let elsewhere = open_documents(live)
        .into_iter()
        .find(|document| document.file.borrow().path() == Some(path));
    let document = match elsewhere {
        Some(document) => document,
        None => match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
            Ok((file, text)) => OpenDocument::new(file, text, window.as_weak()),
            Err(error) => {
                window.set_render_status(format!("開けません: {error}").into());
                return;
            }
        },
    };
    let tab = PaneTab {
        view: TabView {
            vertical: id.vertical(window),
            preview: id.shows_preview(window),
            ..TabView::default()
        },
        ..PaneTab::showing(id, document)
    };
    add_tab(window, live, id, tab);
    // Recorded once it is open, so a file that could not be read does not sit
    // in the history as though it had been (要件 7.7).
    remember_recent(live, path);
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
        open_path_in_focused_pane(window, live, &path);
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
    if let Err(error) = file_tree::rename(from, &to) {
        let told = format!("名前を変えられません: {error}");
        window.set_render_status(told.into());
        return;
    }
    documents_follow(window, live, from, &to);
    {
        let mut open = live.folder.borrow_mut();
        open.selected = Some(to.clone());
        // A folder that was open stays open under its new name, and so does
        // every folder inside it: both are held by path.
        let mut moved = BTreeSet::new();
        for path in &open.expanded {
            let after = file_tree::moved_path(from, &to, path);
            moved.insert(after.unwrap_or_else(|| path.clone()));
        }
        open.expanded = moved;
    }
    publish_left(window, live);
    write_session(window, live);
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
        discard_work_copy(&was);
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
fn place_panes(window: &AppWindow, layout: &Layout) {
    let (placed, boundaries) = layout.place(editor_area(window));
    // **Everything the window believes about the arrangement is set here**, so
    // that none of it can be left behind by a path that forgot to. A session
    // that restored two panes without this said "not split" until something
    // else happened to change the arrangement — and then *both* panes answered
    // a request for the keyboard, so the pane the writer was in was whichever
    // one moved last.
    window.set_split_view(placed.len() > 1);
    let focused = PaneId::from_index(window.get_focused_pane());
    let on_screen = placed
        .iter()
        .any(|(pane, _)| *pane == focused.index() as usize);
    if !on_screen && let Some((first, _)) = placed.first() {
        window.set_focused_pane(*first as i32);
        window.set_editor_mode(*first as i32);
    }
    for id in PaneId::ALL {
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
        remembered.other()
    }
}

/// Write the live state back into the tab it belongs to.
///
/// Called before anything that reads or reorders the list, so the entry for the
/// active tab is never the stale one left there when it was brought out.
fn sync_active_tab(window: &AppWindow, live: &Live) {
    // Every pane, not only the focused one: each has its own caret in its own
    // tab, and the one being written back may not be the one being written in.
    let views = PaneId::ALL.map(|id| live.capture_view(window, id));
    let mut tabs = live.tabs.borrow_mut();
    for (id, view) in PaneId::ALL.into_iter().zip(views) {
        let strip = tabs.of_mut(id);
        if let Some(tab) = strip.tabs.get_mut(strip.active) {
            tab.view = view;
        }
    }
}

/// Put the tab strip and the window title in front of the writer.
///
/// **One strip is on screen while the strips are still drawn by the window**,
/// and it is the focused pane's — the one whose tabs the buttons above would
/// act on. Moving them inside the panes is the next step (ペイン分割設計 6).
fn publish_tabs(window: &AppWindow, live: &Live) {
    let strips = PaneId::ALL.map(|id| {
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        let infos = strip
            .tabs
            .iter()
            .map(|tab| TabInfo {
                // Every name and marker is read from the document. There is no
                // second copy to be fresher than the list any more.
                title: tab.document.file.borrow().title().into(),
                edited: tab.document.text.edited(),
            })
            .collect::<Vec<_>>();
        (infos, strip.active as i32)
    });
    for (id, (infos, active)) in PaneId::ALL.into_iter().zip(strips) {
        id.update_screen(window, |screen| {
            screen.tabs = ModelRc::new(VecModel::from(infos));
            screen.active_tab = active;
        });
    }
    // The name and the unsaved marker in the status bar are the focused pane's
    // document's, which after a switch is not the one they were last set from.
    let showing = live.active(window);
    window.set_document_edited(showing.text.edited());
    show_document_title(window, &showing.file.borrow());
    // **The strips and the arrangement are one thing** (要件 8.5), and this is
    // where the strips change. A boundary dragged without touching a strip is
    // caught by the write on the way out.
    write_session(window, live);
}

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
    publish_tabs(window, live);
}

/// A new empty document (要件 8.4).
fn new_tab(window: &AppWindow, live: &Live, id: PaneId) {
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
        ..PaneTab::showing(id, document)
    };
    add_tab(window, live, id, tab);
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
            discard_work_copy(&work_identity(&document.file.borrow()));
            // Nothing is waiting to be written any more, so neither the tick
            // nor the close can put the copy back.
            document.text.mark_saved();
            finish_close(window, live, pane, index);
            advance_close_run(window, live);
        }
        (Question::SaveConflict, 0) => overwrite_the_outside_change(window, live),
        (Question::SaveConflict, 1) => reload_from_file(window, live),
        (Question::SaveConflict, 2) => save_document(window, live, true),
        (Question::NewFile(parent), 0) => make_entry(window, live, &parent, false),
        (Question::NewFolder(parent), 0) => make_entry(window, live, &parent, true),
        (Question::RenameEntry(path), 0) => rename_entry(window, live, &path),
        (Question::DeleteEntry(path), 0) => delete_entry(window, live, &path),
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
        // pane is not gone**, only off screen with an empty strip; dividing
        // again brings it back with whatever is put in it.
        undivide_away(window, live, id);
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

/// Give the whole editing area to one pane.
///
/// **What the 左ペイン／右ペイン buttons do**: not a mode any more, just an
/// arrangement with one pane in it. The others keep their tabs and come back
/// with them when the area is divided again.
fn show_only(window: &AppWindow, live: &Live, id: PaneId) {
    *live.layout.borrow_mut() = Layout::single(id.index() as usize);
    window.set_focused_pane(id.index());
    window.set_editor_mode(id.index());
    after_layout_change(window, live);
}

/// Take a pane off screen, giving its area to the rest (要件 6.4).
///
/// **The pane is not gone** — it keeps its tabs, its carets and its scroll, and
/// dividing again brings all of it back. What it loses is a place to be drawn.
fn undivide_away(window: &AppWindow, live: &Live, id: PaneId) {
    {
        let mut layout = live.layout.borrow_mut();
        if !layout.remove(id.index() as usize) {
            return;
        }
    }
    let remaining = {
        let layout = live.layout.borrow();
        layout.panes().first().copied().unwrap_or(0) as i32
    };
    window.set_focused_pane(remaining);
    window.set_editor_mode(remaining);
    after_layout_change(window, live);
}

/// Put an empty document in a strip that has nothing left in it.
///
/// The last pane always has a tab: one with nothing to show has nowhere to type.
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
    strip.tabs.push(PaneTab::showing(id, empty));
    strip.active = 0;
}

/// Whether the work copy is due (要件 8.1).
///
/// Two rules, and the second is the one that makes long typing safe: the first
/// alone would never fire while somebody keeps going, which is exactly when
/// there is the most to lose. `since_pending` is measured from the start of the
/// run of changes now waiting, not from the last copy, so the guarantee is
/// "never more than this far apart" rather than "this long after a quiet spell".
fn work_copy_due(since_change: Duration, since_pending: Duration) -> bool {
    since_change >= WORK_COPY_IDLE || since_pending >= WORK_COPY_LONGEST
}

/// A document's work copy with no text: enough to name the file it lives in.
fn work_identity(file: &DocumentFile) -> app_data::WorkCopy {
    app_data::WorkCopy {
        origin: file.path().map(Path::to_path_buf),
        untitled: file.untitled_number(),
        ..app_data::WorkCopy::default()
    }
}

/// Drop a work copy that is no longer needed (要件 8.2).
fn discard_work_copy(copy: &app_data::WorkCopy) {
    let Some(directory) = app_data::work_directory() else {
        return;
    };
    let _ = app_data::discard_in(&directory, copy);
}

/// Write the work copy if either of 要件 8.1's rules says it is time.
///
/// The pending run is cleared whether the write succeeded or not. Left set, a
/// failing write would be retried at every tick for as long as the editor ran;
/// cleared, the next keystroke asks again, which is the same answer arrived at
/// without filling the log.
fn write_work_copy_if_due(window: &AppWindow, live: &Live) {
    // Asked of every open document. Each has its own two clocks, and the one
    // that has stopped being typed in is exactly the one whose two seconds run
    // out first (要件 8.1).
    let now = Instant::now();
    for document in open_documents(live) {
        let Some(pending_since) = document.text.pending_since() else {
            continue;
        };
        let since_change = now.duration_since(document.text.changed_at());
        let since_pending = now.duration_since(pending_since);
        if work_copy_due(since_change, since_pending) {
            write_work_copy_of(window, live, &document);
        }
    }
}

/// Write every open document's work copy whatever the timing says.
///
/// Used before anything structural — a tab switch, a close — so that nothing is
/// riding on the timer across it. Does nothing for a document with nothing
/// waiting, so it is safe to call at any of them.
fn write_work_copy_now(window: &AppWindow, live: &Live) {
    // Every document any pane is holding, not only the focused one: two panes
    // can be looking at two files, and both of them have work to lose (要件
    // 8.1). Collected first, because writing borrows the list again.
    let open = open_documents(live);
    for document in open {
        write_work_copy_of(window, live, &document);
    }
}

/// Write one document's work copy, if it has changes waiting.
fn write_work_copy_of(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>) {
    if document.text.pending_since().is_none() {
        return;
    }
    let cache = &live.cache;
    let file = &document.file;
    let Some(directory) = app_data::work_directory() else {
        return;
    };
    // The caret belongs to a pane that is showing *this* document; every pane
    // keeps its own (3.7), and only one of them can be restored into a single
    // position. The focused pane is asked first, because that is where the
    // writer is.
    let focused = focused_pane(window);
    let showing = [focused, focused.other()]
        .into_iter()
        .find(|id| Rc::ptr_eq(&live.states.document(*id), document));
    let caret = showing.and_then(|id| live.states.of(id).borrow().caret_source_byte);
    let copy = app_data::WorkCopy {
        origin: file.borrow().path().map(Path::to_path_buf),
        untitled: file.borrow().untitled_number(),
        caret,
        text: document.text.borrow().clone(),
    };
    // Marked written as soon as it is handed over, not when it lands. The
    // alternative is to keep asking every tick until the disk answers, which
    // would queue a second copy of the same document behind the first.
    document.text.work_copy_written();
    let path = directory.join(app_data::work_file_name(&copy));
    let bytes = app_data::encode(&copy).into_bytes();
    let length = bytes.len();
    if live.writer.write(path.clone(), bytes) {
        cache
            .borrow_mut()
            .log_diag("work", &format!("queued bytes={length}"));
        return;
    }
    // No writer thread. Written here instead, which is what this did before
    // the thread existed.
    let started = Instant::now();
    let outcome = app_data::write_into(&directory, &copy);
    let elapsed = elapsed_ms(started);
    let message = match outcome {
        Ok(path) => {
            let shown = path.display();
            format!("saved bytes={length} ms={elapsed:.2} path={shown}")
        }
        Err(error) => format!("failed bytes={length} error={error}"),
    };
    cache.borrow_mut().log_diag("work", &message);
}

/// Log what the writer thread has finished since last asked.
///
/// Polled on the same timer that decides when to write, so nothing on that
/// thread has to reach into the UI and no lock is shared with it.
fn collect_write_results(live: &Live) {
    for result in live.writer.drain() {
        let shown = result.path.display();
        let message = match &result.error {
            None => format!(
                "saved bytes={} ms={:.2} path={shown}",
                result.bytes, result.ms
            ),
            Some(error) => format!("failed bytes={} error={error}", result.bytes),
        };
        live.cache.borrow_mut().log_diag("work", &message);
    }
}

/// Notice another program writing the file (要件 8.3).
///
/// **A document with nothing unsaved is reloaded without asking.** That is what
/// the writer would do by hand, and there is nothing of theirs to lose. An
/// edited one is only reported: either side could be the one worth keeping, and
/// choosing without being told is how work disappears.
fn check_external_change(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let file = &document.file;
    if file.borrow().external_change() != ExternalChange::Modified {
        return;
    }
    let Some(stamp) = file.borrow().current_stamp() else {
        return;
    };
    if !file.borrow_mut().take_report(stamp) {
        return;
    }
    if document.text.edited() {
        window.set_render_status("別のアプリがこのファイルを変更しました".into());
        live.cache
            .borrow_mut()
            .log_diag("external", "modified edited=1 action=notify");
        return;
    }
    reload_from_file(window, live);
}

/// Take the file as it now is, in place of what the editor holds (要件 8.3).
///
/// **Also the answer that throws work away**, when it is chosen from the
/// conflict question rather than reached with nothing unsaved. The work copy
/// held exactly what is being given up, so it goes too — left behind, it would
/// bring the discarded text back at the next start.
fn reload_from_file(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let reloaded = document.file.borrow_mut().reload(MAX_DOCUMENT_CHARACTERS);
    match reloaded {
        Some(Ok(text)) => {
            let bytes = text.len();
            replace_document(window, &live.states, &live.cache, &document, text);
            // Reloaded, not edited: the text and the file agree by definition.
            document.text.mark_saved();
            discard_work_copy(&work_identity(&document.file.borrow()));
            publish_tabs(window, live);
            window.set_render_status("外部の変更を読み込みました".into());
            live.cache
                .borrow_mut()
                .log_diag("external", &format!("reloaded bytes={bytes}"));
        }
        Some(Err(error)) => {
            window.set_render_status(format!("読み直せません: {error}").into());
            live.cache
                .borrow_mut()
                .log_diag("external", &format!("reload failed error={error}"));
        }
        None => {}
    }
}

/// The tabs left by the last run (要件 8.1, 8.4).
///
/// Each work copy holds the text; the file it belongs to holds the shape to
/// write it back in, so the original is opened for that and the text it returns
/// thrown away. When the original has gone, the text is kept as an untitled
/// buffer rather than lost — not losing what was typed is the point, and the
/// name is the lesser half of it.
fn restore_tabs(window: &AppWindow) -> Vec<(Rc<OpenDocument>, EditorState)> {
    let Some(directory) = app_data::work_directory() else {
        return Vec::new();
    };
    let mut tabs = Vec::new();
    for copy in app_data::read_all_in(&directory) {
        let untitled = copy.untitled.max(1);
        let file = match &copy.origin {
            Some(path) => match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
                Ok((file, _)) => file,
                Err(_) => DocumentFile::untitled(untitled),
            },
            None => DocumentFile::untitled(untitled),
        };
        // Rounded here, because the copy was written by another run and nothing
        // guarantees the text is the same length now (6.7's rule, applied
        // across runs rather than across panes).
        let caret = copy.caret.map(|byte| floor_char_boundary(&copy.text, byte));
        let state = EditorState {
            caret_source_byte: caret,
            selection_anchor_source_byte: caret,
            ..EditorState::default()
        };
        let document = OpenDocument::new(file, copy.text, window.as_weak());
        // Restored from a work copy: the text does not agree with its file, but
        // the copy on disk already holds it, so nothing is waiting to be
        // written.
        document.text.mark_restored();
        tabs.push((document, state));
    }
    tabs
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

/// Whether a save may go ahead over what the file now holds.
///
/// Only the one question 要件 8.2 asks for. A file that has gone missing is
/// written again without asking: re-creating it is what the writer meant by
/// saving, and there is nothing of anyone else's to lose.
/// Save the document, asking for a name only when it has none.
///
/// `ask_for_name` is 名前を付けて保存. Without it, a document that already has
/// a file is written straight over it with no confirmation — 要件 8.2 asks for
/// one only when the file has changed underneath.
///
/// **No borrow is held across a dialog.** Both of them run their own message
/// loop, and Slint goes on delivering events from inside it.
fn save_document(window: &AppWindow, live: &Live, ask_for_name: bool) {
    let document = live.active(window);
    let file = &document.file;
    let owner = ime::window_handle(window);
    let existing = file.borrow().path().map(Path::to_path_buf);
    let suggested = file.borrow().title();
    let target = if ask_for_name || existing.is_none() {
        file_dialog::save_document_as(owner, &suggested)
    } else {
        existing.clone()
    };
    let Some(target) = target else {
        return;
    };
    // 要件 8.2: the ordinary Ctrl+S is silent, and the one thing it stops for
    // is a file that has changed underneath since it was opened. 要件 8.3 gives
    // that four answers, so the writing waits for one.
    let outside_change = file.borrow().external_change() == ExternalChange::Modified;
    if Some(&target) == existing.as_ref() && outside_change {
        let title = file.borrow().title();
        ask_question(
            window,
            live,
            Question::SaveConflict,
            format!(
                "「{title}」は別のアプリで変更されています。\n\n\
                 読み込むと、保存していない変更は失われます。"
            ),
            &[
                "作業中の内容で上書き",
                "外部の変更を読み込む",
                "別名で保存",
                "キャンセル",
            ],
            1,
        );
        return;
    }
    write_document_to(window, live, &document, target);
}

/// Overwrite the file with what is in the editor, outside change and all
/// (要件 8.3, the first of the four).
fn overwrite_the_outside_change(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let path = document.file.borrow().path().map(Path::to_path_buf);
    let Some(path) = path else {
        return;
    };
    write_document_to(window, live, &document, path);
}

/// Write the document into a path that has already been decided.
///
/// **Which document is an argument**: 全て保存 writes documents that are not in
/// front of any pane, so this cannot be the one the writer is looking at.
fn write_document_to(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    target: PathBuf,
) -> bool {
    let cache = &live.cache;
    let file = &document.file;
    let text = document.text.borrow().clone();
    let bytes = text.len();
    let shown = target.display().to_string();
    // Taken before the save, because 名前を付けて保存 moves the document to
    // another file and the copy on disk is still under the old name.
    let previous = work_identity(&file.borrow());
    let outcome = file.borrow_mut().save_to(target, &text);
    match outcome {
        Ok(()) => {
            document.text.mark_saved();
            discard_work_copy(&previous);
            discard_work_copy(&work_identity(&file.borrow()));
            // The name in the strip changes with 名前を付けて保存, and the
            // unsaved marker changes with every save.
            publish_tabs(window, live);
            window.set_render_status("保存しました".into());
            cache
                .borrow_mut()
                .log_diag("file", &format!("save ok bytes={bytes} path={shown}"));
            true
        }
        Err(error) => {
            window.set_render_status(format!("保存できません: {error}").into());
            cache
                .borrow_mut()
                .log_diag("file", &format!("save failed path={shown} error={error}"));
            false
        }
    }
}

/// Write every open document that has unsaved work (要件 8.2).
///
/// **The files first, then the names.** Everything that already knows where it
/// goes is written without a word; a document with no file needs somewhere to
/// go, and it is asked for afterwards so that the writing is not held up behind
/// a dialog. **The first キャンセル ends the asking**, the way it ends a run of
/// closes: the remaining 無題 keep their work copies (要件 8.1) and their
/// unsaved marks.
///
/// One kind is left where it is: a document another program has changed since it
/// was opened. That is 要件 8.3's four-way question, and it is asked about one
/// document at a time by `Ctrl+S` — silently overwriting it here is exactly what
/// 要件 8.3 exists to prevent. The status bar says how many were left.
fn save_all(window: &AppWindow, live: &Live) {
    let mut saved = 0;
    let mut failed = 0;
    let mut conflicted = 0;
    let mut unnamed: Vec<Rc<OpenDocument>> = Vec::new();
    for document in open_documents(live) {
        if !document.text.edited() {
            continue;
        }
        let path = document.file.borrow().path().map(Path::to_path_buf);
        let Some(path) = path else {
            unnamed.push(document);
            continue;
        };
        if document.file.borrow().external_change() == ExternalChange::Modified {
            conflicted += 1;
            continue;
        }
        if write_document_to(window, live, &document, path) {
            saved += 1;
        } else {
            failed += 1;
        }
    }
    let owner = ime::window_handle(window);
    let mut left = 0;
    let mut stopped = false;
    for document in unnamed {
        if stopped {
            left += 1;
            continue;
        }
        let suggested = document.file.borrow().title();
        let Some(target) = file_dialog::save_document_as(owner, &suggested) else {
            stopped = true;
            left += 1;
            continue;
        };
        if write_document_to(window, live, &document, target) {
            saved += 1;
        } else {
            failed += 1;
        }
    }
    // Written last, over whatever the individual saves said: the count is the
    // answer to 全て保存, and one of the writes saying 保存しました is not.
    let mut told = format!("{saved}件を保存しました");
    if failed > 0 {
        told.push_str(&format!("／{failed}件は保存できません"));
    }
    if left > 0 {
        told.push_str(&format!("／無題{left}件は保存していません"));
    }
    if conflicted > 0 {
        told.push_str(&format!("／外部変更{conflicted}件は個別に保存してください"));
    }
    window.set_render_status(told.clone().into());
    live.cache.borrow_mut().log_diag("file", &told);
}

/// Show the document in front of the pane in Explorer (要件 5.2).
///
/// The same command the tree has, from the pane's own menu, because the file
/// the writer means is at least as often the one they are editing as the one
/// they have selected in the tree.
fn reveal_active_document(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let path = document.file.borrow().path().map(Path::to_path_buf);
    let Some(path) = path else {
        window.set_render_status("まだ保存していない文書です".into());
        return;
    };
    let shown = path.display().to_string();
    live.cache
        .borrow_mut()
        .log_diag("file", &format!("reveal path={shown}"));
    shell::reveal(&path);
}

/// Open a file the writer chooses, in place of the current document.
fn open_document(window: &AppWindow, live: &Live) {
    let owner = ime::window_handle(window);
    let Some(path) = file_dialog::open_document(owner) else {
        return;
    };
    let shown = path.display().to_string();
    match DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS) {
        Ok((opened, text)) => {
            let bytes = text.len();
            let mixed = opened.mixed_newlines();
            // Its own tab, rather than in place of what is open. The document
            // already there may have unsaved work, and 要件 6.3 gives each file
            // a tab of its own.
            let document = OpenDocument::new(opened, text, window.as_weak());
            let id = focused_pane(window);
            let tab = PaneTab {
                view: TabView {
                    vertical: id.vertical(window),
                    preview: id.shows_preview(window),
                    ..TabView::default()
                },
                ..PaneTab::showing(id, document)
            };
            add_tab(window, live, id, tab);
            // 要件 7.7: opened through the dialog counts the same as opened
            // through the tree.
            remember_recent(live, &path);
            let note = if mixed {
                "（改行コードが混在していました）"
            } else {
                ""
            };
            window.set_render_status(format!("開きました{note}").into());
            live.cache.borrow_mut().log_diag(
                "file",
                &format!("open ok bytes={bytes} mixed={mixed} path={shown}"),
            );
        }
        Err(error) => {
            window.set_render_status(format!("開けません: {error}").into());
            live.cache
                .borrow_mut()
                .log_diag("file", &format!("open failed path={shown} error={error}"));
        }
    }
}

fn usable_preview_height(height: f32) -> u32 {
    if height.is_finite() && height >= MIN_PREVIEW_HEIGHT as f32 {
        height as u32
    } else {
        PREVIEW_HEIGHT
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
/// Zoom is passed rather than read back, because the caller may be applying a
/// new one; the rest only ever change through their own callback.
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
    spec.character_spacing = percent(number(Setting::CharAdvance));
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
const SHEET_NUMBERS: usize = 4 + MAX_HEADING_LEVEL;
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
    /// One heading level, 0 being H1.
    Heading(usize),
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
        }
    }

    /// How far one press moves it.
    fn step(self) -> i32 {
        match self {
            Self::BodySize => 1,
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
            Self::Heading(_) => (50, 400),
        }
    }

    fn default_value(self) -> i32 {
        match self {
            Self::BodySize => BASE_FONT_SIZE,
            Self::LineAdvance => 100,
            Self::CharAdvance => 0,
            Self::PageMargin => 12,
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
fn settings_values(window: &AppWindow) -> Vec<(String, String)> {
    let mut values = Vec::new();
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
    numbers: &VecModel<i32>,
    palette: &VecModel<Color>,
    fonts: &VecModel<SharedString>,
    values: &[(String, String)],
) {
    for (written, value) in values {
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
    let zoom = window.get_zoom_percent();
    // Every anchor goes, whether or not its pane is on screen: the anchor is a
    // coordinate in a layout that is about to stop existing, and a pane that
    // comes back holding one would step the caret to a line by the old
    // measurements.
    for id in PaneId::ALL {
        states.of(id).borrow_mut().preferred_line = None;
    }
    // The spec decides the layout on every side, so a change re-measures
    // whichever panes are on screen — **each from the document it is showing**,
    // which is not always the same one (要件 7.6). In reverse pane order: each
    // refresh writes the status line, and the horizontal pane's is the one that
    // has always been left standing (`draw_edit` keeps the same rule).
    let mut bytes = 0;
    for id in PaneId::ALL.into_iter().rev() {
        if id.is_shown(window) {
            let document = states.document(id);
            let source = document.text.borrow().clone();
            bytes = source.len();
            refresh_pane_from_state(window, cache, &document, id, states.of(id), &source);
        }
    }
    // Logged as its own kind of line. A keystroke re-measures one block; this
    // re-measures every block in both panes, so it belongs to a different cost
    // class and averaging it in with the keystrokes would hide both.
    // 要件 9: the settings are the app's and outlive the run. Written here
    // because everything that changes one of them asks for this relayout.
    save_settings(window, cache);
    let typography = typography_for(window, zoom, false, true);
    cache.borrow_mut().log_perf(&format!(
        "relayout total={total:.2} zoom={zoom} font={font:.1} \
         space={space:.2} lead={lead:.2} head={head:.2} bytes={bytes}",
        total = elapsed_ms(started),
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
fn schedule_active_line_reveal(
    timer: &Rc<Timer>,
    window: &AppWindow,
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
            let id = PaneId::Vertical;
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
            selection_source_range(&state),
            state.preedit.clone(),
        )
    };
    refresh_pane(
        window,
        cache,
        document,
        id,
        source,
        window.get_zoom_percent(),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneId {
    Vertical,
    Horizontal,
}

impl PaneId {
    /// Every pane, left to right.
    ///
    /// **The order the pane model's rows are in**, so a row's position, the
    /// `id` inside it and this array cannot drift apart. Everything that has
    /// one of something per pane is built by mapping over this.
    const ALL: [PaneId; 2] = [PaneId::Horizontal, PaneId::Vertical];

    /// Whether this is the right-hand pane.
    ///
    /// **Identity, not direction.** A pane draws whichever way the tab in front
    /// of it says (要件 7.2), which is [`vertical`](PaneId::vertical); this is
    /// only which of the two panes it is, and it is what the numbering, the log
    /// names and the split share are built on.
    fn is_right(self) -> bool {
        self == PaneId::Vertical
    }

    /// Which way this pane is drawing now.
    ///
    /// From the pane's row, which follows the tab in front of it. Everything
    /// about a flow axis, a scroll axis or the shape of a caret asks this.
    fn vertical(self, window: &AppWindow) -> bool {
        self.screen(window).vertical
    }

    /// The pane that is not this one.
    ///
    /// An edit needs it: the other pane is showing the same document and has
    /// to be brought along (要件 7.6). **This is where two panes are assumed**,
    /// and 段階3 turns it into "every other pane showing this document".
    fn other(self) -> Self {
        if self.is_right() {
            PaneId::Horizontal
        } else {
            PaneId::Vertical
        }
    }

    /// The pane's number, left to right across the window. **The same numbering
    /// as `editor-mode`**, so the two cannot drift apart, and the row this pane
    /// will be once the panes are a model (ペイン分割設計 5).
    fn index(self) -> i32 {
        if self.is_right() {
            VERTICAL_MODE
        } else {
            HORIZONTAL_MODE
        }
    }

    /// The pane a number from the UI names.
    ///
    /// Anything unexpected is the horizontal pane rather than a panic: this
    /// number arrives with a keystroke, and a keystroke must not be able to
    /// stop the editor.
    fn from_index(index: i32) -> Self {
        if index == VERTICAL_MODE {
            PaneId::Vertical
        } else {
            PaneId::Horizontal
        }
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
    fn initial_screen(self) -> PaneScreen {
        PaneScreen {
            id: self.index(),
            vertical: self.is_right(),
            // The vertical pane opens on the formatted text and the horizontal
            // one on the source, which is where the four modes are counted from
            // (`TabView::for_pane` says the same thing about a tab).
            preview: self.is_right(),
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

    /// How the performance log names this pane's lines. The two names are older
    /// than the panes being one code path, and README lists them.
    fn perf_kind(self) -> &'static str {
        if self.is_right() {
            "refresh"
        } else {
            "horizontal"
        }
    }

    /// How an edit's log line names the pane it was made in. Not [`perf_kind`],
    /// which names a *refresh* line and carries an older name for the vertical
    /// one.
    ///
    /// [`perf_kind`]: PaneId::perf_kind
    fn log_name(self) -> &'static str {
        if self.is_right() {
            "vertical"
        } else {
            "horizontal"
        }
    }

    /// How the diagnostic log tells this pane's lines from the other's.
    fn diag_suffix(self) -> &'static str {
        if self.is_right() { "v" } else { "h" }
    }

    /// The pane's extent across the flow, which is how long a line may be: a
    /// vertical pane sets its columns into its height, a horizontal one its
    /// lines into its width.
    fn line_extent_px(self, window: &AppWindow) -> u32 {
        let across = self.shown_across_flow(window);
        if self.vertical(window) {
            usable_preview_height(across)
        } else {
            usable_horizontal_width(across)
        }
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

    fn set_scroll(self, window: &AppWindow, offset: f32) {
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
    fn tile(vertical: bool, span: TileSpan, line_extent: u32, source: Image) -> PreviewTile {
        let flow_start = span.flow_start as i32;
        let flow_size = span.flow_size as i32;
        let line_extent = line_extent as i32;
        if vertical {
            PreviewTile {
                x: flow_start,
                y: 0,
                width: flow_size,
                height: line_extent,
                source,
            }
        } else {
            PreviewTile {
                x: 0,
                y: flow_start,
                width: line_extent,
                height: flow_size,
                source,
            }
        }
    }

    fn set_tiles(self, window: &AppWindow, tiles: Vec<PreviewTile>) {
        let model = ModelRc::new(VecModel::from(tiles));
        self.update_screen(window, |screen| screen.tiles = model);
    }

    fn set_selection(self, window: &AppWindow, rects: &[SelectionRect]) {
        let rects = rects
            .iter()
            .map(|rect| PreviewSelectionRect {
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left).max(0.0),
                height: (rect.bottom - rect.top).max(0.0),
            })
            .collect::<Vec<_>>();
        let model = ModelRc::new(VecModel::from(rects));
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
        let other = self.other();
        let follows = states.same_document(self, other);
        // **The other pane's caret follows the edit whether or not anything is
        // drawn now.** A position is about the text, and the text has changed;
        // holding this back with the drawing would leave that pane's caret
        // pointing at where the text used to be.
        if follows {
            carry_state_across(states.of(other), change, source);
        }
        let owed = {
            let mut borrowed = cache.borrow_mut();
            let pace = &mut borrowed.pace[self.index() as usize];
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
        cache.borrow_mut().pace[self.index() as usize].drew(took);
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
        let other = self.other();
        let follows = states.same_document(self, other);
        // **The right-hand pane's refresh writes the status line**, so it goes
        // last and its numbers are the ones left standing. Which pane, not
        // which direction: both may be running the same way now.
        if follows && self.is_right() {
            draw_followed_edit(window, other, states.of(other), cache, document, source);
        }
        match caret {
            Some(caret) => refresh_pane(
                window,
                cache,
                document,
                self,
                source,
                window.get_zoom_percent(),
                Some(source_line_start(source, caret)),
                Some(caret),
                None,
                "",
            ),
            None => refresh_pane_from_state(window, cache, document, self, states.of(self), source),
        }
        if follows && !self.is_right() {
            draw_followed_edit(window, other, states.of(other), cache, document, source);
        }
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
        let pace = &mut borrowed.pace[id.index() as usize];
        if pace.waiting {
            return;
        }
        pace.waiting = true;
    }
    let weak = window.as_weak();
    let states = states.clone();
    let cache = cache.clone();
    let timer = cache.borrow().pace[id.index() as usize].timer.clone();
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
        cache.borrow_mut().pace[id.index() as usize].drew(took);
    });
}

impl RenderCache {
    /// The pane an id names. **The only place the two are told apart by
    /// anything other than a [`PaneId`].**
    fn pane(&mut self, id: PaneId) -> &mut Pane {
        &mut self.panes[id.index() as usize]
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
    ) -> windows::core::Result<(usize, usize, usize, usize)> {
        let scroll = id.scroll(window);
        let shown_flow = id.shown_flow(window);
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
            return Ok((0, 0, 0, 0));
        }
        let line_extent = engine.line_extent();
        // Tiles are cut out of the blocks the viewport crosses. Their size along
        // the flow tracks the pane's extent across it, so a taller window makes
        // tiles narrower rather than making each one costlier to rasterize.
        let desired = engine.visible_tiles(scroll, shown_flow, prefetch);

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
                Some(PaneId::tile(
                    vertical,
                    *span,
                    line_extent,
                    cached.image.clone(),
                ))
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
        Ok((tile_count, rendered, reused, spare_held))
    }

    /// Re-cut the selection rectangles for what the pane now shows.
    fn refresh_pane_selection(
        &mut self,
        window: &AppWindow,
        id: PaneId,
    ) -> windows::core::Result<()> {
        let Some(selection) = self.pane(id).view.selection_utf16 else {
            return Ok(());
        };
        let engine = &mut self.pane(id).graphics.engine;
        let visible = id.flow_range(window, engine.total_flow_size() as f32);
        let rects = engine.selection_rects(Some(selection), visible)?;
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
    selection: Option<(u32, u32)>,
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
    line_extent_px: u32,
    typography: &Typography,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection_source_bytes: Option<(usize, usize)>,
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
    let selection = selection_source_bytes.and_then(|(start, end)| {
        let start = shown.utf16_at_source_byte(start) as u32;
        let end = shown.utf16_at_source_byte(end) as u32;
        (start < end).then_some((start, end - start))
    });
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
    let measured = match engine.update(styled, line_extent_px, typography) {
        Ok(measured) => measured,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}整形: NG / {error}").into());
            return None;
        }
    };
    let layout_ms = elapsed_ms(layout_started);

    pane.view.caret_utf16 = render_caret;
    pane.view.selection_utf16 = selection;
    pane.view.preedit_range = preedit_range;

    Some(PaneLayout {
        render_caret,
        anchor_utf16,
        selection,
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
fn refresh_pane(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    document: &OpenDocument,
    id: PaneId,
    source: &str,
    zoom_percent: i32,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection_source_bytes: Option<(usize, usize)>,
    preedit: &str,
) {
    let refresh_started = Instant::now();
    let typography = typography_for(
        window,
        zoom_percent,
        id.vertical(window),
        id.shows_preview(window),
    );
    let font_size = typography.font_size;
    let line_extent_px = id.line_extent_px(window);
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
        line_extent_px,
        &typography,
        active_line_start,
        caret_source_byte,
        selection_source_bytes,
        preedit,
    );
    let Some(laid_out) = laid_out else {
        return;
    };
    let PaneLayout {
        render_caret,
        anchor_utf16,
        selection,
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
        Some(if id.vertical(window) {
            place.x
        } else {
            place.y
        })
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
                document,
                source,
                selection_source_bytes,
                source_caret(window, id, caret_source_byte),
            );
            return;
        }
    };
    let selection_result = {
        let engine = &mut cache.pane(id).graphics.engine;
        engine.selection_rects(selection, visible)
    };
    let selection_rects = match selection_result {
        Ok(rects) => rects,
        Err(error) => {
            let label = id.label(window);
            window.set_render_status(format!("{label}選択座標: NG / {error}").into());
            update_status(
                window,
                document,
                source,
                selection_source_bytes,
                source_caret(window, id, caret_source_byte),
            );
            return;
        }
    };
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
    update_status(window, document, source, selection_source_bytes, place);
    // 要件 7.7: the outline is of the document in front of the writer, and the
    // pane that has just drawn is only sometimes the one they are in.
    if id == focused_pane(window) {
        draw_outline_if_showing(window, cache, source);
    }
    let stats_ms = elapsed_ms(stats_started);

    let (tile_count, rendered, tiles_reused, spare_held) = match tiles {
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
    // One status line for two panes, and **the right-hand one owns it** — which
    // pane, not which direction: in Split both refresh on the same keystroke,
    // so letting each write would leave the line flickering between two sets of
    // numbers. Short enough to survive an unwrapped half-width pane; the full
    // breakdown goes to the log, where nothing is clipped and both panes have
    // their own line.
    if id.is_right() {
        window.set_render_status(
            format!("縦書き {total_ms:.1}ms / tiles {tiles_ms:.1}ms {tile_count}枚{rendered}新 / 横 {push_ms:.1}ms")
                .into(),
        );
    }
    // Taken before the line is built: the log borrows the cache for the whole
    // of it.
    let held = cache.pace[id.index() as usize].take_held();
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
        kind = id.perf_kind(),
        // How many edits this draw is carrying (`EditPace`). One is the
        // ordinary case; more means a held key was outrunning the drawing.
        held = held,
        // Which panes are alive. Without this the log cannot tell a slow frame
        // caused by the horizontal pane from one caused by a large document,
        // because the two arrive together.
        split = u8::from(window.get_split_view()),
        mode = window.get_editor_mode(),
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
             viewport={viewport_flow:.0} scroll={scroll:.0} blocks={blocks} \
             measured={measured} tiles={tile_count} new={rendered} caret={caret_at} \
             preview={preview} mode={mode} split={split} zoom={zoom_percent}",
            preview = u8::from(id.shows_preview(window)),
            mode = window.get_editor_mode(),
            split = u8::from(window.get_split_view()),
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
    let (ime_x, ime_y) = ime_candidate_anchor(&caret, id.vertical(window));
    id.set_ime_anchor(window, ime_x, ime_y, &caret);
}

/// Redraw a pane after it was scrolled.
///
/// No tile is regenerated unless the viewport reached one it does not hold, and
/// the selection is re-cut because its rectangles are clipped to what is on
/// screen. This is a hit test over the visible blocks only.
fn refresh_after_scroll(window: &AppWindow, cache: &Rc<RefCell<RenderCache>>, id: PaneId) {
    let started = Instant::now();
    let label = id.label(window);
    let mut cache = cache.borrow_mut();
    // **The writer has scrolled, so this is where they want to look now**
    // (要件 8.5). The other way a hold ends is the caret moving, which the
    // layout pass notices for itself (`ViewAnchor`).
    cache.pane(id).view.top_anchor = None;
    let drawn = cache.refresh_pane_tiles(window, id, TILE_PREFETCH_COUNT);
    match drawn {
        Ok((count, new, _, _)) if new > 0 => {
            let ms = elapsed_ms(started);
            let line = format!("{label}遅延スクロール: {count}枚{new}新 / {ms:.1}ms");
            window.set_render_status(line.into());
        }
        Ok(_) => {}
        Err(error) => {
            let line = format!("{label}遅延タイル: NG / {error}");
            window.set_render_status(line.into());
        }
    }
    if let Err(error) = cache.refresh_pane_selection(window, id) {
        window.set_render_status(format!("{label}選択座標: NG / {error}").into());
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
    document: &OpenDocument,
    source: &str,
    selection_source_bytes: Option<(usize, usize)>,
    source_caret: Option<usize>,
) {
    let stats = document.counts.borrow_mut().get(source).stats();
    let selected_characters = selection_source_bytes
        .map(|(start, end)| {
            // Counting characters must never be the thing that brings the app
            // down, so the ends are walked back to character boundaries here
            // too. The paths that *change* the document stay strict.
            let start = floor_char_boundary(source, start);
            let end = floor_char_boundary(source, end).max(start);
            source[start..end].graphemes(true).count()
        })
        .unwrap_or(0);
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
    let body = format!("{} chars", thousands(stats.body_characters));
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
    if extend_selection {
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
    zoom: i32,
    active_line_start: Option<usize>,
    x: f32,
    y: f32,
) -> Option<usize> {
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
    let typography = typography_for(window, zoom, id.vertical(window), id.shows_preview(window));
    let engine = &mut graphics.engine;
    let label = id.label(window);
    if let Err(error) = engine.update(styled, id.line_extent_px(window), &typography) {
        window.set_render_status(format!("{label}整形: NG / {error}").into());
        return None;
    }
    match engine.hit_test(x, y) {
        Ok(hit) => Some(shown.source_byte_at_utf16(hit.utf16_position as usize)),
        Err(error) => {
            window.set_render_status(format!("{label}ヒットテスト: NG / {error}").into());
            None
        }
    }
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
    zoom: i32,
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
    let typography = typography_for(window, zoom, id.vertical(window), id.shows_preview(window));
    let engine = &mut graphics.engine;
    if let Err(error) = engine.update(styled, id.line_extent_px(window), &typography) {
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
    let drag_started = Instant::now();
    let source = document.text.borrow().clone();
    let zoom = window.get_zoom_percent();
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
            zoom,
            active_line_start,
            x,
            y,
        )
    };
    let Some(hit) = hit else {
        return;
    };

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
        if phase == SelectionPhase::Begin || state.selection_anchor_source_byte.is_none() {
            state.selection_anchor_source_byte = Some(hit);
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
        selection_source_range(&state)
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
            selection,
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
        zoom,
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
    selection: Option<(usize, usize)>,
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
        let range = selection.and_then(|(start, end)| {
            let start = shown.utf16_at_source_byte(start);
            let end = shown.utf16_at_source_byte(end);
            (start < end).then_some((start as u32, (end - start) as u32))
        });
        *caret_utf16 = Some(caret);
        *selection_utf16 = range;
        (caret, range)
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
        engine.selection_rects(selection_utf16, visible)
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
            update_status(window, document, source, selection, place);
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
    let mut current = -1;
    let mut target = -1;
    // **Numbered, not sided.** The two panes sit side by side today and one
    // above the other tomorrow, and 要件 6.4 divides further than that; a
    // number says which pane without saying where it is.
    let split = PaneId::ALL.iter().filter(|id| id.is_shown(window)).count() > 1;
    let focused = focused_pane(window);
    for id in PaneId::ALL {
        if !id.is_shown(window) {
            continue;
        }
        let tabs = live.tabs.borrow();
        let strip = tabs.of(id);
        for (index, tab) in strip.tabs.iter().enumerate() {
            let file = tab.document.file.borrow();
            let title = file.title();
            let number = id.index() + 1;
            rows.push(if split {
                format!("{number} · {title}")
            } else {
                title
            });
            if id == focused && index == strip.active {
                current = rows.len() as i32 - 1;
            }
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
        current,
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
    let state = states.of(id);
    id.set_ime_buffer(window, "");
    let started = Instant::now();
    let mut source = document.text.borrow().clone();
    let cloned_ms = elapsed_ms(started);
    if !fits_document_limit(&source, &input) {
        window.set_render_status(over_limit_message(&input).into());
        return;
    }
    let caret = id.caret_byte(state, &source);
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
        let revealed = PaneId::revealed_line(id.vertical(window), state, &source).unwrap_or(line);
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
    log_edit(cache, name, &source, started, cloned_ms, stored_ms);
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

fn delete_adjacent_grapheme(
    window: &AppWindow,
    id: PaneId,
    document: &Rc<OpenDocument>,
    states: &PaneStates,
    cache: &Rc<RefCell<RenderCache>>,
    backward: bool,
) {
    let state = states.of(id);
    let mut source = document.text.borrow().clone();
    let caret = id.caret_byte(state, &source);
    let revealed = PaneId::revealed_line(id.vertical(window), state, &source);
    // What one character is belongs to the text being shown, not to the
    // Markdown behind it (技術検証 3.12), so this asks the same text the pane
    // laid out.
    let (start, end) = {
        let mut borrowed = cache.borrow_mut();
        let slot = &mut borrowed.pane(id).view.preview_slot;
        let shown = pane_text(window, id, slot, &source, revealed);
        match selection_source_range(&state.borrow()) {
            Some(range) => range,
            None if backward => (shown.previous_grapheme(caret), caret),
            None => (caret, shown.next_grapheme(caret)),
        }
    };
    if start >= end {
        return;
    }

    let removed = source[start..end].to_owned();
    let next = replace_source_range(&mut source, (start, end), "");
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(next);
        state.selection_anchor_source_byte = Some(next);
        state.active_line_start = Some(source_line_start(&source, next));
        state.preferred_line = None;
    }
    let change = Change {
        at: start,
        removed: removed.len(),
        inserted: 0,
    };
    document.record(start, removed, String::new());
    *document.text.borrow_mut() = source.clone();
    id.draw_edit(window, states, cache, document, &source, next, change);
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
    let zoom = window.get_zoom_percent();
    let preferred_line = state.borrow().preferred_line;

    let moved = {
        let mut borrowed = cache.borrow_mut();
        let cache = &mut *borrowed;
        let measured = lay_out_for_caret(window, cache, document, id, &source, zoom, revealed);
        let Some(measured) = measured else {
            return;
        };
        let MeasuredPane { shown, engine } = measured;
        let at = shown.utf16_at_source_byte(caret) as u32;
        match direction {
            -1 => Ok((shown.previous_grapheme(caret), None)),
            1 => Ok((shown.next_grapheme(caret), None)),
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
        let selection = update_selection_after_move(&mut state, caret, next, extend_selection);
        state.preferred_line = next_preferred;
        selection
    };
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
        zoom,
        shown_line,
        Some(next),
        selection,
        "",
    );
    if !PaneId::reveals_while_moving(id.vertical(window)) {
        schedule_active_line_reveal(reveal, window, state, cache, document);
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
    let zoom = window.get_zoom_percent();

    let next = if document_edge {
        if to_end { source.len() } else { 0 }
    } else {
        let mut borrowed = cache.borrow_mut();
        let cache = &mut *borrowed;
        let measured = lay_out_for_caret(window, cache, document, id, &source, zoom, revealed);
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
        let selection = update_selection_after_move(&mut state, caret, next, extend_selection);
        state.preferred_line = None;
        selection
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
        zoom,
        shown_line,
        Some(next),
        selection,
        "",
    );
    if !PaneId::reveals_while_moving(id.vertical(window)) {
        schedule_active_line_reveal(reveal, window, state, cache, document);
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
    let selection = selection_source_range(&state.borrow());
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
        window.get_zoom_percent(),
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

/// One edit, as another view of the same document needs to know it (要件 7.6).
///
/// A byte count is all it takes: where the change began, how much went out and
/// how much came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Change {
    at: usize,
    removed: usize,
    inserted: usize,
}

impl Change {
    /// Where a position held somewhere else ends up.
    ///
    /// **Carried, not clamped.** Another pane's caret used only to be rounded to
    /// a character boundary (6.7), which left it on the same byte while the text
    /// under it moved — so typing in one pane slid the other's caret along the
    /// line. A position before the change does not move; one after it moves by
    /// what the change added or took away; one *inside* what was removed has
    /// nothing left to point at and goes to where the change began.
    fn moved(self, position: usize) -> usize {
        if position <= self.at {
            position
        } else if position >= self.at + self.removed {
            position - self.removed + self.inserted
        } else {
            self.at
        }
    }
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

fn replace_source_range(
    source: &mut String,
    (start, end): (usize, usize),
    replacement: &str,
) -> usize {
    source.replace_range(start..end, replacement);
    start + replacement.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_for(text: &str, zoom: i32) -> TextEngine {
        let mut engine = TextEngine::default();
        engine
            .update(
                StyledText::plain(text),
                PREVIEW_HEIGHT,
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
                height,
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
        let alone = Layout::single(PaneId::Vertical.index() as usize);
        let (placed, boundaries) = alone.place(area);

        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].0, PaneId::Vertical.index() as usize);
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
        for id in [PaneId::Vertical, PaneId::Horizontal] {
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

    /// The pane model is indexed by [`PaneId::index`], so a row has to sit at
    /// the position its own number names. A row in the wrong place would send
    /// a keystroke to the pane beside the one it was typed in.
    ///
    /// [`PaneId::index`]: PaneId::index
    #[test]
    fn a_pane_row_sits_at_the_position_its_number_names() {
        let rows = PaneId::ALL.map(PaneId::initial_screen);

        for (position, row) in rows.iter().enumerate() {
            assert_eq!(row.id as usize, position, "{row:?}");
        }
        assert!(!rows[0].vertical);
        assert!(rows[1].vertical);
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

    /// A tab keeps one view per pane, and a fresh one opens each pane in the
    /// mode that pane starts from (要件 7.2).
    ///
    /// The vertical pane on the formatted text and the horizontal one on the
    /// source is the arrangement the four modes are counted from, and it is
    /// the whole of what a fresh view knows about which pane it belongs to.
    #[test]
    fn a_fresh_view_opens_each_pane_in_its_own_starting_mode() {
        for id in PaneId::ALL {
            let view = TabView::for_pane(id);
            assert_eq!(view.vertical, id.is_right(), "{id:?}");
            assert_eq!(view.preview, id.is_right(), "{id:?}");
            assert_eq!(view.scroll, 0.0, "{id:?}");
            assert_eq!(view.state.caret_source_byte, None, "{id:?}");
        }
    }

    /// Everything with one of something per pane is indexed the same way, so
    /// these three have to agree: the array's order, the pane's own number, and
    /// the row it is published in.
    #[test]
    fn every_per_pane_collection_is_in_pane_order() {
        for (position, id) in PaneId::ALL.into_iter().enumerate() {
            assert_eq!(id.index() as usize, position, "{id:?}");
            assert_eq!(id.other().other(), id, "{id:?}");
            assert_ne!(id.other(), id, "{id:?}");
        }
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
            flow_start: 0,
            flow_size: 2,
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
        let tiles = engine.visible_tiles(middle, 640.0, 0);
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
                PREVIEW_HEIGHT,
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
}
