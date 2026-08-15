//! Block splitting, placement and indexing, in either writing direction.
//!
//! A hard line break always starts a new line, so a document can be cut at
//! logical line boundaries without moving a single one. That makes it possible
//! to lay out one `IDWriteTextLayout` per block instead of one for the whole
//! document, and to touch only the blocks that intersect the viewport.
//!
//! # The two axes
//!
//! Everything here is written along the **flow axis**: the direction in which
//! lines stack and the document grows without bound. The perpendicular axis is
//! the **line axis**, along which a line runs, and its extent is fixed by the
//! pane.
//!
//! | | flow axis | line axis |
//! | --- | --- | --- |
//! | vertical writing | screen x, right to left | screen y, pane height |
//! | horizontal writing | screen y, top to bottom | screen x, pane width |
//!
//! So `flow_start` is a screen x in vertical writing and a screen y in
//! horizontal writing, and a "line" is what vertical writing calls a column.
//!
//! Coordinates are the caller's own, and a coordinate is always the low edge of
//! what it describes: `flow_start + flow_size` is the high edge whichever
//! direction is being served. The one thing this module has to be told is which
//! way *reading order* runs along that axis, because vertical writing runs it
//! backwards — the first block sits at the right edge and later blocks have
//! smaller coordinates. That is [`FlowOrder`], and it is the only direction-aware
//! thing here. Which screen axis the flow axis is, and which writing direction
//! produced it, this module still does not know.
//!
//! A "line" here is always a visual line, the thing DirectWrite reports line
//! metrics for. A run of text between two hard breaks is a *logical line*, and
//! it may wrap into several lines.
//!
//! Everything in this module is plain arithmetic over measurements taken from
//! DirectWrite, so it builds and tests on any platform.

use std::{ops::Range, rc::Rc};

/// Which way reading order runs along the caller's flow axis.
///
/// Blocks, lines and UTF-16 positions are always in reading order; this only
/// says whether reading on takes you to larger or smaller flow coordinates. It
/// decides where `place_blocks` starts stacking, which edge of a block its first
/// tile is cut from, and the direction of every ordered search over the placed
/// blocks. Nothing else in this module depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowOrder {
    /// Reading on means larger coordinates. Horizontal writing: block 0 sits at
    /// the top of the pane and the document grows downwards.
    #[default]
    Ascending,
    /// Reading on means smaller coordinates. Vertical writing: block 0 sits at
    /// the right edge and the document grows leftwards.
    Descending,
}

// Block size is counted in cells of line space rather than in characters. Each
// logical line is charged for the whole lines it occupies. A blank line is one
// character and a whole line, so a character budget put no bound on how far a
// block could reach along the flow axis: holding Enter poured lines into one
// block that stayed far under any character count. A block is the unit of
// invalidation, so every tile of that block was redrawn on every keystroke, and
// the cost grew with how long the key was held.

/// Smallest block. Below this a block is not worth a layout of its own.
pub const BLOCK_MIN_CELLS: u32 = 256;
/// Largest block, so one enormous block can never form. A block never splits a
/// logical line, so a single very long line still exceeds this.
pub const BLOCK_MAX_CELLS: u32 = 768;
/// Average cells between boundary candidates, which sets the average block size
/// at roughly `BLOCK_MIN_CELLS` plus this.
const BLOCK_BOUNDARY_STRIDE: u32 = 256;

/// Whether a block may end after this line, decided by the line itself.
///
/// This is the whole reason blocks survive editing. A boundary chosen by a
/// running count from the start of the document moves whenever anything before
/// it changes size, and every block after it is rewritten — one blank line
/// inserted mid-document rewrote up to 13 blocks, and re-measuring them cost
/// more than all the drawing. A boundary that depends only on the text of one
/// line does not move when text is inserted elsewhere, so the block after an
/// edit starts where it always did.
///
/// Longer lines carry more of the budget and are proportionally more likely to
/// end a block, which keeps the average block near a size in cells whatever the
/// line lengths happen to be.
fn ends_a_block(line: &str, line_cells: u32) -> bool {
    // FNV-1a: cheap, and a function of this line and nothing around it.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % u64::from(BLOCK_BOUNDARY_STRIDE) < u64::from(line_cells)
}

/// The half-open span of preview text that one block covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub byte_start: usize,
    pub byte_end: usize,
    pub utf16_start: u32,
    pub utf16_end: u32,
}

impl BlockSpan {
    pub fn utf16_len(&self) -> u32 {
        self.utf16_end - self.utf16_start
    }
}

/// One line of a block, as reported by DirectWrite line metrics and hit tests.
///
/// `flow_start` and `flow_size` are in the block layout's own coordinate space,
/// which is offset from global content coordinates by `content_flow_start`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineInfo {
    /// UTF-16 offset of the line start, relative to the block.
    pub utf16_start: u32,
    /// UTF-16 length of the line, including any trailing newline.
    pub utf16_len: u32,
    /// UTF-16 length of the trailing newline, excluded when moving to the line end.
    pub newline_len: u32,
    pub flow_start: f32,
    pub flow_size: f32,
}

impl LineInfo {
    pub fn utf16_end(&self) -> u32 {
        self.utf16_start + self.utf16_len
    }

    /// The last caret position inside the line, before any newline.
    pub fn utf16_text_end(&self) -> u32 {
        self.utf16_end() - self.newline_len
    }

    pub fn flow_center(&self) -> f32 {
        self.flow_start + self.flow_size * 0.5
    }
}

/// What one block layout measured to. Produced by the DirectWrite side.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockMeasure {
    /// How far the drawn lines reach along the flow axis.
    pub flow_size: f32,
    /// The first drawn flow coordinate in the block layout's own space.
    pub content_flow_start: f32,
    /// The `maxWidth` the block layout was created with. A layout must be
    /// recreated with the same bound to reproduce these coordinates.
    pub max_flow_size: f32,
    /// Shared, not owned: a measurement is cloned into the placement plan on
    /// every update, and copying a line table per block per keystroke was
    /// costing more than the measuring did.
    pub lines: Rc<[LineInfo]>,
}

/// A block placed into global content coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockPlacement {
    pub span: BlockSpan,
    /// Where the block's first drawn pixel sits. Always a whole pixel.
    pub flow_start: f32,
    /// Placed extent along the flow axis, rounded to a whole pixel.
    pub flow_size: f32,
    /// What DirectWrite actually measured, before rounding. Kept so the split
    /// can still be checked against a single whole-document layout, which is the
    /// invariant this entire design rests on and which rounding would blur.
    pub exact_flow_size: f32,
    pub content_flow_start: f32,
    pub max_flow_size: f32,
    pub lines: Rc<[LineInfo]>,
}

impl BlockPlacement {
    /// Where the block layout's own origin must be placed.
    pub fn draw_origin(&self) -> f32 {
        self.flow_start - self.content_flow_start
    }

    /// Convert a global flow coordinate into this block layout's own space.
    pub fn to_layout_flow(&self, global_flow: f32) -> f32 {
        global_flow - self.draw_origin()
    }

    /// Convert a flow coordinate in this block layout's own space into a global one.
    pub fn to_global_flow(&self, layout_flow: f32) -> f32 {
        layout_flow + self.draw_origin()
    }

    pub fn flow_end(&self) -> f32 {
        self.flow_start + self.flow_size
    }

    /// Index of the line holding `utf16` (relative to the block).
    pub fn line_at_utf16(&self, utf16: u32) -> usize {
        self.lines
            .partition_point(|line| line.utf16_start <= utf16)
            .saturating_sub(1)
    }

    /// The block-local UTF-16 range covered by the lines a global flow range
    /// shows, or `None` when the block is entirely off screen.
    ///
    /// One oversized logical line is one block, so clipping to whole blocks is
    /// not enough on its own: selecting such a line would still ask DirectWrite
    /// about every character in it. A block holds tens of lines, so scanning
    /// them costs nothing and stays correct whatever order they come back in.
    pub fn visible_utf16_range(&self, view_start: f32, view_end: f32) -> Option<(u32, u32)> {
        let mut start = u32::MAX;
        let mut end = 0;
        for line in self.lines.iter() {
            let line_flow_start = self.to_global_flow(line.flow_start);
            if line_flow_start < view_end && line_flow_start + line.flow_size > view_start {
                start = start.min(line.utf16_start);
                end = end.max(line.utf16_end());
            }
        }
        (start < end).then_some((start, end))
    }
}

/// Blocks placed end to end along the flow axis, with the document margin applied.
///
/// `blocks` is always in reading order. With [`FlowOrder::Descending`] that means
/// it is in *descending* `flow_start` order, which every ordered search below has
/// to account for. A default plan holds no blocks and so has no geometry to
/// order; `place_blocks` is what sets `order`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockLayoutPlan {
    pub blocks: Vec<BlockPlacement>,
    pub total_flow_size: f32,
    pub margin: f32,
    pub order: FlowOrder,
}

/// How many characters fit in one line at this geometry.
///
/// `line_extent` is the pane's size along the line axis: its height in vertical
/// writing, its width in horizontal writing.
///
/// The layout box is the pane less a margin at each end, and the margin is one
/// and a half times the font size, so three font sizes come off the extent.
pub fn cells_per_line(line_extent: u32, font_size: f32) -> u32 {
    let font_size = font_size.max(1.0);
    let usable = (line_extent as f32 - font_size * 3.0).max(font_size);
    (usable / font_size).floor().max(1.0) as u32
}

/// Cut `text` into blocks at logical line boundaries.
///
/// Every block ends immediately after a `\n` (or at the end of the text), so the
/// lines DirectWrite produces for a block are identical to the ones it would
/// produce for the same text inside a whole-document layout.
///
/// `cells_per_line` only decides where the cuts fall, so it may be an
/// estimate: any cut at a line boundary is correct, and DirectWrite is still
/// what actually wraps the text.
pub fn split_blocks(text: &str, cells_per_line: u32) -> Vec<BlockSpan> {
    let cells_per_line = cells_per_line.max(1);
    let mut blocks = Vec::new();
    let mut byte_cursor = 0;
    let mut utf16_cursor = 0_u32;
    let mut block_byte_start = 0;
    let mut block_utf16_start = 0_u32;
    let mut block_cells = 0_u32;

    while byte_cursor < text.len() {
        let line_end = match text[byte_cursor..].find('\n') {
            Some(relative) => byte_cursor + relative + 1,
            None => text.len(),
        };
        let line = &text[byte_cursor..line_end];
        let line_units = line.encode_utf16().count() as u32;
        // The break itself does not occupy a cell; it ends the line. Trimmed
        // rather than sliced off by length so the hash of the document's last
        // line does not change when something is appended after it.
        let body = line.trim_end_matches('\n');
        let wrapped = body.encode_utf16().count() as u32;
        // Charge for whole lines, so a blank line costs a line like any other.
        let line_cells = wrapped.div_ceil(cells_per_line).max(1) * cells_per_line;
        block_cells += line_cells;
        utf16_cursor += line_units;
        byte_cursor = line_end;

        if block_cells >= BLOCK_MAX_CELLS
            || (block_cells >= BLOCK_MIN_CELLS && ends_a_block(body, line_cells))
        {
            blocks.push(BlockSpan {
                byte_start: block_byte_start,
                byte_end: byte_cursor,
                utf16_start: block_utf16_start,
                utf16_end: utf16_cursor,
            });
            block_byte_start = byte_cursor;
            block_utf16_start = utf16_cursor;
            block_cells = 0;
        }
    }

    if blocks.is_empty() || block_byte_start < text.len() {
        blocks.push(BlockSpan {
            byte_start: block_byte_start,
            byte_end: text.len(),
            utf16_start: block_utf16_start,
            utf16_end: utf16_cursor,
        });
    }

    blocks
}

/// Place measured blocks end to end. Block 0 holds the first text, so it sits at
/// the start of the flow axis and the rest follow in reading order — at the low
/// end for [`FlowOrder::Ascending`], at the high end for
/// [`FlowOrder::Descending`], with the margin outside it either way.
///
/// Every block edge lands on a whole pixel, and each extent is rounded on its
/// own rather than by rounding the running edge.
///
/// Both parts matter, and the second one is easy to get wrong. Tiles are cut out
/// of blocks, so a tile's geometry must depend on nothing but the block it
/// belongs to. A fractional start would shift the glyphs inside the tile
/// whenever the block moved, and an extent taken from the *running* edge would
/// change by a pixel whenever an earlier block changed size — in both cases the
/// pixels of an untouched block stop matching themselves and the tile is redrawn
/// for no reason. Rounding each extent alone costs up to half a pixel per block
/// of drift against the true measurement, which `exact_flow_size` still records.
pub fn place_blocks(
    spans: &[BlockSpan],
    measures: &[BlockMeasure],
    margin: f32,
    order: FlowOrder,
) -> BlockLayoutPlan {
    let margin = margin.round();
    let mut blocks = spans
        .iter()
        .zip(measures)
        .map(|(span, measure)| BlockPlacement {
            span: *span,
            flow_start: 0.0,
            flow_size: measure.flow_size.max(0.0).round(),
            exact_flow_size: measure.flow_size,
            content_flow_start: measure.content_flow_start,
            max_flow_size: measure.max_flow_size,
            lines: measure.lines.clone(),
        })
        .collect::<Vec<_>>();

    // Stack from the end the document starts at. Only the order of the walk
    // differs: a block still runs from its own `flow_start` upwards, because a
    // coordinate is the low edge of what it describes in either direction.
    let mut edge = margin;
    let last = blocks.len().saturating_sub(1);
    for step in 0..blocks.len() {
        let index = match order {
            FlowOrder::Ascending => step,
            FlowOrder::Descending => last - step,
        };
        blocks[index].flow_start = edge;
        edge += blocks[index].flow_size;
    }

    BlockLayoutPlan {
        total_flow_size: edge + margin,
        blocks,
        margin,
        order,
    }
}

impl BlockLayoutPlan {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn utf16_len(&self) -> u32 {
        self.blocks
            .last()
            .map(|block| block.span.utf16_end)
            .unwrap_or(0)
    }

    /// Index of the block holding `utf16`, clamped into range.
    pub fn block_at_utf16(&self, utf16: u32) -> usize {
        if self.blocks.is_empty() {
            return 0;
        }
        self.blocks
            .partition_point(|block| block.span.utf16_start <= utf16)
            .saturating_sub(1)
            .min(self.blocks.len() - 1)
    }

    /// Index of the block covering a global flow coordinate, clamped into range.
    ///
    /// The predicate is "this block is entirely earlier in reading order than
    /// `flow`", which is what stays sorted in both orders.
    pub fn block_at_flow(&self, flow: f32) -> usize {
        if self.blocks.is_empty() {
            return 0;
        }
        let passed = match self.order {
            FlowOrder::Ascending => self
                .blocks
                .partition_point(|block| block.flow_end() <= flow),
            FlowOrder::Descending => self.blocks.partition_point(|block| block.flow_start > flow),
        };
        passed.min(self.blocks.len() - 1)
    }

    /// Blocks intersecting the half-open global flow range.
    pub fn blocks_in_flow_range(&self, view_start: f32, view_end: f32) -> Range<usize> {
        if self.blocks.is_empty() || view_end <= view_start {
            return 0..0;
        }
        // Skip the blocks before the range in reading order, then stop at the
        // first one past it. Which edge answers which question swaps with the
        // order; the two halves do not.
        let (first, end) = match self.order {
            FlowOrder::Ascending => (
                self.blocks
                    .partition_point(|block| block.flow_end() <= view_start),
                self.blocks
                    .partition_point(|block| block.flow_start < view_end),
            ),
            FlowOrder::Descending => (
                self.blocks
                    .partition_point(|block| block.flow_start >= view_end),
                self.blocks
                    .partition_point(|block| block.flow_end() > view_start),
            ),
        };
        first..end.max(first)
    }

    /// Blocks intersecting the half-open UTF-16 range.
    pub fn blocks_in_utf16_range(&self, start: u32, end: u32) -> Range<usize> {
        if self.blocks.is_empty() || end <= start {
            return 0..0;
        }
        let first = self
            .blocks
            .partition_point(|block| block.span.utf16_end <= start);
        let last = self
            .blocks
            .partition_point(|block| block.span.utf16_start < end);
        first..last.max(first)
    }

    /// The block and line holding `utf16`.
    pub fn locate(&self, utf16: u32) -> Option<(usize, usize)> {
        let block_index = self.block_at_utf16(utf16);
        let block = self.blocks.get(block_index)?;
        let local = utf16.saturating_sub(block.span.utf16_start);
        Some((block_index, block.line_at_utf16(local)))
    }

    /// The line before or after `(block_index, line_index)` in reading order.
    ///
    /// `forwards` means the next line to read, whichever way that runs along the
    /// flow axis. Crossing a block boundary is handled here so callers never scan.
    pub fn step_line(
        &self,
        block_index: usize,
        line_index: usize,
        forwards: bool,
    ) -> Option<(usize, usize)> {
        let block = self.blocks.get(block_index)?;
        if forwards {
            if line_index + 1 < block.lines.len() {
                return Some((block_index, line_index + 1));
            }
            let next = self
                .blocks
                .iter()
                .enumerate()
                .skip(block_index + 1)
                .find(|(_, block)| !block.lines.is_empty())?;
            Some((next.0, 0))
        } else {
            if line_index > 0 {
                return Some((block_index, line_index - 1));
            }
            let previous = self
                .blocks
                .iter()
                .enumerate()
                .take(block_index)
                .filter(|(_, block)| !block.lines.is_empty())
                .next_back()?;
            Some((previous.0, previous.1.lines.len() - 1))
        }
    }

    /// Global UTF-16 position of a line start.
    pub fn line_utf16_start(&self, block_index: usize, line_index: usize) -> Option<u32> {
        let block = self.blocks.get(block_index)?;
        let line = block.lines.get(line_index)?;
        Some(block.span.utf16_start + line.utf16_start)
    }

    /// Global UTF-16 position of the last caret slot in a line, before its newline.
    pub fn line_utf16_text_end(&self, block_index: usize, line_index: usize) -> Option<u32> {
        let block = self.blocks.get(block_index)?;
        let line = block.lines.get(line_index)?;
        Some(block.span.utf16_start + line.utf16_text_end())
    }

    /// A line's centre on the flow axis, for re-hit-testing at a preferred
    /// position along the line axis.
    pub fn line_flow_center(&self, block_index: usize, line_index: usize) -> Option<f32> {
        let block = self.blocks.get(block_index)?;
        let line = block.lines.get(line_index)?;
        Some(block.to_global_flow(line.flow_center()))
    }
}

/// One tile of the rendered document: a slice of exactly one block.
///
/// The identity is `(block_index, sub_index)`, and neither says anything about
/// where the block currently sits. That is the point. A tile grid laid over the
/// document has to move whenever the document grows, so an edit that adds a
/// line invalidates every tile on screen even though almost all of
/// them show text that did not change. Cutting tiles out of blocks instead
/// takes the coordinate out of the identity altogether: a block whose text is
/// untouched keeps its pixels however far the layout slides it.
///
/// `flow_start` and `flow_size` are where this tile happens to sit right now,
/// for placing the image. They are not part of what the tile is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileSpan {
    pub block_index: usize,
    /// Which slice of the block, counted from where the block starts.
    pub sub_index: u32,
    pub flow_start: u32,
    pub flow_size: u32,
}

impl TileSpan {
    pub fn flow_end(&self) -> u32 {
        self.flow_start + self.flow_size
    }
}

impl BlockPlacement {
    /// How many tiles this block is cut into.
    fn tile_count(&self, tile_flow_size: u32) -> u32 {
        if tile_flow_size == 0 {
            return 1;
        }
        (self.flow_size.max(0.0) as u32)
            .div_ceil(tile_flow_size)
            .max(1)
    }

    /// The `sub_index`th slice of this block, counted from where it starts in
    /// reading order — so slice 0 is at the low edge going forwards and at the
    /// high edge going backwards.
    ///
    /// The slices are cut to equal widths rather than to `tile_width` with a
    /// remainder, so a block a little over one tile wide becomes two halves
    /// instead of a full tile and a sliver.
    fn tile(&self, sub_index: u32, tile_flow_size: u32, order: FlowOrder) -> TileSpan {
        let count = self.tile_count(tile_flow_size);
        let sub_index = sub_index.min(count - 1);
        let edge = self.flow_start.max(0.0) as u32;
        let size = self.flow_size.max(0.0) as u32;
        // Cut at rounded fractions of the block so the slices tile it exactly.
        let cut = |slice: u32| (size as u64 * slice as u64).div_ceil(count as u64) as u32;
        let (start, end) = match order {
            FlowOrder::Ascending => (edge + cut(sub_index), edge + cut(sub_index + 1)),
            FlowOrder::Descending => (
                edge + size - cut(sub_index + 1),
                edge + size - cut(sub_index),
            ),
        };
        TileSpan {
            block_index: 0,
            sub_index,
            flow_start: start,
            flow_size: end.saturating_sub(start),
        }
    }
}

impl BlockLayoutPlan {
    /// The tiles a viewport shows, in reading order: block by block, and inside
    /// each block slice by slice.
    ///
    /// `prefetch` extends the range by that many tile widths on each side, so
    /// crossing a boundary does not stall on a rasterization the scroll is
    /// already waiting for.
    ///
    /// `viewport_flow` is Slint's negative scroll offset, matching the UI.
    pub fn visible_tiles(
        &self,
        viewport_flow: f32,
        visible_flow: f32,
        tile_flow_size: u32,
        prefetch: u32,
    ) -> Vec<TileSpan> {
        if self.blocks.is_empty() || tile_flow_size == 0 {
            return Vec::new();
        }
        let (view_start, view_end) =
            visible_flow_range(viewport_flow, visible_flow, self.total_flow_size);
        let reach = (prefetch * tile_flow_size) as f32;
        let view_start = (view_start - reach).max(0.0);
        let view_end = (view_end + reach).min(self.total_flow_size.max(1.0));

        let mut tiles = Vec::new();
        for block_index in self.blocks_in_flow_range(view_start, view_end) {
            let block = &self.blocks[block_index];
            for sub_index in 0..block.tile_count(tile_flow_size) {
                let tile = block.tile(sub_index, tile_flow_size, self.order);
                if tile.flow_size > 0
                    && (tile.flow_start as f32) < view_end
                    && tile.flow_end() as f32 > view_start
                {
                    tiles.push(TileSpan {
                        block_index,
                        ..tile
                    });
                }
            }
        }
        tiles
    }
}

/// The global flow range a viewport shows, used to clip selection geometry.
pub fn visible_flow_range(viewport_flow: f32, visible_flow: f32, total_flow: f32) -> (f32, f32) {
    let start = (-viewport_flow).max(0.0);
    let end = (start + visible_flow.max(1.0)).min(total_flow.max(1.0));
    (start, end.max(start))
}

/// A conservative upper bound for one block's flow extent, used as the layout
/// bound DirectWrite is given.
///
/// Overshooting is safe: it only leaves unused space past the end of the block's
/// own layout box, which `content_flow_start` accounts for. Undershooting would
/// make DirectWrite clip lines, so the bound is deliberately generous.
pub fn block_flow_bound(text: &str, line_extent: u32, font_size: f32) -> f32 {
    let font_size = font_size.max(1.0);
    let cells = cells_per_line(line_extent, font_size) as usize;
    let lines = text
        .split('\n')
        .map(|line| line.chars().count().div_ceil(cells).max(1))
        .sum::<usize>()
        .max(1);
    (lines as f32 * font_size * 2.2 + font_size * 4.0).ceil()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells per line at the default pane extent and font size.
    const CELLS: u32 = 20;

    fn span(text: &str, byte_start: usize, byte_end: usize) -> BlockSpan {
        BlockSpan {
            byte_start,
            byte_end,
            utf16_start: text[..byte_start].encode_utf16().count() as u32,
            utf16_end: text[..byte_end].encode_utf16().count() as u32,
        }
    }

    fn measure(flow_size: f32, lines: usize) -> BlockMeasure {
        BlockMeasure {
            flow_size,
            content_flow_start: 0.0,
            max_flow_size: flow_size,
            lines: (0..lines)
                .map(|index| LineInfo {
                    utf16_start: index as u32 * 10,
                    utf16_len: 10,
                    newline_len: 1,
                    flow_start: (lines - 1 - index) as f32 * 40.0,
                    flow_size: 40.0,
                })
                .collect(),
        }
    }

    /// Both orders, so a property that must hold either way is written once.
    const BOTH_ORDERS: [FlowOrder; 2] = [FlowOrder::Ascending, FlowOrder::Descending];

    /// Blocks of the given extents, laid out in order. Only the geometry
    /// matters here, so every block gets one throwaway line.
    fn plan_of(widths: &[f32], margin: f32, order: FlowOrder) -> BlockLayoutPlan {
        let spans = widths.iter().map(|_| span("", 0, 0)).collect::<Vec<_>>();
        let measures = widths
            .iter()
            .map(|flow_size| measure(*flow_size, 1))
            .collect::<Vec<_>>();
        place_blocks(&spans, &measures, margin, order)
    }

    #[test]
    fn splits_only_at_logical_line_boundaries() {
        let line = format!("{}\n", "あ".repeat(100));
        let text = line.repeat(20);

        let blocks = split_blocks(&text, CELLS);

        assert!(blocks.len() > 1, "a long document must produce many blocks");
        for block in &blocks {
            assert!(
                block.byte_end == text.len() || text.as_bytes()[block.byte_end - 1] == b'\n',
                "every block must end just after a newline"
            );
        }
        assert_eq!(blocks[0].byte_start, 0);
        assert_eq!(blocks.last().unwrap().byte_end, text.len());
    }

    #[test]
    fn covers_the_document_without_gaps_or_overlap() {
        let text = "一行目\n\n三行目です\n四行目\n";
        let blocks = split_blocks(text, CELLS);

        let mut byte_cursor = 0;
        let mut utf16_cursor = 0;
        for block in &blocks {
            assert_eq!(block.byte_start, byte_cursor);
            assert_eq!(block.utf16_start, utf16_cursor);
            byte_cursor = block.byte_end;
            utf16_cursor = block.utf16_end;
        }
        assert_eq!(byte_cursor, text.len());
        assert_eq!(utf16_cursor, text.encode_utf16().count() as u32);
    }

    #[test]
    fn keeps_an_oversized_logical_line_in_one_block() {
        let text = format!("{}\n短い行\n", "あ".repeat(BLOCK_MAX_CELLS as usize * 3));
        let blocks = split_blocks(&text, CELLS);

        let long_line_end = "あ".repeat(BLOCK_MAX_CELLS as usize * 3).len() + 1;
        assert_eq!(blocks[0].byte_start, 0);
        assert_eq!(
            blocks[0].byte_end, long_line_end,
            "an over-long line must not be cut in the middle"
        );
    }

    /// The bound the character budget never gave. 200 blank lines are 200
    /// characters, so they used to land in a single block that was 200 lines
    /// wide; every tile of it was redrawn on every keystroke, which is what made
    /// holding Enter get slower the longer it was held.
    ///
    /// This is also the case the maximum exists for. Blank lines are identical,
    /// so they all hash alike and not one of them can be a content-defined
    /// boundary. A run of repeated lines offers nothing to key on, and only the
    /// maximum keeps it from becoming one enormous block.
    #[test]
    fn a_run_of_blank_lines_does_not_make_one_block_unbounded() {
        let paragraph = "段落の本文です。日本語ABC123を含みます。\n\n";
        let text = format!(
            "{}{}{}",
            paragraph.repeat(4),
            "\n".repeat(200),
            paragraph.repeat(4)
        );
        let cells = |line: &str| {
            (line.trim_end_matches('\n').encode_utf16().count() as u32)
                .div_ceil(CELLS)
                .max(1)
                * CELLS
        };

        let blocks = split_blocks(&text, CELLS);

        let document: u32 = text.split_inclusive('\n').map(cells).sum();
        assert!(
            blocks.len() as u32 >= document / BLOCK_MAX_CELLS,
            "{} cells of text must not fit in {} blocks",
            document,
            blocks.len()
        );
        for block in &blocks {
            let lines = text[block.byte_start..block.byte_end]
                .split_inclusive('\n')
                .collect::<Vec<_>>();
            let total = lines.iter().copied().map(cells).sum::<u32>();
            // A block is only ever overrun by the line that closed it, and a
            // logical line is never split, so that last line is the only slack.
            let last = lines.last().copied().map(cells).unwrap_or(0);
            assert!(
                total <= BLOCK_MAX_CELLS + last,
                "a block reached {total} cells"
            );
        }
    }

    /// The property the whole content-defined boundary exists for: an edit must
    /// rewrite the block it lands in and, at worst, the one after it.
    ///
    /// With boundaries taken from a running count this failed on nearly half of
    /// the positions tried, rewriting up to 13 blocks at a time, and re-measuring
    /// them cost more than every other part of a keystroke put together.
    #[test]
    fn an_inserted_line_rewrites_at_most_the_block_it_lands_in() {
        // Lines of varied length and, importantly, varied content. Repeating one
        // paragraph would give the hash nothing to key on: identical lines all
        // decide alike, so every boundary would fall back on the maximum.
        let text = (0..40)
            .map(|n| {
                format!(
                    "## 第{n}節\n\n\
                     短い行{n}。\n\
                     これはやや長い段落{n}で、句読点や全角ＡＢＣ、半角ABC123を含み、折り返しも起きます。\n\n\
                     - 箇条書きの項目{n}\n- もう一つの項目{n}\n\n\
                     本文がもう一段落続きます{n}。日本語の文章として自然な長さを持たせています。\n\n"
                )
            })
            .collect::<String>();
        let before = split_blocks(&text, CELLS);
        assert!(before.len() > 8, "the sample must span many blocks");

        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .filter(|&index| index < text.len());
        for cut in line_starts.step_by(3) {
            let edited = format!("{}\n{}", &text[..cut], &text[cut..]);
            let after = split_blocks(&edited, CELLS);

            let rewritten = (0..before.len().min(after.len()))
                .filter(|&index| {
                    text[before[index].byte_start..before[index].byte_end]
                        != edited[after[index].byte_start..after[index].byte_end]
                })
                .count()
                + before.len().abs_diff(after.len());
            assert!(
                rewritten <= 2,
                "inserting a line at byte {cut} rewrote {rewritten} of {} blocks",
                before.len()
            );
        }
    }

    #[test]
    fn produces_one_block_for_empty_text() {
        let blocks = split_blocks("", CELLS);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].utf16_end, 0);
    }

    #[test]
    fn places_the_first_block_at_the_right_edge() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];

        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Descending);

        assert_eq!(plan.total_flow_size, 30.0 + 350.0 + 30.0);
        assert_eq!(plan.blocks[0].flow_start, 30.0 + 250.0);
        assert_eq!(plan.blocks[1].flow_start, 30.0 + 50.0);
        assert_eq!(plan.blocks[2].flow_start, 30.0);
        assert_eq!(plan.blocks[0].flow_end(), plan.total_flow_size - 30.0);
    }

    /// The same stacking with the flow axis running the other way, which is what
    /// horizontal writing needs: the first block is at the top of the pane.
    #[test]
    fn places_the_first_block_at_the_low_edge_going_forwards() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];

        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Ascending);

        assert_eq!(plan.total_flow_size, 30.0 + 350.0 + 30.0);
        assert_eq!(plan.blocks[0].flow_start, 30.0);
        assert_eq!(plan.blocks[1].flow_start, 30.0 + 100.0);
        assert_eq!(plan.blocks[2].flow_start, 30.0 + 300.0);
        assert_eq!(plan.blocks[2].flow_end(), plan.total_flow_size - 30.0);
    }

    /// A block's placed geometry must depend on nothing but its own
    /// measurement, or a tile cut from it would change when a distant block did.
    #[test]
    fn a_block_is_placed_on_whole_pixels_and_ignores_its_neighbours() {
        for order in BOTH_ORDERS {
            let widths = [105.7, 35.4, 62.9, 35.4];
            let plan = plan_of(&widths, 33.4, order);

            assert_eq!(plan.margin, 33.0, "the margin is rounded too, so x can be");
            for block in &plan.blocks {
                assert_eq!(
                    block.flow_start,
                    block.flow_start.round(),
                    "block left edge {}",
                    block.flow_start
                );
                assert_eq!(block.flow_size, block.flow_size.round(), "block width");
            }
            for pair in plan.blocks.windows(2) {
                let (earlier, later) = match order {
                    FlowOrder::Ascending => (pair[0].flow_end(), pair[1].flow_start),
                    FlowOrder::Descending => (pair[1].flow_end(), pair[0].flow_start),
                };
                assert_eq!(earlier, later, "blocks must still abut exactly ({order:?})");
            }
            assert_eq!(
                plan.blocks.iter().map(|b| b.exact_flow_size).sum::<f32>(),
                widths.iter().sum::<f32>(),
                "the true measurement must survive the rounding"
            );

            // Resize the last block. Every other block keeps its extent, which is
            // what lets their tiles keep their pixels.
            let mut moved = widths;
            moved[3] += 36.0;
            let after = plan_of(&moved, 33.4, order);
            for (before, after) in plan.blocks.iter().zip(&after.blocks).take(3) {
                assert_eq!(before.flow_size, after.flow_size);
            }
        }
    }

    #[test]
    fn finds_the_block_covering_a_global_flow_coordinate() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];
        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Descending);

        assert_eq!(plan.block_at_flow(300.0), 0);
        assert_eq!(plan.block_at_flow(280.0), 0);
        assert_eq!(plan.block_at_flow(200.0), 1);
        assert_eq!(plan.block_at_flow(80.0), 1);
        assert_eq!(plan.block_at_flow(40.0), 2);
        assert_eq!(plan.block_at_flow(-100.0), 2, "clamps below the content");
        assert_eq!(plan.block_at_flow(9_999.0), 0, "clamps above the content");
    }

    #[test]
    fn finds_the_block_covering_a_global_flow_coordinate_going_forwards() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];
        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Ascending);

        // Block 0 covers [30, 130), block 1 covers [130, 330), block 2 covers [330, 380).
        assert_eq!(plan.block_at_flow(30.0), 0);
        assert_eq!(plan.block_at_flow(129.0), 0);
        assert_eq!(plan.block_at_flow(130.0), 1);
        assert_eq!(plan.block_at_flow(329.0), 1);
        assert_eq!(plan.block_at_flow(330.0), 2);
        assert_eq!(plan.block_at_flow(-100.0), 0, "clamps below the content");
        assert_eq!(plan.block_at_flow(9_999.0), 2, "clamps above the content");
    }

    #[test]
    fn selects_only_the_blocks_intersecting_the_viewport() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];
        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Descending);

        // Block 0 covers [280, 380), block 1 covers [80, 280), block 2 covers [30, 80).
        assert_eq!(plan.blocks_in_flow_range(290.0, 400.0), 0..1);
        assert_eq!(plan.blocks_in_flow_range(100.0, 200.0), 1..2);
        assert_eq!(
            plan.blocks_in_flow_range(60.0, 90.0),
            1..3,
            "a viewport straddling a block edge needs both blocks"
        );
        assert_eq!(plan.blocks_in_flow_range(0.0, 400.0), 0..3);
        assert_eq!(plan.blocks_in_flow_range(400.0, 400.0), 0..0);
    }

    #[test]
    fn selects_only_the_blocks_intersecting_the_viewport_going_forwards() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(100.0, 2), measure(200.0, 4), measure(50.0, 1)];
        let plan = place_blocks(&spans, &measures, 30.0, FlowOrder::Ascending);

        // Block 0 covers [30, 130), block 1 covers [130, 330), block 2 covers [330, 380).
        assert_eq!(plan.blocks_in_flow_range(0.0, 40.0), 0..1);
        assert_eq!(plan.blocks_in_flow_range(140.0, 300.0), 1..2);
        assert_eq!(
            plan.blocks_in_flow_range(120.0, 140.0),
            0..2,
            "a viewport straddling a block edge needs both blocks"
        );
        assert_eq!(plan.blocks_in_flow_range(0.0, 400.0), 0..3);
        assert_eq!(
            plan.blocks_in_flow_range(380.0, 400.0),
            3..3,
            "past the last block nothing is left to draw"
        );
    }

    #[test]
    fn selects_only_the_blocks_intersecting_a_utf16_range() {
        let text = "アイウ\nエオカ\nキクケ\n";
        let spans = [
            span(text, 0, 10),
            span(text, 10, 20),
            span(text, 20, text.len()),
        ];
        let measures = [measure(100.0, 1), measure(100.0, 1), measure(100.0, 1)];
        let plan = place_blocks(&spans, &measures, 0.0, FlowOrder::Descending);

        assert_eq!(plan.blocks_in_utf16_range(0, 1), 0..1);
        assert_eq!(plan.blocks_in_utf16_range(4, 9), 1..3);
        assert_eq!(plan.blocks_in_utf16_range(5, 5), 0..0);
    }

    #[test]
    fn steps_across_a_block_boundary_without_scanning() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(80.0, 2), measure(80.0, 2)];
        let plan = place_blocks(&spans, &measures, 0.0, FlowOrder::Descending);

        assert_eq!(plan.step_line(0, 0, true), Some((0, 1)));
        assert_eq!(
            plan.step_line(0, 1, true),
            Some((1, 0)),
            "stepping off the last line enters the next block"
        );
        assert_eq!(plan.step_line(1, 0, false), Some((0, 1)));
        assert_eq!(
            plan.step_line(0, 0, false),
            None,
            "nothing before the first line"
        );
        assert_eq!(
            plan.step_line(1, 1, true),
            None,
            "nothing after the last line"
        );
    }

    #[test]
    fn steps_over_a_block_that_has_no_columns() {
        let text = "";
        let spans = [span(text, 0, 0), span(text, 0, 0), span(text, 0, 0)];
        let measures = [measure(80.0, 1), measure(0.0, 0), measure(80.0, 1)];
        let plan = place_blocks(&spans, &measures, 0.0, FlowOrder::Descending);

        assert_eq!(plan.step_line(0, 0, true), Some((2, 0)));
        assert_eq!(plan.step_line(2, 0, false), Some((0, 0)));
    }

    #[test]
    fn locates_the_column_holding_a_position() {
        let text = "";
        let spans = [span(text, 0, 0)];
        // measure() already lays the lines out at 0, 10 and 20.
        let measures = [measure(120.0, 3)];
        let plan = place_blocks(&spans, &measures, 0.0, FlowOrder::Descending);

        assert_eq!(plan.locate(0), Some((0, 0)));
        assert_eq!(plan.locate(9), Some((0, 0)));
        assert_eq!(plan.locate(10), Some((0, 1)));
        assert_eq!(plan.locate(25), Some((0, 2)));
    }

    #[test]
    fn reports_a_column_end_before_its_newline() {
        let line = LineInfo {
            utf16_start: 4,
            utf16_len: 6,
            newline_len: 1,
            flow_start: 0.0,
            flow_size: 40.0,
        };

        assert_eq!(line.utf16_end(), 10);
        assert_eq!(line.utf16_text_end(), 9);
    }

    #[test]
    fn converts_between_global_and_block_layout_coordinates() {
        let text = "";
        let spans = [span(text, 0, 0)];
        let measures = [BlockMeasure {
            flow_size: 100.0,
            content_flow_start: 400.0,
            max_flow_size: 500.0,
            lines: Rc::from(Vec::new()),
        }];
        let plan = place_blocks(&spans, &measures, 20.0, FlowOrder::Descending);
        let block = &plan.blocks[0];

        assert_eq!(block.flow_start, 20.0);
        assert_eq!(block.draw_origin(), -380.0);
        assert_eq!(block.to_layout_flow(20.0), 400.0);
        assert_eq!(block.to_global_flow(400.0), 20.0);
    }

    #[test]
    fn reports_only_the_columns_a_viewport_shows() {
        let text = "";
        let spans = [span(text, 0, 0)];
        // measure() lays out 4 lines of 40px, in reverse order, 10 units each.
        let measures = [measure(160.0, 4)];
        let plan = place_blocks(&spans, &measures, 0.0, FlowOrder::Descending);
        let block = &plan.blocks[0];

        // Line 0 sits at global [120, 160), line 3 at [0, 40).
        assert_eq!(block.visible_utf16_range(120.0, 160.0), Some((0, 10)));
        assert_eq!(block.visible_utf16_range(0.0, 40.0), Some((30, 40)));
        // [70, 130) clips into lines 0, 1 and 2, which cover units 0..30.
        assert_eq!(block.visible_utf16_range(70.0, 130.0), Some((0, 30)));
        assert_eq!(block.visible_utf16_range(0.0, 160.0), Some((0, 40)));
        assert_eq!(
            block.visible_utf16_range(400.0, 500.0),
            None,
            "a block scrolled off screen contributes nothing"
        );
    }

    #[test]
    fn tiles_cut_each_block_and_cover_it_exactly_once() {
        for order in BOTH_ORDERS {
            // Block 0 is wider than a tile, so it is cut; the other two are not.
            let plan = plan_of(&[2500.0, 1000.0, 700.0], 30.0, order);
            let all = plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0);

            assert_eq!(
                all.first().map(|tile| (tile.block_index, tile.sub_index)),
                Some((0, 0)),
                "the tiles come back in reading order ({order:?})"
            );
            let first = all.first().copied().unwrap();
            let block = &plan.blocks[0];
            match order {
                // Slice 0 is cut from the edge the block is read from.
                FlowOrder::Ascending => assert_eq!(first.flow_start, block.flow_start as u32),
                FlowOrder::Descending => assert_eq!(first.flow_end(), block.flow_end() as u32),
            }
            assert_eq!(
                all.iter().filter(|tile| tile.block_index == 0).count(),
                3,
                "2500px at 1024px per tile is three slices"
            );
            assert_eq!(
                all.iter()
                    .filter(|tile| tile.block_index == 2)
                    .map(|tile| tile.flow_size)
                    .collect::<Vec<_>>(),
                vec![700],
                "a block narrower than a tile is one whole tile"
            );
            assert_eq!(
                all.iter().map(|tile| tile.flow_size).sum::<u32>(),
                4200,
                "the tiles must cover the blocks exactly once"
            );
            for pair in all.windows(2) {
                let (earlier, later) = match order {
                    FlowOrder::Ascending => (pair[0].flow_end(), pair[1].flow_start),
                    FlowOrder::Descending => (pair[1].flow_end(), pair[0].flow_start),
                };
                assert_eq!(earlier, later, "tiles must abut without gaps ({order:?})");
            }
        }
    }

    #[test]
    fn keeps_only_visible_tiles_for_a_long_document() {
        for order in BOTH_ORDERS {
            let plan = plan_of(&[900.0; 40], 0.0, order);

            let at_start = plan.visible_tiles(0.0, 640.0, 1024, 0);
            assert_eq!(at_start.len(), 1);
            let expected = match order {
                FlowOrder::Ascending => 0,
                FlowOrder::Descending => 39,
            };
            assert_eq!(
                at_start[0].block_index, expected,
                "the block at the origin end of the flow axis ({order:?})"
            );

            let middle = plan.visible_tiles(-18_000.0, 640.0, 1024, 0);
            assert!(middle.len() <= 2);
            assert!(
                middle
                    .iter()
                    .any(|tile| tile.flow_start <= 18_000 && tile.flow_end() > 18_000)
            );
        }
    }

    /// The point of cutting tiles out of blocks: adding a line to one block
    /// slides half the document sideways, and none of the other blocks' tiles may
    /// change identity or size, because none of their pixels changed.
    ///
    /// Which half slides is the one thing the order decides. The document is
    /// anchored at the end it starts from, so going backwards the blocks *before*
    /// the edit move and going forwards the ones *after* it do.
    #[test]
    fn widening_one_block_leaves_every_other_block_s_tiles_alone() {
        for order in BOTH_ORDERS {
            let line = 36.0;
            let mut widths = [900.0; 10];
            let before = plan_of(&widths, 30.0, order);
            widths[5] += line;
            let after = plan_of(&widths, 30.0, order);

            let tiles_of =
                |plan: &BlockLayoutPlan| plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0);
            let (before, after) = (tiles_of(&before), tiles_of(&after));
            assert_eq!(before.len(), after.len());

            for (b, a) in before.iter().zip(&after) {
                assert_eq!(
                    (a.block_index, a.sub_index),
                    (b.block_index, b.sub_index),
                    "the tile identity must not depend on where the block sits"
                );
                if a.block_index == 5 {
                    continue;
                }
                assert_eq!(
                    a.flow_size, b.flow_size,
                    "an untouched block keeps its slicing"
                );
                let moves = match order {
                    FlowOrder::Ascending => a.block_index > 5,
                    FlowOrder::Descending => a.block_index < 5,
                };
                let shift = if moves { line as u32 } else { 0 };
                assert_eq!(
                    a.flow_start,
                    b.flow_start + shift,
                    "only the blocks past the edit move ({order:?})"
                );
            }
        }
    }

    /// The two orders are one layout seen from opposite ends. Any placement,
    /// slicing or search that holds for one and not the other is a direction bug,
    /// and this catches it without a second set of expected coordinates.
    #[test]
    fn the_two_orders_are_mirror_images_of_each_other() {
        let widths = [900.0, 2500.0, 640.0, 1000.0];
        let forwards = plan_of(&widths, 30.0, FlowOrder::Ascending);
        let backwards = plan_of(&widths, 30.0, FlowOrder::Descending);
        let total = forwards.total_flow_size;

        assert_eq!(total, backwards.total_flow_size);
        for (ahead, back) in forwards.blocks.iter().zip(&backwards.blocks) {
            assert_eq!(ahead.flow_size, back.flow_size);
            assert_eq!(ahead.flow_start, total - back.flow_end());
        }

        let tiles_of =
            |plan: &BlockLayoutPlan| plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0);
        let (ahead, back) = (tiles_of(&forwards), tiles_of(&backwards));
        assert_eq!(ahead.len(), back.len());
        for (ahead, back) in ahead.iter().zip(&back) {
            assert_eq!(
                (ahead.block_index, ahead.sub_index),
                (back.block_index, back.sub_index),
                "tiles come back in reading order either way"
            );
            assert_eq!(ahead.flow_size, back.flow_size);
            assert_eq!(ahead.flow_start, total as u32 - back.flow_end());
        }

        // Points well inside a block, since a mirrored half-open interval is
        // closed at the other end and the two disagree on an exact edge.
        for flow in [100.0, 1000.0, 3500.0, 4500.0] {
            assert_eq!(
                forwards.block_at_flow(flow),
                backwards.block_at_flow(total - flow),
                "the same distance into the document is the same block"
            );
        }
    }

    #[test]
    fn prefetches_one_tile_on_each_side_without_leaving_the_document() {
        for order in BOTH_ORDERS {
            let plan = plan_of(&[900.0; 40], 0.0, order);

            let plain = plan.visible_tiles(-18_000.0, 640.0, 1024, 0);
            let prefetched = plan.visible_tiles(-18_000.0, 640.0, 1024, 1);
            assert!(prefetched.len() > plain.len());
            assert!(
                prefetched
                    .iter()
                    .all(|tile| tile.flow_end() <= plan.total_flow_size as u32)
            );
            assert!(
                plan.visible_tiles(0.0, 640.0, 1024, 1)
                    .iter()
                    .all(|tile| tile.flow_size > 0)
            );
        }
    }

    #[test]
    fn clips_the_visible_range_to_the_content() {
        assert_eq!(visible_flow_range(0.0, 640.0, 10_000.0), (0.0, 640.0));
        assert_eq!(
            visible_flow_range(-2500.0, 640.0, 10_000.0),
            (2500.0, 3140.0)
        );
        assert_eq!(
            visible_flow_range(-9800.0, 640.0, 10_000.0),
            (9800.0, 10_000.0)
        );
    }

    #[test]
    fn bounds_a_block_width_above_its_column_count() {
        let text = "あ".repeat(100);
        let bound = block_flow_bound(&text, 520, 22.0);

        let usable = 520.0_f32 - 22.0 * 3.0;
        let cells = (usable / 22.0).floor() as usize;
        let lines = 100_usize.div_ceil(cells);
        assert!(
            bound > lines as f32 * 22.0 * 1.7,
            "the bound must exceed the width DirectWrite actually needs"
        );
    }
}
