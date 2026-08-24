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

/// Deepest Markdown heading level, and so the number of heading sizes a
/// [`Typography`] carries.
pub const MAX_HEADING_LEVEL: usize = 6;

/// How the document is set: the base size, and the three quantities the
/// requirement asks to be free — the advance between characters, the advance
/// between lines, and the size of a heading.
///
/// Every field but `font_size` is a ratio, so one spec reads the same at any
/// zoom. The engine holds a single spec for the whole document; the only thing
/// that varies from line to line is the heading level, and that comes from the
/// line itself rather than from anything before it.
/// **Not `Copy`.** It carries the four font family names (要件 9), and a name
/// is a string; everything that reads a spec takes it by reference anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct Typography {
    /// Body size in pixels.
    pub font_size: f32,
    /// Extra advance per character along the line axis, as a fraction of that
    /// character's own size. `0.0` leaves DirectWrite's own advance.
    pub character_spacing: f32,
    /// Multiplier on each line's own advance along the flow axis. `1.0` leaves
    /// DirectWrite's own line height. A multiplier rather than a fixed pitch,
    /// because a line that genuinely needs more room still has to get it (4.2).
    pub line_spacing: f32,
    /// Size multiplier per heading level, index 0 being level 1. Read as given:
    /// nothing here assumes the sizes descend, or that they exceed body size.
    pub heading_scale: [f32; MAX_HEADING_LEVEL],
    /// The ink and the paper (要件 9), as sRGB channels from 0 to 1.
    ///
    /// **Kept here, in the module that knows nothing about Windows**, because
    /// they are part of how the document is set: the same spec that decides how
    /// large a heading is decides what colour it is drawn in. What turns them
    /// into a Direct2D colour is the renderer's business.
    ///
    /// Both are this sheet's: a pane writing the other way has a paper of its
    /// own (要件 9).
    pub ink: [f32; 3],
    pub paper: [f32; 3],
    /// The families 要件 9 asks to be free.
    ///
    /// **One spec is one writing direction's** (要件 9, revised 2026-08-22), so
    /// nothing here says which way the text runs: the vertical body font is the
    /// `body_font` of the vertical sheet. A heading has a family of its own per
    /// level, beside its own size and its own ink — what is being set is that
    /// heading, and those are three sides of it.
    ///
    /// An empty name means "whatever DirectWrite would have chosen", which is
    /// what a name nobody has set looks like.
    pub body_font: String,
    pub heading_font: [String; MAX_HEADING_LEVEL],
    pub code_font: String,
    /// The ink of each heading level, index 0 being H1 (要件 9).
    ///
    /// **A heading is a different colour as readily as it is a different
    /// size**, and the two are the same kind of decision — so they are set the
    /// same way, one value per level. A level set to the body's ink simply
    /// looks like body text, which is what every level starts as.
    pub heading_ink: [[f32; 3]; MAX_HEADING_LEVEL],
}

/// What the editor sets text in until the writer says otherwise (要件 9).
///
/// **The values the editor already used**, not the ones the design proposes —
/// changing what is on screen is the writer's to do now that these are
/// settings, and a default that moves under them is not an improvement.
pub const DEFAULT_BODY_FONT: &str = "Yu Mincho";
pub const DEFAULT_HEADING_FONT: &str = "Yu Mincho";
/// Something the body font is not. Whatever it lacks — every Japanese glyph, in
/// Consolas' case — DirectWrite falls back for, so this narrows the Latin and
/// leaves the rest.
pub const DEFAULT_CODE_FONT: &str = "Consolas";

/// The ink `doc-ink` in `ui/tokens.slint`: the one colour in the app with no
/// purple in it, because it is the one a reader looks at for an hour.
pub const DEFAULT_INK: [f32; 3] = [36.0 / 255.0, 33.0 / 255.0, 30.0 / 255.0];
/// `paper`, the horizontal sheet's.
pub const DEFAULT_PAPER: [f32; 3] = [252.0 / 255.0, 249.0 / 255.0, 239.0 / 255.0];
/// `paper-alt`, the vertical sheet's. **A shade deeper on purpose**: the two
/// panes differ just enough to answer 「どちらの向きで書いているか」 without a
/// word being read. It is a default now rather than a rule — each sheet has a
/// paper of its own, and the writer may set them the same.
pub const DEFAULT_VERTICAL_PAPER: [f32; 3] = [249.0 / 255.0, 244.0 / 255.0, 226.0 / 255.0];

impl Default for Typography {
    fn default() -> Self {
        Self::new(16.0)
    }
}

impl Typography {
    /// DirectWrite's own metrics at this size: no added advance anywhere, and
    /// headings set at body size.
    pub fn new(font_size: f32) -> Self {
        Self {
            font_size: font_size.max(1.0),
            character_spacing: 0.0,
            line_spacing: 1.0,
            heading_scale: [1.0; MAX_HEADING_LEVEL],
            body_font: DEFAULT_BODY_FONT.to_owned(),
            heading_font: [const { String::new() }; MAX_HEADING_LEVEL]
                .map(|_| DEFAULT_HEADING_FONT.to_owned()),
            code_font: DEFAULT_CODE_FONT.to_owned(),
            ink: DEFAULT_INK,
            paper: DEFAULT_PAPER,
            heading_ink: [DEFAULT_INK; MAX_HEADING_LEVEL],
        }
    }

    /// The family the body is set in, or the fallback when nobody has said.
    pub fn body_family(&self) -> &str {
        if self.body_font.is_empty() {
            DEFAULT_BODY_FONT
        } else {
            &self.body_font
        }
    }

    /// The family a line at this heading level is set in. Level 0 is body text,
    /// and so is any level past the deepest one.
    pub fn family_for(&self, heading_level: u8) -> &str {
        if heading_level == 0 {
            return self.body_family();
        }
        match self.heading_font.get(heading_level as usize - 1) {
            Some(family) if !family.is_empty() => family,
            _ => self.body_family(),
        }
    }

    /// The ink a line at this heading level is drawn in. Level 0 is body text,
    /// and so is any level past the deepest one.
    pub fn ink_for(&self, heading_level: u8) -> [f32; 3] {
        if heading_level == 0 {
            return self.ink;
        }
        self.heading_ink
            .get(heading_level as usize - 1)
            .copied()
            .unwrap_or(self.ink)
    }

    /// Headings running from `top` at level 1 down towards body size at the
    /// deepest level.
    ///
    /// **Only the tests use this.** It was the one knob the panel had; 要件 9
    /// asks for the six levels to be set individually, so the window now writes
    /// all six and the engine reads them as given. An evenly spaced ramp is
    /// still a useful thing for a test to say in one line, which is why it is
    /// here and why it is not compiled into the program.
    #[cfg(test)]
    pub fn with_heading_ramp(mut self, top: f32) -> Self {
        let steps = MAX_HEADING_LEVEL as f32;
        for (index, scale) in self.heading_scale.iter_mut().enumerate() {
            *scale = 1.0 + (top - 1.0) * (steps - index as f32) / steps;
        }
        self
    }

    /// The font size multiplier of a line at this heading level. Level 0 is body
    /// text, and so is any level past the deepest one.
    pub fn size_scale(&self, heading_level: u8) -> f32 {
        if heading_level == 0 {
            return 1.0;
        }
        self.heading_scale
            .get(heading_level as usize - 1)
            .copied()
            .unwrap_or(1.0)
            .max(0.1)
    }

    /// The flow-axis multiplier of a line at this heading level. Both the
    /// line's own size and the line spacing stretch its advance.
    pub fn flow_scale(&self, heading_level: u8) -> f32 {
        self.size_scale(heading_level) * self.line_spacing.max(0.1)
    }

    /// The line-axis advance of one body character, spacing included.
    pub fn cell_advance(&self) -> f32 {
        (self.font_size * (1.0 + self.character_spacing)).max(1.0)
    }

    /// One step of indenting (要件 7.3.2).
    ///
    /// **The same two cells everywhere it is used**: the box standing over a
    /// list marker, and each level a blockquote sets its block in by. A quoted
    /// item and a plain one then begin under the same rule.
    pub fn indent_step(&self) -> f32 {
        (self.cell_advance() * 2.0).max(1.0)
    }
}

/// How one logical line is set, beyond what its own characters say.
///
/// One value per logical line, and a block reads only its own slice — the same
/// arrangement the heading level arrived in, widened rather than duplicated.
///
/// **Not every attribute here is decided by the line alone.** A heading is: the
/// marker is in the line. Beginning a paragraph is not — it depends on the line
/// before being blank. That is settled once, over the document, when the values
/// are built (`document.rs`); by the time a block sees them they are per-line
/// facts like any other, and the block still depends on nothing outside itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineStyle {
    /// Markdown heading level, 0 for body text.
    pub heading_level: u8,
    /// What the whole line is (要件 7.3.2).
    pub kind: LineKind,
    /// How many blockquote markers the line carries, 0 for a line that is not
    /// quoted. **At most one for now**, because the preview takes one marker
    /// off and no more; the field is a count so that deeper quoting is a
    /// change to one rule rather than to the shape of this.
    pub quote_depth: u8,
}

/// What a whole logical line is, beyond the size its heading marker asks for
/// (要件 7.3.2).
///
/// **One kind per line rather than a set of flags**, which is what separates
/// this from [`Marks`]: marks combine within a line, kinds do not. A line
/// inside a fence is code even when it begins with `-`, and a rule holds
/// nothing at all. What does combine — being quoted — is counted beside this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LineKind {
    #[default]
    Body,
    /// `-`, `*` or `+` followed by a space.
    Bullet,
    /// `1.` or `1)` followed by a space.
    Ordered,
    /// A bullet whose item begins with `[ ]` or `[x]`.
    Task { done: bool },
    /// `---`, `***` or `___` on a line of its own.
    Rule,
    /// The fence that opens or closes a code block.
    Fence,
    /// A line inside a fenced code block.
    Code,
}

impl LineKind {
    /// Whether the line is set in the code family, and shown exactly as it was
    /// written.
    ///
    /// **The fence counts as code.** It is part of the block it delimits, and
    /// setting it as body text would leave one proportional line at each end of
    /// every code block.
    pub fn is_code(self) -> bool {
        matches!(self, Self::Fence | Self::Code)
    }
}

/// How a stretch of text is marked, beyond the size its line is set at
/// (要件 7.3.2).
///
/// **Four independent flags rather than one kind**, because they combine: the
/// inside of `**太字の*ここだけ*斜体**` is both. Nothing here says anything about
/// geometry — a marked stretch takes the space its glyphs take, and no line
/// moves because of one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// What is drawn in place of the marker a box stands over (要件 7.3.2).
///
/// **The box is the space and this is the ink.** The box hides the marker's own
/// glyphs and reserves one width for every kind of marker, so `-`, `10.` and
/// `- [x]` all set their text at the same indent (技術検証 4.12); what appears
/// in that space is drawn in the tile pass, where the render target already is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ornament {
    /// A bullet, in place of `-`, `*` or `+`.
    Bullet,
    /// An empty checkbox, in place of `- [ ]`.
    TaskOpen,
    /// A ticked one, in place of `- [x]`.
    TaskDone,
    /// **The number the box stands over, drawn again.** A bullet may be
    /// replaced by one glyph for every list in the document, but `10.` says
    /// something `9.` does not, so what is drawn here is the boxed range's own
    /// text.
    Number,
    /// **Nothing at all, in place of `---`.** This box is here to hide the
    /// marks; the stroke drawn across them is a [`LineRun`], because it
    /// crosses the whole page rather than sitting at the head of the line.
    ///
    /// A blockquote has no box of its own — its indent belongs to the block
    /// (`BlockSpan::quote_depth`), and the preview takes its marker off.
    Rule,
}

impl Ornament {
    /// Whether anything is drawn inside the box, as against the box being
    /// there only to hide what it covers.
    ///
    /// Asked before the hit test that finds where to draw, so a document of
    /// rules never asks DirectWrite about a rectangle nothing goes into.
    pub fn draws_ink(self) -> bool {
        !matches!(self, Self::Rule)
    }
}

/// The marker at the head of one logical line, and how wide it is
/// (要件 7.3.2).
///
/// **Always at the head**: the preview takes the blockquote marker off, so a
/// list marker begins the line whether or not it was quoted — and one level of
/// quoting is the block's indent, not the marker's. The length is in UTF-16
/// units of the line **as the preview shows it**, which is what a DirectWrite
/// range is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineMarker {
    pub utf16_len: u32,
    pub ornament: Ornament,
}

impl Marks {
    /// A stretch set in the code family and marked nothing else.
    pub fn code() -> Self {
        Self {
            code: true,
            ..Self::default()
        }
    }
}

/// One marked stretch of one logical line (要件 7.3.2).
///
/// **Offsets into the line, not the document**, for the reason [`StyleRun`] is
/// block-local: what a line's markers enclose depends on that line alone, so
/// nothing about it is recomputed when something before it changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Emphasis {
    pub utf16_start: u32,
    pub utf16_len: u32,
    pub marks: Marks,
}

impl LineStyle {
    /// **Only the tests build one this way.** Every line the editor sets comes
    /// from `document::line_styles`, which decides the kind and the quoting
    /// beside the level.
    #[cfg(test)]
    pub fn heading(level: u8) -> Self {
        Self {
            heading_level: level,
            ..Self::default()
        }
    }

    pub fn of_kind(kind: LineKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Whether the preview shows this line exactly as it was written, markers
    /// and all — which is what a fenced block and a rule both mean
    /// (要件 7.3.2).
    ///
    /// **A rule is literal for the same reason code is**: `***` is the line's
    /// own marks, not emphasis wrapped around nothing, and letting the preview
    /// read it as emphasis leaves one asterisk where three were written. The
    /// box that hides a rule is measured on the line the preview shows, so the
    /// two have to agree about how long that line is.
    pub fn is_literal(&self) -> bool {
        self.kind.is_code() || matches!(self.kind, LineKind::Rule)
    }
}

/// A document together with the per-logical-line attributes its text does not
/// carry.
///
/// The vertical pane lays out the *preview*, where heading markers have already
/// been removed, so a block cannot tell from its own characters that it holds a
/// heading. The levels travel alongside instead, one entry per logical line.
/// Blocks only ever cut at logical line boundaries, so a block's entries are a
/// contiguous slice of this one — the attributes stay as block-local as the text
/// is, and nothing about them accumulates from the start of the document.
#[derive(Debug, Clone, Copy, Default)]
pub struct StyledText<'a> {
    pub text: &'a str,
    /// How each logical line is set. May be shorter than the text has lines; a
    /// line past the end is plain body text.
    pub lines: &'a [LineStyle],
    /// What is marked inside each logical line (要件 7.3.2). Indexed the same
    /// way as `lines`, and empty for text nobody has worked the markers out
    /// for — the source panes, where the markers are still in the text and
    /// nothing has been taken out to mark.
    pub spans: &'a [Vec<Emphasis>],
    /// The marker standing at the head of each logical line (要件 7.3.2).
    /// Indexed the same way, and empty wherever `spans` is empty: a box hides
    /// the marker's glyphs, so it belongs exactly where the preview is already
    /// hiding things and nowhere else.
    pub markers: &'a [Option<LineMarker>],
}

impl<'a> StyledText<'a> {
    /// Text set as plain body throughout.
    pub fn plain(text: &'a str) -> Self {
        Self {
            text,
            lines: &[],
            spans: &[],
            markers: &[],
        }
    }

    pub fn new(text: &'a str, lines: &'a [LineStyle]) -> Self {
        Self {
            text,
            lines,
            spans: &[],
            markers: &[],
        }
    }

    /// Text whose lines also carry what is marked inside them (要件 7.3.2).
    pub fn marked(text: &'a str, lines: &'a [LineStyle], spans: &'a [Vec<Emphasis>]) -> Self {
        Self {
            text,
            lines,
            spans,
            markers: &[],
        }
    }

    /// And what stands at the head of each of them (要件 7.3.2).
    ///
    /// Separate from [`Self::marked`] because the two are decided in different
    /// places, not because they travel apart: a pane that has one has the
    /// other.
    pub fn with_markers(mut self, markers: &'a [Option<LineMarker>]) -> Self {
        self.markers = markers;
        self
    }

    pub fn style_at(&self, line_index: usize) -> LineStyle {
        self.lines.get(line_index).copied().unwrap_or_default()
    }

    pub fn level_at(&self, line_index: usize) -> u8 {
        self.style_at(line_index).heading_level
    }

    /// What one logical line is (要件 7.3.2).
    pub fn kind_at(&self, line_index: usize) -> LineKind {
        self.style_at(line_index).kind
    }

    /// What is marked inside one logical line, and nothing for a line nobody
    /// worked out.
    pub fn marks_at(&self, line_index: usize) -> &'a [Emphasis] {
        match self.spans.get(line_index) {
            Some(spans) => spans.as_slice(),
            None => &[],
        }
    }

    /// The marker standing at the head of one logical line (要件 7.3.2).
    pub fn marker_at(&self, line_index: usize) -> Option<LineMarker> {
        self.markers.get(line_index).copied().flatten()
    }

    /// Whether this is what a preview pane shows, as against a source pane.
    ///
    /// **The markers are the signal**, and only a preview has any: on a source
    /// pane the markers are the characters being edited (要件 7.3.1).
    /// **Everything that stands in for markup follows this** — the boxes over
    /// the head of a line, the indent a quote sets its block in, the bar beside
    /// it and the stroke across a rule. Drawn on the source they would double
    /// the very characters the writer is reading, and the indent would move the
    /// markup itself.
    pub fn is_preview(&self) -> bool {
        !self.markers.is_empty()
    }
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
    /// How deeply the whole block is quoted, 0 for a block that is not
    /// (要件 7.3.2).
    ///
    /// **A block, not a line.** The indent has to move every visual line a
    /// quoted paragraph wrapped to, and the only thing that can do that is the
    /// layout box the block is set in — DirectWrite has no per-paragraph
    /// indent, and the box at the head of a line reaches the head only
    /// (技術検証 7.1). That is why a change of quoting ends a block.
    pub quote_depth: u8,
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

/// How many body characters fit in one line at this geometry.
///
/// `line_extent` is the pane's size along the line axis: its height in vertical
/// writing, its width in horizontal writing.
///
/// The layout box is the pane less a margin at each end, and the margin is one
/// and a half times the font size, so three font sizes come off the extent. What
/// divides into the rest is the *advance*, not the size, so widening the
/// character spacing fits fewer characters in the same pane.
pub fn cells_per_line(line_extent: u32, typography: &Typography) -> u32 {
    let font_size = typography.font_size.max(1.0);
    let advance = typography.cell_advance();
    let usable = (line_extent as f32 - font_size * 3.0).max(advance);
    (usable / advance).floor().max(1.0) as u32
}

/// The flow space one logical line is charged, in cells of body line space.
///
/// A heading is charged twice over, and the two are different quantities. It
/// fits fewer characters per line, so it wraps into more lines; and each of
/// those lines advances further. Charging only for the second would let a page
/// of headings quietly reach much further along the flow axis than a page of
/// body text with the same block budget.
fn line_cells(
    characters: u32,
    cells_per_line: u32,
    typography: &Typography,
    style: LineStyle,
) -> u32 {
    let level = style.heading_level;
    let cells_per_line = cells_per_line.max(1);
    let size_scale = typography.size_scale(level);
    let fitting = (cells_per_line as f32 / size_scale).floor().max(1.0) as u32;
    let wrapped = characters.div_ceil(fitting).max(1);
    let charged = wrapped as f32 * cells_per_line as f32 * typography.flow_scale(level);
    charged.ceil().clamp(1.0, u32::MAX as f32) as u32
}

/// Where a run of text wraps when it is laid out on its own.
///
/// A block boundary must be a position where a line starts, or the block after
/// it begins mid-line and every line in it is drawn in the wrong place. A hard
/// break is always such a position, which is why the split could ignore this
/// until now. Cutting *inside* a logical line needs the positions the layout
/// engine chose, and only the layout engine knows them — they depend on the
/// font, the geometry, and the line breaking rules for the script.
///
/// Consulted only for a logical line too long to be one block, so a document of
/// ordinary paragraphs never calls it.
pub trait WrapPoints {
    /// Byte offsets in `text` where a visual line starts, increasing, excluding
    /// 0 and the end of the text.
    ///
    /// Returning nothing means "do not cut this line", which is always safe:
    /// the line becomes one oversized block, which is what it was before.
    /// Implementations report a failure that way rather than by an error,
    /// because a layout that cannot be built is a reason to leave the text
    /// alone, not a reason to stop laying out the document.
    ///
    /// **The whole style, not just the heading level.** A cut position is only
    /// a line start for a layout of the same width, and quoting narrows the
    /// box the line is set in (要件 7.3.2) — asking at the pane's full width
    /// would hand back positions the quoted block does not break at.
    fn line_starts(&mut self, text: &str, style: LineStyle) -> Vec<usize>;
}

/// A [`WrapPoints`] that never cuts: the behaviour of the split before it could
/// cut inside a line. Only the tests that are not about long paragraphs want
/// this, so it is not built into the editor.
#[cfg(test)]
pub struct NeverWraps;

#[cfg(test)]
impl WrapPoints for NeverWraps {
    fn line_starts(&mut self, _text: &str, _style: LineStyle) -> Vec<usize> {
        Vec::new()
    }
}

/// Cut `text` into blocks.
///
/// Blocks end immediately after a `\n` wherever they can, so the lines
/// DirectWrite produces for a block are identical to the ones it would produce
/// for the same text inside a whole-document layout. A logical line too long to
/// be one block on its own is cut at the wrap positions `wraps` reports, which
/// are line starts for the same reason a hard break is.
///
/// `cells_per_line` only decides where the cuts fall, so it may be an estimate:
/// any cut at a line start is correct, and DirectWrite is still what actually
/// wraps the text.
pub fn split_blocks(
    styled: StyledText<'_>,
    cells_per_line: u32,
    typography: &Typography,
    wraps: &mut dyn WrapPoints,
) -> Vec<BlockSpan> {
    let text = styled.text;
    // 要件 7.3.1: a source pane is not indented, for the reason it gets no
    // boxes — the markers are its text.
    let indents = styled.is_preview();
    let cells_per_line = cells_per_line.max(1);
    let mut blocks = Vec::new();
    let mut byte_cursor = 0;
    let mut utf16_cursor = 0_u32;
    let mut line_index = 0_usize;
    let mut block_byte_start = 0;
    let mut block_utf16_start = 0_u32;
    let mut block_cells = 0_u32;
    let mut block_quote_depth = 0_u8;

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
        let characters = body.encode_utf16().count() as u32;
        // Charge for whole lines, so a blank line costs a line like any other,
        // and for the heading size, so a heading costs what it takes up.
        let mut style = styled.style_at(line_index);
        if !indents {
            style.quote_depth = 0;
        }
        let line_cells = line_cells(characters, cells_per_line, typography, style);
        line_index += 1;

        // 要件 7.3.2: a block is set in one layout box, so a change of quoting
        // ends one **whatever size it has reached**. The other two reasons to
        // end a block are about how big it has grown; this one is about what it
        // is, and a block holding both would have to be set at two widths at
        // once. **Before the long-line branch below**, so the piece already
        // gathered is closed under the depth it was gathered at.
        if block_quote_depth != style.quote_depth {
            if block_byte_start < byte_cursor {
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: byte_cursor,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    quote_depth: block_quote_depth,
                });
                block_byte_start = byte_cursor;
                block_utf16_start = utf16_cursor;
                block_cells = 0;
            }
            block_quote_depth = style.quote_depth;
        }

        // A line that fills a block on its own is cut inside itself. The block
        // being accumulated is closed first, so the long line starts one of its
        // own: a cut position inside it is a line start only for text that
        // begins where the layout began.
        if line_cells > BLOCK_MAX_CELLS {
            if block_byte_start < byte_cursor {
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: byte_cursor,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    quote_depth: block_quote_depth,
                });
                block_byte_start = byte_cursor;
                block_utf16_start = utf16_cursor;
            }
            let pieces = cut_long_line(body, cells_per_line, typography, style, wraps);
            for piece_end in pieces {
                let piece_end = byte_cursor + piece_end;
                utf16_cursor += text[block_byte_start..piece_end].encode_utf16().count() as u32;
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: piece_end,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    quote_depth: block_quote_depth,
                });
                block_byte_start = piece_end;
                block_utf16_start = utf16_cursor;
            }
            // Whatever is left of the line, plus its newline, closes as one
            // block rather than being carried on: the tail of a long paragraph
            // has nothing to do with the short lines that follow it.
            utf16_cursor += text[block_byte_start..line_end].encode_utf16().count() as u32;
            blocks.push(BlockSpan {
                byte_start: block_byte_start,
                byte_end: line_end,
                utf16_start: block_utf16_start,
                utf16_end: utf16_cursor,
                quote_depth: block_quote_depth,
            });
            block_byte_start = line_end;
            block_utf16_start = utf16_cursor;
            block_cells = 0;
            byte_cursor = line_end;
            continue;
        }

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
                quote_depth: block_quote_depth,
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
            quote_depth: block_quote_depth,
        });
    }

    blocks
}

/// Byte offsets inside `body` where the long line should be cut, relative to its
/// own start, in increasing order and never including its end.
///
/// The cuts are every `n`th wrap position, `n` chosen so a piece holds about a
/// full block. Every position comes from `wraps`, so every one of them is a line
/// start; choosing which of them to use is all this decides.
///
/// Unlike the boundaries between logical lines (see [`ends_a_block`]), these are
/// **not content-defined and do move when the text changes**. They cannot be
/// anything else: a wrap position is a property of the layout, and an edit early
/// in a paragraph moves every wrap position after it whatever rule picks among
/// them. What does hold is that the wrap positions *before* an edit do not move,
/// so the pieces before it keep their text and their measurements.
fn cut_long_line(
    body: &str,
    cells_per_line: u32,
    typography: &Typography,
    style: LineStyle,
    wraps: &mut dyn WrapPoints,
) -> Vec<usize> {
    let scale = typography.flow_scale(style.heading_level);
    let per_visual_line = (cells_per_line as f32 * scale).max(1.0);
    let lines_per_piece = (BLOCK_MAX_CELLS as f32 / per_visual_line).floor().max(1.0) as usize;
    wraps
        .line_starts(body, style)
        .into_iter()
        .enumerate()
        .filter(|(index, offset)| (index + 1) % lines_per_piece == 0 && *offset < body.len())
        .map(|(_, offset)| offset)
        .collect()
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
///
/// `styled` is the block's own text and its own slice of heading levels, so the
/// bound grows with the sizes actually set in this block. A bound taken from the
/// body size alone clips the moment a heading is larger than `2.2` body sizes.
pub fn block_flow_bound(styled: StyledText<'_>, line_extent: u32, typography: &Typography) -> f32 {
    let font_size = typography.font_size.max(1.0);
    let cells = cells_per_line(line_extent, typography);
    let flow = styled
        .text
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let style = styled.style_at(index);
            let characters = line.encode_utf16().count() as u32;
            let charged = line_cells(characters, cells, typography, style);
            charged as f32 / cells as f32
        })
        .sum::<f32>()
        .max(1.0);
    (flow * font_size * 2.2 + font_size * 4.0 * typography.line_spacing.max(1.0)).ceil()
}

/// One stretch of a block that is set at a size of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StyleRun {
    /// UTF-16 offset **relative to the block**, which is what DirectWrite text
    /// ranges are relative to and what keeps this independent of everything
    /// before the block.
    pub utf16_start: u32,
    pub utf16_len: u32,
    pub heading_level: u8,
    /// What is marked over this range (要件 7.3.2), nothing for a heading run.
    pub marks: Marks,
    /// Set when a box stands over this range instead of its glyphs being drawn
    /// (要件 7.3.2). **Kept in the same list rather than a second one** so the
    /// ranges a block hands DirectWrite stay one list with one cache key: a
    /// block whose boxes differ is not the same layout, and a separate list
    /// would have to be threaded through every key beside this one.
    pub ornament: Option<Ornament>,
}

/// What is drawn over a whole logical line rather than over a stretch of its
/// characters (要件 7.3.2).
///
/// **The other half of [`Ornament`].** A marker's ink goes in the box standing
/// at the head of one line and is measured in characters; these run the whole
/// length of the line, wrapped continuations and all, and are measured in
/// lines. Neither changes any geometry — a line marked this way takes exactly
/// the room it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineOrnament {
    /// The bar standing beside a blockquote, one per level of quoting.
    Quote { depth: u8 },
    /// The stroke a `---` line is set as.
    Rule,
}

/// One whole logical line of a block, and what is drawn over it (要件 7.3.2).
///
/// The range is the line's own text without its break, block-local for the
/// reason [`StyleRun`]'s is. **The drawing side finds the line's rectangle by
/// where each visual line starts**, rather than by counting: how many visual
/// lines a logical line became is a property of the layout, and only their
/// text says which logical line they came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRun {
    pub utf16_start: u32,
    pub utf16_len: u32,
    pub ornament: LineOrnament,
}

/// The block-local whole-line ornaments.
///
/// Undecorated lines are left out, so a document with no quotes and no rules
/// costs nothing here and nothing in the tile pass.
pub fn line_runs(styled: StyledText<'_>) -> Vec<LineRun> {
    // 要件 7.3.1: a source pane shows `>` and `---` themselves, so a bar beside
    // one and a stroke across the other would say the same thing twice — and
    // the stroke would be drawn straight over the marks it stands for.
    if !styled.is_preview() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let mut utf16_start = 0_u32;
    for (index, line) in styled.text.split('\n').enumerate() {
        let utf16_len = line.encode_utf16().count() as u32;
        let style = styled.style_at(index);
        // **The two are not exclusive.** `> ---` is a rule inside a quote and
        // carries both marks, the way `quote_depth` sits beside `kind` rather
        // than inside it.
        if style.quote_depth > 0 {
            runs.push(LineRun {
                utf16_start,
                utf16_len,
                ornament: LineOrnament::Quote {
                    depth: style.quote_depth,
                },
            });
        }
        if matches!(style.kind, LineKind::Rule) {
            runs.push(LineRun {
                utf16_start,
                utf16_len,
                ornament: LineOrnament::Rule,
            });
        }
        // Past the newline this split consumed.
        utf16_start += utf16_len + 1;
    }
    runs
}

/// The block-local ranges that are not body text.
///
/// Body lines are left out, so a document with no headings costs nothing to
/// format. The trailing newline is left out of every run: a line's advance is
/// the tallest thing on it, so the break itself never needs the heading size,
/// and leaving it at body size keeps the empty line a block gives up (see
/// `measure_block`) the size it has always been.
pub fn style_runs(styled: StyledText<'_>) -> Vec<StyleRun> {
    let mut runs = Vec::new();
    let mut utf16_start = 0_u32;
    for (index, line) in styled.text.split('\n').enumerate() {
        let utf16_len = line.encode_utf16().count() as u32;
        let heading_level = styled.level_at(index);
        let kind = styled.kind_at(index);
        // 要件 7.3.2: the box standing at the head of the line. First, because
        // that is where it sits; nothing else on the line overlaps it, so the
        // order only has to read well.
        if let Some(marker) = styled.marker_at(index)
            && marker.utf16_len <= utf16_len
        {
            runs.push(StyleRun {
                utf16_start,
                utf16_len: marker.utf16_len,
                heading_level: 0,
                marks: Marks::default(),
                ornament: Some(marker.ornament),
            });
        }
        if heading_level > 0 && utf16_len > 0 {
            runs.push(StyleRun {
                utf16_start,
                utf16_len,
                heading_level,
                marks: Marks::default(),
                ornament: None,
            });
        }
        // 要件 7.3.2: a fenced line is set in the code family from end to end.
        // **The same range attribute an inline code span already uses**, so
        // nothing about the line's geometry moves — a code block costs what the
        // same text costs as a paragraph.
        if kind.is_code() && utf16_len > 0 {
            runs.push(StyleRun {
                utf16_start,
                utf16_len,
                heading_level: 0,
                marks: Marks::code(),
                ornament: None,
            });
        }
        // 要件 7.3.2: the marked stretches inside the line. **Each carries its
        // line's heading level**, because a run says everything about the range
        // it covers — one that left the level at 0 would set the bold part of a
        // heading back to body size.
        for emphasis in styled.marks_at(index) {
            runs.push(StyleRun {
                utf16_start: utf16_start + emphasis.utf16_start,
                utf16_len: emphasis.utf16_len,
                heading_level,
                marks: emphasis.marks,
                ornament: None,
            });
        }
        // Past the newline this split consumed.
        utf16_start += utf16_len + 1;
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells per line at the default pane extent and font size.
    const CELLS: u32 = 20;

    /// The body-only spec every test that is not about typography uses.
    fn plain_typography() -> Typography {
        Typography::new(22.0)
    }

    /// Split with no styling and no cutting inside a line, which is what every
    /// test that is not about long paragraphs wants.
    fn split(text: &str) -> Vec<BlockSpan> {
        split_with(StyledText::plain(text), &plain_typography())
    }

    fn split_with(styled: StyledText<'_>, typography: &Typography) -> Vec<BlockSpan> {
        split_blocks(styled, CELLS, typography, &mut NeverWraps)
    }

    fn span(text: &str, byte_start: usize, byte_end: usize) -> BlockSpan {
        BlockSpan {
            byte_start,
            byte_end,
            utf16_start: text[..byte_start].encode_utf16().count() as u32,
            utf16_end: text[..byte_end].encode_utf16().count() as u32,
            quote_depth: 0,
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

        let blocks = split(&text);

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
        let blocks = split(text);

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

    /// A stand-in for DirectWrite: wraps every `cells` characters, which is what
    /// a line of uniform ideographs does.
    struct EveryNCharacters(usize);

    impl WrapPoints for EveryNCharacters {
        fn line_starts(&mut self, text: &str, _style: LineStyle) -> Vec<usize> {
            text.char_indices()
                .enumerate()
                .filter(|(index, _)| *index > 0 && index % self.0 == 0)
                .map(|(_, (offset, _))| offset)
                .collect()
        }
    }

    /// Split with a stand-in [`WrapPoints`] that breaks every `CELLS` characters.
    fn split_wrapped(text: &str) -> Vec<BlockSpan> {
        split_blocks(
            StyledText::plain(text),
            CELLS,
            &plain_typography(),
            &mut EveryNCharacters(CELLS as usize),
        )
    }

    /// **A change of quoting ends a block whatever size it has reached**
    /// (要件 7.3.2). A block is set in one layout box and its indent is part of
    /// that box, so one holding both quoted and plain lines would have to be
    /// set at two widths at once.
    #[test]
    fn a_change_of_quoting_ends_a_block() {
        let text = "本文\n> 引用\n> まだ引用\n本文へ戻る\n";
        let quoted = LineStyle {
            quote_depth: 1,
            ..LineStyle::default()
        };
        let plain = LineStyle::default();
        let levels = [plain, quoted, quoted, plain, plain];
        // A preview pane's text, which is the only one that indents (要件 7.3.1).
        let markers = [None; 5];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let blocks = split_with(styled, &plain_typography());

        let depths = blocks
            .iter()
            .map(|block| block.quote_depth)
            .collect::<Vec<u8>>();
        let ends = blocks
            .iter()
            .map(|block| block.byte_end)
            .collect::<Vec<usize>>();
        assert_eq!(depths, vec![0, 1, 0]);
        // The source pane shows the `>` itself, so nothing about it indents and
        // nothing about it ends a block.
        let source = split_with(StyledText::new(text, &levels), &plain_typography());
        assert_eq!(source.len(), 1);
        assert_eq!(source[0].quote_depth, 0);
        // The boundaries fall where the quoting changes and nowhere else: these
        // lines are far too short to end a block on their own.
        assert_eq!(
            ends,
            vec![
                "本文\n".len(),
                "本文\n> 引用\n> まだ引用\n".len(),
                text.len(),
            ]
        );
    }

    /// A cut position is a line start only for a layout of the same width, so a
    /// quoted paragraph has to be **asked about at its own width** (要件 7.3.2).
    /// What goes through is the whole style, not just the heading level.
    #[test]
    fn a_long_line_is_asked_about_under_its_own_style() {
        struct Records(Vec<LineStyle>);

        impl WrapPoints for Records {
            fn line_starts(&mut self, _text: &str, style: LineStyle) -> Vec<usize> {
                self.0.push(style);
                Vec::new()
            }
        }

        let text = format!("{}\n", "あ".repeat(CELLS as usize * 200));
        let quoted = LineStyle {
            quote_depth: 1,
            ..LineStyle::default()
        };
        let levels = [quoted, LineStyle::default()];
        let markers = [None; 2];
        let mut asked = Records(Vec::new());

        split_blocks(
            StyledText::new(&text, &levels).with_markers(&markers),
            CELLS,
            &plain_typography(),
            &mut asked,
        );

        assert_eq!(asked.0, vec![quoted]);
    }

    /// The point of cutting inside a line: a paragraph with no break in it must
    /// stop being one unbounded block.
    #[test]
    fn cuts_a_long_logical_line_at_wrap_positions() {
        let text = format!("{}\n", "あ".repeat(CELLS as usize * 200));

        let uncut = split(&text);
        let cut = split_wrapped(&text);

        assert_eq!(uncut.len(), 1, "one line was one block before");
        assert!(cut.len() > 4, "{} pieces is not a split", cut.len());

        // Every cut lands on a position `WrapPoints` reported, so every piece
        // starts where a line starts. That is the whole correctness argument.
        let body = text.trim_end_matches('\n');
        let stub = LineStyle::default();
        let wraps = EveryNCharacters(CELLS as usize).line_starts(body, stub);
        for piece in &cut[..cut.len() - 1] {
            assert!(
                wraps.contains(&piece.byte_end),
                "a piece ended at byte {}, which is not a wrap position",
                piece.byte_end
            );
        }
    }

    /// Cutting must not change what the blocks cover, in either unit. The UTF-16
    /// running total is kept by hand down this path, so an error here would be
    /// silent everywhere else and would put every caret position off by the
    /// drift.
    #[test]
    fn a_cut_line_is_still_covered_exactly_once() {
        let long = "日本語ABCあいう".repeat(CELLS as usize * 20);
        let text = format!("短い行\n{long}\n最後の行\n");

        let blocks = split_wrapped(&text);

        let mut byte_cursor = 0;
        let mut utf16_cursor = 0;
        for block in &blocks {
            assert_eq!(block.byte_start, byte_cursor, "a byte gap or overlap");
            assert_eq!(block.utf16_start, utf16_cursor, "a UTF-16 gap or overlap");
            assert!(text.is_char_boundary(block.byte_start));
            let bytes = &text[block.byte_start..block.byte_end];
            assert_eq!(
                block.utf16_len(),
                bytes.encode_utf16().count() as u32,
                "a block's UTF-16 length does not match its bytes"
            );
            byte_cursor = block.byte_end;
            utf16_cursor = block.utf16_end;
        }
        assert_eq!(byte_cursor, text.len());
        assert_eq!(utf16_cursor, text.encode_utf16().count() as u32);
    }

    /// The asymmetry the whole approach rests on (6.9). Wrap positions before an
    /// edit cannot move, so the pieces before it keep their text — and a block
    /// that keeps its text keeps its measurement.
    ///
    /// Nothing makes these boundaries content-defined the way the boundaries
    /// between logical lines are (4.5); they move because the wrapping moves.
    /// What this asks is only that they move *after* the edit and not before it.
    #[test]
    fn an_edit_late_in_a_long_line_leaves_the_earlier_pieces_alone() {
        let paragraph = "日本語ABCあいうえお".repeat(CELLS as usize * 20);
        let text = format!("{paragraph}\n");
        let ninth = paragraph.chars().count() * 9 / 10;
        let cut = text.char_indices().nth(ninth).expect("long enough").0;
        let edited = format!("{}編集{}", &text[..cut], &text[cut..]);

        let before = split_wrapped(&text);
        let after = split_wrapped(&edited);

        let same = |b: &BlockSpan, a: &BlockSpan| {
            text[b.byte_start..b.byte_end] == edited[a.byte_start..a.byte_end]
        };
        let kept = before
            .iter()
            .zip(&after)
            .take_while(|(b, a)| same(b, a))
            .count();
        assert!(
            kept * 2 > before.len(),
            "only {kept} of {} pieces survived an edit in the last tenth",
            before.len()
        );
    }

    #[test]
    fn keeps_an_oversized_logical_line_in_one_block() {
        let text = format!("{}\n短い行\n", "あ".repeat(BLOCK_MAX_CELLS as usize * 3));
        let blocks = split(&text);

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

        let blocks = split(&text);

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
        let before = split(&text);
        assert!(before.len() > 8, "the sample must span many blocks");

        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .filter(|&index| index < text.len());
        for cut in line_starts.step_by(3) {
            let edited = format!("{}\n{}", &text[..cut], &text[cut..]);
            let after = split(&edited);

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
        let blocks = split("");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].utf16_end, 0);
    }

    /// The advance is what divides into the pane, not the size, so asking for
    /// half a size of air between characters must cost a third of the line.
    #[test]
    fn a_wider_character_advance_fits_fewer_characters_in_a_line() {
        let plain = plain_typography();
        let spaced = Typography {
            character_spacing: 0.5,
            ..plain.clone()
        };

        let tight = cells_per_line(600, &plain);
        let loose = cells_per_line(600, &spaced);

        assert!(
            loose * 3 <= tight * 2 + 1,
            "{tight} cells became {loose} at half a size of extra advance"
        );
    }

    /// A heading is charged twice: it fits fewer characters per line, and each
    /// of those lines advances further. Both have to be in the charge, or a page
    /// of headings reaches much further along the flow axis than the block
    /// budget says it does.
    #[test]
    fn a_heading_is_charged_for_the_space_it_takes() {
        let typography = plain_typography().with_heading_ramp(2.0);
        let text = format!("{}\n", "あ".repeat(CELLS as usize)).repeat(40);
        let levels = vec![LineStyle::heading(1); 40];

        let body = split_with(StyledText::plain(&text), &typography);
        let heading = split_with(StyledText::new(&text, &levels), &typography);

        assert!(
            heading.len() > body.len(),
            "the same 40 lines fell into {} blocks as headings and {} as body",
            heading.len(),
            body.len()
        );
    }

    /// Undershooting this bound makes DirectWrite clip lines, and a heading is
    /// the case where a bound taken from the body size alone does undershoot.
    #[test]
    fn the_flow_bound_grows_with_the_heading_size() {
        let typography = plain_typography().with_heading_ramp(2.4);
        let text = "見出し\n見出し\n見出し\n";
        let levels = [LineStyle::heading(1); 3];

        let body = block_flow_bound(StyledText::plain(text), 520, &typography);
        let heading = block_flow_bound(StyledText::new(text, &levels), 520, &typography);

        // Not 2.4 times: the bound carries a fixed slack term that does not
        // scale, which is exactly the safety this test is protecting.
        assert!(
            heading >= body * 1.5,
            "a bound of {body} for body text only grew to {heading} at 2.4 times the size"
        );
    }

    /// Line spacing stretches every line, so the bound has to stretch with it.
    #[test]
    fn the_flow_bound_grows_with_the_line_spacing() {
        let plain = plain_typography();
        let airy = Typography {
            line_spacing: 2.0,
            ..plain.clone()
        };
        let text = "本文の行\n本文の行\n本文の行\n";

        let tight = block_flow_bound(StyledText::plain(text), 520, &plain);
        let loose = block_flow_bound(StyledText::plain(text), 520, &airy);

        assert!(loose >= tight * 1.8, "{tight} only grew to {loose}");
    }

    /// The ranges DirectWrite is given are relative to the block, and stop
    /// before the break, so nothing about them depends on where the block sits.
    #[test]
    fn style_runs_are_block_local_and_stop_before_the_break() {
        let text = "見出し\n本文です\n## 深い見出し\n";
        let levels = [
            LineStyle::heading(1),
            LineStyle::default(),
            LineStyle::heading(2),
        ];

        let runs = style_runs(StyledText::new(text, &levels));

        assert_eq!(
            runs,
            vec![
                StyleRun {
                    utf16_start: 0,
                    utf16_len: 3,
                    heading_level: 1,
                    marks: Marks::default(),
                    ornament: None,
                },
                StyleRun {
                    utf16_start: "見出し\n本文です\n".encode_utf16().count() as u32,
                    utf16_len: "## 深い見出し".encode_utf16().count() as u32,
                    heading_level: 2,
                    marks: Marks::default(),
                    ornament: None,
                },
            ]
        );
    }

    /// A fenced line is one run in the code family from end to end
    /// (要件 7.3.2), and the fences themselves are part of the block they
    /// delimit — a proportional line at each end would be the only thing in a
    /// code block that was not code.
    #[test]
    fn every_line_of_a_fenced_block_is_one_code_run() {
        let text = "```\nlet x = 1;\n```\n本文";
        let levels = [
            LineStyle::of_kind(LineKind::Fence),
            LineStyle::of_kind(LineKind::Code),
            LineStyle::of_kind(LineKind::Fence),
            LineStyle::default(),
        ];

        let runs = style_runs(StyledText::new(text, &levels));

        let ranges = runs
            .iter()
            .map(|run| (run.utf16_start, run.utf16_len))
            .collect::<Vec<(u32, u32)>>();
        assert_eq!(ranges, vec![(0, 3), (4, 10), (15, 3)]);
        assert!(runs.iter().all(|run| run.marks == Marks::code()));
        // The body line asks for nothing, so a document with no code block in
        // it costs what it always did.
        assert_eq!(runs.len(), 3);
    }

    /// The box at the head of a line is a run like any other, placed by where
    /// its line begins in the block (要件 7.3.2). **It sets no font**: what it
    /// stands over is hidden, and the ink is drawn in the tile pass.
    #[test]
    fn a_box_is_a_run_covering_the_head_of_its_line() {
        let text = "見出し\n- 箇条書き\n1. 番号";
        let levels = [
            LineStyle::heading(1),
            LineStyle::of_kind(LineKind::Bullet),
            LineStyle::of_kind(LineKind::Ordered),
        ];
        let bullet = LineMarker {
            utf16_len: 2,
            ornament: Ornament::Bullet,
        };
        let number = LineMarker {
            utf16_len: 3,
            ornament: Ornament::Number,
        };
        let markers = [None, Some(bullet), Some(number)];

        let styled = StyledText::new(text, &levels).with_markers(&markers);
        let boxes = style_runs(styled)
            .into_iter()
            .filter(|run| run.ornament.is_some())
            .collect::<Vec<StyleRun>>();

        let after = |shown: &str| shown.encode_utf16().count() as u32;
        let placed = boxes
            .iter()
            .map(|run| (run.utf16_start, run.utf16_len, run.ornament))
            .collect::<Vec<(u32, u32, Option<Ornament>)>>();
        assert_eq!(
            placed,
            vec![
                (after("見出し\n"), 2, Some(Ornament::Bullet)),
                (after("見出し\n- 箇条書き\n"), 3, Some(Ornament::Number)),
            ]
        );
        // A box says nothing about the font its range would have been set in.
        assert!(boxes.iter().all(|run| run.marks == Marks::default()));
        assert!(boxes.iter().all(|run| run.heading_level == 0));
    }

    /// A source pane hands over no markers and gets no boxes. **The markers are
    /// the text there**, and a box hides the glyphs it stands over, so one set
    /// on a source pane would hide what is being edited.
    #[test]
    fn text_with_no_markers_asks_for_no_boxes() {
        let text = "- 箇条書き";
        let levels = [LineStyle::of_kind(LineKind::Bullet)];

        let runs = style_runs(StyledText::new(text, &levels));
        assert!(runs.iter().all(|run| run.ornament.is_none()));
    }

    /// A quote and a rule are marks on the **whole line** rather than on a
    /// stretch of its characters (要件 7.3.2), so each comes back as a run of
    /// its own, covering its line's text without the break.
    #[test]
    fn a_quote_and_a_rule_are_marks_on_whole_lines() {
        let text = "引用\n本文\n---";
        let levels = [
            LineStyle {
                quote_depth: 1,
                ..LineStyle::default()
            },
            LineStyle::default(),
            LineStyle::of_kind(LineKind::Rule),
        ];

        let markers = [None; 3];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let runs = line_runs(styled);

        let after = |shown: &str| shown.encode_utf16().count() as u32;
        assert_eq!(
            runs,
            vec![
                LineRun {
                    utf16_start: 0,
                    utf16_len: after("引用"),
                    ornament: LineOrnament::Quote { depth: 1 },
                },
                LineRun {
                    utf16_start: after("引用\n本文\n"),
                    utf16_len: after("---"),
                    ornament: LineOrnament::Rule,
                },
            ]
        );
    }

    /// **The two are not exclusive.** `> ---` is a rule inside a quote and
    /// carries both marks, the way `quote_depth` sits beside `kind` rather than
    /// inside it.
    #[test]
    fn a_quoted_rule_carries_both_marks() {
        let quoted_rule = LineStyle {
            kind: LineKind::Rule,
            quote_depth: 1,
            ..LineStyle::default()
        };
        let levels = [quoted_rule];
        let markers = [None; 1];
        let styled = StyledText::new("---", &levels).with_markers(&markers);

        let runs = line_runs(styled);

        let ornaments = runs
            .iter()
            .map(|run| run.ornament)
            .collect::<Vec<LineOrnament>>();
        assert_eq!(
            ornaments,
            vec![LineOrnament::Quote { depth: 1 }, LineOrnament::Rule]
        );
    }

    /// A document with neither asks for nothing, so nothing is drawn and
    /// nothing goes into the tile's fingerprint.
    #[test]
    fn plain_text_asks_for_no_whole_line_marks() {
        let levels = [LineStyle::heading(1), LineStyle::default()];
        let markers = [None; 2];
        let styled = StyledText::new("見出し\n本文", &levels).with_markers(&markers);

        let runs = line_runs(styled);

        assert!(runs.is_empty());
    }

    /// **And a source pane asks for nothing whatever it holds** (要件 7.3.1).
    /// It shows the `>` and the `---` themselves, so a bar beside one and a
    /// stroke across the other would say the same thing twice — and the stroke
    /// would be drawn straight over the marks it stands for.
    #[test]
    fn a_source_pane_asks_for_no_whole_line_marks() {
        let levels = [
            LineStyle {
                quote_depth: 1,
                ..LineStyle::default()
            },
            LineStyle::of_kind(LineKind::Rule),
        ];

        let runs = line_runs(StyledText::new("引用\n---", &levels));

        assert!(runs.is_empty());
    }

    /// A marked stretch becomes a run of its own, offset by where its line
    /// begins in the block, and it keeps the line's heading size (要件 7.3.2).
    #[test]
    fn a_marked_stretch_is_a_run_inside_its_line() {
        let text = "見出し\n太字とふつう";
        let levels = [LineStyle::heading(1), LineStyle::default()];
        let bold = Marks {
            bold: true,
            ..Marks::default()
        };
        let spans = [
            vec![Emphasis {
                utf16_start: 1,
                utf16_len: 2,
                marks: bold,
            }],
            vec![Emphasis {
                utf16_start: 0,
                utf16_len: 2,
                marks: bold,
            }],
        ];

        let runs = style_runs(StyledText::marked(text, &levels, &spans));

        // The heading's own run, then what is marked inside it, then the
        // marked stretch on the body line.
        let shape: Vec<(u32, u32, u8, bool)> = runs
            .iter()
            .map(|run| {
                let marked = run.marks.bold;
                (run.utf16_start, run.utf16_len, run.heading_level, marked)
            })
            .collect();
        let wanted = vec![(0, 3, 1, false), (1, 2, 1, true), (4, 2, 0, true)];
        assert_eq!(shape, wanted);
        // A line nobody worked out has no marked stretches, and the headings
        // are unchanged by the shorter list.
        let plain = style_runs(StyledText::new(text, &levels));
        assert_eq!(plain.len(), 1);
    }

    /// A level the spec has no size for is body text, not a panic and not a
    /// silent zero. Levels arrive from Markdown, which allows six.
    #[test]
    fn a_level_past_the_deepest_heading_is_body_text() {
        let typography = plain_typography().with_heading_ramp(2.0);

        assert_eq!(typography.size_scale(0), 1.0);
        assert_eq!(typography.size_scale(7), 1.0);
        assert!(typography.size_scale(1) > typography.size_scale(6));
        assert!(typography.size_scale(6) > 1.0);
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
        let bound = block_flow_bound(StyledText::plain(&text), 520, &plain_typography());

        let usable = 520.0_f32 - 22.0 * 3.0;
        let cells = (usable / 22.0).floor() as usize;
        let lines = 100_usize.div_ceil(cells);
        assert!(
            bound > lines as f32 * 22.0 * 1.7,
            "the bound must exceed the width DirectWrite actually needs"
        );
    }
}
