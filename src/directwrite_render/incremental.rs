//! E17: foreground prefixes and a latest-only, cancellable layout worker.
//! Only owned text and numeric layout data cross the thread boundary.
use super::*;
use std::sync::{
    Condvar,
    atomic::{AtomicU64, Ordering},
};

const WINDOW_CHARACTERS: usize = 2048;
const FOREGROUND_LOOKAHEAD: usize = 2048;
const FOREGROUND_BUDGET: Duration = Duration::from_millis(8);

#[derive(Clone)]
pub(super) struct Cancellation {
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl Cancellation {
    fn check(&self) -> Result<()> {
        if self.generation.load(Ordering::Relaxed) == self.expected {
            Ok(())
        } else {
            Err(Error::new(E_FAIL, "superseded layout"))
        }
    }
}

struct Job {
    generation: u64,
    mode: WritingMode,
    text: String,
    styles: Vec<LineStyle>,
    marks: Vec<Vec<Emphasis>>,
    markers: Vec<Option<LineMarker>>,
    source_line: Option<usize>,
    fit: LineFit,
    typography: Typography,
    wraps: Vec<ParagraphWraps>,
    measures: HashMap<u64, MeasuredBlock>,
}

struct Answer {
    generation: u64,
    result: std::result::Result<Completed, String>,
}

struct Completed {
    plan: BlockLayoutPlan,
    wraps: Vec<ParagraphWraps>,
    measures: HashMap<u64, MeasuredBlock>,
    block_lines: Vec<Range<usize>>,
    page_extent: u32,
    margin: f32,
    numbers: Option<NumberColumn>,
    wrapping_items: usize,
}

#[derive(Default)]
struct Mailbox {
    job: Option<Job>,
    answer: Option<Answer>,
    stopped: bool,
}

pub(super) struct BackgroundLayout {
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    generation: Arc<AtomicU64>,
    pending: bool,
}

impl Drop for BackgroundLayout {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        let mut mailbox = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.stopped = true;
        mailbox.job = None;
        self.mailbox.1.notify_one();
        // No join on the UI thread. The active job checks cancellation between
        // bounded DirectWrite calls and then releases its own COM resources.
    }
}

impl BackgroundLayout {
    fn new() -> Option<Self> {
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let shared = mailbox.clone();
        let versions = generation.clone();
        thread::Builder::new()
            .name("editor-layout-tail".into())
            .spawn(move || {
                loop {
                    let job = {
                        let mut slot = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        while slot.job.is_none() && !slot.stopped {
                            slot = shared.1.wait(slot).unwrap_or_else(|e| e.into_inner());
                        }
                        if slot.stopped {
                            break;
                        }
                        slot.job.take().unwrap()
                    };
                    let token = Cancellation {
                        generation: versions.clone(),
                        expected: job.generation,
                    };
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut engine = TextEngine::new(job.mode);
                        engine.fit = job.fit;
                        engine.typography = job.typography.clone();
                        engine.wraps = job.wraps;
                        engine.measures = job.measures;
                        engine.work_cancel = Some(token.clone());
                        let styled = StyledText::marked(&job.text, &job.styles, &job.marks)
                            .with_markers(&job.markers)
                            .with_source_line(job.source_line);
                        engine.update_inner(styled, job.fit, &job.typography, None)?;
                        token.check()?;
                        Ok::<_, Error>(Completed {
                            plan: engine.plan,
                            wraps: engine.wraps,
                            measures: engine.measures,
                            block_lines: engine.block_lines,
                            page_extent: engine.page_extent,
                            margin: engine.margin,
                            numbers: engine.numbers,
                            wrapping_items: engine.wrapping_items,
                        })
                    }))
                    .map_err(|_| "layout worker panicked".to_owned())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                    if token.check().is_ok() {
                        let mut slot = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        if !slot.stopped && token.check().is_ok() {
                            slot.answer = Some(Answer {
                                generation: job.generation,
                                result,
                            });
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            mailbox,
            generation,
            pending: false,
        })
    }

    fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.pending = false;
        let mut slot = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.job = None;
        slot.answer = None;
    }
}

impl TextEngine {
    pub(super) fn check_cancelled(&self) -> Result<()> {
        self.work_cancel
            .as_ref()
            .map_or(Ok(()), Cancellation::check)
    }

    pub(super) fn cancel_background(&mut self) {
        if let Some(worker) = &mut self.background {
            worker.cancel();
        }
    }

    pub fn layout_pending(&self) -> bool {
        !self.deferred_blocks.is_empty()
    }

    pub fn position_ready(&self, at: u32) -> bool {
        !self.deferred_blocks.contains(&self.plan.block_at_utf16(at))
    }

    pub fn point_ready(&self, x: f32, y: f32) -> bool {
        let (flow, _) = self.mode.to_axes(x, y);
        !self
            .deferred_blocks
            .contains(&self.plan.block_at_flow(flow))
    }

    pub fn layout_ready(&self) -> bool {
        if self.layout_pending() && self.background.is_none() {
            return true;
        }
        self.background.as_ref().is_some_and(|worker| {
            worker
                .mailbox
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .answer
                .is_some()
        })
    }

    pub fn viewport_anchor(&mut self, scroll: f32, extent: f32) -> Option<(u32, f32)> {
        let visible =
            crate::text_blocks::visible_flow_range(scroll, extent, self.plan.flow_bounds());
        let at = self
            .plan
            .blocks
            .iter()
            .enumerate()
            .find(|(index, block)| {
                !self.deferred_blocks.contains(index)
                    && block.flow_start < visible.1
                    && block.flow_start + block.flow_size > visible.0
            })
            .map(|(_, block)| block.span.utf16_start);
        let Some(at) = at else {
            // No text has been shown here yet. Preserve the requested location
            // in the estimated tail; it is never used as a mouse hit result.
            let flow = if self.mode == WritingMode::Vertical {
                visible.1
            } else {
                visible.0
            };
            let block = self.plan.blocks.get(self.plan.block_at_flow(flow))?;
            let fraction = ((flow - block.flow_start) / block.flow_size.max(1.0)).clamp(0.0, 1.0);
            let fraction = if self.mode == WritingMode::Vertical {
                1.0 - fraction
            } else {
                fraction
            };
            let approximate =
                block.span.utf16_start + (block.span.utf16_len() as f32 * fraction) as u32;
            let byte = byte_at_utf16(&self.text, approximate);
            let at = self.text[..byte].encode_utf16().count() as u32;
            return Some((at, flow + scroll));
        };
        let geometry = self.caret_geometry(at).ok()?;
        let flow = if self.mode == WritingMode::Vertical {
            geometry.x
        } else {
            geometry.y
        };
        Some((at, flow + scroll))
    }

    /// Scrolling into a pending tail asks for another bounded prefix now and
    /// replaces the background request with its more advanced snapshot.
    pub fn prepare_viewport(&mut self, scroll: f32, extent: f32) -> Result<()> {
        if !self.layout_pending() {
            return Ok(());
        }
        let visible =
            crate::text_blocks::visible_flow_range(scroll, extent, self.plan.flow_bounds());
        let needed = self
            .deferred_blocks
            .iter()
            .filter_map(|index| self.plan.blocks.get(*index))
            .filter(|block| {
                block.flow_start < visible.1 && block.flow_start + block.flow_size > visible.0
            })
            .map(|block| {
                block
                    .span
                    .utf16_start
                    .saturating_add(FOREGROUND_LOOKAHEAD as u32)
            })
            .max();
        let Some(through) = needed else {
            return Ok(());
        };
        let text = self.text.clone();
        let styles = self.line_styles.clone();
        let marks = self.line_spans.clone();
        let markers = self.line_markers.clone();
        let typography = self.typography.clone();
        let styled = StyledText::marked(&text, &styles, &marks)
            .with_markers(&markers)
            .with_source_line(self.source_line);
        self.update_interactive(styled, self.fit, &typography, through)?;
        Ok(())
    }

    /// Return an old-view text position, not an absolute pixel coordinate:
    /// vertical content changes its origin as the estimated tail changes size.
    pub fn viewport_end_utf16(&self, scroll: f32, extent: f32) -> u32 {
        let visible =
            crate::text_blocks::visible_flow_range(scroll, extent, self.plan.flow_bounds());
        self.plan
            .blocks
            .iter()
            .filter(|block| {
                block.flow_start < visible.1 && block.flow_start + block.flow_size > visible.0
            })
            .filter(|block| {
                !self
                    .deferred_blocks
                    .contains(&self.plan.block_at_utf16(block.span.utf16_start))
            })
            .map(|block| block.span.utf16_end)
            .max()
            .unwrap_or(0)
    }

    pub fn update_interactive(
        &mut self,
        styled: StyledText<'_>,
        fit: LineFit,
        typography: &Typography,
        through_utf16: u32,
    ) -> Result<UpdateCost> {
        if fit == LineFit::Free {
            return self.update(styled, fit, typography);
        }
        let same = self.matches(styled, fit, typography);
        if same && self.layout_ready() && self.background.is_some() {
            let worker = self.background.as_mut().unwrap();
            let answer = worker
                .mailbox
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .answer
                .take();
            if let Some(answer) = answer {
                if answer.generation == worker.generation.load(Ordering::Relaxed) {
                    worker.pending = false;
                    match answer.result {
                        Ok(done) => {
                            self.plan = done.plan;
                            self.wraps = done.wraps;
                            self.measures = done.measures;
                            self.block_lines = done.block_lines;
                            self.page_extent = done.page_extent;
                            self.margin = done.margin;
                            self.numbers = done.numbers;
                            self.wrapping_items = done.wrapping_items;
                            self.deferred_blocks.clear();
                            self.layouts.clear();
                            return Ok(UpdateCost::default());
                        }
                        Err(_) => {
                            // Retry in bounded foreground windows on subsequent
                            // refreshes; never synchronously redo the whole tail.
                            self.background = None;
                        }
                    }
                }
            }
        }
        if same && !self.layout_pending() {
            return Ok(UpdateCost::default());
        }
        let target = byte_at_utf16(styled.text, through_utf16);
        if same
            && self
                .background
                .as_ref()
                .is_some_and(|worker| worker.pending)
            && !self
                .deferred_blocks
                .iter()
                .any(|index| self.plan.blocks[*index].span.byte_start <= target)
        {
            return Ok(UpdateCost::default());
        }
        self.cancel_background();
        let limit = advance_characters(styled.text, target, FOREGROUND_LOOKAHEAD);
        let cost = self.update_inner(styled, fit, typography, Some(limit))?;
        if self.layout_pending() {
            if self.background.is_none() {
                self.background = BackgroundLayout::new();
            }
            if let Some(worker) = &mut self.background {
                let generation = worker.generation.fetch_add(1, Ordering::Relaxed) + 1;
                let job = Job {
                    generation,
                    mode: self.mode,
                    text: self.text.clone(),
                    styles: self.line_styles.clone(),
                    marks: self.line_spans.clone(),
                    markers: self.line_markers.clone(),
                    source_line: self.source_line,
                    fit: self.fit,
                    typography: self.typography.clone(),
                    wraps: self.wraps.clone(),
                    measures: self.measures.clone(),
                };
                worker.pending = true;
                worker
                    .mailbox
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .job = Some(job);
                worker.mailbox.1.notify_one();
            }
        }
        Ok(cost)
    }
}

fn byte_at_utf16(text: &str, target: u32) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= target {
            return byte;
        }
        units += ch.len_utf16() as u32;
    }
    text.len()
}

fn advance_characters(text: &str, from: usize, count: usize) -> usize {
    text[from..]
        .char_indices()
        .nth(count)
        .map_or(text.len(), |(at, _)| from + at)
}

pub(super) fn deferred_ranges(_text: &str, wraps: &[ParagraphWraps]) -> Vec<Range<usize>> {
    wraps
        .iter()
        .filter(|wrap| !wrap.complete)
        .map(|wrap| {
            wrap.byte_start + wrap.starts.last().copied().unwrap_or(0)
                ..wrap.byte_start + wrap.text.len()
        })
        .collect()
}

pub(super) fn estimate(span: &BlockSpan, cells: u32, typography: &Typography) -> BlockMeasure {
    let rows = (span.utf16_len() as f32 / cells.max(1) as f32)
        .ceil()
        .max(1.0);
    let flow = rows * typography.font_size * typography.line_spacing.max(0.1);
    BlockMeasure {
        flow_size: flow,
        line_reach: 0.0,
        content_flow_start: 0.0,
        max_flow_size: flow.max(1.0),
        lines: Arc::from([]),
        grid: None,
    }
}

/// Only complete lines before the lookahead margin are returned. A partial
/// result is never cached as a complete paragraph, even when its text matches.
pub(super) fn wrap_prefix(
    graphics: &mut Graphics,
    format: &IDWriteTextFormat,
    page: &WrapPage,
    line: LongLine<'_>,
    from: usize,
    stop: Option<usize>,
    cancel: Option<&Cancellation>,
) -> Result<(Vec<usize>, bool, u32)> {
    if stop.is_none() && cancel.is_none() {
        return wrap_offsets(graphics, format, page, line, from).map(|offsets| {
            (
                offsets,
                true,
                line.text[from..].encode_utf16().count() as u32,
            )
        });
    }
    if stop.is_some()
        && line.text[from..]
            .chars()
            .take(WINDOW_CHARACTERS * 2 + 1)
            .count()
            <= WINDOW_CHARACTERS * 2
    {
        return wrap_offsets(graphics, format, page, line, from).map(|offsets| {
            (
                offsets,
                true,
                line.text[from..].encode_utf16().count() as u32,
            )
        });
    }
    let mut starts = Vec::new();
    let mut measured = 0;
    let mut windows = 0;
    let started = std::time::Instant::now();
    let mut start = from;
    while start < line.text.len() {
        if let Some(token) = cancel {
            token.check()?;
        }
        let end = advance_characters(line.text, start, WINDOW_CHARACTERS);
        measured += line.text[start..end].encode_utf16().count() as u32;
        windows += 1;
        let found = wrap_offsets_in(graphics, format, page, line, start, end)?;
        let whole = end == line.text.len();
        let usable = if whole {
            found.len()
        } else {
            found.len().saturating_sub(WRAP_REUSE_MARGIN)
        };
        if !whole && usable == 0 {
            // A line may contain an unbreakable run. Do not invent a boundary.
            // Let the full worker use the established full-layout fallback.
            if stop.is_some() {
                return Ok((starts, false, measured));
            }
            let rest = wrap_offsets(graphics, format, page, line, start)?;
            measured += line.text[start..].encode_utf16().count() as u32;
            starts.extend(rest);
            return Ok((starts, true, measured));
        }
        starts.extend(found[..usable].iter().map(|offset| start + offset));
        if whole {
            return Ok((starts, true, measured));
        }
        start += found[usable - 1];
        if stop.is_some_and(|stop| {
            start >= stop || windows >= 2 || started.elapsed() >= FOREGROUND_BUDGET
        }) {
            return Ok((starts, false, measured));
        }
    }
    Ok((starts, true, measured))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn finish(engine: &mut TextEngine, text: &str, typography: &Typography) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.layout_pending() {
            assert!(
                Instant::now() < deadline,
                "background layout did not finish"
            );
            if engine.layout_ready() {
                engine
                    .update_interactive(
                        StyledText::plain(text),
                        LineFit::Extent(700),
                        typography,
                        0,
                    )
                    .unwrap();
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    #[test]
    fn foreground_is_bounded_and_background_matches_full_layout() {
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let typography = Typography::default();
            let text = "日本語ABCと句読点、括弧（かっこ）を含む文章。".repeat(2000);
            let mut engine = TextEngine::new(mode);
            engine
                .update(StyledText::plain(&text), LineFit::Extent(700), &typography)
                .unwrap();
            for at in [0, text.len() / 2 / 3 * 3] {
                let at = (0..=at)
                    .rev()
                    .find(|at| text.is_char_boundary(*at))
                    .unwrap();
                let mut edited = text.clone();
                edited.insert_str(at, "追加");
                let target = edited[..at + "追加".len()].encode_utf16().count() as u32;
                let cost = engine
                    .update_interactive(
                        StyledText::plain(&edited),
                        LineFit::Extent(700),
                        &typography,
                        target,
                    )
                    .unwrap();
                assert!(engine.layout_pending());
                assert!(cost.utf16 < 8_000, "measured {} units", cost.utf16);
                let caret = engine.caret_geometry(target).unwrap();
                let before = engine.plan.clone();
                let exact_spans: Vec<_> = before
                    .blocks
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !engine.deferred_blocks.contains(index))
                    .map(|(_, block)| (block.span, block.lines.clone()))
                    .collect();
                finish(&mut engine, &edited, &typography);
                let mut full = TextEngine::new(mode);
                full.update(
                    StyledText::plain(&edited),
                    LineFit::Extent(700),
                    &typography,
                )
                .unwrap();
                assert_eq!(engine.plan, full.plan);
                for (span, lines) in exact_spans {
                    if let Some(block) = full.plan.blocks.iter().find(|block| block.span == span) {
                        assert_eq!(lines, block.lines);
                    }
                }
                // The caret before the edit keeps its coordinate in both modes:
                // the origin is where the document starts (2026-09-16).
                let after = engine.caret_geometry(target).unwrap();
                assert_eq!((caret.x, caret.y), (after.x, after.y), "{mode:?}");
                engine
                    .update(StyledText::plain(&text), LineFit::Extent(700), &typography)
                    .unwrap();
            }
        }
    }

    #[test]
    fn rapid_edits_and_geometry_changes_keep_only_latest_result() {
        let mut typography = Typography::default();
        let text = "連続入力の世代を確認する長い文章。".repeat(3000);
        let mut engine = TextEngine::new(WritingMode::Vertical);
        let mut latest = text.clone();
        for count in 1..12 {
            latest = format!("{}{text}", "追加".repeat(count));
            typography.font_size = 18.0 + (count % 3) as f32;
            engine
                .update_interactive(
                    StyledText::plain(&latest),
                    LineFit::Extent(700),
                    &typography,
                    30,
                )
                .unwrap();
            let worker = engine.background.as_ref().unwrap();
            let mailbox = worker.mailbox.0.lock().unwrap();
            assert!(mailbox.job.as_ref().is_none_or(|job| job.text == latest));
        }
        finish(&mut engine, &latest, &typography);
        assert_eq!(engine.text, latest);
        let mut full = TextEngine::new(WritingMode::Vertical);
        full.update(
            StyledText::plain(&latest),
            LineFit::Extent(700),
            &typography,
        )
        .unwrap();
        assert_eq!(engine.plan, full.plan);
    }

    #[test]
    fn estimates_are_not_drawn_or_hit_tested_and_sync_update_finishes() {
        let typography = Typography::default();
        let text = "見えていない段落を計算しています。".repeat(3000);
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                0,
            )
            .unwrap();
        let index = *engine.deferred_blocks.iter().next().unwrap();
        let block = &engine.plan.blocks[index];
        let (at, flow) = (block.span.utf16_start, block.flow_start);
        assert!(engine.caret_geometry(at).is_err());
        assert!(engine.hit_test(300.0, flow + 10.0).is_err());
        assert!(
            engine
                .visible_tiles(-flow, 500.0, 0, 0.0, 700.0)
                .iter()
                .all(|tile| !engine.deferred_blocks.contains(&tile.block_index))
        );
        engine
            .update(StyledText::plain(&text), LineFit::Extent(700), &typography)
            .unwrap();
        assert!(!engine.layout_pending());
        assert!(engine.caret_geometry(at).is_ok());
    }

    #[test]
    fn stale_completion_is_rejected_and_worker_error_can_recover() {
        let typography = Typography::default();
        let text = "失敗と古い結果の確認。".repeat(5000);
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                0,
            )
            .unwrap();
        let plan = engine.plan.clone();
        engine.background = Some(BackgroundLayout {
            mailbox: Arc::new((
                Mutex::new(Mailbox {
                    answer: Some(Answer {
                        generation: 6,
                        result: Ok(Completed {
                            plan: BlockLayoutPlan::default(),
                            wraps: Vec::new(),
                            measures: HashMap::new(),
                            block_lines: Vec::new(),
                            page_extent: 1,
                            margin: 0.0,
                            numbers: None,
                            wrapping_items: 0,
                        }),
                    }),
                    ..Mailbox::default()
                }),
                Condvar::new(),
            )),
            generation: Arc::new(AtomicU64::new(7)),
            pending: true,
        });
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                0,
            )
            .unwrap();
        assert_eq!(engine.plan, plan);
        engine
            .background
            .as_ref()
            .unwrap()
            .mailbox
            .0
            .lock()
            .unwrap()
            .answer = Some(Answer {
            generation: 7,
            result: Err("injected failure".into()),
        });
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                0,
            )
            .unwrap();
        assert!(engine.position_ready(10));
        finish(&mut engine, &text, &typography);
        assert_eq!(engine.utf16_len(), text.encode_utf16().count() as u32);
    }

    #[test]
    fn large_paste_does_not_synchronously_wrap_to_the_caret() {
        let typography = Typography::default();
        let text = "貼り付けた長い段落です。".repeat(5000);
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        let end = text.encode_utf16().count() as u32;
        let cost = engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                end,
            )
            .unwrap();
        assert!(cost.wrapped <= 4096);
        assert!(engine.layout_pending());
        assert!(!engine.position_ready(end));
        finish(&mut engine, &text, &typography);
        assert!(engine.caret_geometry(end).is_ok());
    }

    #[test]
    fn duplicate_long_paragraphs_and_crlf_have_distinct_deferred_offsets() {
        let typography = Typography::default();
        let paragraph = "同じ長い段落。".repeat(1500);
        let text = format!("{paragraph}\r\n{paragraph}\r\n後続。");
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                0,
            )
            .unwrap();
        assert_eq!(engine.wraps.len(), 2);
        assert_eq!(engine.wraps[1].byte_start, paragraph.len() + 2);
        assert_eq!(engine.deferred_blocks.len(), 2);
        finish(&mut engine, &text, &typography);
        let mut full = TextEngine::new(WritingMode::Horizontal);
        full.update(StyledText::plain(&text), LineFit::Extent(700), &typography)
            .unwrap();
        assert_eq!(engine.plan, full.plan);
    }

    #[test]
    fn decorated_preview_and_scroll_extension_match_complete_layout() {
        let typography = Typography::new(22.0);
        let source = format!(
            "> {}\n\n後続。",
            "｜漢字《かんじ》と《《傍点》》12、🙂e\u{301}、**太字**、括弧（かっこ）。".repeat(500)
        );
        let preview = crate::document::PreviewDocument::from_source(&source);
        let styles = crate::document::line_styles(&source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let mut engine = TextEngine::new(mode);
            engine
                .update_interactive(styled, LineFit::Extent(700), &typography, 0)
                .unwrap();
            assert!(engine.layout_pending());
            let previous = engine.wraps[0].starts.len();
            let index = *engine.deferred_blocks.iter().next().unwrap();
            let block = &engine.plan.blocks[index];
            let offset = if mode == WritingMode::Horizontal {
                -block.flow_start
            } else {
                -(block.flow_start + block.flow_size - 500.0)
            };
            engine.prepare_viewport(offset, 500.0).unwrap();
            assert!(engine.wraps[0].complete || engine.wraps[0].starts.len() > previous);
            let deadline = Instant::now() + Duration::from_secs(15);
            while engine.layout_pending() {
                assert!(Instant::now() < deadline);
                if engine.layout_ready() {
                    engine
                        .update_interactive(styled, LineFit::Extent(700), &typography, 0)
                        .unwrap();
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            let mut full = TextEngine::new(mode);
            full.update(styled, LineFit::Extent(700), &typography)
                .unwrap();
            assert_eq!(engine.plan, full.plan);
        }
    }
}
