//! E17: foreground prefixes and a latest-only, cancellable layout worker.
//! Only owned text and numeric layout data cross the thread boundary.
use super::*;
use std::sync::{
    Condvar,
    atomic::{AtomicU64, Ordering},
};

const WINDOW_CHARACTERS: usize = 2048;
const FOREGROUND_LOOKAHEAD: usize = 2048;
/// How far before what the writer is looking at the foreground lays out too,
/// in characters (RFN01-6 A). The caret can sit at the bottom of the view, and
/// the text above it is on screen.
const FOREGROUND_LOOKBEHIND: usize = 2048;
#[cfg(not(test))]
const FOREGROUND_BUDGET: Duration = Duration::from_millis(8);
/// Tests stop on the window count alone: a budget of time makes where the
/// foreground stops depend on how fast the machine was that moment, and two
/// engines compared step by step would part for no reason in the code.
#[cfg(test)]
const FOREGROUND_BUDGET: Duration = Duration::MAX;

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
    /// The epoch the job was asked in ([`BackgroundLayout::generation`]).
    generation: u64,
    /// Which request this is, counted per worker. The newest one is the only
    /// one whose plan is shown; the others are kept for what they measured.
    serial: u64,
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
    serial: u64,
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
    block_keys: Vec<Option<(u64, u64)>>,
    block_measures: Vec<BlockMeasure>,
    measure_refs: HashMap<u64, u32>,
    line_count: usize,
}

#[derive(Default)]
struct Mailbox {
    job: Option<Job>,
    answer: Option<Answer>,
    stopped: bool,
    /// The epoch of the job the worker is laying out now, if it is.
    running: Option<u64>,
}

/// 手前の組版が正確に組む範囲（E17、RFN01-6のA、2026-09-24）。
///
/// **ここに入らない、まだ測っていないブロックは、推定の大きさのまま裏に任せる。**
/// 以前は長い段落の組み終わっていない尾だけを裏に回し、短い段落のブロックは
/// 文書の頭から入力位置まで全部その場で測っていた——小説風の990万字を開くと、
/// 25,838ブロックを測り終えるまで40秒、窓も出なかった。範囲は入力位置・
/// 見ている所・留めている所のそれぞれの前後で、離れた2か所のあいだは組まない
/// （頭に入力位置を残したまま末尾を見ても、そのあいだを全部組まずに済む）。
pub(super) struct Window {
    /// Byte ranges into the text, in order and not overlapping.
    ranges: Vec<Range<usize>>,
}

/// Where the new text differs from the one laid out last (RFN01-6 B): the
/// same up to `prefix`, and the same again from `old_end` in the old text and
/// `new_end` in the new one. All three are character boundaries. An identical
/// text is an empty change at its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TextDiff {
    pub prefix: usize,
    pub old_end: usize,
    pub new_end: usize,
}

impl TextDiff {
    /// Compared a page at a time, which is `memcmp` and a few milliseconds for
    /// the largest document the editor opens — against the tens of
    /// milliseconds a pass that looks at each character costs.
    pub(super) fn between(old: &str, new: &str) -> Self {
        const PAGE: usize = 4096;
        let (a, b) = (old.as_bytes(), new.as_bytes());
        let shorter = a.len().min(b.len());
        let mut prefix = 0;
        while prefix + PAGE <= shorter && a[prefix..prefix + PAGE] == b[prefix..prefix + PAGE] {
            prefix += PAGE;
        }
        while prefix < shorter && a[prefix] == b[prefix] {
            prefix += 1;
        }
        while !old.is_char_boundary(prefix) {
            prefix -= 1;
        }
        let room = shorter - prefix;
        let mut suffix = 0;
        while suffix + PAGE <= room
            && a[a.len() - suffix - PAGE..a.len() - suffix]
                == b[b.len() - suffix - PAGE..b.len() - suffix]
        {
            suffix += PAGE;
        }
        while suffix < room && a[a.len() - suffix - 1] == b[b.len() - suffix - 1] {
            suffix += 1;
        }
        // The same bytes follow in both, so a boundary in one is a boundary
        // in the other; only a character cut in half has to be given back.
        while !old.is_char_boundary(a.len() - suffix) || !new.is_char_boundary(b.len() - suffix) {
            suffix -= 1;
        }
        Self {
            prefix,
            old_end: a.len() - suffix,
            new_end: b.len() - suffix,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.old_end == self.prefix && self.new_end == self.prefix
    }
}

impl Window {
    /// Each stretch the caller needs, widened by the look-behind and the
    /// look-ahead. `needed` is in UTF-16 units of `text`; `bytes` turns them
    /// into bytes of it, all at once.
    fn around(text: &str, needed: &[Range<u32>], bytes: impl FnOnce(&[u32]) -> Vec<usize>) -> Self {
        let ends = needed
            .iter()
            .flat_map(|range| [range.start, range.end.max(range.start)])
            .collect::<Vec<u32>>();
        let bytes = bytes(&ends);
        let mut ranges = bytes
            .chunks(2)
            .map(|pair| {
                retreat_characters(text, pair[0], FOREGROUND_LOOKBEHIND)
                    ..advance_characters(text, pair[1], FOREGROUND_LOOKAHEAD)
            })
            .collect::<Vec<Range<usize>>>();
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        Self { ranges: merged }
    }

    /// Whether any of the window reaches `span`. Touching counts: a block that
    /// ends where the window begins is measured, which costs one block and
    /// saves asking whether its last line is on screen.
    pub(super) fn touches(&self, span: Range<usize>) -> bool {
        self.ranges
            .iter()
            .any(|range| span.start <= range.end && span.end >= range.start)
    }

    /// How far into `span` the window reaches, or `None` if it does not reach
    /// it at all. A paragraph is wrapped from its head, so this is the one
    /// number its wrap search needs.
    pub(super) fn reach_in(&self, span: Range<usize>) -> Option<usize> {
        self.ranges
            .iter()
            .filter(|range| span.start <= range.end && span.end >= range.start)
            .map(|range| range.end.min(span.end))
            .max()
    }
}

/// **打鍵では中止しない**（E17の改訂、2026-09-24）。`generation`は取り消しの
/// 世代で、上がるのは行の長さや書式が変わって、組んだものが使えなくなるとき
/// だけ。打鍵は新しい依頼（`latest`）を置くだけで、走っている仕事は最後まで
/// 組む——その結果は中身の一致する段落とブロックとして使い回し、次の仕事にも
/// 引き継ぐ。打鍵のたびに中止していた頃は、長い段落の続く100万字の文書で
/// 裏の組版が打ち続けるかぎり終わらなかった（RFN01-6の測定）。
pub(super) struct BackgroundLayout {
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    generation: Arc<AtomicU64>,
    latest: u64,
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
                            drop(slot);
                            // 錠の外で道具を手放してから終わる（`release_graphics`）。
                            release_graphics();
                            break;
                        }
                        let job = slot.job.take().unwrap();
                        slot.running = Some(job.generation);
                        job
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
                        engine.update_inner(styled, job.fit, &job.typography, None, None)?;
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
                            block_keys: engine.block_keys,
                            block_measures: engine.block_measures,
                            measure_refs: engine.measure_refs,
                            line_count: engine.line_count,
                        })
                    }))
                    .map_err(|_| "layout worker panicked".to_owned())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                    {
                        let mut slot = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        slot.running = None;
                        if !slot.stopped && token.check().is_ok() {
                            // The request waiting behind this one starts from what
                            // this one found, not from the snapshot it was asked
                            // with — that snapshot predates all of it.
                            if let (Ok(done), Some(next)) = (&result, slot.job.as_mut())
                                && next.generation == job.generation
                            {
                                carry_over(&mut next.wraps, &mut next.measures, done);
                            }
                            slot.answer = Some(Answer {
                                generation: job.generation,
                                serial: job.serial,
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
            latest: 0,
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
            .map(|block| block.span.utf16_start..block.span.utf16_start)
            .collect::<Vec<Range<u32>>>();
        if needed.is_empty() {
            return Ok(());
        }
        let text = self.text.clone();
        let styles = self.line_styles.clone();
        let marks = self.line_spans.clone();
        let markers = self.line_markers.clone();
        let typography = self.typography.clone();
        let styled = StyledText::marked(&text, &styles, &marks)
            .with_markers(&markers)
            .with_source_line(self.source_line);
        self.update_interactive(styled, self.fit, &typography, &needed)?;
        Ok(())
    }

    /// The text on screen, as a range of the last plan's UTF-16 positions —
    /// text positions rather than pixels, because vertical content changes its
    /// origin as the estimated blocks change size. `None` before anything has
    /// been laid out, when there is no screen to speak of.
    ///
    /// **Estimated blocks count** (RFN01-6 A): one that is on screen is exactly
    /// what the next foreground update has to measure.
    pub fn viewport_utf16(&self, scroll: f32, extent: f32) -> Option<Range<u32>> {
        let visible =
            crate::text_blocks::visible_flow_range(scroll, extent, self.plan.flow_bounds());
        let mut shown = self.plan.blocks.iter().filter(|block| {
            block.flow_start < visible.1 && block.flow_start + block.flow_size > visible.0
        });
        let first = shown.next()?;
        let (start, end) = shown.fold(
            (first.span.utf16_start, first.span.utf16_end),
            |(start, end), block| {
                (
                    start.min(block.span.utf16_start),
                    end.max(block.span.utf16_end),
                )
            },
        );
        Some(start..end)
    }

    /// The byte in `text` of each UTF-16 position in it, found through the last
    /// plan rather than by counting from the head (RFN01-6 B): before the
    /// change a position is where it was, after it where it was moved by the
    /// change, and only inside it is the new text counted.
    fn bytes_near(&self, text: &str, diff: &TextDiff, targets: &[u32]) -> Vec<usize> {
        if self.plan.blocks.is_empty() || self.text.len() + diff.new_end < diff.old_end {
            return bytes_at_utf16(text, targets);
        }
        let old = self.text.as_str();
        let units = |slice: &str| slice.encode_utf16().count() as u32;
        // In the old text: the block holding the position, then its own text.
        let old_byte = |target: u32| {
            let block = &self.plan.blocks[self.plan.block_at_utf16(target)].span;
            let mut at = block.utf16_start;
            let mut byte = block.byte_start;
            for (offset, ch) in old[block.byte_start..].char_indices() {
                if at >= target {
                    return block.byte_start + offset;
                }
                at += ch.len_utf16() as u32;
                byte = block.byte_start + offset + ch.len_utf8();
            }
            byte
        };
        let prefix_utf16 = {
            let block = &self.plan.blocks[self
                .plan
                .blocks
                .partition_point(|block| block.span.byte_start <= diff.prefix)
                .saturating_sub(1)]
            .span;
            block.utf16_start + units(&old[block.byte_start.min(diff.prefix)..diff.prefix])
        };
        let old_end_utf16 = prefix_utf16 + units(&old[diff.prefix..diff.old_end]);
        let new_end_utf16 = prefix_utf16 + units(&text[diff.prefix..diff.new_end]);
        targets
            .iter()
            .map(|&target| {
                if target <= prefix_utf16 {
                    old_byte(target).min(diff.prefix)
                } else if target >= new_end_utf16 {
                    let was = old_byte(target - new_end_utf16 + old_end_utf16).max(diff.old_end);
                    was - diff.old_end + diff.new_end
                } else {
                    let mut at = prefix_utf16;
                    for (offset, ch) in text[diff.prefix..diff.new_end].char_indices() {
                        if at >= target {
                            return diff.prefix + offset;
                        }
                        at += ch.len_utf16() as u32;
                    }
                    diff.new_end
                }
            })
            .collect()
    }

    /// What a pane's foreground update has to lay out: the screen, and the
    /// positions it is holding on to — the caret, a kept view — each on its own
    /// (RFN01-6 A). With nothing on screen yet and nothing held, the head.
    pub fn needed_utf16(&self, scroll: f32, extent: f32, held: &[Option<u32>]) -> Vec<Range<u32>> {
        let mut needed = self
            .viewport_utf16(scroll, extent)
            .into_iter()
            .collect::<Vec<Range<u32>>>();
        needed.extend(held.iter().flatten().map(|at| *at..*at));
        if needed.is_empty() {
            needed.push(0..0);
        }
        needed
    }

    pub fn update_interactive(
        &mut self,
        styled: StyledText<'_>,
        fit: LineFit,
        typography: &Typography,
        needed: &[Range<u32>],
    ) -> Result<UpdateCost> {
        if fit == LineFit::Free {
            return self.update(styled, fit, typography);
        }
        // **One comparison of the text for the whole update** (RFN01-6 B): it
        // says whether anything changed, where, and — through the last plan —
        // where the positions asked for are, without walking the text.
        let diff = TextDiff::between(&self.text, styled.text);
        let same = diff.is_empty()
            && self.text.len() == styled.text.len()
            && self.same_setting(styled, fit, typography);
        // A background answer merged below changes what the caches hold for
        // blocks the near update would keep as they were, so that update is
        // the whole one.
        let mut merged = false;
        if let Some(worker) = self.background.as_mut() {
            let answer = worker
                .mailbox
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .answer
                .take();
            if let Some(answer) = answer
                .filter(|answer| answer.generation == worker.generation.load(Ordering::Relaxed))
            {
                let newest = answer.serial == worker.latest;
                match answer.result {
                    Ok(done) if newest && same => {
                        worker.pending = false;
                        self.plan = done.plan;
                        self.wraps = done.wraps;
                        self.measures = done.measures;
                        self.block_lines = done.block_lines;
                        self.page_extent = done.page_extent;
                        self.margin = done.margin;
                        self.numbers = done.numbers;
                        self.wrapping_items = done.wrapping_items;
                        self.block_keys = done.block_keys;
                        self.block_measures = done.block_measures;
                        self.measure_refs = done.measure_refs;
                        self.line_count = done.line_count;
                        self.deferred_blocks.clear();
                        self.layouts.clear();
                        return Ok(UpdateCost::default());
                    }
                    // An earlier text: what it measured is still true of every
                    // paragraph and block the edits since have not touched.
                    Ok(done) => {
                        carry_over(&mut self.wraps, &mut self.measures, &done);
                        merged = true;
                    }
                    Err(_) if newest => {
                        // Retry in bounded foreground windows on subsequent
                        // refreshes; never synchronously redo the whole tail.
                        self.background = None;
                    }
                    Err(_) => {}
                }
            }
        }
        if same && !self.layout_pending() {
            return Ok(UpdateCost::default());
        }
        let window = Window::around(styled.text, needed, |targets| {
            self.bytes_near(styled.text, &diff, targets)
        });
        // **Not when an answer was just taken for an older text**: the worker
        // is idle now and was asked nothing since (see below), so this update
        // is the one that asks again.
        if same
            && !merged
            && self
                .background
                .as_ref()
                .is_some_and(|worker| worker.pending)
            && !self.deferred_blocks.iter().any(|index| {
                let span = &self.plan.blocks[*index].span;
                window.touches(span.byte_start..span.byte_end)
            })
        {
            return Ok(UpdateCost::default());
        }
        let settled = settled(fit, typography);
        // Only a change that makes what the worker is laying out useless stops
        // it; an edit leaves it running (see [`BackgroundLayout`]).
        if (self.fit, &self.typography) != (settled.0, &settled.1) {
            self.cancel_background();
        }
        let near = (!merged).then_some(&diff);
        let cost = self.update_inner(styled, fit, typography, Some(&window), near)?;
        if self.layout_pending() {
            if self.background.is_none() {
                self.background = BackgroundLayout::new();
            }
            if let Some(worker) = &mut self.background {
                // **No new request while the worker is still on one of this
                // epoch** (RFN01-6 B). A request is a copy of the whole text and
                // its settings — 6ms a keystroke at ten million characters —
                // and the worker only takes the newest when it is done, so
                // every copy but the last was thrown away unread. When the
                // answer comes back, the update that takes it asks again with
                // the text as it is then. A worker still finishing a cancelled
                // epoch is asked at once: nothing of that epoch will come back
                // to ask again.
                let epoch = worker.generation.load(Ordering::Relaxed);
                let busy = worker
                    .mailbox
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .running
                    == Some(epoch);
                // The serial moves on either way: the answer on its way is
                // for a text before this one, and must not be taken as the
                // newest just because nothing newer was sent.
                if busy {
                    worker.latest += 1;
                    worker.pending = true;
                    return Ok(cost);
                }
                worker.latest += 1;
                let job = Job {
                    generation: worker.generation.load(Ordering::Relaxed),
                    serial: worker.latest,
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

/// Adds what a finished layout found to `wraps` and `measures`, where a later
/// layout finds them by their contents: a paragraph's wraps by its text and how
/// it is set ([`wrap_reuse`]), a block's measurement by its key and text. Only
/// finished paragraphs are added, and none that is already there.
fn carry_over(
    wraps: &mut Vec<ParagraphWraps>,
    measures: &mut HashMap<u64, MeasuredBlock>,
    done: &Completed,
) {
    for found in done.wraps.iter().filter(|found| found.complete) {
        let known = wraps.iter().any(|kept| {
            kept.complete
                && kept.text == found.text
                && kept.style == found.style
                && kept.indent_cells == found.indent_cells
                && kept.marker == found.marker
                && kept.marks == found.marks
        });
        if !known {
            wraps.push(found.clone());
        }
    }
    for (key, measured) in &done.measures {
        measures.entry(*key).or_insert_with(|| measured.clone());
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

/// The byte of each UTF-16 position, in one pass however many are asked.
fn bytes_at_utf16(text: &str, targets: &[u32]) -> Vec<usize> {
    let mut order = (0..targets.len()).collect::<Vec<usize>>();
    order.sort_by_key(|index| targets[*index]);
    let mut found = vec![text.len(); targets.len()];
    let mut next = order.into_iter().peekable();
    let mut units = 0u32;
    for (byte, ch) in text.char_indices() {
        while let Some(index) = next.next_if(|index| units >= targets[*index]) {
            found[index] = byte;
        }
        if next.peek().is_none() {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    found
}

fn retreat_characters(text: &str, from: usize, count: usize) -> usize {
    text[..from]
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(0, |(at, _)| at)
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
                        &[0..0],
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
                        &[target..target],
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
    fn a_keystroke_leaves_the_paragraphs_past_its_margin_to_the_background() {
        // RFN01-6: the unfinished paragraphs after the caret used to be taken a
        // window further on every keystroke, all of them.
        let typography = Typography::default();
        let paragraph = "先の段落は裏で組む。".repeat(1200);
        let text = format!("{paragraph}\n").repeat(6);
        let mut engine = TextEngine::new(WritingMode::Vertical);
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[0..0],
            )
            .unwrap();
        engine.cancel_background();
        let mut edited = text.clone();
        edited.insert_str(0, "追");
        let cost = engine
            .update_interactive(
                StyledText::plain(&edited),
                LineFit::Extent(700),
                &typography,
                &[1..1],
            )
            .unwrap();
        assert!(engine.layout_pending());
        assert!(
            cost.wrapped <= 3 * WINDOW_CHARACTERS as u32,
            "wrapped {} units for one keystroke",
            cost.wrapped
        );
        finish(&mut engine, &edited, &typography);
        let mut full = TextEngine::new(WritingMode::Vertical);
        full.update(
            StyledText::plain(&edited),
            LineFit::Extent(700),
            &typography,
        )
        .unwrap();
        assert_eq!(engine.plan, full.plan);
    }

    #[test]
    fn the_background_keeps_going_while_the_writer_types() {
        // RFN01-6: an edit used to stop the worker and throw away what it had
        // laid out, so a writer who kept typing never let it finish.
        let typography = Typography::default();
        let paragraph = "打ち続けても裏は進む。".repeat(1200);
        let text = format!("{paragraph}\n").repeat(6);
        let mut engine = TextEngine::new(WritingMode::Vertical);
        let mut edited = text.clone();
        let deadline = Instant::now() + Duration::from_secs(20);
        let finished = |engine: &TextEngine| engine.wraps.iter().filter(|w| w.complete).count();
        let mut typed = 0;
        while finished(&engine) < 5 {
            assert!(
                Instant::now() < deadline,
                "only {} paragraphs finished while typing",
                finished(&engine)
            );
            edited.insert_str(0, "追");
            typed += 1;
            engine
                .update_interactive(
                    StyledText::plain(&edited),
                    LineFit::Extent(700),
                    &typography,
                    &[1..1],
                )
                .unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(typed > 1, "the first keystroke cannot have found them");
        finish(&mut engine, &edited, &typography);
        let mut full = TextEngine::new(WritingMode::Vertical);
        full.update(
            StyledText::plain(&edited),
            LineFit::Extent(700),
            &typography,
        )
        .unwrap();
        assert_eq!(engine.plan, full.plan);
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
                    &[30..30],
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
                &[0..0],
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
                &[0..0],
            )
            .unwrap();
        let plan = engine.plan.clone();
        engine.background = Some(BackgroundLayout {
            mailbox: Arc::new((
                Mutex::new(Mailbox {
                    answer: Some(Answer {
                        generation: 6,
                        serial: 3,
                        result: Ok(Completed {
                            plan: BlockLayoutPlan::default(),
                            wraps: Vec::new(),
                            measures: HashMap::new(),
                            block_lines: Vec::new(),
                            page_extent: 1,
                            margin: 0.0,
                            numbers: None,
                            wrapping_items: 0,
                            block_keys: Vec::new(),
                            block_measures: Vec::new(),
                            measure_refs: HashMap::new(),
                            line_count: 0,
                        }),
                    }),
                    ..Mailbox::default()
                }),
                Condvar::new(),
            )),
            generation: Arc::new(AtomicU64::new(7)),
            latest: 3,
            pending: true,
        });
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[0..0],
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
            serial: 3,
            result: Err("injected failure".into()),
        });
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[0..0],
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
                &[end..end],
            )
            .unwrap();
        assert!(cost.wrapped <= 4096);
        assert!(engine.layout_pending());
        assert!(!engine.position_ready(end));
        finish(&mut engine, &text, &typography);
        assert!(engine.caret_geometry(end).is_ok());
    }

    /// RFN01-6 A: 短い段落の多い文書（小説）は、見ている所の前後だけを組んで
    /// 残りを推定で置く。以前は入力位置より手前を全部その場で測っていた。
    fn novel(paragraphs: usize) -> String {
        (0..paragraphs)
            .map(|index| format!("{index}番目の段落。短い文が続く小説の一段落である。\n"))
            .collect()
    }

    fn full_plan(mode: WritingMode, text: &str, typography: &Typography) -> BlockLayoutPlan {
        let mut full = TextEngine::new(mode);
        full.update(StyledText::plain(text), LineFit::Extent(700), typography)
            .unwrap();
        full.plan
    }

    #[test]
    fn opening_a_long_novel_measures_only_around_the_caret() {
        let typography = Typography::default();
        let text = novel(4000);
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let mut engine = TextEngine::new(mode);
            let cost = engine
                .update_interactive(
                    StyledText::plain(&text),
                    LineFit::Extent(700),
                    &typography,
                    &[0..0],
                )
                .unwrap();
            assert!(engine.layout_pending(), "{mode:?}");
            let blocks = engine.plan.blocks.len();
            assert!(
                (cost.blocks as usize) * 10 < blocks,
                "{mode:?}: measured {} of {blocks} blocks",
                cost.blocks
            );
            assert!(engine.position_ready(0));
            finish(&mut engine, &text, &typography);
            assert_eq!(engine.plan, full_plan(mode, &text, &typography), "{mode:?}");
        }
    }

    #[test]
    fn a_caret_at_the_end_does_not_lay_out_everything_before_it() {
        // A document restored with its caret at the end, or Ctrl+End before the
        // background has finished: the text before is estimated, the end exact.
        let typography = Typography::default();
        let text = novel(4000);
        let end = text.encode_utf16().count() as u32;
        let mut engine = TextEngine::new(WritingMode::Vertical);
        let cost = engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[end..end],
            )
            .unwrap();
        let blocks = engine.plan.blocks.len();
        assert!(
            (cost.blocks as usize) * 10 < blocks,
            "measured {} of {blocks} blocks",
            cost.blocks
        );
        assert!(engine.position_ready(end));
        assert!(!engine.position_ready(0));
        assert!(engine.caret_geometry(end).is_ok());
        finish(&mut engine, &text, &typography);
        assert_eq!(
            engine.plan,
            full_plan(WritingMode::Vertical, &text, &typography)
        );
    }

    /// RFN01-6 B: a document with every kind of block the split treats
    /// differently — headings, short and long paragraphs, a fence, a table, a
    /// list and a quote. Numbered, so that no two blocks share their text.
    fn mixed_document() -> String {
        let mut text = String::new();
        for index in 0..48 {
            match index % 8 {
                0 => text.push_str(&format!("# 見出し{index}\n\n")),
                1 => text.push_str(&format!("{index}番目の段落。短い文が続く。\n")),
                2 => {
                    // Numbered sentences, so that no two pieces the paragraph
                    // is cut into read the same. One is long enough that the
                    // foreground leaves an estimated tail of it.
                    let sentences = if index == 26 { 1200 } else { 60 + index * 4 };
                    for sentence in 0..sentences {
                        text.push_str(&format!("長い段落{index}の{sentence}番目の文。"));
                    }
                    text.push('\n');
                }
                3 => text.push_str(&format!("```\ncode {index}\nmore code\n```\n")),
                4 => text.push_str(&format!(
                    "| 見出し{index} | 値 |\n| --- | --- |\n| a{index} | b |\n"
                )),
                5 => text.push_str(&format!("- 項目{index}\n- 次の項目{index}\n")),
                6 => text.push_str(&format!("> 引用{index}の文。\n")),
                _ => text.push('\n'),
            }
        }
        text
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, below: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % below.max(1)
        }
    }

    /// Everything a near update and a whole one have to agree on.
    fn assert_same_engine(near: &TextEngine, whole: &TextEngine, step: usize, what: &str) {
        let context = format!("step {step}: {what}");
        assert_eq!(near.text, whole.text, "{context}: text");
        if near.plan != whole.plan {
            let first = near
                .plan
                .blocks
                .iter()
                .zip(&whole.plan.blocks)
                .position(|(a, b)| a != b)
                .unwrap_or(near.plan.blocks.len().min(whole.plan.blocks.len()));
            let show = |engine: &TextEngine| {
                engine
                    .plan
                    .blocks
                    .iter()
                    .enumerate()
                    .skip(first.saturating_sub(1))
                    .take(3)
                    .map(|(index, block)| {
                        format!(
                            "#{index} {:?} {:?} flow={} size={} deferred={} lines={}",
                            block.span,
                            engine.text[block.span.byte_start..block.span.byte_end]
                                .chars()
                                .take(12)
                                .collect::<String>(),
                            block.flow_start,
                            block.flow_size,
                            engine.deferred_blocks.contains(&index),
                            block.lines.len()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n  ")
            };
            panic!(
                "{context}: plan differs at block {first} of {} / {}\nnear:\n  {}\nwhole:\n  {}",
                near.plan.blocks.len(),
                whole.plan.blocks.len(),
                show(near),
                show(whole)
            );
        }
        assert_eq!(
            near.deferred_blocks, whole.deferred_blocks,
            "{context}: deferred"
        );
        assert_eq!(
            near.block_lines, whole.block_lines,
            "{context}: block lines"
        );
        assert_eq!(near.line_count, whole.line_count, "{context}: line count");
        assert_eq!(near.line_styles, whole.line_styles, "{context}: styles");
        assert_eq!(near.line_spans, whole.line_spans, "{context}: spans");
        assert_eq!(near.line_markers, whole.line_markers, "{context}: markers");
        assert_eq!(
            near.source_line, whole.source_line,
            "{context}: source line"
        );
        let wraps = |engine: &TextEngine| {
            engine
                .wraps
                .iter()
                .map(|wrap| {
                    (
                        wrap.byte_start,
                        wrap.text.len(),
                        wrap.starts.len(),
                        wrap.starts.last().copied(),
                        wrap.complete,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            wraps(near),
            wraps(whole),
            "{context}: wraps (start, bytes, lines, last, complete)"
        );
        assert!(
            near.wraps
                .iter()
                .zip(&whole.wraps)
                .all(|(a, b)| a.text == b.text && a.starts == b.starts),
            "{context}: wrap texts or starts"
        );
    }

    /// RFN01-6 B: **the near update makes what the whole one makes**, edit
    /// after edit, in both writing modes and in the preview — where the
    /// settings of lines move with the text (a fence opened by the edit, a
    /// table row that becomes a header) and the line shown as its own source
    /// moves with the caret.
    ///
    /// The background is cancelled after every update on both sides, so both
    /// go on from the same estimates. One thing the two may do differently is
    /// not exercised: an estimated block whose text another block shares, once
    /// that other block is measured, is taken from the cache by the whole
    /// update and left estimated by the near one until the window reaches it.
    #[test]
    fn a_near_update_makes_what_a_whole_update_makes() {
        let typography = Typography::new(22.0);
        for (mode, steps, seed) in [
            (WritingMode::Vertical, 70, 7_u64),
            (WritingMode::Horizontal, 40, 11),
        ] {
            let mut source = mixed_document();
            let mut near = TextEngine::new(mode);
            let mut whole = TextEngine::new(mode);
            whole.take_whole_updates();
            let mut random = Lcg(seed);
            let mut caret;
            for step in 0..steps {
                let characters = source.chars().count();
                let at = |index: usize, source: &str| {
                    source
                        .char_indices()
                        .nth(index)
                        .map_or(source.len(), |(byte, _)| byte)
                };
                let pick = random.next(characters + 1);
                let byte = at(pick, &source);
                let what = match random.next(9) {
                    0 | 1 => {
                        source.insert_str(byte, "あ");
                        caret = byte + "あ".len();
                        "insert"
                    }
                    2 if characters > 0 => {
                        let end = at(pick + 1, &source);
                        source.replace_range(byte..end, "");
                        caret = byte;
                        "delete"
                    }
                    3 => {
                        source.insert(byte, '\n');
                        caret = byte + 1;
                        "break"
                    }
                    4 => {
                        let line = source[..byte].rfind('\n').map_or(0, |at| at + 1);
                        let fence = format!("```{step}\n");
                        source.insert_str(line, &fence);
                        caret = line + fence.len();
                        "fence"
                    }
                    5 => {
                        let line = source[..byte].rfind('\n').map_or(0, |at| at + 1);
                        let heading = format!("# {step} ");
                        source.insert_str(line, &heading);
                        caret = line + heading.len();
                        "heading"
                    }
                    6 => {
                        let end = at(pick + random.next(30), &source);
                        source.replace_range(byte..end, "");
                        caret = byte;
                        "delete range"
                    }
                    7 => {
                        // Numbered like everything else put in, so that no
                        // two blocks the edits make share their text.
                        let table = format!("|x{step}|y|\n|{}|-|\n", "-".repeat(step + 1));
                        source.insert_str(byte, &table);
                        caret = byte + table.len();
                        "table"
                    }
                    _ => {
                        caret = byte;
                        "move"
                    }
                };
                let line = source[..caret].rfind('\n').map_or(0, |at| at + 1);
                let preview = crate::document::PreviewDocument::from_source_with_active_line(
                    &source,
                    Some(line),
                );
                let styles = crate::document::line_styles(&source);
                let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                    .with_markers(preview.markers())
                    .with_source_line(preview.active_line());
                let shown = preview.utf16_at_source_byte(caret) as u32;
                let mut needed = vec![shown..shown];
                if random.next(4) == 0 {
                    let far = random.next(preview.utf16_len() + 1) as u32;
                    needed.push(far..far);
                }
                for engine in [&mut near, &mut whole] {
                    engine
                        .update_interactive(styled, LineFit::Extent(700), &typography, &needed)
                        .unwrap();
                    engine.cancel_background();
                }
                assert_same_engine(&near, &whole, step, what);
            }
        }
    }

    #[test]
    fn two_places_far_apart_leave_the_text_between_them_estimated() {
        // The caret at the head and the view at the end, as after dragging the
        // scroll bar: laying out everything between the two would be the whole
        // document.
        let typography = Typography::default();
        let text = novel(4000);
        let end = text.encode_utf16().count() as u32;
        let middle = end / 2;
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[0..0, end..end],
            )
            .unwrap();
        assert!(engine.position_ready(0));
        assert!(engine.position_ready(end));
        assert!(!engine.position_ready(middle));
        finish(&mut engine, &text, &typography);
        assert!(engine.position_ready(middle));
        assert_eq!(
            engine.plan,
            full_plan(WritingMode::Horizontal, &text, &typography)
        );
    }

    #[test]
    fn duplicate_long_paragraphs_and_crlf_have_distinct_deferred_offsets() {
        let typography = Typography::default();
        let paragraph = "同じ長い段落。".repeat(1500);
        let text = format!("{paragraph}\r\n{paragraph}\r\n後続。");
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        // The whole text is asked for, so that both paragraphs are reached and
        // what is left of each is its own deferred tail (RFN01-6 A: a paragraph
        // the window does not reach is not started at all).
        let whole = text.encode_utf16().count() as u32;
        engine
            .update_interactive(
                StyledText::plain(&text),
                LineFit::Extent(700),
                &typography,
                &[0..whole],
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
                .update_interactive(styled, LineFit::Extent(700), &typography, &[0..0])
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
                        .update_interactive(styled, LineFit::Extent(700), &typography, &[0..0])
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
