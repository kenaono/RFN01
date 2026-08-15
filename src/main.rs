mod directwrite_probe;
mod directwrite_render;
mod document;
mod text_blocks;
#[cfg(test)]
mod vertical_layout;

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::BTreeMap,
    fs::File,
    io::Write,
    rc::Rc,
    time::{Duration, Instant},
};

use directwrite_render::{TextEngine, WritingMode};
use document::{DocumentStats, PreviewDocument};
use slint::{
    ComponentHandle, Image, ModelRc, RenderingState, Rgba8Pixel, SharedPixelBuffer, Timer,
    TimerMode, VecModel,
};
use text_blocks::{TileSpan, visible_flow_range};
use unicode_segmentation::UnicodeSegmentation;

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
/// How long the caret must stop moving before the Markdown of its line is
/// revealed. Revealing rewrites that block's text, which costs a whole block's
/// worth of pixels; a held arrow key would pay that on every repeat.
const REVEAL_SETTLE: Duration = Duration::from_millis(120);
const BASE_FONT_SIZE: f32 = 22.0;
/// Tiles rendered on each side of the viewport, so crossing a tile boundary does
/// not stall on a rasterization the scroll is already waiting for. Caret moves
/// scroll the pane as much as the scrollbar does, so both paths prefetch.
const TILE_PREFETCH_COUNT: u32 = 1;
/// Resident tiles. Enough that scrolling back over ground already covered is
/// free, small enough to stay a fixed cost on any document length.
const TILE_CACHE_LIMIT: usize = 6;
const HORIZONTAL_MODE: i32 = 0;
const VERTICAL_MODE: i32 = 1;
/// Every refresh appends one line here. The status bar is a single unwrapped
/// line in a half-width pane, so anything past the first few figures is clipped;
/// this keeps the full breakdown somewhere it can actually be read afterwards.
const PERF_LOG_PATH: &str = "perf_log.txt";

/// One pane's caret, selection and pending IME text, all in source bytes.
///
/// Both panes keep one of these. Everything here is about the document, not
/// about how a pane draws it, so the same struct serves either writing
/// direction: `preferred_line` is the coordinate on the *line* axis to hold on
/// to when stepping between lines, which is a y in the vertical pane and an x in
/// the horizontal one. `active_line_start` is the vertical pane's alone — it
/// decides which line shows its Markdown, and the horizontal pane draws the
/// Markdown throughout.
#[derive(Debug, Default)]
struct EditorState {
    caret_source_byte: Option<usize>,
    selection_anchor_source_byte: Option<usize>,
    active_line_start: Option<usize>,
    preedit: String,
    preferred_line: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionPhase {
    Begin,
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
    preview: Option<PreviewDocument>,
}

impl PreviewSlot {
    fn get(&mut self, source: &str, active_line_start: Option<usize>) -> &PreviewDocument {
        let stale = self.preview.is_none()
            || self.active_line_start != active_line_start
            || self.source != source;
        if stale {
            self.preview = Some(PreviewDocument::from_source_with_active_line(
                source,
                active_line_start,
            ));
            self.source = source.to_owned();
            self.active_line_start = active_line_start;
        }
        self.preview.as_ref().expect("preview built above")
    }
}

/// Line and character counts, kept until the source changes. Selecting and
/// scrolling leave the source alone, so neither pays for a full recount.
#[derive(Default)]
struct StatsSlot {
    source: String,
    stats: Option<DocumentStats>,
}

impl StatsSlot {
    fn get(&mut self, source: &str) -> DocumentStats {
        if self.stats.is_none() || self.source != source {
            self.stats = Some(DocumentStats::from_source(source));
            self.source = source.to_owned();
        }
        self.stats.expect("stats computed above")
    }
}

/// Frames slower than this are worth a line of their own in the log.
const SLOW_FRAME_MS: f64 = 8.0;
/// Enough samples to take a median from without the buffer growing while idle.
const FRAME_SAMPLE_LIMIT: usize = 256;

/// The cost on the far side of `refresh_preview`, which its own timings cannot see.
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
    /// Where this image was last placed on the flow axis, for ranking evictions
    /// by distance.
    last_flow: i32,
}

/// Everything the horizontal pane draws with.
///
/// It renders the Markdown source itself rather than the formatted preview, so
/// the position mapping is the identity: a UTF-16 offset into what is drawn is a
/// UTF-16 offset into the document. That is why this pane needs no
/// `PreviewDocument` and no active-line reveal, and why it is a much smaller
/// thing than the vertical side despite drawing the same way.
struct HorizontalPane {
    engine: TextEngine,
    tiles: BTreeMap<u64, CachedTile>,
    /// Kept because scrolling and dragging re-cut the selection without going
    /// back through the document, exactly as on the vertical side.
    selection_utf16: Option<(u32, u32)>,
    preedit_range: Option<(u32, u32)>,
    uploaded_bytes: usize,
}

impl Default for HorizontalPane {
    /// Hand written because the engine has to be told its writing mode; every
    /// other field is empty until the first refresh.
    fn default() -> Self {
        Self {
            engine: TextEngine::new(WritingMode::Horizontal),
            tiles: BTreeMap::new(),
            selection_utf16: None,
            preedit_range: None,
            uploaded_bytes: 0,
        }
    }
}

#[derive(Default)]
struct RenderCache {
    engine: TextEngine,
    horizontal: HorizontalPane,
    preview_slot: PreviewSlot,
    stats_slot: StatsSlot,
    tiles: BTreeMap<u64, CachedTile>,
    /// Shared with the rendering notifier, which runs between refreshes.
    frames: Rc<RefCell<FrameProbe>>,
    /// Pixel bytes of the images handed to Slint since the last log line. Every
    /// one of them is a new texture the renderer has to upload.
    uploaded_bytes: usize,
    /// Caret and selection in preview UTF-16, so a scroll can re-clip the
    /// selection without going back through the document model.
    caret_utf16: Option<u32>,
    selection_utf16: Option<(u32, u32)>,
    preedit_range: Option<(u32, u32)>,
    /// How long the last push of the source text into the horizontal pane took.
    /// Only Split keeps that pane alive, so this isolates what Split adds.
    source_push_ms: Option<f64>,
    perf_log: Option<File>,
    perf_log_failed: bool,
}

impl RenderCache {
    /// Append one line to the performance log, best effort.
    fn log_perf(&mut self, line: &str) {
        if self.perf_log_failed {
            return;
        }
        if self.perf_log.is_none() {
            match File::options()
                .create(true)
                .append(true)
                .open(PERF_LOG_PATH)
            {
                Ok(file) => self.perf_log = Some(file),
                Err(_) => {
                    self.perf_log_failed = true;
                    return;
                }
            }
        }
        if let Some(file) = self.perf_log.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    let initial = SAMPLE_MARKDOWN.to_owned();
    let shared_document = Rc::new(RefCell::new(initial.clone()));
    // One caret per pane over one document. Neither pane follows the other's.
    let editor_state = Rc::new(RefCell::new(EditorState::default()));
    let horizontal_state = Rc::new(RefCell::new(EditorState::default()));
    let render_cache = Rc::new(RefCell::new(RenderCache::default()));
    window.set_directwrite_status(directwrite_status().into());

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

    refresh_preview(&window, &render_cache, &initial, 100, None, None, None, "");
    refresh_horizontal(&window, &render_cache, &initial, 100, None, None, "");

    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_vertical_scroll_changed(move |_| {
        if let Some(window) = weak.upgrade() {
            let started = Instant::now();
            let mut cache = cache.borrow_mut();
            let tiles = cache.refresh_visible_tiles(&window, TILE_PREFETCH_COUNT);
            match tiles {
                Ok((tile_count, rendered)) if rendered > 0 => window.set_render_status(
                    format!(
                        "DirectWrite遅延スクロール: {tile_count} tiles / {rendered} rendered / {:.1}ms",
                        elapsed_ms(started)
                    )
                    .into(),
                ),
                Ok(_) => {}
                Err(error) => {
                    window.set_render_status(format!("DirectWrite遅延タイル: NG / {error}").into())
                }
            }
            // Selection geometry is clipped to the viewport, so panning has to
            // re-cut it. This is a hit test over the visible blocks only.
            if let Err(error) = cache.refresh_visible_selection(&window) {
                window.set_render_status(format!("DirectWrite選択座標: NG / {error}").into());
            }
        }
    });

    // Resizing changes the column height, which changes how many characters fit
    // in a column and therefore every block measurement. Dragging a window edge
    // produces a change per frame, so the relayout waits for the drag to stop.
    let resize_timer = Rc::new(Timer::default());
    let reveal_timer = Rc::new(Timer::default());
    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    let timer = resize_timer.clone();
    window.on_vertical_resized(move || {
        let weak = weak.clone();
        let state = state.clone();
        let cache = cache.clone();
        let document = document.clone();
        timer.start(TimerMode::SingleShot, RESIZE_SETTLE, move || {
            if let Some(window) = weak.upgrade() {
                let started = Instant::now();
                let source = document.borrow().clone();
                refresh_preview_from_state(&window, &state, &cache, &source);
                let (height, blocks, tile_width) = {
                    let cache = cache.borrow();
                    (
                        cache.engine.line_extent(),
                        cache.engine.block_count(),
                        cache.engine.tile_flow_size(),
                    )
                };
                cache.borrow_mut().log_perf(&format!(
                    "resize height={height} blocks={blocks} tile_width={tile_width} total={:.2}",
                    elapsed_ms(started)
                ));
            }
        });
    });

    let weak = window.as_weak();
    let cache = render_cache.clone();
    window.on_horizontal_scroll_changed(move |_| {
        if let Some(window) = weak.upgrade() {
            let started = Instant::now();
            let mut cache = cache.borrow_mut();
            match cache.refresh_horizontal_tiles(&window, TILE_PREFETCH_COUNT) {
                Ok((tile_count, rendered)) if rendered > 0 => window.set_render_status(
                    format!(
                        "横書き遅延スクロール: {tile_count} tiles / {rendered} rendered / {:.1}ms",
                        elapsed_ms(started)
                    )
                    .into(),
                ),
                Ok(_) => {}
                Err(error) => {
                    window.set_render_status(format!("横書きタイル: NG / {error}").into())
                }
            }
            if let Err(error) = cache.refresh_horizontal_selection(&window) {
                window.set_render_status(format!("横書き選択: NG / {error}").into());
            }
        }
    });

    // The horizontal pane's *width* decides how many characters fit on a line,
    // so dragging the window edge or the split divider invalidates every block
    // measurement here just as a height change does on the vertical side.
    let horizontal_resize_timer = Rc::new(Timer::default());
    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    let timer = horizontal_resize_timer.clone();
    window.on_horizontal_resized(move || {
        let weak = weak.clone();
        let state = state.clone();
        let cache = cache.clone();
        let document = document.clone();
        timer.start(TimerMode::SingleShot, RESIZE_SETTLE, move || {
            if let Some(window) = weak.upgrade() {
                let started = Instant::now();
                let source = document.borrow().clone();
                refresh_horizontal_from_state(&window, &state, &cache, &source);
                let (width, blocks, tile_height) = {
                    let cache = cache.borrow();
                    (
                        cache.horizontal.engine.line_extent(),
                        cache.horizontal.engine.block_count(),
                        cache.horizontal.engine.tile_flow_size(),
                    )
                };
                cache.borrow_mut().log_perf(&format!(
                    "horizontal resize width={width} blocks={blocks} \
                     tile_height={tile_height} total={:.2}",
                    elapsed_ms(started)
                ));
            }
        });
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_selection_start(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_horizontal_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::Begin,
            );
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_selection_update(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_horizontal_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::Update,
            );
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_selection_end(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_horizontal_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::End,
            );
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let vertical = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_text_input(move |text| {
        if let Some(window) = weak.upgrade() {
            insert_horizontal_text(&window, &document, &state, &cache, &vertical, text.as_str());
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let vertical = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_tab(move || {
        if let Some(window) = weak.upgrade() {
            insert_horizontal_text(&window, &document, &state, &cache, &vertical, TAB_INDENT);
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_preedit_changed(move |text| {
        if let Some(window) = weak.upgrade() {
            let source = document.borrow().clone();
            let caret = horizontal_caret_byte(&state, &source);
            let selection = selection_source_range(&state.borrow());
            let preedit = text.to_string();
            {
                let mut state = state.borrow_mut();
                state.caret_source_byte = Some(caret);
                state.preedit = preedit.clone();
                state.preferred_line = None;
            }
            refresh_horizontal(
                &window,
                &cache,
                &source,
                window.get_zoom_percent(),
                Some(caret),
                selection,
                &preedit,
            );
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let vertical = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_backspace(move || {
        if let Some(window) = weak.upgrade() {
            edit_adjacent_grapheme_horizontal(&window, &document, &state, &cache, &vertical, true);
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let vertical = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_delete(move || {
        if let Some(window) = weak.upgrade() {
            edit_adjacent_grapheme_horizontal(&window, &document, &state, &cache, &vertical, false);
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_move(move |direction, extend_selection| {
        if let Some(window) = weak.upgrade() {
            move_horizontal_caret(
                &window,
                &document,
                &state,
                &cache,
                direction,
                extend_selection,
            );
        }
    });

    let weak = window.as_weak();
    let state = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_horizontal_home_end(move |to_end, document_edge, extend_selection| {
        if let Some(window) = weak.upgrade() {
            move_horizontal_to_line_edge(
                &window,
                &document,
                &state,
                &cache,
                to_end,
                document_edge,
                extend_selection,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_selection_start(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_vertical_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::Begin,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_selection_update(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_vertical_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::Update,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_selection_end(move |x, y| {
        if let Some(window) = weak.upgrade() {
            update_vertical_selection(
                &window,
                &document,
                &state,
                &cache,
                x,
                y,
                SelectionPhase::End,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_text_input(move |text| {
        if let Some(window) = weak.upgrade() {
            insert_vertical_text(
                &window,
                &document,
                &state,
                &horizontal,
                &cache,
                text.as_str(),
                false,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_tab(move || {
        if let Some(window) = weak.upgrade() {
            insert_vertical_text(
                &window,
                &document,
                &state,
                &horizontal,
                &cache,
                TAB_INDENT,
                true,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_preedit_changed(move |text| {
        if let Some(window) = weak.upgrade() {
            let source = document.borrow().clone();
            let (active_line_start, source_byte) = current_source_caret(&state, &source);
            let selection = selection_source_range(&state.borrow());
            let preedit = text.to_string();
            {
                let mut state = state.borrow_mut();
                state.caret_source_byte = Some(source_byte);
                state.active_line_start = Some(active_line_start);
                state.preedit = preedit.clone();
                state.preferred_line = None;
            }
            refresh_preview(
                &window,
                &cache,
                &source,
                window.get_zoom_percent(),
                Some(active_line_start),
                Some(source_byte),
                selection,
                &preedit,
            );
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_backspace(move || {
        if let Some(window) = weak.upgrade() {
            edit_adjacent_grapheme(&window, &document, &state, &horizontal, &cache, true);
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_vertical_delete(move || {
        if let Some(window) = weak.upgrade() {
            edit_adjacent_grapheme(&window, &document, &state, &horizontal, &cache, false);
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    let reveal = reveal_timer.clone();
    window.on_vertical_move(move |direction, extend_selection| {
        if let Some(window) = weak.upgrade() {
            let source = document.borrow().clone();
            let (active_line_start, source_byte) = current_source_caret(&state, &source);
            let zoom = window.get_zoom_percent();
            let preferred_line = state.borrow().preferred_line;

            let moved = {
                let mut cache = cache.borrow_mut();
                let RenderCache {
                    engine,
                    preview_slot,
                    ..
                } = &mut *cache;
                let preview = preview_slot.get(&source, Some(active_line_start));
                let caret = preview.utf16_at_source_byte(source_byte);
                if let Err(error) =
                    engine.update(&preview.text, preview_height(&window), font_size_for(zoom))
                {
                    window.set_render_status(format!("DirectWrite縦書き整形: NG / {error}").into());
                    return;
                }

                match direction {
                    -1 => Ok((preview.previous_grapheme_position(caret), None)),
                    1 => Ok((preview.next_grapheme_position(caret), None)),
                    -2 | 2 => {
                        let anchor_y = match preferred_line {
                            Some(y) => Ok(y),
                            None => engine
                                .caret_geometry(caret as u32)
                                .map(|geometry| geometry.y),
                        };
                        anchor_y.and_then(|y| {
                            engine
                                .move_caret_by_line(caret as u32, direction, Some(y))
                                .map(|hit| (hit.utf16_position as usize, Some(y)))
                        })
                    }
                    _ => Ok((caret, None)),
                }
                .map(|(next, next_preferred_y)| {
                    (preview.source_byte_at_utf16(next), next_preferred_y)
                })
            };

            let (next_source_byte, next_preferred_y) = match moved {
                Ok(moved) => moved,
                Err(error) => {
                    window.set_render_status(
                        format!("DirectWriteキャレット移動: NG / {error}").into(),
                    );
                    return;
                }
            };
            // The revealed line is deliberately left alone here. Revealing the
            // Markdown of a new line rewrites that block's text, which redraws
            // every tile the block touches; doing that per key repeat is what
            // made held arrow keys stutter. The caret moves inside the layout
            // that is already on screen, and the reveal catches up once the
            // caret settles.
            let selection = {
                let mut state = state.borrow_mut();
                let selection = update_selection_after_move(
                    &mut state,
                    source_byte,
                    next_source_byte,
                    extend_selection,
                );
                state.preferred_line = next_preferred_y;
                selection
            };
            refresh_preview(
                &window,
                &cache,
                &source,
                zoom,
                Some(active_line_start),
                Some(next_source_byte),
                selection,
                "",
            );
            schedule_active_line_reveal(&reveal, &window, &state, &cache, &document);
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    let reveal = reveal_timer.clone();
    window.on_vertical_home_end(move |to_end, document_edge, extend_selection| {
        if let Some(window) = weak.upgrade() {
            let source = document.borrow().clone();
            let (active_line_start, source_byte) = current_source_caret(&state, &source);
            let zoom = window.get_zoom_percent();

            let next_source_byte = if document_edge {
                if to_end { source.len() } else { 0 }
            } else {
                let mut cache = cache.borrow_mut();
                let RenderCache {
                    engine,
                    preview_slot,
                    ..
                } = &mut *cache;
                let preview = preview_slot.get(&source, Some(active_line_start));
                let caret = preview.utf16_at_source_byte(source_byte);
                if let Err(error) =
                    engine.update(&preview.text, preview_height(&window), font_size_for(zoom))
                {
                    window.set_render_status(format!("DirectWrite縦書き整形: NG / {error}").into());
                    return;
                }
                // Column edges come from cached line metrics, so this costs nothing.
                let edge = engine.move_caret_to_line_edge(caret as u32, to_end);
                preview.source_byte_at_utf16(edge as usize)
            };

            // Same as the arrow keys: move inside the layout already on screen
            // and let the reveal follow once the caret settles.
            let selection = {
                let mut state = state.borrow_mut();
                let selection = update_selection_after_move(
                    &mut state,
                    source_byte,
                    next_source_byte,
                    extend_selection,
                );
                state.preferred_line = None;
                selection
            };
            refresh_preview(
                &window,
                &cache,
                &source,
                zoom,
                Some(active_line_start),
                Some(next_source_byte),
                selection,
                "",
            );
            schedule_active_line_reveal(&reveal, &window, &state, &cache, &document);
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_view_mode_requested(move |requested_mode| {
        if let Some(window) = weak.upgrade() {
            let mode = if requested_mode == HORIZONTAL_MODE {
                HORIZONTAL_MODE
            } else {
                VERTICAL_MODE
            };
            if !window.get_split_view() && window.get_editor_mode() == mode {
                return;
            }

            window.set_split_view(false);
            window.set_editor_mode(mode);
            let source = document.borrow().clone();
            // Only the pane that is about to be shown is laid out. The other one
            // keeps its blocks and tiles, which are still valid when it comes
            // back: the document is what they were measured from.
            if mode == HORIZONTAL_MODE {
                refresh_horizontal_from_state(&window, &horizontal, &cache, &source);
            } else {
                refresh_preview_from_state(&window, &state, &cache, &source);
            }
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_split_toggled(move || {
        if let Some(window) = weak.upgrade() {
            let split = !window.get_split_view();
            window.set_split_view(split);
            if split {
                let source = document.borrow().clone();
                refresh_horizontal_from_state(&window, &horizontal, &cache, &source);
                refresh_preview_from_state(&window, &state, &cache, &source);
            }
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_zoom_in(move || {
        if let Some(window) = weak.upgrade() {
            let zoom = (window.get_zoom_percent() + 10).min(240);
            apply_zoom(&window, &state, &horizontal, &cache, &document, zoom);
        }
    });

    let weak = window.as_weak();
    let state = editor_state.clone();
    let horizontal = horizontal_state.clone();
    let cache = render_cache.clone();
    let document = shared_document.clone();
    window.on_zoom_out(move || {
        if let Some(window) = weak.upgrade() {
            let zoom = (window.get_zoom_percent() - 10).max(50);
            apply_zoom(&window, &state, &horizontal, &cache, &document, zoom);
        }
    });

    let weak = window.as_weak();
    let state = editor_state;
    let horizontal = horizontal_state;
    let cache = render_cache;
    let document = shared_document;
    window.on_zoom_reset(move || {
        if let Some(window) = weak.upgrade() {
            apply_zoom(&window, &state, &horizontal, &cache, &document, 100);
        }
    });

    window.run()
}

/// The column height, which is the height of the vertical pane.
///
/// This decides how many characters fit in a column, so it decides the whole
/// layout: a taller window means fewer, longer columns and a narrower document.
fn preview_height(window: &AppWindow) -> u32 {
    usable_preview_height(window.get_preview_visible_height())
}

fn usable_preview_height(height: f32) -> u32 {
    if height.is_finite() && height >= MIN_PREVIEW_HEIGHT as f32 {
        height as u32
    } else {
        PREVIEW_HEIGHT
    }
}

fn font_size_for(zoom_percent: i32) -> f32 {
    BASE_FONT_SIZE * zoom_percent as f32 / 100.0
}

/// The global x range the vertical pane currently shows. Everything that clips
/// work to the viewport goes through this, so the bounds cannot drift apart.
fn viewport_flow_range(window: &AppWindow, total_flow: f32) -> (f32, f32) {
    visible_flow_range(
        window.get_preview_scroll_x(),
        window.get_preview_visible_width().max(640.0),
        total_flow,
    )
}

fn apply_zoom(
    window: &AppWindow,
    state: &Rc<RefCell<EditorState>>,
    horizontal_state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    document: &Rc<RefCell<String>>,
    zoom: i32,
) {
    window.set_zoom_percent(zoom);
    let (active_line_start, caret_source_byte, selection, preedit) = {
        let mut state = state.borrow_mut();
        state.preferred_line = None;
        (
            state.active_line_start,
            state.caret_source_byte,
            selection_source_range(&state),
            state.preedit.clone(),
        )
    };
    let source = document.borrow().clone();
    if vertical_view_visible(window) {
        refresh_preview(
            window,
            cache,
            &source,
            zoom,
            active_line_start,
            caret_source_byte,
            selection,
            &preedit,
        );
    }
    // The font size decides the layout on both sides, so a zoom re-measures
    // whichever panes are on screen.
    if horizontal_view_visible(window) {
        horizontal_state.borrow_mut().preferred_line = None;
        refresh_horizontal_from_state(window, horizontal_state, cache, &source);
    }
}

/// Mirror an edit made in the vertical pane into the horizontal one, timing it.
///
/// This used to hand the string to Slint's `TextInput`, which laid out the whole
/// document on every keystroke and cost Split a 90ms frame (技術検証 6.4). The
/// pane now lays out through the same block-split engine, so what is timed here
/// is a second engine's update and the tiles its viewport needs.
fn push_source_to_horizontal(
    window: &AppWindow,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
) {
    // Clamped even when the pane is hidden. Its caret still has to survive every
    // edit made while it was away, or the first refresh after it comes back
    // works from a position that no longer exists.
    clamp_state_into(state, source);
    if !horizontal_view_visible(window) {
        cache.borrow_mut().source_push_ms = None;
        return;
    }
    let started = Instant::now();
    refresh_horizontal_from_state(window, state, cache, source);
    cache.borrow_mut().source_push_ms = Some(elapsed_ms(started));
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
    document: &Rc<RefCell<String>>,
) {
    let weak = window.as_weak();
    let state = state.clone();
    let cache = cache.clone();
    let document = document.clone();
    timer.start(TimerMode::SingleShot, REVEAL_SETTLE, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let source = document.borrow().clone();
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
            refresh_preview_from_state(&window, &state, &cache, &source);
        }
    });
}

fn horizontal_view_visible(window: &AppWindow) -> bool {
    view_visibility(window.get_editor_mode(), window.get_split_view()).0
}

fn vertical_view_visible(window: &AppWindow) -> bool {
    view_visibility(window.get_editor_mode(), window.get_split_view()).1
}

fn view_visibility(editor_mode: i32, split_view: bool) -> (bool, bool) {
    (
        split_view || editor_mode == HORIZONTAL_MODE,
        split_view || editor_mode == VERTICAL_MODE,
    )
}

fn refresh_preview_from_state(
    window: &AppWindow,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
) {
    let (active_line_start, caret_source_byte, selection, preedit) = {
        let state = state.borrow();
        (
            state.active_line_start,
            state.caret_source_byte,
            selection_source_range(&state),
            state.preedit.clone(),
        )
    };
    refresh_preview(
        window,
        cache,
        source,
        window.get_zoom_percent(),
        active_line_start,
        caret_source_byte,
        selection,
        &preedit,
    );
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

impl RenderCache {
    /// Render the tiles the viewport needs and drop the ones it no longer does.
    ///
    /// A tile is a slice of one block, so this cost tracks the viewport, not the
    /// document, and an edit only invalidates the block it changed.
    fn refresh_visible_tiles(
        &mut self,
        window: &AppWindow,
        prefetch: u32,
    ) -> windows::core::Result<(usize, usize)> {
        let content_width = self.engine.total_flow_size();
        if content_width == 0 {
            return Ok((0, 0));
        }

        let viewport_x = window.get_preview_scroll_x();
        let visible_width = window.get_preview_visible_width().max(640.0);
        let line_extent = self.engine.line_extent();
        // Tiles are cut out of the blocks the viewport crosses. The slice width
        // tracks the pane height, so a taller window makes tiles narrower rather
        // than making each one more expensive to rasterize.
        let desired = self
            .engine
            .visible_tiles(viewport_x, visible_width, prefetch);

        // Keyed by the fingerprint, never by position. The fingerprint names one
        // block's text at one slice of it, so a tile the layout moved is found
        // again unchanged, and two blocks with the same text share one image.
        let preedit = self.preedit_range;
        let keyed = desired
            .iter()
            .map(|span| (*span, self.engine.tile_signature(*span, preedit)))
            .collect::<Vec<_>>();
        let mut missing: Vec<(TileSpan, u64)> = Vec::new();
        for (span, signature) in &keyed {
            let already = self.tiles.contains_key(signature)
                || missing.iter().any(|(_, queued)| queued == signature);
            if !already {
                missing.push((*span, *signature));
            }
        }
        let rendered = missing.len();

        let mut uploaded = 0_usize;
        if !missing.is_empty() {
            let spans = missing.iter().map(|(span, _)| *span).collect::<Vec<_>>();
            let mut produced = Vec::with_capacity(spans.len());
            self.engine
                .render_tiles(&spans, preedit, |span, width, height, bgra| {
                    uploaded += bgra.len();
                    // The engine reports the image's own size: this pane is the
                    // vertical one, so that is the tile's width by the pane's
                    // height.
                    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                    // Direct2D hands back BGRA and this reads it in place, so the
                    // pixels are walked once and copied once.
                    for (target, source) in
                        pixels.make_mut_slice().iter_mut().zip(bgra.chunks_exact(4))
                    {
                        *target = Rgba8Pixel {
                            r: source[2],
                            g: source[1],
                            b: source[0],
                            a: source[3],
                        };
                    }
                    produced.push((span, Image::from_rgba8(pixels)));
                })?;
            for (span, image) in produced {
                let Some((_, signature)) = missing.iter().find(|(queued, _)| *queued == span)
                else {
                    continue;
                };
                self.tiles.insert(
                    *signature,
                    CachedTile {
                        image,
                        last_flow: span.flow_start as i32,
                    },
                );
            }
        }

        self.uploaded_bytes += uploaded;

        // Placement is not cached, so a tile that slid sideways with the right
        // edge costs one property assignment rather than a rasterization.
        let tiles = keyed
            .iter()
            .filter_map(|(span, signature)| {
                let cached = self.tiles.get_mut(signature)?;
                cached.last_flow = span.flow_start as i32;
                Some(PreviewTile {
                    // A vertical tile is as tall as the pane and stacked on x.
                    x: span.flow_start as i32,
                    y: 0,
                    width: span.flow_size as i32,
                    height: line_extent as i32,
                    source: cached.image.clone(),
                })
            })
            .collect::<Vec<_>>();

        let viewport_center = -viewport_x + visible_width * 0.5;
        let wanted = keyed
            .iter()
            .map(|(_, signature)| *signature)
            .collect::<Vec<_>>();
        evict_distant_tiles(&mut self.tiles, &wanted, viewport_center);

        let tile_count = tiles.len();
        window.set_vertical_preview_tiles(ModelRc::new(VecModel::from(tiles)));
        Ok((tile_count, rendered))
    }

    /// Re-cut the selection rectangles for the current viewport.
    fn refresh_visible_selection(&mut self, window: &AppWindow) -> windows::core::Result<()> {
        let Some(selection) = self.selection_utf16 else {
            return Ok(());
        };
        let visible = viewport_flow_range(window, self.engine.total_flow_size() as f32);
        let rects = self.engine.selection_rects(Some(selection), visible)?;
        set_selection_model(window, &rects);
        Ok(())
    }

    /// The same for the horizontal pane, with the flow axis on y.
    ///
    /// Deliberately a second function rather than one parameterised over the
    /// pane: the two differ only in which axis a tile is placed on and which
    /// properties are written, and the shared version of that came out harder to
    /// read than the duplication.
    fn refresh_horizontal_tiles(
        &mut self,
        window: &AppWindow,
        prefetch: u32,
    ) -> windows::core::Result<(usize, usize)> {
        // Bound once so every access below is one field short of the vertical
        // version's, and the two read the same.
        let pane = &mut self.horizontal;
        let content_height = pane.engine.total_flow_size();
        if content_height == 0 {
            return Ok((0, 0));
        }

        let viewport_y = window.get_horizontal_scroll_y();
        let visible_height = horizontal_visible_flow(window);
        let line_extent = pane.engine.line_extent();
        let desired = pane
            .engine
            .visible_tiles(viewport_y, visible_height, prefetch);

        let preedit = pane.preedit_range;
        let keyed = desired
            .iter()
            .map(|span| (*span, pane.engine.tile_signature(*span, preedit)))
            .collect::<Vec<_>>();
        let mut missing: Vec<(TileSpan, u64)> = Vec::new();
        for (span, signature) in &keyed {
            let already = pane.tiles.contains_key(signature)
                || missing.iter().any(|(_, queued)| queued == signature);
            if !already {
                missing.push((*span, *signature));
            }
        }
        let rendered = missing.len();

        let mut uploaded = 0_usize;
        if !missing.is_empty() {
            let spans = missing.iter().map(|(span, _)| *span).collect::<Vec<_>>();
            let mut produced = Vec::with_capacity(spans.len());
            pane.engine
                .render_tiles(&spans, preedit, |span, width, height, bgra| {
                    uploaded += bgra.len();
                    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                    for (target, source) in
                        pixels.make_mut_slice().iter_mut().zip(bgra.chunks_exact(4))
                    {
                        *target = Rgba8Pixel {
                            r: source[2],
                            g: source[1],
                            b: source[0],
                            a: source[3],
                        };
                    }
                    produced.push((span, Image::from_rgba8(pixels)));
                })?;
            for (span, image) in produced {
                let Some((_, signature)) = missing.iter().find(|(queued, _)| *queued == span)
                else {
                    continue;
                };
                pane.tiles.insert(
                    *signature,
                    CachedTile {
                        image,
                        last_flow: span.flow_start as i32,
                    },
                );
            }
        }

        pane.uploaded_bytes += uploaded;

        let tiles = keyed
            .iter()
            .filter_map(|(span, signature)| {
                let cached = pane.tiles.get_mut(signature)?;
                cached.last_flow = span.flow_start as i32;
                Some(PreviewTile {
                    // A horizontal tile spans the pane and is stacked on y.
                    x: 0,
                    y: span.flow_start as i32,
                    width: line_extent as i32,
                    height: span.flow_size as i32,
                    source: cached.image.clone(),
                })
            })
            .collect::<Vec<_>>();

        let viewport_center = -viewport_y + visible_height * 0.5;
        let wanted = keyed
            .iter()
            .map(|(_, signature)| *signature)
            .collect::<Vec<_>>();
        evict_distant_tiles(&mut pane.tiles, &wanted, viewport_center);

        let tile_count = tiles.len();
        window.set_horizontal_preview_tiles(ModelRc::new(VecModel::from(tiles)));
        Ok((tile_count, rendered))
    }

    /// Re-cut the horizontal pane's selection rectangles for its viewport.
    fn refresh_horizontal_selection(&mut self, window: &AppWindow) -> windows::core::Result<()> {
        let pane = &mut self.horizontal;
        let Some(selection) = pane.selection_utf16 else {
            return Ok(());
        };
        let total = pane.engine.total_flow_size() as f32;
        let visible = horizontal_viewport_flow_range(window, total);
        let rects = pane.engine.selection_rects(Some(selection), visible)?;
        set_horizontal_selection_model(window, &rects);
        Ok(())
    }
}

/// Keep the tiles the viewport wants plus the nearest others, up to the cap.
fn evict_distant_tiles(
    tiles: &mut BTreeMap<u64, CachedTile>,
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
        tiles.remove(&key);
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh_preview(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
    zoom_percent: i32,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection_source_bytes: Option<(usize, usize)>,
    preedit: &str,
) {
    let refresh_started = Instant::now();
    let font_size = font_size_for(zoom_percent);
    let height_px = preview_height(window);
    let mut borrowed = cache.borrow_mut();
    // Reborrow once so the field accesses below are disjoint. Going through
    // `RefMut` for each of them would borrow the whole cache every time.
    let cache = &mut *borrowed;

    let (render_caret, measured, previous_width, preview_ms, layout_ms) = {
        let RenderCache {
            engine,
            preview_slot,
            caret_utf16,
            selection_utf16,
            preedit_range,
            ..
        } = &mut *cache;
        // An edit invalidates the preview, so this rebuild is the one step still
        // proportional to the whole document on every keystroke.
        let preview_started = Instant::now();
        let preview = preview_slot.get(source, active_line_start);
        let caret = caret_source_byte.map(|position| preview.utf16_at_source_byte(position));
        let selection = selection_source_bytes.and_then(|(start, end)| {
            let start = preview.utf16_at_source_byte(start);
            let end = preview.utf16_at_source_byte(end);
            (start < end).then_some((start as u32, (end - start) as u32))
        });
        let (render_text, render_caret, range) = preview_with_preedit(preview, caret, preedit);
        let preview_ms = elapsed_ms(preview_started);

        let layout_started = Instant::now();
        let previous_width = engine.total_flow_size();
        let measured = match engine.update(&render_text, height_px, font_size) {
            Ok(measured) => measured,
            Err(error) => {
                window.set_render_status(format!("DirectWrite縦書き整形: NG / {error}").into());
                return;
            }
        };
        let layout_ms = elapsed_ms(layout_started);
        *caret_utf16 = render_caret;
        *selection_utf16 = selection;
        *preedit_range = range;
        (
            render_caret,
            measured,
            previous_width,
            preview_ms,
            layout_ms,
        )
    };

    let width = cache.engine.total_flow_size();
    let height = cache.engine.line_extent();
    // Anchor the document start, which in vertical text is the right edge.
    //
    // Blocks are placed right to left, so adding a column widens the content at
    // the right: text *before* the edit slides right unless the viewport slides
    // with it. Keeping the distance from the right edge constant leaves the
    // earlier text where it was and lets the later text flow leftwards, which is
    // the direction Japanese vertical text actually grows. This used to be
    // skipped whenever a caret existed, so it never ran while editing.
    if previous_width > 0 && previous_width != width {
        window.set_preview_scroll_x(scroll_after_content_resize(
            window.get_preview_scroll_x(),
            window.get_preview_visible_width().max(640.0),
            previous_width as f32,
            width as f32,
        ));
    }
    window.set_preview_width(width as i32);
    window.set_preview_height(height as i32);

    let geometry_started = Instant::now();
    let visible = viewport_flow_range(window, width as f32);
    // Both results are bound to locals first: a call left in a `match`
    // scrutinee keeps its borrow of the cache alive through every arm.
    let caret_result = match render_caret {
        Some(position) => cache.engine.caret_geometry(position).map(Some),
        None => Ok(None),
    };
    let caret = match caret_result {
        Ok(caret) => caret,
        Err(error) => {
            window.set_render_status(format!("DirectWrite座標計算: NG / {error}").into());
            update_status_with(window, cache, source, selection_source_bytes);
            return;
        }
    };
    let selection = cache.selection_utf16;
    let selection_result = cache.engine.selection_rects(selection, visible);
    let selection_rects = match selection_result {
        Ok(rects) => rects,
        Err(error) => {
            window.set_render_status(format!("DirectWrite選択座標: NG / {error}").into());
            update_status_with(window, cache, source, selection_source_bytes);
            return;
        }
    };
    let selection_rect_count = selection_rects.len();
    apply_preview_geometry(window, width as f32, caret, &selection_rects);
    let geometry_ms = elapsed_ms(geometry_started);

    // Prefetch here too. Moving the caret scrolls the pane to keep it visible,
    // and without a tile in hand on the leading edge every such scroll stalls on
    // a rasterization. Tiles whose content did not change are already cached, so
    // the neighbour usually costs nothing.
    let tiles_started = Instant::now();
    let tiles = cache.refresh_visible_tiles(window, TILE_PREFETCH_COUNT);
    let tiles_ms = elapsed_ms(tiles_started);

    // Counting lines and characters walks the source again; timed here so its
    // share of a keystroke is visible rather than assumed.
    let stats_started = Instant::now();
    update_status_with(window, cache, source, selection_source_bytes);
    let stats_ms = elapsed_ms(stats_started);

    let blocks = cache.engine.block_count();
    let (tile_count, rendered) = match tiles {
        Ok(counts) => counts,
        Err(error) => {
            window.set_render_status(format!("DirectWrite遅延タイル: NG / {error}").into());
            return;
        }
    };

    let total_ms = elapsed_ms(refresh_started);
    let push_ms = cache.source_push_ms.unwrap_or(0.0);
    // The frames these numbers describe are the ones the *previous* refresh
    // caused: Slint renders after the callback returns, not during it.
    let frame = cache.frames.borrow_mut().take();
    let upload_kb = cache.uploaded_bytes / 1024;
    cache.uploaded_bytes = 0;
    // Short enough to survive an unwrapped half-width pane. The full breakdown
    // goes to the log, where nothing is clipped.
    window.set_render_status(
        format!("縦書き {total_ms:.1}ms / tiles {tiles_ms:.1}ms {tile_count}枚{rendered}新 / 横 {push_ms:.1}ms")
            .into(),
    );
    cache.log_perf(&format!(
        "refresh total={total_ms:.2} preview={preview_ms:.2} layout={layout_ms:.2} geom={geometry_ms:.2} tiles={tiles_ms:.2} stats={stats_ms:.2} push={push_ms:.2} \
         frames={frames} frame_min={frame_min:.2} frame_med={frame_med:.2} frame_max={frame_max:.2} frame_slow={frame_slow} gap_med={gap_med:.2} upload_kb={upload_kb} \
         split={split} mode={mode} \
         blocks={blocks} measured={measured} tiles_shown={tile_count} tiles_new={rendered} rects={selection_rect_count} width={width} font={font_size:.1} preedit={}",
        preedit.chars().count(),
        // Which panes are alive. Without this the log cannot tell a slow frame
        // caused by the horizontal pane from one caused by a large document,
        // because the two arrive together.
        split = u8::from(window.get_split_view()),
        mode = window.get_editor_mode(),
        frames = frame.frames,
        frame_min = frame.min_ms,
        frame_med = frame.median_ms,
        frame_max = frame.max_ms,
        frame_slow = frame.slow,
        gap_med = frame.gap_median_ms,
    ));
}

/// Lay out and draw the horizontal pane.
///
/// The counterpart of `refresh_preview`, and shorter for one reason: this pane
/// draws the source itself, so there is no preview to rebuild and no mapping to
/// go through. What is left is the layout, the geometry and the tiles.
fn refresh_horizontal(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
    zoom_percent: i32,
    caret_source_byte: Option<usize>,
    selection_source_bytes: Option<(usize, usize)>,
    preedit: &str,
) {
    let started = Instant::now();
    let font_size = font_size_for(zoom_percent);
    let width_px = horizontal_width(window);
    let mut borrowed = cache.borrow_mut();
    let cache = &mut *borrowed;

    let caret = caret_source_byte.map(|byte| utf16_at_byte(source, byte) as u32);
    let selection = selection_source_bytes.and_then(|(start, end)| {
        let start = utf16_at_byte(source, start) as u32;
        let end = utf16_at_byte(source, end) as u32;
        (start < end).then_some((start, end - start))
    });
    let (render_text, render_caret, preedit_range) = text_with_preedit(source, caret, preedit);

    let layout_started = Instant::now();
    let pane = &mut cache.horizontal;
    let measured = match pane.engine.update(&render_text, width_px, font_size) {
        Ok(measured) => measured,
        Err(error) => {
            window.set_render_status(format!("横書き整形: NG / {error}").into());
            return;
        }
    };
    let layout_ms = elapsed_ms(layout_started);
    pane.selection_utf16 = selection;
    pane.preedit_range = preedit_range;

    // No scroll compensation, unlike the vertical pane. This document starts at
    // the top and grows downwards, so adding a line moves nothing that is
    // already above it.
    let content_flow = pane.engine.total_flow_size();
    let line_extent = pane.engine.line_extent();
    window.set_horizontal_width(line_extent as i32);
    window.set_horizontal_height(content_flow as i32);

    let geometry_started = Instant::now();
    let visible = horizontal_viewport_flow_range(window, content_flow as f32);
    // Bound to locals first: a call left in a `match` scrutinee keeps its borrow
    // of the pane alive through every arm.
    let caret_result = match render_caret {
        Some(position) => pane.engine.caret_geometry(position).map(Some),
        None => Ok(None),
    };
    let caret = match caret_result {
        Ok(caret) => caret,
        Err(error) => {
            window.set_render_status(format!("横書き座標: NG / {error}").into());
            return;
        }
    };
    let selection_result = pane.engine.selection_rects(selection, visible);
    let selection_rects = match selection_result {
        Ok(rects) => rects,
        Err(error) => {
            window.set_render_status(format!("横書き選択: NG / {error}").into());
            return;
        }
    };
    apply_horizontal_geometry(window, content_flow as f32, caret, &selection_rects);
    let geometry_ms = elapsed_ms(geometry_started);

    let tiles_started = Instant::now();
    let tiles = cache.refresh_horizontal_tiles(window, TILE_PREFETCH_COUNT);
    let tiles_ms = elapsed_ms(tiles_started);
    let (tile_count, rendered) = match tiles {
        Ok(counts) => counts,
        Err(error) => {
            window.set_render_status(format!("横書きタイル: NG / {error}").into());
            return;
        }
    };

    // Shared with the vertical pane: whichever one acted last owns the counts.
    let stats_started = Instant::now();
    update_status_with(window, cache, source, selection_source_bytes);
    let stats_ms = elapsed_ms(stats_started);

    let blocks = cache.horizontal.engine.block_count();
    let upload_kb = cache.horizontal.uploaded_bytes / 1024;
    cache.horizontal.uploaded_bytes = 0;
    // `reported` is what the pane says its height is and `visible` is what is
    // actually used. They differed once, and the difference was invisible in
    // every other figure here.
    let reported = window.get_horizontal_visible_height();
    let visible = horizontal_visible_flow(window);
    cache.log_perf(&format!(
        "horizontal total={:.2} layout={layout_ms:.2} geom={geometry_ms:.2} \
         tiles={tiles_ms:.2} stats={stats_ms:.2} blocks={blocks} \
         measured={measured} tiles_shown={tile_count} tiles_new={rendered} \
         upload_kb={upload_kb} width={width_px} height={content_flow} \
         reported={reported:.0} visible={visible:.0} font={font_size:.1}",
        elapsed_ms(started)
    ));
}

fn refresh_horizontal_from_state(
    window: &AppWindow,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
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
    refresh_horizontal(
        window,
        cache,
        source,
        window.get_zoom_percent(),
        caret_source_byte,
        selection,
        &preedit,
    );
}

/// The pane width, which is what decides how many characters fit on a line and
/// therefore the whole horizontal layout.
fn horizontal_width(window: &AppWindow) -> u32 {
    usable_horizontal_width(window.get_horizontal_visible_width())
}

fn usable_horizontal_width(width: f32) -> u32 {
    if width.is_finite() && width >= MIN_HORIZONTAL_WIDTH as f32 {
        width as u32
    } else {
        HORIZONTAL_WIDTH
    }
}

/// How far down the pane shows at once, which is what decides the tiles it needs.
///
/// Floored by the window's own height. The pane reports its size through a
/// property, and a pane created after start-up was observed never to report a
/// height at all, leaving the flow extent at its default and the lower half of
/// the pane with no tiles. A pane can never be taller than the window, so this
/// bound is always safe, and erring high only costs a tile or two: erring low
/// leaves the reader looking at blank paper.
fn horizontal_visible_flow(window: &AppWindow) -> f32 {
    let reported = window.get_horizontal_visible_height();
    let window_height = window.window().size().height as f32;
    reported.max(window_height).max(320.0)
}

/// The global y range the horizontal pane currently shows.
fn horizontal_viewport_flow_range(window: &AppWindow, total_flow: f32) -> (f32, f32) {
    visible_flow_range(
        window.get_horizontal_scroll_y(),
        horizontal_visible_flow(window),
        total_flow,
    )
}

fn set_horizontal_selection_model(window: &AppWindow, rects: &[directwrite_render::SelectionRect]) {
    let model = rects
        .iter()
        .map(|rect| PreviewSelectionRect {
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(0.0),
            height: (rect.bottom - rect.top).max(0.0),
        })
        .collect::<Vec<_>>();
    window.set_horizontal_selection_rects(ModelRc::new(VecModel::from(model)));
}

fn apply_horizontal_geometry(
    window: &AppWindow,
    content_flow: f32,
    caret: Option<directwrite_render::CaretGeometry>,
    selection_rects: &[directwrite_render::SelectionRect],
) {
    set_horizontal_selection_model(window, selection_rects);
    window.set_horizontal_caret_visible(caret.is_some());
    if let Some(caret) = caret {
        window.set_horizontal_caret_x(caret.x);
        window.set_horizontal_caret_y(caret.y);
        window.set_horizontal_caret_height(caret.height);
        let scroll_y = caret_visible_scroll(
            window.get_horizontal_scroll_y(),
            horizontal_visible_flow(window),
            content_flow,
            caret.y,
            caret.height,
        );
        window.set_horizontal_scroll_y(scroll_y);
        let (ime_x, ime_y) = ime_candidate_anchor(&caret);
        window.set_horizontal_ime_anchor_x(ime_x);
        window.set_horizontal_ime_anchor_y(ime_y);
        window.set_horizontal_ime_anchor_width(caret.width);
        window.set_horizontal_ime_anchor_height(caret.height);
    }
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

fn set_selection_model(window: &AppWindow, rects: &[directwrite_render::SelectionRect]) {
    let model = rects
        .iter()
        .map(|rect| PreviewSelectionRect {
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(0.0),
            height: (rect.bottom - rect.top).max(0.0),
        })
        .collect::<Vec<_>>();
    window.set_vertical_selection_rects(ModelRc::new(VecModel::from(model)));
}

fn apply_preview_geometry(
    window: &AppWindow,
    content_width: f32,
    caret: Option<directwrite_render::CaretGeometry>,
    selection_rects: &[directwrite_render::SelectionRect],
) {
    set_selection_model(window, selection_rects);
    window.set_preview_caret_visible(caret.is_some());
    if let Some(caret) = caret {
        window.set_preview_caret_x(caret.x);
        window.set_preview_caret_y(caret.y);
        window.set_preview_caret_width(caret.width);
        let scroll_x = caret_visible_scroll(
            window.get_preview_scroll_x(),
            window.get_preview_visible_width(),
            content_width,
            caret.x,
            caret.width,
        );
        window.set_preview_scroll_x(scroll_x);
        let (ime_x, ime_y) = ime_candidate_anchor(&caret);
        window.set_ime_anchor_x(ime_x);
        window.set_ime_anchor_y(ime_y);
        window.set_ime_anchor_width(caret.width);
        window.set_ime_anchor_height(caret.height);
    }
}

fn update_status(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
    selection_source_bytes: Option<(usize, usize)>,
) {
    let mut cache = cache.borrow_mut();
    update_status_with(window, &mut cache, source, selection_source_bytes);
}

fn update_status_with(
    window: &AppWindow,
    cache: &mut RenderCache,
    source: &str,
    selection_source_bytes: Option<(usize, usize)>,
) {
    let stats = cache.stats_slot.get(source);
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
    window.set_status_text(
        format!(
            "{}行 | 本文 {}文字 | ソース {}文字 | 選択 {}文字",
            stats.logical_lines,
            stats.body_characters,
            stats.source_characters,
            selected_characters
        )
        .into(),
    );
}

fn ime_candidate_anchor(caret: &directwrite_render::CaretGeometry) -> (f32, f32) {
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

fn preview_with_preedit<'a>(
    preview: &'a PreviewDocument,
    caret_utf16: Option<usize>,
    preedit: &str,
) -> (Cow<'a, str>, Option<u32>, Option<(u32, u32)>) {
    let Some(caret) = caret_utf16 else {
        return (Cow::Borrowed(&preview.text), None, None);
    };
    if preedit.is_empty() {
        return (Cow::Borrowed(&preview.text), Some(caret as u32), None);
    }

    let mut text = preview.text.clone();
    text.insert_str(preview.preview_byte_at_utf16(caret), preedit);
    let preedit_length = preedit.encode_utf16().count() as u32;
    (
        Cow::Owned(text),
        Some(caret as u32 + preedit_length),
        Some((caret as u32, preedit_length)),
    )
}

fn current_source_caret(state: &Rc<RefCell<EditorState>>, source: &str) -> (usize, usize) {
    let state = state.borrow();
    let source_byte = state
        .caret_source_byte
        .unwrap_or(source.len())
        .min(source.len());
    let active_line_start = state
        .active_line_start
        .unwrap_or_else(|| source_line_start(source, source_byte));
    (active_line_start, source_byte)
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

fn update_vertical_selection(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    x: f32,
    y: f32,
    phase: SelectionPhase,
) {
    let drag_started = Instant::now();
    let source = document.borrow().clone();
    let active_line_start = state.borrow().active_line_start;
    let zoom = window.get_zoom_percent();

    // A drag reuses the cached preview, the cached block measurements and the
    // cached layouts: nothing here walks the document.
    let hit = {
        let mut cache = cache.borrow_mut();
        let RenderCache {
            engine,
            preview_slot,
            ..
        } = &mut *cache;
        let preview = preview_slot.get(&source, active_line_start);
        if let Err(error) =
            engine.update(&preview.text, preview_height(&window), font_size_for(zoom))
        {
            window.set_render_status(format!("DirectWrite縦書き整形: NG / {error}").into());
            return;
        }
        match engine.hit_test(x, y) {
            Ok(hit) => preview.source_byte_at_utf16(hit.utf16_position as usize),
            Err(error) => {
                window.set_render_status(format!("DirectWriteヒットテスト: NG / {error}").into());
                return;
            }
        }
    };

    let next_active_line_start = source_line_start(&source, hit);
    let selection = {
        let mut state = state.borrow_mut();
        if phase == SelectionPhase::Begin || state.selection_anchor_source_byte.is_none() {
            state.selection_anchor_source_byte = Some(hit);
        }
        state.caret_source_byte = Some(hit);
        if phase != SelectionPhase::Update {
            state.active_line_start = Some(next_active_line_start);
        }
        state.preedit.clear();
        state.preferred_line = None;
        selection_source_range(&state)
    };
    window.set_ime_buffer("".into());

    if phase != SelectionPhase::Update {
        refresh_preview(
            window,
            cache,
            &source,
            zoom,
            Some(next_active_line_start),
            Some(hit),
            selection,
            "",
        );
        return;
    }

    // Mid-drag the text has not changed, so no tile is regenerated: only the
    // caret and the on-screen part of the selection are hit tested again.
    let mut borrowed = cache.borrow_mut();
    let cache = &mut *borrowed;
    let (caret_utf16, selection_utf16) = {
        let RenderCache {
            preview_slot,
            caret_utf16,
            selection_utf16,
            ..
        } = &mut *cache;
        let preview = preview_slot.get(&source, active_line_start);
        let caret = preview.utf16_at_source_byte(hit) as u32;
        let range = selection.and_then(|(start, end)| {
            let start = preview.utf16_at_source_byte(start);
            let end = preview.utf16_at_source_byte(end);
            (start < end).then_some((start as u32, (end - start) as u32))
        });
        *caret_utf16 = Some(caret);
        *selection_utf16 = range;
        (caret, range)
    };

    let content_width = cache.engine.total_flow_size() as f32;
    let visible = viewport_flow_range(window, content_width);
    // Bound to locals first: a method call inside a `match` scrutinee would hold
    // its borrow of the cache for the whole match.
    let caret = cache.engine.caret_geometry(caret_utf16);
    let rects = cache.engine.selection_rects(selection_utf16, visible);
    match (caret, rects) {
        (Ok(caret), Ok(rects)) => {
            let rect_count = rects.len();
            apply_preview_geometry(window, content_width, Some(caret), &rects);
            update_status_with(window, cache, &source, selection);
            // Nothing reported the mid-drag cost before, which is exactly the
            // path the slowness was reported on.
            window.set_render_status(
                format!(
                    "縦書きドラッグ選択: {rect_count} rects / {:.1}ms",
                    elapsed_ms(drag_started)
                )
                .into(),
            );
        }
        (Err(error), _) | (_, Err(error)) => {
            window.set_render_status(format!("DirectWrite選択座標: NG / {error}").into())
        }
    }
}

/// The rendered text for a pane that draws the source verbatim, with any IME
/// pre-edit spliced in at the caret.
///
/// The pre-edit never reaches the document: it exists only in what is drawn,
/// exactly as on the vertical side.
fn text_with_preedit<'a>(
    text: &'a str,
    caret_utf16: Option<u32>,
    preedit: &str,
) -> (Cow<'a, str>, Option<u32>, Option<(u32, u32)>) {
    let Some(caret) = caret_utf16 else {
        return (Cow::Borrowed(text), None, None);
    };
    if preedit.is_empty() {
        return (Cow::Borrowed(text), Some(caret), None);
    }

    let mut rendered = text.to_owned();
    rendered.insert_str(byte_at_utf16(text, caret as usize), preedit);
    let preedit_length = preedit.encode_utf16().count() as u32;
    (
        Cow::Owned(rendered),
        Some(caret + preedit_length),
        Some((caret, preedit_length)),
    )
}

/// Hit test the horizontal pane and move its caret and selection there.
fn update_horizontal_selection(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    x: f32,
    y: f32,
    phase: SelectionPhase,
) {
    let drag_started = Instant::now();
    let source = document.borrow().clone();
    let zoom = window.get_zoom_percent();

    // A drag reuses the block measurements and layouts already in hand: the text
    // has not changed, so nothing here re-measures.
    let hit = {
        let mut cache = cache.borrow_mut();
        let engine = &mut cache.horizontal.engine;
        if let Err(error) = engine.update(&source, horizontal_width(window), font_size_for(zoom)) {
            window.set_render_status(format!("横書き整形: NG / {error}").into());
            return;
        }
        match engine.hit_test(x, y) {
            Ok(hit) => byte_at_utf16(&source, hit.utf16_position as usize),
            Err(error) => {
                window.set_render_status(format!("横書きヒット: NG / {error}").into());
                return;
            }
        }
    };

    let selection = {
        let mut state = state.borrow_mut();
        if phase == SelectionPhase::Begin || state.selection_anchor_source_byte.is_none() {
            state.selection_anchor_source_byte = Some(hit);
        }
        state.caret_source_byte = Some(hit);
        state.preedit.clear();
        state.preferred_line = None;
        selection_source_range(&state)
    };
    window.set_horizontal_ime_buffer("".into());

    refresh_horizontal(window, cache, &source, zoom, Some(hit), selection, "");
    if phase == SelectionPhase::Update {
        window.set_render_status(
            format!("横書きドラッグ選択: {:.1}ms", elapsed_ms(drag_started)).into(),
        );
    }
}

/// Insert text at the horizontal pane's caret, replacing its selection.
fn insert_horizontal_text(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    vertical_state: &Rc<RefCell<EditorState>>,
    text: &str,
) {
    let input = normalize_typed_input(text);
    if input.is_empty() {
        return;
    }
    window.set_horizontal_ime_buffer("".into());
    let mut source = document.borrow().clone();
    let caret = horizontal_caret_byte(state, &source);
    let selection = selection_source_range(&state.borrow());
    let next_source_byte = match selection {
        Some(range) => replace_source_range(&mut source, range, &input),
        None => {
            source.insert_str(caret, &input);
            caret + input.len()
        }
    };
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(next_source_byte);
        state.selection_anchor_source_byte = Some(next_source_byte);
        state.preedit.clear();
        state.preferred_line = None;
    }
    *document.borrow_mut() = source.clone();
    refresh_horizontal(
        window,
        cache,
        &source,
        window.get_zoom_percent(),
        Some(next_source_byte),
        None,
        "",
    );
    refresh_vertical_after_horizontal_edit(window, vertical_state, cache, &source);
}

/// Delete the grapheme beside the horizontal caret, or its selection.
fn edit_adjacent_grapheme_horizontal(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    vertical_state: &Rc<RefCell<EditorState>>,
    backward: bool,
) {
    let mut source = document.borrow().clone();
    let caret = horizontal_caret_byte(state, &source);
    let (start, end) = match selection_source_range(&state.borrow()) {
        Some(range) => range,
        None if backward => (previous_grapheme_byte(&source, caret), caret),
        None => (caret, next_grapheme_byte(&source, caret)),
    };
    if start >= end {
        return;
    }

    let next_source_byte = replace_source_range(&mut source, (start, end), "");
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(next_source_byte);
        state.selection_anchor_source_byte = Some(next_source_byte);
        state.preferred_line = None;
    }
    *document.borrow_mut() = source.clone();
    refresh_horizontal(
        window,
        cache,
        &source,
        window.get_zoom_percent(),
        Some(next_source_byte),
        None,
        "",
    );
    refresh_vertical_after_horizontal_edit(window, vertical_state, cache, &source);
}

/// Move the horizontal caret: one grapheme sideways, or one line up or down.
///
/// `direction` follows the vertical pane's convention — ±1 steps a grapheme and
/// ±2 steps a line — so both panes hand the engine the same kind of number and
/// the engine decides what "the next line" means for its own writing direction.
fn move_horizontal_caret(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    direction: i32,
    extend_selection: bool,
) {
    let source = document.borrow().clone();
    let caret = horizontal_caret_byte(state, &source);
    let caret_utf16 = utf16_at_byte(&source, caret) as u32;
    let zoom = window.get_zoom_percent();
    let preferred_line = state.borrow().preferred_line;

    let moved = {
        let mut borrowed = cache.borrow_mut();
        let engine = &mut borrowed.horizontal.engine;
        if let Err(error) = engine.update(&source, horizontal_width(window), font_size_for(zoom)) {
            window.set_render_status(format!("横書き整形: NG / {error}").into());
            return;
        }
        match direction {
            -1 => Ok((previous_grapheme_byte(&source, caret), None)),
            1 => Ok((next_grapheme_byte(&source, caret), None)),
            -2 | 2 => {
                let anchor = match preferred_line {
                    Some(x) => Ok(x),
                    None => engine.caret_geometry(caret_utf16).map(|caret| caret.x),
                };
                anchor.and_then(|x| {
                    engine
                        .move_caret_by_line(caret_utf16, direction, Some(x))
                        .map(|hit| {
                            let byte = byte_at_utf16(&source, hit.utf16_position as usize);
                            (byte, Some(x))
                        })
                })
            }
            _ => Ok((caret, None)),
        }
    };

    let (next_source_byte, next_preferred) = match moved {
        Ok(moved) => moved,
        Err(error) => {
            window.set_render_status(format!("横書き移動: NG / {error}").into());
            return;
        }
    };
    let selection = {
        let mut state = state.borrow_mut();
        let selection =
            update_selection_after_move(&mut state, caret, next_source_byte, extend_selection);
        state.preferred_line = next_preferred;
        selection
    };
    refresh_horizontal(
        window,
        cache,
        &source,
        zoom,
        Some(next_source_byte),
        selection,
        "",
    );
}

/// `Home` and `End` in the horizontal pane, and their `Ctrl` forms.
fn move_horizontal_to_line_edge(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    to_end: bool,
    document_edge: bool,
    extend_selection: bool,
) {
    let source = document.borrow().clone();
    let caret = horizontal_caret_byte(state, &source);
    let zoom = window.get_zoom_percent();

    let next_source_byte = if document_edge {
        if to_end { source.len() } else { 0 }
    } else {
        let mut borrowed = cache.borrow_mut();
        let engine = &mut borrowed.horizontal.engine;
        if let Err(error) = engine.update(&source, horizontal_width(window), font_size_for(zoom)) {
            window.set_render_status(format!("横書き整形: NG / {error}").into());
            return;
        }
        // Line edges come from the cached line metrics, so this costs nothing.
        let edge = engine.move_caret_to_line_edge(utf16_at_byte(&source, caret) as u32, to_end);
        byte_at_utf16(&source, edge as usize)
    };

    let selection = {
        let mut state = state.borrow_mut();
        let selection =
            update_selection_after_move(&mut state, caret, next_source_byte, extend_selection);
        state.preferred_line = None;
        selection
    };
    refresh_horizontal(
        window,
        cache,
        &source,
        zoom,
        Some(next_source_byte),
        selection,
        "",
    );
}

/// The horizontal caret, clamped into the current document.
fn horizontal_caret_byte(state: &Rc<RefCell<EditorState>>, source: &str) -> usize {
    let byte = state.borrow().caret_source_byte.unwrap_or(source.len());
    floor_char_boundary(source, byte)
}

/// Push an edit made in one pane into the other.
///
/// The two panes hold their own carets over one shared document, so an edit on
/// either side leaves the other's caret where it was and only re-lays it out.
fn refresh_vertical_after_horizontal_edit(
    window: &AppWindow,
    vertical_state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    source: &str,
) {
    // Clamped first, and whether or not the pane is on screen: see
    // `push_source_to_horizontal`.
    clamp_state_into(vertical_state, source);
    if !vertical_view_visible(window) {
        update_status(window, cache, source, None);
        return;
    }
    refresh_preview_from_state(window, vertical_state, cache, source);
}

/// Keep a pane's stored positions inside a document the other pane changed.
///
/// Both the length and the character boundaries can have moved under them, and
/// an unclamped position is what every slice downstream panics on.
fn clamp_state_into(state: &Rc<RefCell<EditorState>>, source: &str) {
    let mut state = state.borrow_mut();
    let clamp = |byte: usize| floor_char_boundary(source, byte);
    state.caret_source_byte = state.caret_source_byte.map(clamp);
    state.selection_anchor_source_byte = state.selection_anchor_source_byte.map(clamp);
    state.active_line_start = state
        .caret_source_byte
        .map(|caret| source_line_start(source, caret));
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

fn insert_vertical_text(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    horizontal_state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    text: &str,
    indent_line_start: bool,
) {
    let input = normalize_typed_input(text);
    if input.is_empty() {
        return;
    }
    window.set_ime_buffer("".into());
    let mut source = document.borrow().clone();
    let (active_line_start, current_source_byte) = current_source_caret(state, &source);
    let selection = selection_source_range(&state.borrow());
    let source_byte = if let Some((start, end)) = selection {
        replace_source_range(&mut source, (start, end), &input);
        start
    } else {
        let mut cache = cache.borrow_mut();
        let preview = cache.preview_slot.get(&source, Some(active_line_start));
        let caret = preview.utf16_at_source_byte(current_source_byte);
        let source_byte =
            vertical_insertion_source_byte(&source, preview, caret, indent_line_start);
        drop(cache);
        source.insert_str(source_byte, &input);
        source_byte
    };
    let next_source_byte = source_byte + input.len();
    let next_active_line_start = source_line_start(&source, next_source_byte);
    {
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(next_source_byte);
        state.selection_anchor_source_byte = Some(next_source_byte);
        state.active_line_start = Some(next_active_line_start);
        state.preedit.clear();
        state.preferred_line = None;
    }
    *document.borrow_mut() = source.clone();
    push_source_to_horizontal(window, horizontal_state, cache, &source);
    refresh_preview(
        window,
        cache,
        &source,
        window.get_zoom_percent(),
        Some(next_active_line_start),
        Some(next_source_byte),
        None,
        "",
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

fn edit_adjacent_grapheme(
    window: &AppWindow,
    document: &Rc<RefCell<String>>,
    state: &Rc<RefCell<EditorState>>,
    horizontal_state: &Rc<RefCell<EditorState>>,
    cache: &Rc<RefCell<RenderCache>>,
    backward: bool,
) {
    let mut source = document.borrow().clone();
    let (active_line_start, current_source_byte) = current_source_caret(state, &source);
    let selected_source_range = selection_source_range(&state.borrow());
    let (source_start, source_end) = if let Some(range) = selected_source_range {
        range
    } else {
        let mut cache = cache.borrow_mut();
        let preview = cache.preview_slot.get(&source, Some(active_line_start));
        let caret = preview.utf16_at_source_byte(current_source_byte);
        let adjacent = if backward {
            preview.previous_grapheme_position(caret)
        } else {
            preview.next_grapheme_position(caret)
        };
        let (start, end) = if backward {
            (adjacent, caret)
        } else {
            (caret, adjacent)
        };
        (
            preview.source_byte_at_utf16(start),
            preview.source_byte_at_utf16(end),
        )
    };

    if source_start < source_end {
        let next_source_byte = replace_source_range(&mut source, (source_start, source_end), "");
        let next_active_line_start = source_line_start(&source, next_source_byte);
        {
            let mut state = state.borrow_mut();
            state.caret_source_byte = Some(next_source_byte);
            state.selection_anchor_source_byte = Some(next_source_byte);
            state.active_line_start = Some(next_active_line_start);
            state.preferred_line = None;
        }
        *document.borrow_mut() = source.clone();
        push_source_to_horizontal(window, horizontal_state, cache, &source);
        refresh_preview(
            window,
            cache,
            &source,
            window.get_zoom_percent(),
            Some(next_active_line_start),
            Some(next_source_byte),
            None,
            "",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_for(text: &str, zoom: i32) -> TextEngine {
        let mut engine = TextEngine::default();
        engine
            .update(text, PREVIEW_HEIGHT, font_size_for(zoom))
            .expect("vertical layout");
        engine
    }

    fn engine_for_height(text: &str, height: u32) -> TextEngine {
        let mut engine = TextEngine::default();
        engine
            .update(text, height, font_size_for(100))
            .expect("vertical layout");
        engine
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

    #[test]
    fn renders_only_the_selected_view_unless_split_is_enabled() {
        assert_eq!(view_visibility(HORIZONTAL_MODE, false), (true, false));
        assert_eq!(view_visibility(VERTICAL_MODE, false), (false, true));
        assert_eq!(view_visibility(HORIZONTAL_MODE, true), (true, true));
        assert_eq!(view_visibility(VERTICAL_MODE, true), (true, true));
    }

    #[test]
    fn inserts_preedit_only_into_the_rendered_preview() {
        let preview = PreviewDocument::from_source("# 見出し");

        let (text, caret, range) = preview_with_preedit(&preview, Some(2), "変換");

        assert_eq!(preview.text, "見出し");
        assert_eq!(text, "見出変換し");
        assert_eq!(caret, Some(4));
        assert_eq!(range, Some((2, 2)));
    }

    #[test]
    fn borrows_the_preview_when_there_is_no_preedit() {
        let preview = PreviewDocument::from_source("# 見出し\n本文");

        let (text, _, range) = preview_with_preedit(&preview, Some(1), "");

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

        let (rendered, render_caret, range) = text_with_preedit(source, Some(caret), "かん");

        assert_eq!(rendered, "本文かんです");
        assert_eq!(
            render_caret,
            Some(caret + 2),
            "the caret follows the preedit"
        );
        assert_eq!(range, Some((caret, 2)));
        assert_eq!(source, "本文です", "the document itself is untouched");

        let (borrowed, _, none) = text_with_preedit(source, Some(caret), "");
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
    fn offsets_the_ime_candidate_anchor_right_and_below_the_vertical_caret() {
        let caret = directwrite_render::CaretGeometry {
            x: 100.0,
            y: 64.0,
            width: 22.0,
            height: 22.0,
        };

        assert_eq!(ime_candidate_anchor(&caret), (130.0, 94.0));
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

    #[test]
    fn evicts_the_tiles_furthest_from_the_viewport() {
        let mut tiles = tile_map(10, 1024);

        evict_distant_tiles(&mut tiles, &[5, 6], 5600.0);

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

        evict_distant_tiles(&mut tiles, &wanted, 1024.0);

        for key in &wanted {
            assert!(tiles.contains_key(key), "dropped a wanted tile {key}");
        }
    }

    #[test]
    fn keeps_a_tile_that_is_still_wanted_even_when_far_from_the_centre() {
        let mut tiles = tile_map(10, 1024);

        evict_distant_tiles(&mut tiles, &[0], 8000.0);

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
            (after.y - before.y).abs() <= font_size_for(100),
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
            .update(&text, PREVIEW_HEIGHT, font_size_for(100))
            .expect("repeat update");

        assert_eq!(measured, 0);
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
