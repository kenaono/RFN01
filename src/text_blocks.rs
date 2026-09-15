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

use std::{ops::Range, sync::Arc};

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
    /// タイルが紙の色を塗るか（追加要件 2026-09-15、背景の壁紙）。
    ///
    /// **壁紙を敷いているあいだは塗らない。**紙は面の側が壁紙の上に濃さ付きで
    /// 1枚だけ塗り、タイルは字だけを透明な地に置く——タイルごとに紙を塗ると、
    /// 壁紙が文字の帯ごとに隠れる。色と同じ側にいて、幾何には効かない。
    pub paper_painted: bool,
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
    /// E11: bold, italic, strike, background, heading rule bits, Body/H1..H6.
    pub decorations: [u8; 7],
    pub backgrounds: [[f32; 3]; 7],
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
    /// Whether the page carries its line numbers beside it (要件 9、2026-09-07
    /// 追加).
    ///
    /// **A number on the spec, because it moves the text.** The numbers stand
    /// in the page's own margin, so turning them on widens it — which is a
    /// measurement, not a colour, and everything laid out at the old width has
    /// to be laid out again. Keeping it here is what makes that happen: two
    /// specs that differ by this are not the same page and never were.
    pub line_numbers: bool,
    pub whitespace: bool,
    /// ルビと傍点の大きさ、親文字に対する比率（要件 7.8・要件 9）。
    ///
    /// **幾何には効かない。**ルビは幅0の箱の脇に描かれるので、この値が動いても
    /// 折り返しも行送りも変わらない——変わるのは絵だけである。だから仕様に
    /// 入れておくのは*測り直しのため*ではなく、**タイルの署名に入れるため**で
    /// ある（6.18の色と同じ罠：見た目だけが変わると古い絵が残る）。
    pub ruby_scale: f32,
    /// ルビと傍点を、行の箱の中でどれだけ字へ寄せるか（要件 7.8）。
    /// 本文の大きさに対する比率で、正が字へ近づく向き。
    pub ruby_offset: f32,
    /// 縦中横を効かせるか（要件 7.8・要件 9、書き手の決定 2026-09-09）。
    ///
    /// **既定は入**。要件 7.8 は「書き手が何も書かなくても効く」と言っており、
    /// `20歳`が`2`と`0`に割れて縦に並ぶのは縦書きの原稿の姿ではない。それでも
    /// 切れるようにしたのは、**書き手が「気持ち悪い」と言ったから**である
    /// ——組み方の好みは書き手のもので、編集器が決めることではない（要件 9）。
    ///
    /// **幾何に効く。**3桁の数字は1マスに収まるのと1桁ずつ縦に並ぶのとで
    /// 占める長さが違うので、これは色ではなく寸法の側にいる
    /// （`hash_typography`＝組み直しの合図に入れてある）。
    ///
    /// 横書きの面には初めから効かない（縦中横は縦書きの中でだけ起きる）ので、
    /// シートに出すのは縦書きの面だけ——**働かない切り替えを画面に置かない。**
    pub upright_digits: bool,
    /// 箇条書きの印として**画面に出る字**（要件 9、書き手の決定 2026-09-11）。
    ///
    /// **原稿の記号1つにつき1つ**——`-`／`*`／`+`の順で、原稿にその記号で書かれた
    /// 項目を、画面ではこの字で出す。**記号に意味を与えるとはこのこと**で、
    /// 見せ方は紙のものだから、同じ原稿を別の紙で開けば別の丸に見えてよい。
    ///
    /// **色と同じ側にいる。**箱は幅0なので、この字が変わっても折り返しも行送りも
    /// 1画素も動かない——古くなるのはタイルだけである（`hash_typography`に入れて
    /// あるのはそのため。6.18の罠）。
    pub bullets: [char; 3],
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

/// 箇条書きの印として画面に出る字の既定（要件 9、書き手の決定 2026-09-11）。
///
/// **いままで描いていた字。**設定になったからといって、書き手の画面が動くいわれは
/// ない（`DEFAULT_BODY_FONT`と同じ考え方）——**3つの記号とも同じ丸から始める**。
pub const DEFAULT_BULLET: char = '•';

/// The ink `doc-ink` in `ui/tokens.slint`: the one colour in the app with no
/// purple in it, because it is the one a reader looks at for an hour.
pub const DEFAULT_INK: [f32; 3] = [36.0 / 255.0, 33.0 / 255.0, 30.0 / 255.0];
/// `paper`, the horizontal sheet's.
pub const DEFAULT_PAPER: [f32; 3] = [1.0, 254.0 / 255.0, 250.0 / 255.0];
/// `paper-alt`, the vertical sheet's. **A shade deeper on purpose**: the two
/// panes differ just enough to answer 「どちらの向きで書いているか」 without a
/// word being read. It is a default now rather than a rule — each sheet has a
/// paper of its own, and the writer may set them the same.
pub const DEFAULT_VERTICAL_PAPER: [f32; 3] = [253.0 / 255.0, 251.0 / 255.0, 244.0 / 255.0];

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
            // 要件 7.8: 半分が日本語の組版の当たり前。位置は行の箱の端のまま。
            ruby_scale: 0.5,
            ruby_offset: 0.0,
            heading_scale: [1.0; MAX_HEADING_LEVEL],
            decorations: [0; 7],
            backgrounds: [DEFAULT_PAPER; 7],
            body_font: DEFAULT_BODY_FONT.to_owned(),
            heading_font: [const { String::new() }; MAX_HEADING_LEVEL]
                .map(|_| DEFAULT_HEADING_FONT.to_owned()),
            code_font: DEFAULT_CODE_FONT.to_owned(),
            ink: DEFAULT_INK,
            paper: DEFAULT_PAPER,
            paper_painted: true,
            heading_ink: [DEFAULT_INK; MAX_HEADING_LEVEL],
            line_numbers: false,
            whitespace: false,
            // 要件 7.8: 書き手が何も書かなくても効く、が既定。
            upright_digits: true,
            // E10の③: いままで描いていた字。
            bullets: [DEFAULT_BULLET; 3],
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

    /// The ink a comment inside code is drawn in (要件 7.3.2).
    ///
    /// **Mixed from the two colours the writer set** rather than being a
    /// setting of its own. 要件 9 gives the writer the ink, the paper and the
    /// headings, and the reason a link is underlined instead of coloured
    /// applies here as well: **a colour nobody can set is a colour that will
    /// not suit somebody's paper.** The writer's own ink, faded towards their
    /// own paper, suits whatever they chose — and says the same thing a
    /// comment is for saying, which is "this is beside the point".
    pub fn comment_ink(&self) -> [f32; 3] {
        const FADE: f32 = 0.45;
        let mix = |ink: f32, paper: f32| ink + (paper - ink) * FADE;
        [
            mix(self.ink[0], self.paper[0]),
            mix(self.ink[1], self.paper[1]),
            mix(self.ink[2], self.paper[2]),
        ]
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
    /// What starts a comment in this line's code, for a line inside a fence
    /// (要件 7.3.2).
    ///
    /// **A property of the line, because it is a fence that decides it** — the
    /// language is named on the opening fence and reaches every line under it,
    /// exactly as the fence itself does. Keeping it here is also what makes the
    /// preview notice: a kept line is reusable while its style is what it was,
    /// so changing ```` ```rust ```` to ```` ```python ```` re-marks the lines
    /// below without any of their text having changed.
    pub comment: CommentSyntax,
    /// How many steps a list sets this line in, 0 for a line no list touches
    /// (要件 7.3.2).
    ///
    /// **The whole step, not the nesting on top of it**: one for an item at the
    /// margin, one more for each level it is under. It is a count of steps
    /// rather than a depth of nesting because **the lines that continue an item
    /// are set in with it and are not items themselves** — a paragraph written
    /// under an item lines up with that item's text, and has no marker and no
    /// depth of its own to derive it from.
    pub list_indent: u8,
}

/// What begins a comment in one language, for the lines inside a fence
/// (要件 7.3.2).
///
/// **Line comments only, and only the marker.** A block comment carries across
/// lines, which would make one line's marking depend on the one before it in a
/// second way besides the fence — and 要件 4.2 has already said this editor
/// does not read code. `None` is the answer for a fence with no language on it
/// and for every language not named: **nothing is coloured unless the writer
/// said what the block is.**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CommentSyntax {
    #[default]
    None,
    /// `//` — C and everything shaped like it.
    Slashes,
    /// `#` — the shells, Python, Ruby, YAML, TOML.
    Hash,
    /// `--` — SQL, Lua, Haskell.
    Dashes,
    /// `;` — the Lisps, assembly, INI.
    Semicolon,
    /// `%` — TeX, Erlang, MATLAB.
    Percent,
}

impl CommentSyntax {
    /// The characters that begin a comment, or `None` where nothing does.
    pub fn marker(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Slashes => Some("//"),
            Self::Hash => Some("#"),
            Self::Dashes => Some("--"),
            Self::Semicolon => Some(";"),
            Self::Percent => Some("%"),
        }
    }
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
    /// One row of a table, the header row included (要件 7.3.2).
    TableRow,
    /// The row of dashes under the header, which is what names the columns.
    TableRule,
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

    /// Whether the line begins with a list marker.
    pub fn is_list(self) -> bool {
        matches!(self, Self::Bullet | Self::Ordered | Self::Task { .. })
    }

    /// Whether the line belongs to a table (要件 7.3.2).
    ///
    /// **The one thing on the page whose geometry is not the line's own.** A
    /// column is as wide as the widest cell anywhere in the table, so a row
    /// cannot be set without the rows around it — which is why a table is one
    /// block, the way a fenced block is (`split_blocks`).
    pub fn is_table(self) -> bool {
        matches!(self, Self::TableRow | Self::TableRule)
    }
}

/// How one column of a table is set, as its delimiter row says (要件 7.3.2).
///
/// **Along the flow axis rather than to the left and the right**, for the
/// reason [`FlowOrder`] is named that way: a column is aligned against the axis
/// its text runs along, and that axis is the screen's horizontal in a
/// horizontal pane only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Align {
    /// `---` and `:---`, and every column no delimiter row named.
    #[default]
    Start,
    /// `:---:`
    Center,
    /// `---:`
    End,
}

/// One cell of one table row (要件 7.3.2).
///
/// **One cell per bar**, the cell being whatever stands between that bar and
/// the next. A row written the usual way closes with a bar, so its last cell is
/// empty; that costs one column the delimiter row never named, and a column no
/// delimiter row named is given no width.
///
/// **The bar is the byte before `byte_start`** and is not kept beside it: a bar
/// is one ASCII character, so where the cell begins says where its bar is. Its
/// place in UTF-16 units is kept, because that is what a DirectWrite range is
/// measured in and counting to it again is a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableCell {
    /// The bar that opens the cell, in UTF-16 units of the line. One unit long.
    pub bar_utf16: u32,
    /// The cell's own text, both bars excluded.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// The cells of one table row (要件 7.3.2).
///
/// **A bar behind a backslash is a bar the writer wrote**, not a divider, which
/// is the only way to put one inside a cell.
pub fn table_cells(line: &str) -> Vec<TableCell> {
    let mut bars = Vec::new();
    let mut units = 0_u32;
    let mut escaped = false;
    for (byte, character) in line.char_indices() {
        if character == '|' && !escaped {
            bars.push((byte, units));
        }
        escaped = character == '\\' && !escaped;
        units += character.len_utf16() as u32;
    }
    bars.iter()
        .enumerate()
        .map(|(index, (byte, unit))| TableCell {
            bar_utf16: *unit,
            byte_start: byte + 1,
            byte_end: bars.get(index + 1).map_or(line.len(), |(next, _)| *next),
        })
        .collect()
}

/// Whether a line is a row of a table (要件 7.3.2).
///
/// **A bar at the very head of the line, and another one after it.** Prose is
/// full of bars and none of it is a table; asking the line to open with one is
/// what keeps a sentence from being read as a row. A quoted or indented table
/// is not read as one — 要件定義 §14 keeps tables at the margin.
pub fn is_table_row(line: &str) -> bool {
    line.starts_with('|') && table_cells(line).len() >= 2
}

/// The columns a delimiter row names, and `None` for a line that is not one
/// (要件 7.3.2).
///
/// `| --- | :---: | ---: |`. Every cell is dashes with a colon at one end, both
/// ends or neither; the empty cell the closing bar leaves is not a column.
pub fn table_alignments(line: &str) -> Option<Vec<Align>> {
    if !is_table_row(line) {
        return None;
    }
    let cells = table_cells(line);
    let mut alignments = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        let text = line[cell.byte_start..cell.byte_end].trim();
        if text.is_empty() && index + 1 == cells.len() {
            break;
        }
        alignments.push(alignment_of(text)?);
    }
    (!alignments.is_empty()).then_some(alignments)
}

/// One cell of a delimiter row.
fn alignment_of(text: &str) -> Option<Align> {
    let opens = text.starts_with(':');
    let closes = text.ends_with(':');
    let dashes = text.trim_matches(':');
    if dashes.is_empty() || !dashes.chars().all(|letter| letter == '-') {
        return None;
    }
    Some(match (opens, closes) {
        (true, true) => Align::Center,
        (false, true) => Align::End,
        _ => Align::Start,
    })
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
    /// The text a link shows, with the link itself taken off (要件 7.3.2).
    ///
    /// **Unlike the others this is not a pair of markers around the text.** A
    /// link's markup is `[shown](where)` or `[[note|shown]]`, so what is hidden
    /// is on both sides *and* between them; only the part a reader is meant to
    /// read comes through, and this is what says which part that was.
    pub link: bool,
    /// E12: the destination has not been resolved. This does not assert that
    /// the file is missing; no folder scan is performed while parsing text.
    pub unresolved_link: bool,
    /// A comment inside a fenced code block, from its marker to the end of the
    /// line.
    ///
    /// **The one thing this editor says about the inside of code.** 要件 4.2
    /// rules out syntax highlighting; what is left is the distinction a reader
    /// of prose actually wants, which is which lines are the writer talking and
    /// which are the program.
    pub comment: bool,
    /// 傍点（圏点）が振られている範囲（要件 7.8）。
    ///
    /// **旗であって箱ではない。**傍点は本文の字をそのまま見せたまま、その脇に
    /// 点を打つ——字を隠す[`Ornament`]とは逆の仕事である。だから太字や斜体と
    /// 同じ側にいて、同じように入れ子になれる。点そのものはタイル描画側で、
    /// この範囲が当たった矩形へ打つ。
    ///
    /// **幾何は動かさない。**点は行の外（ルビと同じ帯）に出るので、字送りも
    /// 折り返し位置も傍点の有無で変わらない。
    pub dots: bool,
}

/// What is drawn in place of the marker a box stands over (要件 7.3.2).
///
/// **The block is the space and this is the ink.** The box over a marker hides
/// its glyphs and nothing else; the step that `-`, `10.` and `- [x]` all begin
/// their text after is the block's own indent, which is the only thing that
/// reaches the lines a wrapped item ran on to (要件 7.3.2). What appears in the
/// gutter that indent opens is drawn in the tile pass, where the render target
/// already is.
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
    /// **Nothing at all: a box over a whole line that is all marks.** `---`
    /// and the ``` that opens or closes a fence are both this — what stands in
    /// their place runs the length of the line or the height of the block, and
    /// is a [`LineRun`] rather than ink in a box.
    ///
    /// The line still takes the room a line takes, which is what gives a code
    /// block its padding at each end.
    ///
    /// A blockquote has no box of its own — the preview takes its marker off,
    /// and its indent belongs to the block ([`BlockSpan::indent_steps`]) the
    /// way a list item's does.
    Hidden,
    /// **編集中の行の、行頭の記号そのもの**（要件 7.3.1、書き手の報告 2026-09-10）。
    ///
    /// カーソルのある行は原文で出る（記号も見える）が、**段下げは記号が隠れている
    /// 前提のまま**なので、その行だけ本文が記号の幅ぶん右にあった——触っているあいだ
    /// だけ位置が違い、離れると左へ戻る（「入力中に右に大きくズレて戻る」）。
    ///
    /// **覆った字を、そのまま溝に描く**（`Number`と同じ道）。記号は見えたまま、
    /// 本文の位置は組み上がりと同じになる。
    Markup,
    /// **The white space a writer typed to line a continuation up under its
    /// item.** Nothing is drawn in its place and it keeps no room either: what
    /// sets the line in is the block, and space that also took room would set
    /// it in twice — and only on the first line it wrapped to.
    Indent,
    /// ルビの読み——`《かんじ》`のほう（要件 7.8）。
    ///
    /// **読みは本文に居残ったまま、箱で隠される。**消してしまうと描くときに
    /// 読む字が無くなり、`Emphasis`に文字列を持たせることになる——`Emphasis`も
    /// [`StyleRun`]も`Copy`でハッシュ可能で、レイアウトキャッシュの鍵に入って
    /// いるので、そこに`String`は置けない。箱なら幅0で字が消え、**読みは
    /// ブロックの本文から読み出せる**（`Ornament::Number`が数字を読み出すのと
    /// 同じ道）。
    ///
    /// `base_utf16`は**この箱の手前にある親文字の長さ**（UTF-16単位）。
    /// 描くときはそこから親文字の矩形を出し、その脇へ読みを小さく組む。
    /// 親と読みが1つの走りに収まっているので、**組の対応が壊れようがない**。
    Ruby { base_utf16: u32 },
    /// 縦中横——縦書きの列の中で、半角の数字を正立させる（要件 7.8）。
    ///
    /// **書き手は何も書かない。**「20歳」が「2」と「0」に割れて縦に並ぶのは、
    /// 縦書きの原稿として当たり前の姿ではない——だから記法ではなく、
    /// 縦書きの面が数字をそう組む、という決まりにしてある。
    ///
    /// 箱は**1文字ぶんの送りを取り**、その中に数字を正立・横並びで組む
    /// （[`Ornament::Number`]と同じく、箱が覆っている範囲の字をそのまま描く）。
    /// 1〜2桁で1つの箱、3桁以上は**1桁につき1つ**——要件 7.8 の「3桁以上は
    /// 縦に並べる」がそれで、桁ごとに正立した箱が列に並ぶ。
    Upright,
}

impl Ornament {
    /// Whether anything is drawn inside the box, as against the box being
    /// there only to hide what it covers.
    ///
    /// Asked before the hit test that finds where to draw, so a document of
    /// rules never asks DirectWrite about a rectangle nothing goes into.
    pub fn draws_ink(self) -> bool {
        !matches!(self, Self::Hidden | Self::Indent)
    }

    /// Whether the box's ink goes beside the line rather than in the gutter the
    /// block's indent opened (要件 7.8).
    ///
    /// **A marker's ink stands before the text and ruby stands over it**, so
    /// the two are placed off different axes. Asked here rather than at the
    /// place that draws, so that a new ornament has to answer it.
    pub fn rides_beside_the_line(self) -> bool {
        matches!(self, Self::Ruby { .. })
    }

    /// How far the box reaches along the line axis.
    ///
    /// **Three answers, and most boxes give the third.** A box over a whole
    /// line of marks keeps that line's room — a rule and a fence leave their
    /// line behind as blank space, which is what gives a code block its padding
    /// at each end. A box standing digits upright (要件 7.8) takes the one
    /// character it stands them in. Everything else is standing where an indent
    /// will be, and an indent is the block's (要件 7.3.2), so it takes nothing.
    pub fn box_advance(self, indent_step: f32, font_size: f32) -> f32 {
        match self {
            Self::Hidden => indent_step,
            Self::Upright => font_size,
            _ => 0.0,
        }
    }

    /// Whether the ink goes inside the box rather than in the gutter the
    /// block's indent opened.
    ///
    /// **Only the upright digits do.** A marker's ink stands before the text,
    /// ruby stands over it, and these stand exactly where the box is.
    pub fn stands_in_its_box(self) -> bool {
        matches!(self, Self::Upright)
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
    /// Set when a box stands over this stretch instead of its glyphs being
    /// drawn (要件 7.8).
    ///
    /// **The line's own boxes come the same way the head one does** — through
    /// [`style_runs`] and into a [`StyleRun`]. Ruby's reading is the first box
    /// that is not at the head of a line, so this is where a mid-line one is
    /// said; everything downstream already knew what to do with it.
    pub ornament: Option<Ornament>,
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

    /// A line of one kind, at the margin.
    ///
    /// **An item made this way is an item at the margin**, which is to say it
    /// asks for the one step being an item is worth. `kind` and `list_indent`
    /// are separate facts — a line that continues an item has the indent
    /// without the kind — and this is where the two are kept from drifting for
    /// everything that does not come from `document::line_styles`.
    pub fn of_kind(kind: LineKind) -> Self {
        Self {
            kind,
            list_indent: u8::from(kind.is_list()),
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
        self.kind.is_code() || matches!(self.kind, LineKind::Rule | LineKind::TableRule)
    }

    /// How many steps of indenting a line set this way asks its block for
    /// (要件 7.3.2).
    ///
    /// **A list item asks for one, and one more for each level it is nested
    /// under; a line that continues an item asks for what that item asked**
    /// (`list_indent`). The step
    /// used to be a box at the head of the line, which reached that head and no
    /// further, so the continuation of a wrapped item came back to the margin
    /// (技術検証 7.1). The block's box is the only thing that moves every
    /// visual line, so the indent belongs there and the marker's box keeps only
    /// the half of its job that was ever a box's: hiding the glyphs it covers.
    ///
    /// **One rule read in two places**: the split ends a block where this
    /// changes, and the search for a long line's wrap positions lays it out at
    /// the width this leaves. A second opinion would cut blocks at one width
    /// and measure them at another.
    pub fn indent_steps(&self) -> u8 {
        self.quote_depth + self.list_indent
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
    /// The one logical line shown as its own source, if the caret is on one
    /// (要件 7.3.1).
    ///
    /// **Nothing else can say which line that is.** The line's own text is
    /// already the source — that is what being active means — and a table row
    /// reads the same either way, because a bar is a bar in both. What tells
    /// the two apart is only that nothing is put over this one: no marker box
    /// on a list item, and no boxes over a table's bars.
    ///
    /// Block-local wherever the styling is, and `None` for a source pane, where
    /// every line is its own source and nothing is put over any of them.
    pub source_line: Option<usize>,
}

impl<'a> StyledText<'a> {
    /// Text set as plain body throughout.
    pub fn plain(text: &'a str) -> Self {
        Self {
            text,
            lines: &[],
            spans: &[],
            markers: &[],
            source_line: None,
        }
    }

    /// Lines set at a size each, with nothing marked inside them.
    ///
    /// **Only the tests build one this way.** Every pane hands over what its
    /// lines have marked and what stands at their heads, because leaving
    /// either out lays the text out differently from the way it is drawn —
    /// which is the whole of why `line_starts` now takes a [`LongLine`].
    #[cfg(test)]
    pub fn new(text: &'a str, lines: &'a [LineStyle]) -> Self {
        Self {
            text,
            lines,
            spans: &[],
            markers: &[],
            source_line: None,
        }
    }

    /// Text whose lines also carry what is marked inside them (要件 7.3.2).
    pub fn marked(text: &'a str, lines: &'a [LineStyle], spans: &'a [Vec<Emphasis>]) -> Self {
        Self {
            text,
            lines,
            spans,
            markers: &[],
            source_line: None,
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

    /// And which of them is shown as its own source (要件 7.3.1).
    pub fn with_source_line(mut self, line: Option<usize>) -> Self {
        self.source_line = line;
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
    /// How far in the whole block is set, in steps of
    /// [`Typography::indent_step`], 0 for a block at the margin (要件 7.3.2).
    ///
    /// **A block, not a line.** The indent has to move every visual line a
    /// quoted paragraph or a wrapped list item ran to, and the only thing that
    /// can do that is the layout box the block is set in — DirectWrite has no
    /// per-paragraph indent, and the box at the head of a line reaches the head
    /// only (技術検証 7.1). That is why a change of indenting ends a block.
    ///
    /// **A count of steps rather than a depth of quoting**, because two things
    /// ask for it and they add: an item inside a quote is set in by both. See
    /// [`LineStyle::indent_steps`], which is where the two are counted.
    pub indent_steps: u8,
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

/// A table set out as a grid of cells (要件 7.3.2).
///
/// **The one block that is not a single layout.** Everywhere else a block is
/// one `IDWriteTextLayout` and every question about a position is that layout's
/// hit test. A table cannot be: a row that wraps has to show the second line of
/// one cell *beside* the second line of the next, and the text of one layout
/// runs in one order — cell by cell, not row by row. So each cell is set in a
/// box of its own, and this says where the boxes are.
///
/// **Coordinates are the block's own**, the same space [`LineInfo`] uses: along
/// the flow axis from the block's content start, along the line axis from where
/// the block's text begins.
#[derive(Debug, Clone, PartialEq)]
pub struct TableGrid {
    /// Every cell, in reading order.
    pub cells: Vec<GridCell>,
    /// Where a rule stands across the table: one before each row, and one after
    /// the last.
    pub rules: Vec<f32>,
    /// And down it: the table's two edges, and each boundary between columns.
    pub columns: Vec<f32>,
    /// How far the table reaches along the line axis.
    pub reach: f32,
}

/// One cell of a table, and the box it is set in (要件 7.3.2).
#[derive(Debug, Clone, PartialEq)]
pub struct GridCell {
    /// The cell's own text, in UTF-16 units of the block.
    ///
    /// **What the reader sees and nothing else**: the bars and the padding the
    /// writer typed around them are not in any cell. A row shown as its own
    /// source (要件 7.3.1) is one cell holding the whole line, bars included.
    pub utf16_start: u32,
    pub utf16_len: u32,
    /// Which row and column, so that a rule and a cell can be talked about
    /// together.
    pub row: usize,
    pub column: usize,
    pub flow_start: f32,
    pub flow_size: f32,
    pub line_start: f32,
    pub line_size: f32,
    /// How the cell is set inside its box, as the delimiter row said.
    pub align: Align,
    /// Whether the header's own weight stands over it.
    pub header: bool,
    /// What is marked inside the cell, in UTF-16 units of the cell itself.
    ///
    /// **Kept rather than worked out again where it is drawn.** The cell was
    /// measured with these, and a cell drawn with anything else is a column
    /// that does not line up — the rule `WrapPoints::line_starts` was changed
    /// for (6.10).
    pub marks: Vec<StyleRun>,
}

impl TableGrid {
    /// The cell a position belongs to.
    ///
    /// **Every position in a table belongs to one.** The bars, the padding the
    /// writer typed around them and the delimiter row are in no cell at all —
    /// and a caret can still be put in any of them, so the answer has to be a
    /// place on the page. **The cell that begins last before it** is that
    /// place: it is the one the reader would say the caret was just after.
    pub fn cell_at(&self, utf16: u32) -> Option<usize> {
        let mut found = None;
        for (index, cell) in self.cells.iter().enumerate() {
            if cell.utf16_start <= utf16 {
                found = Some(index);
            }
        }
        found.or(if self.cells.is_empty() { None } else { Some(0) })
    }
}

/// What one block layout measured to. Produced by the DirectWrite side.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockMeasure {
    /// How far the drawn lines reach along the flow axis.
    pub flow_size: f32,
    /// How far the longest of them reaches **across** it, inside the block's own
    /// box — its indent is not counted, because the box already had it taken
    /// off. Only a document that is not wrapped needs this: there the page is as
    /// wide as the longest line, and this is where that width comes from
    /// (要件 9).
    pub line_reach: f32,
    /// The first drawn flow coordinate in the block layout's own space.
    pub content_flow_start: f32,
    /// The `maxWidth` the block layout was created with. A layout must be
    /// recreated with the same bound to reproduce these coordinates.
    pub max_flow_size: f32,
    /// Shared, not owned: a measurement is cloned into the placement plan on
    /// every update, and copying a line table per block per keystroke was
    /// costing more than the measuring did.
    ///
    /// **`Arc` rather than `Rc`, so that a measurement can be taken on another
    /// thread** (要件 2, 技術検証 7.4). A block is measured from its own text
    /// and nothing else, so measuring is the part of the work that divides;
    /// what stopped it dividing was this one field, because a line table behind
    /// a non-atomic count cannot leave the thread that made it. The atomic
    /// costs a few nanoseconds per clone against the copy this exists to avoid.
    pub lines: Arc<[LineInfo]>,
    /// Set for the one block that is a table (要件 7.3.2), and `None` for every
    /// other. Shared for the reason the line table is.
    pub grid: Option<Arc<TableGrid>>,
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
    pub lines: Arc<[LineInfo]>,
    pub grid: Option<Arc<TableGrid>>,
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
/// The layout box is the pane less a margin at each end. Each margin reserves
/// six marker cells and half a cell of separation, so thirteen font sizes come off. What
/// divides into the rest is the *advance*, not the size, so widening the
/// character spacing fits fewer characters in the same pane.
pub fn cells_per_line(line_extent: u32, typography: &Typography) -> u32 {
    let font_size = typography.font_size.max(1.0);
    let advance = typography.cell_advance();
    let usable = (line_extent as f32 - font_size * 13.0).max(advance);
    (usable / advance).floor().max(1.0) as u32
}

/// The line extent that holds exactly `cells` characters of body text (要件 9).
///
/// **The inverse of [`cells_per_line`]**, so that a width the writer gives in
/// characters and a width the engine is given in pixels mean the same thing.
/// Characters rather than pixels is the whole point: the number stays true when
/// the body size or the zoom changes, and a pixel width would not.
pub fn line_extent_for_cells(cells: u32, typography: &Typography) -> u32 {
    let advance = typography.cell_advance();
    let padding = typography.font_size.max(1.0) * 13.0;
    (cells.max(1) as f32 * advance + padding).ceil() as u32
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
    let wrapped = visual_lines(characters, cells_per_line, typography, style);
    let charged = wrapped as f32 * cells_per_line as f32 * typography.flow_scale(level);
    charged.ceil().clamp(1.0, u32::MAX as f32) as u32
}

/// How many lines one logical line is estimated to take.
///
/// The first half of what [`line_cells`] charges for, on its own because
/// **whether a line wraps is a different question from how far it reaches**:
/// the charge multiplies by the line spacing, so one line of loosely set text
/// already costs more than a line's worth of cells and cannot be compared
/// against one.
///
/// An estimate, like everything the split cuts by. DirectWrite breaks at words
/// rather than wherever the character count lands, so a line this calls one
/// line may be two.
fn visual_lines(
    characters: u32,
    cells_per_line: u32,
    typography: &Typography,
    style: LineStyle,
) -> u32 {
    let cells_per_line = cells_per_line.max(1);
    let size_scale = typography.size_scale(style.heading_level);
    let fitting = (cells_per_line as f32 / size_scale).floor().max(1.0) as u32;
    characters.div_ceil(fitting).max(1)
}

/// How many of a document's list items take more than one line, at the geometry
/// the split is cutting by (要件 7.3.2).
///
/// **The number that says what a hanging indent costs.** An indent has to be
/// the block's, because a box at the head of a line reaches that head and no
/// further (技術検証 7.1), so every item given one has to be cut out as a block
/// of its own. An item that fits on one line has no continuation to align and
/// needs no indent — so this, beside the count of every item, is the difference
/// between indenting all of them and indenting only the ones it shows.
///
/// **A count, so the estimate matters differently here.** For the split, a line
/// called one line when it is two is the safe direction; for this it is an item
/// missed. Near the boundary the number is low rather than wrong, and it is the
/// same estimate the blocks would be cut by, so it is off exactly where they
/// would be.
///
/// Walks the document, so it is asked once per split and not once per refresh.
pub fn wrapping_list_lines(
    styled: StyledText<'_>,
    cells_per_line: u32,
    typography: &Typography,
) -> usize {
    let mut wrapping = 0;
    for (index, line) in styled.text.split_inclusive('\n').enumerate() {
        let style = styled.style_at(index);
        if !style.kind.is_list() {
            continue;
        }
        let characters = line.trim_end_matches('\n').encode_utf16().count() as u32;
        if visual_lines(characters, cells_per_line, typography, style) > 1 {
            wrapping += 1;
        }
    }
    wrapping
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
/// One logical line too long to be a block, as the search for its wrap
/// positions has to see it (要件 2.3).
///
/// **Everything that moves a break.** A cut position is a line start only for a
/// layout set the way the pieces will be set: the size a heading asks for, the
/// width one level of quoting leaves, the box standing at the head, and the
/// families and weights the markers inside call for. Asked without them, the
/// answer is a list of positions the block does not actually break at, and a
/// piece beginning at one of those is drawn from the middle of a line.
#[derive(Clone, Copy)]
pub struct LongLine<'a> {
    pub text: &'a str,
    pub style: LineStyle,
    /// What is marked inside it, in UTF-16 units from the head of the line.
    pub marks: &'a [Emphasis],
    /// The box standing at its head, if one does.
    pub marker: Option<LineMarker>,
    /// How far in the block holding it is set (要件 7.3.2).
    ///
    /// **Told rather than worked out again.** Whether a line is indented at all
    /// depends on which pane is asking (要件 7.3.1), and the split is where
    /// that is decided; a search that read [`LineStyle::indent_steps`] for
    /// itself would look for wrap positions at the full width of a source pane
    /// whose blocks are cut at it, and at the full width of a preview whose
    /// blocks are not.
    pub indent_steps: u8,
}

impl LongLine<'_> {
    /// The marks that reach into the suffix beginning at `byte`, measured from
    /// that suffix's own start.
    ///
    /// **A window and a resumed search both lay out a suffix**, and a mark is
    /// measured from the head of the line: one that ended before the suffix has
    /// nothing to say about it, and one that straddles the cut says it about
    /// the part that is there. The box at the head belongs to the head alone,
    /// and is left to the caller.
    pub fn marks_from(&self, byte: usize) -> Vec<Emphasis> {
        if byte == 0 {
            return self.marks.to_vec();
        }
        let before = self.text[..byte].encode_utf16().count() as u32;
        self.marks
            .iter()
            .filter_map(|mark| {
                let mark_end = mark.utf16_start + mark.utf16_len;
                let start = mark.utf16_start.max(before);
                (mark_end > start).then(|| Emphasis {
                    utf16_start: start - before,
                    utf16_len: mark_end - start,
                    marks: mark.marks,
                    // **切られた側の箱は連れていかない**（要件 7.8）。ルビの
                    // 箱は親文字を手前に数えて置き場所を決めるので、親が向こう
                    // 側へ残った切れ端では指す先が無い。読みは本文に居るので
                    // 字が消えることもない——組めないルビは、組まない。
                    ornament: (mark.utf16_start >= before)
                        .then_some(mark.ornament)
                        .flatten(),
                })
            })
            .collect()
    }
}

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
    /// **The whole line, not just its heading level.** See [`LongLine`] for
    /// what has to travel with it and why.
    fn line_starts(&mut self, line: LongLine<'_>) -> Vec<usize>;
}

/// One logical line a split asked about, in a form that can leave the thread.
///
/// Everything [`LongLine`] borrows, owned — because the answer is worked out
/// somewhere else and the document goes on being edited in the meantime.
#[derive(Clone)]
pub struct AskedLine {
    pub byte_start: usize,
    pub text: String,
    pub style: LineStyle,
    pub marks: Vec<Emphasis>,
    pub marker: Option<LineMarker>,
    pub indent_steps: u8,
}

impl AskedLine {
    /// The borrowed form, for a search that is about to be run.
    pub fn borrowed(&self) -> LongLine<'_> {
        LongLine {
            text: &self.text,
            style: self.style,
            marks: &self.marks,
            marker: self.marker,
            indent_steps: self.indent_steps,
        }
    }
}

/// A [`WrapPoints`] that answers nothing and writes down what it was asked
/// (要件 2).
///
/// **The questions, not the answers.** Which logical lines a split has to ask
/// about is decided by the split, from each line's own size; asking it is the
/// only way to find out without keeping a second copy of that rule — and a
/// second copy is a second opinion about where blocks end, which is the one
/// thing this module cannot afford (see [`split_blocks`]).
///
/// Answering nothing is safe: every line it was asked about stays the one
/// oversized block it would have been. The blocks of this pass are used as they
/// are when nothing was asked, and thrown away when something was.
#[derive(Default)]
pub struct RecordedWraps {
    pub asked: Vec<AskedLine>,
    source_address: Option<usize>,
}

impl RecordedWraps {
    pub fn for_text(text: &str) -> Self {
        Self {
            asked: Vec::new(),
            source_address: Some(text.as_ptr() as usize),
        }
    }
}

impl WrapPoints for RecordedWraps {
    fn line_starts(&mut self, line: LongLine<'_>) -> Vec<usize> {
        self.asked.push(AskedLine {
            byte_start: self
                .source_address
                .map_or(0, |base| line.text.as_ptr() as usize - base),
            text: line.text.to_owned(),
            style: line.style,
            marks: line.marks.to_vec(),
            marker: line.marker,
            indent_steps: line.indent_steps,
        });
        Vec::new()
    }
}

/// A [`WrapPoints`] that answers from a table worked out ahead of time
/// (要件 2).
///
/// **Answered in the order they are asked**, which is exact rather than
/// hopeful: the recording pass and this one run the same split over the same
/// text with the same spec, and what a split asks about depends on each line's
/// own size and on nothing that came back. So the *n*th question here is the
/// *n*th question there.
pub struct PreparedWraps {
    answers: Vec<Vec<usize>>,
    next: usize,
}

impl PreparedWraps {
    pub fn new(answers: Vec<Vec<usize>>) -> Self {
        Self { answers, next: 0 }
    }
}

impl WrapPoints for PreparedWraps {
    fn line_starts(&mut self, _line: LongLine<'_>) -> Vec<usize> {
        let answer = self.answers.get(self.next).cloned().unwrap_or_default();
        self.next += 1;
        answer
    }
}

/// A [`WrapPoints`] that never cuts: the behaviour of the split before it could
/// cut inside a line. Only the tests that are not about long paragraphs want
/// this, so it is not built into the editor.
#[cfg(test)]
pub struct NeverWraps;

#[cfg(test)]
impl WrapPoints for NeverWraps {
    fn line_starts(&mut self, _line: LongLine<'_>) -> Vec<usize> {
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
    let mut block_indent = 0_u8;
    let mut block_table = false;

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
        // **Counted by subtraction, not by walking the line again.** Only `\n`
        // was trimmed and it is one byte and one UTF-16 unit, so what came off
        // is the same number in both. Walking twice cost a second pass over the
        // whole document on every keystroke, and the split already walks it
        // once (2026-09-06).
        let characters = line_units - (line.len() - body.len()) as u32;
        // Charge for whole lines, so a blank line costs a line like any other,
        // and for the heading size, so a heading costs what it takes up.
        let index = line_index;
        line_index += 1;
        let mut style = styled.style_at(index);
        if !indents {
            style.quote_depth = 0;
        }
        // 要件 7.3.1: the source pane is set at the margin, for the reason it
        // gets no boxes — the markers are its text, and an indent would move
        // the very markup being read.
        let indent = if indents { style.indent_steps() } else { 0 };
        let line_cells = line_cells(characters, cells_per_line, typography, style);

        // 要件 7.3.2: a block is set in one layout box, so a change of indenting
        // ends one **whatever size it has reached**. The other two reasons to
        // end a block are about how big it has grown; this one is about what it
        // is, and a block holding both would have to be set at two widths at
        // once. **Before the long-line branch below**, so the piece already
        // gathered is closed under the indent it was gathered at.
        //
        // **A run of items is one block, not one block per item.** They share
        // an indent, and what differs between them — which marker stands in the
        // gutter — is a line's business and is drawn from the line's own
        // rectangle. Cutting per item would have multiplied the blocks of a
        // list-heavy document by the number of its items (技術検証 7.1).
        // 要件 7.3.2: **and a table is a block of its own**, for a reason of
        // the same kind. Its rows are not set as lines of text at all — each
        // cell is set in its own box so that a long one wraps inside its column
        // (技術検証 7.7) — so a block holding a table and a paragraph would
        // have to be laid out two ways at once. It was already true that a
        // table could not be cut in two; this says it cannot share either.
        let table = style.kind.is_table();
        if block_indent != indent || block_table != table {
            if block_byte_start < byte_cursor {
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: byte_cursor,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    indent_steps: block_indent,
                });
                block_byte_start = byte_cursor;
                block_utf16_start = utf16_cursor;
                block_cells = 0;
            }
            block_indent = indent;
            block_table = table;
        }

        // A line that fills a block on its own is cut inside itself. The block
        // being accumulated is closed first, so the long line starts one of its
        // own: a cut position inside it is a line start only for text that
        // begins where the layout began.
        //
        // **Never a table row** (要件 7.3.2): a row cut in half is two rows,
        // and each half would be measured into columns of its own. A row long
        // enough to reach here is a wide table, and a wide table is what 要件 9
        // lets run off the side of the pane.
        if line_cells > BLOCK_MAX_CELLS && !style.kind.is_table() {
            if block_byte_start < byte_cursor {
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: byte_cursor,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    indent_steps: block_indent,
                });
                block_byte_start = byte_cursor;
                block_utf16_start = utf16_cursor;
            }
            let long = LongLine {
                text: body,
                style,
                marks: styled.marks_at(index),
                marker: styled.marker_at(index),
                indent_steps: indent,
            };
            let pieces = cut_long_line(long, cells_per_line, typography, wraps);
            for piece_end in pieces {
                let piece_end = byte_cursor + piece_end;
                utf16_cursor += text[block_byte_start..piece_end].encode_utf16().count() as u32;
                blocks.push(BlockSpan {
                    byte_start: block_byte_start,
                    byte_end: piece_end,
                    utf16_start: block_utf16_start,
                    utf16_end: utf16_cursor,
                    indent_steps: block_indent,
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
                indent_steps: block_indent,
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

        // 要件 7.3.2: **a fenced block is one thing on the page**, so an
        // ordinary boundary does not fall inside it. A ground cut in two has a
        // seam, and the half that holds no fence cannot tell its own end from
        // the cut — a piece that was only the closing fence looked like a whole
        // code block and was rounded as one. `BLOCK_MAX_CELLS` still caps the
        // block, so this cannot make one unbounded.
        //
        // **This is the one boundary that looks past its own line**: a fence
        // opened far above decides it. `line_cells` already reads the style for
        // the same reason, so the boundaries were never quite text-local once a
        // fence was in the document; this widens that rather than starting it.
        //
        // 要件 7.3.2: **a table is one block for a stronger reason still.** Its
        // columns are as wide as the widest cell anywhere in it, so the two
        // halves of a table cut in two would be measured apart and set at
        // different widths — the seam would be visible in every row, not only
        // at the cut (技術検証 7.7).
        let may_end = !style.kind.is_code() && !style.kind.is_table();
        // **And the cap does not hold a table back either** (2026-09-06). A
        // fence is capped because a cut inside one costs a seam and nothing
        // more; a table cut anywhere is a second table — its own widest cells,
        // its own header row — so a 25-row table came out as five tables of
        // different shapes stacked on each other. The size cap buys nothing
        // here in exchange: the columns are a function of every row, so half a
        // table costs what measuring the whole of it costs, and the cut only
        // adds a second measurement of the other half.
        //
        // **This is the one block whose size the document decides.** A table
        // the writer keeps growing keeps one block growing with it; that is
        // what 要件 7.3.2 asks for, and no cut can be put back without the
        // seam coming with it.
        let capped = !style.kind.is_table();
        if (capped && block_cells >= BLOCK_MAX_CELLS)
            || (block_cells >= BLOCK_MIN_CELLS && may_end && ends_a_block(body, line_cells))
        {
            blocks.push(BlockSpan {
                byte_start: block_byte_start,
                byte_end: byte_cursor,
                utf16_start: block_utf16_start,
                utf16_end: utf16_cursor,
                indent_steps: block_indent,
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
            indent_steps: block_indent,
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
    line: LongLine<'_>,
    cells_per_line: u32,
    typography: &Typography,
    wraps: &mut dyn WrapPoints,
) -> Vec<usize> {
    let body = line.text;
    let scale = typography.flow_scale(line.style.heading_level);
    let per_visual_line = (cells_per_line as f32 * scale).max(1.0);
    let lines_per_piece = (BLOCK_MAX_CELLS as f32 / per_visual_line).floor().max(1.0) as usize;
    wraps
        .line_starts(line)
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
            grid: measure.grid.clone(),
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

    /// How many layout lines the plan holds, over every block.
    ///
    /// Only a bound: 要件 7.1's rectangle walks lines one at a time, and this
    /// is how the walk knows it cannot be going round.
    pub fn line_count(&self) -> usize {
        self.blocks.iter().map(|block| block.lines.len()).sum()
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
    /// Which slice across the flow, counted from the near edge of the page
    /// (要件 9). **A page no wider than the pane is one slice**, which is every
    /// wrapped document and exactly what this was before a line could be longer
    /// than the pane it is written in.
    pub cross_index: u32,
    pub flow_start: u32,
    pub flow_size: u32,
    pub cross_start: u32,
    pub cross_size: u32,
}

/// How the page is cut across the flow, and which of it the pane is showing
/// (要件 9).
///
/// **A tile is bounded by construction.** Its flow extent is chosen to keep the
/// pixels per tile roughly constant, and without this its extent across the flow
/// was the whole page — which is fine while the page is a pane wide and
/// impossible once a line may be as long as the writer likes.
#[derive(Debug, Clone, Copy)]
pub struct CrossSlices {
    /// The page's whole extent across the flow.
    pub extent: u32,
    /// How far one slice reaches.
    pub tile_size: u32,
    /// Where the pane is looking: the scroll offset, zero or negative.
    pub viewport: f32,
    /// And how much of it the pane shows.
    pub visible: f32,
}

impl CrossSlices {
    /// The slices the pane is showing, as `(first, last + 1)`.
    fn shown(&self) -> (u32, u32) {
        let size = self.tile_size.max(1);
        let count = self.extent.max(1).div_ceil(size);
        let (start, end) = visible_flow_range(self.viewport, self.visible, self.extent as f32);
        let first = (start as u32 / size).min(count - 1);
        let last = ((end.ceil() as u32).saturating_sub(1) / size).min(count - 1);
        (first, last + 1)
    }

    /// Where one slice begins and how far it reaches, held inside the page.
    fn slice(&self, index: u32) -> (u32, u32) {
        let size = self.tile_size.max(1);
        let start = index * size;
        (start, size.min(self.extent.saturating_sub(start)))
    }
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
            // The slice across the flow is not the block's business: every block
            // is cut the same way there, by the page (`CrossSlices`).
            cross_index: 0,
            flow_start: start,
            flow_size: end.saturating_sub(start),
            cross_start: 0,
            cross_size: 0,
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
        across: CrossSlices,
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
                    // 要件 9: and one per slice of the page the pane is showing
                    // across the flow. **Only the ones on screen** — a line the
                    // writer has to scroll to see is a line whose far end costs
                    // nothing until they do.
                    let (first, end) = across.shown();
                    for cross_index in first..end {
                        let (cross_start, cross_size) = across.slice(cross_index);
                        tiles.push(TileSpan {
                            block_index,
                            cross_index,
                            cross_start,
                            cross_size,
                            ..tile
                        });
                    }
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
    ///
    /// **One bar for the whole quote, not one per line.** Two bars that meet
    /// are two shapes drawn over the same edge, and the pixel they share is
    /// composited twice — lighter or heavier than the rest of the bar, and the
    /// seam is visible.
    Quote { depth: u8 },
    /// The stroke a `---` line is set as.
    Rule,
    /// The ground a fenced block sits on.
    ///
    /// **One run for the whole block**, for the reason a quote's bar is one
    /// bar, and because a ground drawn line by line could not be rounded at
    /// its corners. The fences at each end are hidden rather than removed, so
    /// the run reaches over them and they become the block's padding.
    Code,
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
    /// Whether each end of this run is the mark's own end, as against the point
    /// where the block it was gathered in ran out.
    ///
    /// Only a ground has corners to round, and **a corner rounded at a seam is
    /// a notch**. A fenced block cut in two by a block boundary must be square
    /// where the halves meet and round only at its fences — and **where that
    /// boundary fell is a property of the split, not of the document**, so the
    /// fences are the only thing that says which end is which.
    ///
    /// **A fence at the end of a run is that end**, which holds because an
    /// ordinary boundary never falls inside a fenced block (`split_blocks`): a
    /// closing fence therefore never begins a block, and a run that begins at
    /// a fence begins at the opening one. A bar and a stroke have no corners;
    /// theirs are set true and read by nobody.
    pub own_ends: (bool, bool),
}

/// One row of a table, as offsets into the block that holds it (要件 7.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableRowSpan {
    /// Which of the block's logical lines the row is, so that what its cells
    /// have marked can be found beside it.
    pub line: usize,
    /// The row's own line, as bytes of the block, its break excluded.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Where that line begins, in UTF-16 units of the block — which is what a
    /// DirectWrite range is measured in.
    pub utf16_start: u32,
}

/// One table a block holds (要件 7.3.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    /// The rows that show cells. **The delimiter row is not one of them**: it
    /// goes under one box whole, so it has no cells to measure and no bars to
    /// hold a column's width.
    pub rows: Vec<TableRowSpan>,
    /// That delimiter row, which is where the rule under the header is drawn
    /// (要件 7.3.2). `None` for a table whose delimiter row fell outside this
    /// block, which a table never does (`split_blocks`) — the field is an
    /// option because a `Default` table has to be able to say it has none yet.
    pub rule: Option<TableRowSpan>,
}

/// The tables one block holds (要件 7.3.2).
///
/// **One entry per table rather than one per row**: a column is as wide as the
/// widest cell in its own table, and two tables in one block share nothing. A
/// table never crosses a block boundary (`split_blocks`), so every table here
/// is a whole one.
///
/// 要件 7.3.1: a source pane gets none, for the reason it gets no boxes — the
/// bars are the characters being edited.
pub fn tables(styled: StyledText<'_>) -> Vec<Table> {
    if !styled.is_preview() {
        return Vec::new();
    }
    let mut tables: Vec<Table> = Vec::new();
    let mut open = false;
    let mut byte_start = 0;
    let mut utf16_start = 0_u32;
    for (index, line) in styled.text.split('\n').enumerate() {
        let kind = styled.kind_at(index);
        if kind.is_table() {
            if !open {
                tables.push(Table::default());
                open = true;
            }
            if let Some(table) = tables.last_mut() {
                let span = TableRowSpan {
                    line: index,
                    byte_start,
                    byte_end: byte_start + line.len(),
                    utf16_start,
                };
                match kind {
                    LineKind::TableRule => table.rule = Some(span),
                    _ => table.rows.push(span),
                }
            }
        } else {
            open = false;
        }
        // Past the newline this split consumed.
        byte_start += line.len() + 1;
        utf16_start += line.encode_utf16().count() as u32 + 1;
    }
    tables
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
    // What is being gathered, where it began and where its last line ended.
    // **Gathered rather than emitted line by line**, for the reason
    // [`LineOrnament`] gives: two shapes that meet share an edge, and a shared
    // edge is a seam.
    let mut held: Option<Gathering> = None;
    for (index, line) in styled.text.split('\n').enumerate() {
        let utf16_len = line.encode_utf16().count() as u32;
        let style = styled.style_at(index);
        // **Code before quoting, and the two never meet anyway**: a fence
        // inside a blockquote is not a fence (`document::line_style`), so a
        // line is at most one of these.
        let ornament = if style.kind.is_code() {
            Some(LineOrnament::Code)
        } else if style.quote_depth > 0 {
            Some(LineOrnament::Quote {
                depth: style.quote_depth,
            })
        } else {
            None
        };
        let line_end = utf16_start + utf16_len;
        // **Only a fence says where a ground's own end is** (see
        // [`LineRun::own_ends`]).
        let own_end =
            !matches!(ornament, Some(LineOrnament::Code)) || matches!(style.kind, LineKind::Fence);
        if held.map(|held| held.ornament) == ornament {
            if let Some(held) = held.as_mut() {
                held.utf16_end = line_end;
                held.own_ends.1 = own_end;
            }
        } else {
            runs.extend(finished(held.take()));
            held = ornament.map(|ornament| Gathering {
                ornament,
                utf16_start,
                utf16_end: line_end,
                own_ends: (own_end, own_end),
            });
        }
        // **A rule is one line and never more**, so it is not gathered — and a
        // quoted one carries both marks, the way `quote_depth` sits beside
        // `kind` rather than inside it.
        if matches!(style.kind, LineKind::Rule) {
            runs.push(LineRun {
                utf16_start,
                utf16_len,
                ornament: LineOrnament::Rule,
                own_ends: (true, true),
            });
        }
        // Past the newline this split consumed.
        utf16_start += utf16_len + 1;
    }
    // A quote or a fence that reaches the end of the block, which is what a
    // document being typed into looks like.
    runs.extend(finished(held));
    runs
}

/// The stretch of consecutive lines being gathered into one whole-line mark.
#[derive(Clone, Copy)]
struct Gathering {
    ornament: LineOrnament,
    utf16_start: u32,
    utf16_end: u32,
    own_ends: (bool, bool),
}

/// The gathered stretch as a run, if there was one.
fn finished(held: Option<Gathering>) -> Option<LineRun> {
    let held = held?;
    Some(LineRun {
        utf16_start: held.utf16_start,
        utf16_len: held.utf16_end - held.utf16_start,
        ornament: held.ornament,
        own_ends: held.own_ends,
    })
}

/// The block-local ranges that are not body text.
///
/// Body lines are left out, so a document with no headings costs nothing to
/// format. The trailing newline is left out of every run: a line's advance is
/// the tallest thing on it, so the break itself never needs the heading size,
/// and leaving it at body size keeps the empty line a block gives up (see
/// `measure_block`) the size it has always been.
///
/// `upright_digits`は**縦書きの面だけが立てる旗**（要件 7.8）。ここが
/// [`WritingMode`]を知らずに真偽で受けるのは、`text_blocks`が画面の向きを
/// 一度も知らずに済んでいるからで、知る必要があるのは「この面は数字を正立
/// させるか」だけである。面ごとに違う答えでよい——ブロックを測るのは面ごと
/// なので、同じ文書が横書きの面では数字をそのまま組む。
pub fn style_runs(styled: StyledText<'_>, upright_digits: bool) -> Vec<StyleRun> {
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
                heading_level,
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
                // 要件 7.8: ルビの読みを隠す箱はここから来る。**行頭の箱と
                // 同じ道**を通るので、幅0の`MarkerBox`を張るのも、当たった
                // 矩形へ墨を置くのも、増やした仕組みは無い。
                ornament: emphasis.ornament,
            });
        }
        // 要件 7.8: 縦中横。**最後に足す**ので、上で置かれた箱（行頭のマーカー、
        // ルビの読み）の範囲がもう分かっている——同じ字に2つの箱は張れない。
        if upright_digits && !kind.is_code() {
            // **借りて、返す。**張ってある箱の範囲を先に写しておく——
            // 数字の箱を足しながら同じ`runs`を読むことはできない。
            let boxed = runs
                .iter()
                .filter(|run: &&StyleRun| run.ornament.is_some())
                .map(|run| (run.utf16_start, run.utf16_start + run.utf16_len))
                .collect::<Vec<_>>();
            let taken =
                |from: u32, to: u32| boxed.iter().any(|(start, end)| *start < to && from < *end);
            for (from, len) in digit_runs(line) {
                // 1〜2桁は1つの箱に並べ、3桁以上は1桁ずつ縦に並べる。
                let step = if len <= 2 { len } else { 1 };
                for cell in (0..len).step_by(step as usize) {
                    let start = utf16_start + from + cell;
                    let length = step.min(len - cell);
                    if taken(start, start + length) {
                        continue;
                    }
                    runs.push(StyleRun {
                        utf16_start: start,
                        utf16_len: length,
                        heading_level,
                        marks: Marks::default(),
                        ornament: Some(Ornament::Upright),
                    });
                }
            }
        }
        // Past the newline this split consumed.
        utf16_start += utf16_len + 1;
    }
    runs
}

/// 行の中の半角数字の連なり——`(始まり, 長さ)`をUTF-16単位で（要件 7.8）。
///
/// **連なりで見るのは、桁数が組み方を決めるからである。**`2026`の`20`だけを
/// 縦中横にすると、読めない数になる。半角の`0-9`だけを数字とする——全角の
/// `０-９`は縦書きの中でもとから正立しているので、何もしなくてよい。
fn digit_runs(line: &str) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    let mut at = 0_u32;
    let mut run: Option<(u32, u32)> = None;
    for letter in line.chars() {
        let units = letter.len_utf16() as u32;
        if letter.is_ascii_digit() {
            run = Some(match run {
                Some((from, len)) => (from, len + units),
                None => (at, units),
            });
        } else if let Some(span) = run.take() {
            found.push(span);
        }
        at += units;
    }
    found.extend(run);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 要件 7.8: 縦中横。**1〜2桁は1マス、3桁以上は1桁ずつ。**書き手は何も
    /// 書かない——縦書きの面がそう組む、という決まりである。
    #[test]
    fn digits_stand_upright_a_cell_at_a_time() {
        let boxes = |text: &str| -> Vec<(u32, u32)> {
            let levels = vec![LineStyle::default(); text.split('\n').count()];
            style_runs(StyledText::new(text, &levels), true)
                .into_iter()
                .filter(|run| run.ornament == Some(Ornament::Upright))
                .map(|run| (run.utf16_start, run.utf16_len))
                .collect()
        };

        // 2桁は1つの箱に並ぶ。
        assert_eq!(boxes("20歳"), vec![(0, 2)]);
        // 1桁も正立させる——寝た数字はどの桁数でも数字に見えない。
        assert_eq!(boxes("5歳"), vec![(0, 1)]);
        // 3桁以上は1桁につき1マス（要件 7.8 の「縦に並べる」）。
        assert_eq!(boxes("2026年"), vec![(0, 1), (1, 1), (2, 1), (3, 1)]);
        // 連なりで見る——`2026`の`20`だけを縦中横にすると読めない数になる。
        assert_eq!(boxes("第2章と第10章"), vec![(1, 1), (5, 2)]);
        // 全角の数字はもともと正立しているので何もしない。
        assert_eq!(boxes("２０歳"), vec![]);
    }

    /// **横書きの面は数字に触らない。**横書きの数字はもともと正立していて、
    /// そこへ箱を張れば送りだけが変わる——何も直さずに幾何を動かすことになる。
    #[test]
    fn a_horizontal_sheet_leaves_its_digits_alone() {
        let text = "20歳";
        let levels = vec![LineStyle::default()];
        let runs = style_runs(StyledText::new(text, &levels), false);

        assert!(runs.iter().all(|run| run.ornament.is_none()));
    }

    /// **同じ字に箱は2つ張れない。**順序付きリストの`10.`はもうマーカーの箱が
    /// 覆っているので、縦中横はそこを避ける——避けなければ、あとから張った箱が
    /// 数字を1マスに詰め、マーカーの墨と重なる。
    #[test]
    fn digits_already_under_a_box_are_left_to_it() {
        let text = "10. 項目";
        let levels = vec![LineStyle::of_kind(LineKind::Ordered)];
        let markers = vec![Some(LineMarker {
            utf16_len: 4,
            ornament: Ornament::Number,
        })];
        let runs = style_runs(StyledText::new(text, &levels).with_markers(&markers), true);

        assert_eq!(
            runs.iter()
                .filter(|run| run.ornament == Some(Ornament::Upright))
                .count(),
            0
        );
    }

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
            indent_steps: 0,
        }
    }

    fn measure(flow_size: f32, lines: usize) -> BlockMeasure {
        BlockMeasure {
            flow_size,
            line_reach: 0.0,
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
            grid: None,
        }
    }

    /// Both orders, so a property that must hold either way is written once.
    const BOTH_ORDERS: [FlowOrder; 2] = [FlowOrder::Ascending, FlowOrder::Descending];

    /// Blocks of the given extents, laid out in order. Only the geometry
    /// matters here, so every block gets one throwaway line.
    /// One slice across the whole page, which is what every wrapped document
    /// has: the pane is as wide as the page (要件 9).
    fn one_slice() -> CrossSlices {
        CrossSlices {
            extent: 600,
            tile_size: 600,
            viewport: 0.0,
            visible: 600.0,
        }
    }

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
        fn line_starts(&mut self, line: LongLine<'_>) -> Vec<usize> {
            let text = line.text;
            text.char_indices()
                .enumerate()
                .filter(|(index, _)| *index > 0 && index % self.0 == 0)
                .map(|(_, (offset, _))| offset)
                .collect()
        }
    }

    /// **Asking and then answering must cut where one pass would** (要件 2).
    /// The recording pass exists so the answers can be found somewhere else,
    /// and the whole idea rests on two things: that it asks exactly what a real
    /// pass asks, and that it asks in the same order — which is why the answers
    /// can be matched to the questions by position.
    ///
    /// It holds because **what a split asks about depends on each line's own
    /// size and on nothing that came back**. If that ever stops being true this
    /// is the test that says so, and it says it without DirectWrite.
    #[test]
    fn asking_and_then_answering_cuts_where_one_pass_would() {
        let text = format!("{}\n短い行\n", "あ".repeat(CELLS as usize * 200));
        let styled = StyledText::plain(&text);
        let typography = plain_typography();
        let mut once_through = EveryNCharacters(CELLS as usize);

        let once = split_blocks(styled, CELLS, &typography, &mut once_through);

        let mut asking = RecordedWraps::default();
        let recorded = split_blocks(styled, CELLS, &typography, &mut asking);
        let answers = asking
            .asked
            .iter()
            .map(|line| EveryNCharacters(CELLS as usize).line_starts(line.borrowed()))
            .collect::<Vec<Vec<usize>>>();
        let twice = split_blocks(styled, CELLS, &typography, &mut PreparedWraps::new(answers));

        assert_eq!(asking.asked.len(), 1, "one line was too long to be a block");
        // The recording pass answers nothing, so it cuts nothing: that is what
        // makes it safe to use its blocks when it was asked nothing at all.
        assert!(
            recorded.len() < once.len(),
            "answering nothing cut something"
        );
        assert_eq!(twice, once);
    }

    /// A line the split never asks about leaves the recording pass with the
    /// blocks the editor keeps: **an ordinary document is split once.**
    #[test]
    fn a_document_with_no_long_line_asks_nothing() {
        let text = "短い行\n".repeat(40);
        let mut asking = RecordedWraps::default();

        let blocks = split_blocks(
            StyledText::plain(&text),
            CELLS,
            &plain_typography(),
            &mut asking,
        );

        assert!(asking.asked.is_empty());
        assert_eq!(blocks, split(&text));
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

    /// **A fenced block is one thing on the page**, so an ordinary boundary
    /// does not fall inside it (要件 7.3.2): the ground would be cut in two,
    /// and the half holding no fence cannot tell its own end from the cut.
    /// `BLOCK_MAX_CELLS` still caps the block, so this cannot make one
    /// unbounded — the fence here is well under it.
    #[test]
    fn an_ordinary_boundary_does_not_fall_inside_a_fence() {
        let typography = plain_typography();
        let code = LineStyle::of_kind(LineKind::Code);
        let charged = |line: &str| {
            let characters = line.encode_utf16().count() as u32;
            line_cells(characters, CELLS, &typography, code)
        };
        // **Every line inside would end a block on its own**, so without the
        // guard the fence is certainly cut and this says something.
        let inside = (0..)
            .map(|n| format!("{}{n}", "あ".repeat(CELLS as usize - 4)))
            .filter(|line| ends_a_block(line, charged(line)))
            .take(20)
            .collect::<Vec<String>>();

        // Twelve short lines: under `BLOCK_MIN_CELLS`, so the block reaches the
        // fence still open and every line inside it is a candidate.
        let mut text = "本文\n".repeat(12);
        let mut levels = vec![LineStyle::default(); 12];
        let opened = text.len();
        text.push_str("```\n");
        levels.push(LineStyle::of_kind(LineKind::Fence));
        for line in &inside {
            text.push_str(line);
            text.push('\n');
            levels.push(code);
        }
        text.push_str("```\n");
        levels.push(LineStyle::of_kind(LineKind::Fence));
        let closed = text.len();
        levels.push(LineStyle::default());

        let blocks = split_with(StyledText::new(&text, &levels), &typography);

        for block in &blocks {
            assert!(
                block.byte_start <= opened || block.byte_start >= closed,
                "a block began at {} inside the fence {opened}..{closed}",
                block.byte_start
            );
        }
    }

    /// **And a table is a block of its own** (要件 7.3.2): nothing that is not
    /// part of it shares one. Its rows are not set as lines of text at all —
    /// each cell is set in its own box so that a long one wraps inside its
    /// column — so a block holding a table and a paragraph would have to be
    /// laid out two ways at once (技術検証 7.7).
    #[test]
    fn a_table_shares_its_block_with_nothing() {
        let text = "本文\n| 見出し | 二 |\n| --- | --- |\n| あ | い |\n本文\n";
        let levels = [
            LineStyle::default(),
            LineStyle::of_kind(LineKind::TableRow),
            LineStyle::of_kind(LineKind::TableRule),
            LineStyle::of_kind(LineKind::TableRow),
            LineStyle::default(),
        ];
        let blocks = split_with(StyledText::new(text, &levels), &plain_typography());

        let table_start = text.find("| 見出し").expect("the table is in the text");
        let table_end = text.rfind("本文").expect("the text after it");
        let holding = blocks
            .iter()
            .filter(|block| block.byte_start < table_end && block.byte_end > table_start)
            .collect::<Vec<&BlockSpan>>();
        assert_eq!(holding.len(), 1, "{blocks:?}");
        assert_eq!(holding[0].byte_start, table_start, "{blocks:?}");
        assert_eq!(holding[0].byte_end, table_end, "{blocks:?}");
    }

    /// **A table is one block** (要件 7.3.2), for a stronger reason than a
    /// fenced block is: its columns are as wide as the widest cell anywhere in
    /// it, so two halves measured apart would be set at different widths and
    /// every row would show the seam (技術検証 7.7).
    #[test]
    fn an_ordinary_boundary_does_not_fall_inside_a_table() {
        let typography = plain_typography();
        let row = LineStyle::of_kind(LineKind::TableRow);
        let charged = |line: &str| {
            let characters = line.encode_utf16().count() as u32;
            line_cells(characters, CELLS, &typography, row)
        };
        // **Every row would end a block on its own**, so without the guard the
        // table is certainly cut and this says something.
        let rows = (0..)
            .map(|n| format!("| {}{n} |", "あ".repeat(CELLS as usize - 10)))
            .filter(|line| ends_a_block(line, charged(line)))
            .take(15)
            .collect::<Vec<String>>();

        // Twelve short lines first, so the block reaches the table still under
        // `BLOCK_MIN_CELLS` and every row inside it is a candidate. Fifteen
        // rows short enough not to wrap keep the whole of it under
        // `BLOCK_MAX_CELLS`, which is the one boundary this guard does not hold
        // back.
        let mut text = "本文\n".repeat(12);
        let mut levels = vec![LineStyle::default(); 12];
        let opened = text.len();
        text.push_str("| 見出し |\n| --- |\n");
        levels.push(row);
        levels.push(LineStyle::of_kind(LineKind::TableRule));
        for line in &rows {
            text.push_str(line);
            text.push('\n');
            levels.push(row);
        }
        let closed = text.len();
        text.push_str("本文\n");
        levels.push(LineStyle::default());

        let blocks = split_with(StyledText::new(&text, &levels), &typography);

        for block in &blocks {
            assert!(
                block.byte_start <= opened || block.byte_start >= closed,
                "a block began at {} inside the table {opened}..{closed}",
                block.byte_start
            );
        }
    }

    /// **And neither does the size cap** (要件 7.3.2, 2026-09-06). This is the
    /// boundary the guard above did not hold back, and it is the one a real
    /// table meets: a 25-row chapter table came out of the editor as five
    /// tables of different widths stacked on each other, each reading its own
    /// first row as a header. A table cut anywhere is a second table.
    #[test]
    fn the_size_cap_does_not_fall_inside_a_table() {
        let typography = plain_typography();
        let row = LineStyle::of_kind(LineKind::TableRow);
        // Rows wide enough to wrap, so the table passes `BLOCK_MAX_CELLS`
        // several times over — which is what a table of ordinary prose cells
        // does at any pane width a person writes in.
        let wide = format!("| {} |", "あ".repeat(CELLS as usize * 2));
        let charged = line_cells(wide.encode_utf16().count() as u32, CELLS, &typography, row);
        let rows = (BLOCK_MAX_CELLS / charged + 4).max(4) as usize;

        let mut text = String::from("| 見出し |\n| --- |\n");
        let mut levels = vec![row, LineStyle::of_kind(LineKind::TableRule)];
        let opened = 0;
        for _ in 0..rows {
            text.push_str(&wide);
            text.push('\n');
            levels.push(row);
        }
        let closed = text.len();
        text.push_str("本文\n");
        levels.push(LineStyle::default());

        let blocks = split_with(StyledText::new(&text, &levels), &typography);

        for block in &blocks {
            assert!(
                block.byte_start <= opened || block.byte_start >= closed,
                "a block began at {} inside the table {opened}..{closed} ({rows} rows)",
                block.byte_start
            );
        }
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
            .map(|block| block.indent_steps)
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
        assert_eq!(source[0].indent_steps, 0);
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

    /// **A run of items is one block, and it is the run that ends one**
    /// (要件 7.3.2). An item's indent belongs to its block, so items and body
    /// text cannot share one; but items share that indent with each other, so
    /// cutting per item would buy nothing and would multiply the blocks of a
    /// list-heavy document by its item count (技術検証 7.1). Which marker
    /// stands in the gutter is a line's business, and is drawn from the line's
    /// own rectangle rather than from a block of its own.
    #[test]
    fn a_run_of_items_is_one_indented_block() {
        let text = "本文\n- 一つめ\n- 二つめ\n- 三つめ\n本文へ戻る\n";
        let item = LineStyle::of_kind(LineKind::Bullet);
        let plain = LineStyle::default();
        let levels = [plain, item, item, item, plain];
        // A preview pane's text, which is the only one that indents (要件 7.3.1).
        let markers = [None; 5];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let blocks = split_with(styled, &plain_typography());

        let steps = blocks
            .iter()
            .map(|block| block.indent_steps)
            .collect::<Vec<u8>>();
        let run_end = "本文\n- 一つめ\n- 二つめ\n- 三つめ\n".len();
        assert_eq!(steps, vec![0, 1, 0]);
        assert_eq!(blocks[1].byte_end, run_end);
        // 要件 7.3.1: the source pane shows the markers themselves, so nothing
        // about them indents and nothing about them ends a block.
        let source = split_with(StyledText::new(text, &levels), &plain_typography());
        assert_eq!(source.len(), 1);
        assert_eq!(source[0].indent_steps, 0);
    }

    /// Being quoted and being an item are both indents, and they add: a block
    /// holding an item inside a quote is set in by two steps. **A count rather
    /// than a depth** is what lets them.
    #[test]
    fn a_quoted_item_is_set_in_by_both() {
        let quoted_item = LineStyle {
            kind: LineKind::Bullet,
            quote_depth: 1,
            list_indent: 1,
            ..LineStyle::default()
        };

        assert_eq!(LineStyle::default().indent_steps(), 0);
        assert_eq!(LineStyle::of_kind(LineKind::Bullet).indent_steps(), 1);
        assert_eq!(quoted_item.indent_steps(), 2);
        // A rule and a fence are whole lines of marks, not things set in.
        assert_eq!(LineStyle::of_kind(LineKind::Rule).indent_steps(), 0);
        // **A line that continues an item has the indent without the kind**,
        // which is the whole reason this is a count of steps rather than a
        // depth of nesting: there is no marker under it to count from.
        let continuing = LineStyle {
            list_indent: 2,
            ..LineStyle::default()
        };
        assert!(!continuing.kind.is_list());
        assert_eq!(continuing.indent_steps(), 2);
    }

    /// A cut position is a line start only for a layout set the way the pieces
    /// will be, so a long line has to be **asked about as it is set**
    /// (要件 7.3.2): its width, what is marked inside it, and the box at its
    /// head all travel with it.
    #[test]
    fn a_long_line_is_asked_about_as_it_is_set() {
        struct Records(Vec<(LineStyle, usize, bool)>);

        impl WrapPoints for Records {
            fn line_starts(&mut self, line: LongLine<'_>) -> Vec<usize> {
                let held = (line.style, line.marks.len(), line.marker.is_some());
                self.0.push(held);
                Vec::new()
            }
        }

        let text = format!("{}\n", "あ".repeat(CELLS as usize * 200));
        let quoted = LineStyle {
            quote_depth: 1,
            ..LineStyle::default()
        };
        let bold = Emphasis {
            utf16_start: 0,
            utf16_len: 4,
            marks: Marks {
                bold: true,
                ..Marks::default()
            },
            ornament: None,
        };
        let bullet = LineMarker {
            utf16_len: 2,
            ornament: Ornament::Bullet,
        };
        let levels = [quoted, LineStyle::default()];
        let spans = [vec![bold], Vec::new()];
        let markers = [Some(bullet), None];
        let mut asked = Records(Vec::new());

        split_blocks(
            StyledText::marked(&text, &levels, &spans).with_markers(&markers),
            CELLS,
            &plain_typography(),
            &mut asked,
        );

        assert_eq!(asked.0, vec![(quoted, 1, true)]);
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
        let stub = LongLine {
            text: body,
            style: LineStyle::default(),
            marks: &[],
            marker: None,
            indent_steps: 0,
        };
        let wraps = EveryNCharacters(CELLS as usize).line_starts(stub);
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

    /// 要件 9: a width given in characters and a width in pixels have to mean
    /// the same thing, whatever the body is set to.
    #[test]
    fn a_line_length_in_characters_holds_that_many_characters() {
        let plain = plain_typography();
        let spaced = Typography {
            character_spacing: 0.5,
            font_size: 24.0,
            ..plain.clone()
        };

        for asked in [1, 10, 40, 200] {
            for spec in [&plain, &spaced] {
                let extent = line_extent_for_cells(asked, spec);
                assert_eq!(cells_per_line(extent, spec), asked, "at {asked} cells");
            }
        }
    }

    /// What a hanging indent would cost is the items that wrap, not the items
    /// (要件 7.3.2). **Set loosely on purpose**: a single line of text spaced
    /// at 190% is charged nearly two lines' worth of cells, so a count that
    /// asked `line_cells` whether a line filled one would call every item here
    /// a wrapping one.
    #[test]
    fn counts_only_the_list_items_that_take_more_than_one_line() {
        let typography = Typography {
            line_spacing: 1.9,
            ..plain_typography()
        };
        let short = "あ".repeat(CELLS as usize / 2);
        let long = "あ".repeat(CELLS as usize * 3);
        let text = format!("- {short}\n- {long}\n- {short}\n");
        let lines = vec![LineStyle::of_kind(LineKind::Bullet); 3];
        let styled = StyledText::new(&text, &lines);

        assert_eq!(wrapping_list_lines(styled, CELLS, &typography), 1);
    }

    /// Only an item has a marker to hang its continuation under, so a paragraph
    /// that wraps is not one of these however long it is.
    #[test]
    fn a_long_line_that_is_not_an_item_is_not_counted() {
        let typography = plain_typography();
        let long = "あ".repeat(CELLS as usize * 3);
        let text = format!("{long}\n- {long}\n");
        let lines = vec![LineStyle::default(), LineStyle::of_kind(LineKind::Bullet)];
        let styled = StyledText::new(&text, &lines);

        assert_eq!(wrapping_list_lines(styled, CELLS, &typography), 1);
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

        let runs = style_runs(StyledText::new(text, &levels), false);

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

        let runs = style_runs(StyledText::new(text, &levels), false);

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
        let boxes = style_runs(styled, false)
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

        let runs = style_runs(StyledText::new(text, &levels), false);
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
                    own_ends: (true, true),
                },
                LineRun {
                    utf16_start: after("引用\n本文\n"),
                    utf16_len: after("---"),
                    ornament: LineOrnament::Rule,
                    own_ends: (true, true),
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
            vec![LineOrnament::Rule, LineOrnament::Quote { depth: 1 }]
        );
    }

    /// **One bar for the whole quote, not one per line** (要件 7.3.2). Two bars
    /// that meet share an edge, and a shared edge is composited twice: the seam
    /// shows, and it moves as the block boundaries move under the caret.
    #[test]
    fn a_run_of_quoted_lines_asks_for_one_bar() {
        let text = "引用の一行目\n引用の二行目\n本文";
        let quoted = LineStyle {
            quote_depth: 1,
            ..LineStyle::default()
        };
        let levels = [quoted, quoted, LineStyle::default()];
        let markers = [None; 3];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let runs = line_runs(styled);

        let after = |shown: &str| shown.encode_utf16().count() as u32;
        let expected = LineRun {
            utf16_start: 0,
            utf16_len: after("引用の一行目\n引用の二行目"),
            ornament: LineOrnament::Quote { depth: 1 },
            own_ends: (true, true),
        };
        assert_eq!(runs, vec![expected]);
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

    /// **One run for the whole fenced block**, reaching over the fences at each
    /// end (要件 7.3.2). A ground drawn line by line would seam at every break
    /// and could not be rounded at its corners.
    #[test]
    fn a_fenced_block_asks_for_one_ground() {
        let text = "本文\n```\nlet x = 1;\n```\n本文";
        let levels = [
            LineStyle::default(),
            LineStyle::of_kind(LineKind::Fence),
            LineStyle::of_kind(LineKind::Code),
            LineStyle::of_kind(LineKind::Fence),
            LineStyle::default(),
        ];
        let markers = [None; 5];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let runs = line_runs(styled);

        let after = |shown: &str| shown.encode_utf16().count() as u32;
        let expected = LineRun {
            utf16_start: after("本文\n"),
            utf16_len: after("```\nlet x = 1;\n```"),
            ornament: LineOrnament::Code,
            own_ends: (true, true),
        };
        assert_eq!(runs, vec![expected]);
    }

    /// A fence nobody closes grounds the rest of the block, which is what a
    /// document being typed into looks like.
    #[test]
    fn an_unclosed_fence_grounds_the_rest_of_the_block() {
        let text = "```\nlet x = 1;";
        let levels = [
            LineStyle::of_kind(LineKind::Fence),
            LineStyle::of_kind(LineKind::Code),
        ];
        let markers = [None; 2];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let runs = line_runs(styled);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].utf16_len, text.encode_utf16().count() as u32);
        // It opens at a fence and closes at nothing, because nothing closed it.
        assert_eq!(runs[0].own_ends, (true, false));
    }

    /// **A ground cut by a block boundary is square where the halves meet.**
    /// The half that holds no fence at an end did not end there — the split
    /// did — and where a split fell says nothing about the document, so only a
    /// fence may round a corner (要件 7.3.2).
    #[test]
    fn a_ground_that_holds_no_fence_does_not_own_that_end() {
        let text = "let x = 1;\nlet y = 2;\n```";
        let levels = [
            LineStyle::of_kind(LineKind::Code),
            LineStyle::of_kind(LineKind::Code),
            LineStyle::of_kind(LineKind::Fence),
        ];
        let markers = [None; 3];
        let styled = StyledText::new(text, &levels).with_markers(&markers);

        let runs = line_runs(styled);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].own_ends, (false, true));
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
                ornament: None,
            }],
            vec![Emphasis {
                utf16_start: 0,
                utf16_len: 2,
                marks: bold,
                ornament: None,
            }],
        ];

        let runs = style_runs(StyledText::marked(text, &levels, &spans), false);

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
        let plain = style_runs(StyledText::new(text, &levels), false);
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
            line_reach: 0.0,
            content_flow_start: 400.0,
            max_flow_size: 500.0,
            lines: Arc::from(Vec::new()),
            grid: None,
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
            let all = plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0, one_slice());

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

    /// 要件 9: a page wider than the pane is cut across the flow as well, and
    /// the pane is handed the slices it is showing and no others.
    #[test]
    fn a_page_wider_than_the_pane_is_cut_across_the_flow() {
        let plan = plan_of(&[900.0], 0.0, FlowOrder::Ascending);
        // A page of 5000 in slices of 2000: 0..2000, 2000..4000, 4000..5000.
        let across = |viewport: f32| CrossSlices {
            extent: 5_000,
            tile_size: 2_000,
            viewport,
            visible: 600.0,
        };

        let near = plan.visible_tiles(0.0, 900.0, 1024, 0, across(0.0));
        assert_eq!(
            near.iter().map(|tile| tile.cross_index).collect::<Vec<_>>(),
            [0],
            "the near edge shows the first slice alone"
        );
        assert_eq!((near[0].cross_start, near[0].cross_size), (0, 2_000));

        // Scrolled to sit across the boundary: both slices are wanted.
        let both = plan.visible_tiles(0.0, 900.0, 1024, 0, across(-1_800.0));
        assert_eq!(
            both.iter().map(|tile| tile.cross_index).collect::<Vec<_>>(),
            [0, 1]
        );

        // The last slice is short: a slice never reaches past the page.
        let far = plan.visible_tiles(0.0, 900.0, 1024, 0, across(-4_400.0));
        assert_eq!(far.len(), 1);
        assert_eq!((far[0].cross_start, far[0].cross_size), (4_000, 1_000));
    }

    /// And the case every wrapped document is: one slice, the whole page.
    #[test]
    fn a_page_that_fits_the_pane_is_one_slice() {
        let plan = plan_of(&[900.0; 3], 0.0, FlowOrder::Ascending);
        let tiles = plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0, one_slice());

        assert!(
            tiles.iter().all(|tile| tile.cross_index == 0
                && tile.cross_start == 0
                && tile.cross_size == 600),
            "every tile covers the page across the flow"
        );
    }

    #[test]
    fn keeps_only_visible_tiles_for_a_long_document() {
        for order in BOTH_ORDERS {
            let plan = plan_of(&[900.0; 40], 0.0, order);

            let at_start = plan.visible_tiles(0.0, 640.0, 1024, 0, one_slice());
            assert_eq!(at_start.len(), 1);
            let expected = match order {
                FlowOrder::Ascending => 0,
                FlowOrder::Descending => 39,
            };
            assert_eq!(
                at_start[0].block_index, expected,
                "the block at the origin end of the flow axis ({order:?})"
            );

            let middle = plan.visible_tiles(-18_000.0, 640.0, 1024, 0, one_slice());
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

            let tiles_of = |plan: &BlockLayoutPlan| {
                plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0, one_slice())
            };
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

        let tiles_of = |plan: &BlockLayoutPlan| {
            plan.visible_tiles(0.0, plan.total_flow_size, 1024, 0, one_slice())
        };
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

            let plain = plan.visible_tiles(-18_000.0, 640.0, 1024, 0, one_slice());
            let prefetched = plan.visible_tiles(-18_000.0, 640.0, 1024, 1, one_slice());
            assert!(prefetched.len() > plain.len());
            assert!(
                prefetched
                    .iter()
                    .all(|tile| tile.flow_end() <= plan.total_flow_size as u32)
            );
            assert!(
                plan.visible_tiles(0.0, 640.0, 1024, 1, one_slice())
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

        let usable = 520.0_f32 - 22.0 * 13.0;
        let cells = (usable / 22.0).floor() as usize;
        let lines = 100_usize.div_ceil(cells);
        assert!(
            bound > lines as f32 * 22.0 * 1.7,
            "the bound must exceed the width DirectWrite actually needs"
        );
    }

    /// 要件 7.3.2: a row is cut at its bars, and each cell is what stands
    /// between one bar and the next.
    #[test]
    fn a_row_is_cut_at_its_bars() {
        let line = "| 一 | 二 |";

        let cells = table_cells(line);

        let said = cells
            .iter()
            .map(|cell| &line[cell.byte_start..cell.byte_end])
            .collect::<Vec<_>>();
        assert_eq!(said, vec![" 一 ", " 二 ", ""]);
        // The bar that opens a cell is the byte before it.
        let bars: Vec<usize> = cells.iter().map(|cell| cell.byte_start - 1).collect();
        assert_eq!(bars, vec![0, 6, 12]);
    }

    /// 要件 7.3.2: a bar behind a backslash is one the writer wrote, and the
    /// only way to put one inside a cell.
    #[test]
    fn an_escaped_bar_does_not_divide() {
        let cells = table_cells(r"| a \| b | c |");

        assert_eq!(cells.len(), 3, "the escaped bar must not open a cell");
    }

    /// 要件 7.3.2: prose is full of bars and none of it is a table.
    #[test]
    fn only_a_line_that_opens_with_a_bar_is_a_row() {
        assert!(is_table_row("| 一 | 二 |"));
        assert!(is_table_row("|一|"));
        assert!(!is_table_row("A|B は選言である"));
        assert!(!is_table_row("  | 字下げされた表 |"));
        assert!(!is_table_row("|"), "one bar divides nothing");
    }

    /// 要件 7.3.2: a block hands over the rows that show cells, where they are
    /// in its own text, and nothing else. **The delimiter row is not one of
    /// them** — it goes under one box whole.
    #[test]
    fn a_block_hands_over_the_rows_that_show_cells() {
        let text = "本文\n| 一 | 二 |\n| --- | --- |\n| 三 | 四 |\n";
        let lines = [
            LineStyle::default(),
            LineStyle::of_kind(LineKind::TableRow),
            LineStyle::of_kind(LineKind::TableRule),
            LineStyle::of_kind(LineKind::TableRow),
            LineStyle::default(),
        ];
        let markers = vec![None; lines.len()];
        let styled = StyledText::marked(text, &lines, &[]).with_markers(&markers);

        let found = tables(styled);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rows.len(), 2, "the delimiter row shows no cells");
        let rule = found[0]
            .rule
            .expect("the delimiter row is where the rule goes");
        assert_eq!(&text[rule.byte_start..rule.byte_end], "| --- | --- |");
        let last = found[0].rows[1];
        assert_eq!(found[0].rows[0].line, 1);
        assert_eq!(last.line, 3);
        assert_eq!(&text[last.byte_start..last.byte_end], "| 三 | 四 |");
        // 3 units for 本文 and its break, 10 for the header row, 14 for the
        // delimiter row.
        assert_eq!(last.utf16_start, 27);
    }

    /// 要件 7.3.2: the delimiter row is what names the columns, and how each
    /// of them is set.
    #[test]
    fn the_delimiter_row_names_the_columns() {
        assert_eq!(
            table_alignments("| --- | :--- | :---: | ---: |"),
            Some(vec![Align::Start, Align::Start, Align::Center, Align::End])
        );
        assert_eq!(table_alignments("|---|---|"), Some(vec![Align::Start; 2]));
        assert_eq!(
            table_alignments("| 一 | 二 |"),
            None,
            "a row of text names no columns"
        );
        assert_eq!(
            table_alignments("| :- : |"),
            None,
            "a cell that is not dashes names no column"
        );
    }
}
