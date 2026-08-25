use unicode_segmentation::UnicodeSegmentation;

use crate::text_blocks::{Emphasis, LineKind, LineMarker, LineStyle, Marks, Ornament};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentStats {
    pub logical_lines: usize,
    pub body_characters: usize,
    pub source_characters: usize,
    /// Characters in the longest logical line.
    ///
    /// A paragraph is a logical line, and how long a paragraph is decides what
    /// editing inside it costs (技術検証 6.9). This is the one figure a writer
    /// can act on, and unlike the engine's own limits it can be stated in
    /// characters at all: it describes the document rather than the layout, so
    /// it does not move when the window is resized.
    pub longest_line_characters: usize,
}

/// One logical line of the preview, with its mapping kept line-relative.
///
/// **Relative on purpose.** The offsets a lookup needs are absolute, but storing
/// them that way means every one of them moves when any earlier line changes
/// length, and the whole table has to be built again. Held relative to the line,
/// a line's table survives every edit that is not in that line, and the absolute
/// bases are a running total over lines — a few hundred additions rather than a
/// pass over every character (技術検証 7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewLine {
    /// The source line, including its trailing break, kept so the next update
    /// can tell what changed.
    source: String,
    /// The line as the preview shows it, including its trailing break.
    visible: String,
    /// Whether this line was shown with its Markdown, which is what the active
    /// line does. Kept so moving the caret to another line rebuilds the two
    /// lines that change form and no others.
    active: bool,
    /// How the line was set when it was built (要件 7.3.2). **Kept for the same
    /// reason `active` is**: a fence opening above it changes how a line is
    /// read without changing a character of it, so a line whose text did not
    /// move may still have to be built again.
    style: LineStyle,
    /// The marker standing at the head of this line, if one does. Empty for the
    /// active line, which is shown as its source.
    marker: Option<LineMarker>,
    /// UTF-16 position within `visible` to byte offset within `source`.
    source_byte: Vec<u32>,
    /// UTF-16 position within `visible` to byte offset within `visible`.
    preview_byte: Vec<u32>,
    /// Grapheme cluster boundaries within `visible`, in UTF-16 units.
    graphemes: Vec<u32>,
    /// What is marked inside this line, in UTF-16 units within `visible`
    /// (要件 7.3.2). Empty for the active line, which is shown as its source.
    marks: Vec<Emphasis>,
}

impl PreviewLine {
    /// Build one line's text and mapping.
    ///
    /// The body of this is the whole-document loop it replaced, narrowed to one
    /// line. A break belongs to the line before it, so a line is self-contained:
    /// no grapheme cluster and no mapping step ever crosses from one to the
    /// next.
    fn build(source_line: &str, active: bool, style: LineStyle, has_break: bool) -> Self {
        let mut visible = String::with_capacity(source_line.len() + 1);
        let mut marks = Vec::new();
        let mut marker = None;
        if active {
            // The line the caret is on is shown as it was written, markers and
            // all (要件 7.3.1), so there is nothing hidden to mark — and nothing
            // may stand over its marker either. A box hides the glyphs it
            // covers, and on this line those are the ones being edited.
            visible.push_str(source_line);
        } else {
            push_visible_line(source_line, style, &mut visible, &mut marks);
            marker = line_marker(source_line, style);
        }
        let mut source = String::with_capacity(source_line.len() + 1);
        source.push_str(source_line);
        if has_break {
            visible.push('\n');
            source.push('\n');
        }

        let units = visible.encode_utf16().count();
        let mut source_byte = vec![source.len() as u32; units + 1];
        let mut preview_byte = vec![visible.len() as u32; units + 1];
        let mut source_cursor = 0;
        let mut utf16_cursor = 0;

        for (preview_offset, character) in visible.char_indices() {
            // The preview only ever deletes from the source, so the next
            // preview character is almost always sitting at the cursor already.
            let remaining = &source[source_cursor..];
            let source_offset = if remaining.starts_with(character) {
                source_cursor
            } else {
                remaining
                    .find(character)
                    .map(|relative| source_cursor + relative)
                    .unwrap_or(source_cursor)
            };
            let source_end = source_offset + character.len_utf8();
            let preview_end = preview_offset + character.len_utf8();
            let utf16_units = character.len_utf16();

            for boundary in 0..utf16_units {
                source_byte[utf16_cursor + boundary] = source_offset as u32;
            }
            // A position inside a surrogate pair resolves to the byte after the
            // pair, which is what the caret and preedit insertion rely on.
            preview_byte[utf16_cursor] = preview_offset as u32;
            for boundary in 1..=utf16_units {
                preview_byte[utf16_cursor + boundary] = preview_end as u32;
            }
            utf16_cursor += utf16_units;
            source_byte[utf16_cursor] = source_end as u32;
            source_cursor = source_end;
        }

        let mut graphemes = Vec::with_capacity(units / 2 + 1);
        let mut boundary = 0;
        for grapheme in visible.graphemes(true) {
            graphemes.push(boundary);
            boundary += grapheme.encode_utf16().count() as u32;
        }

        Self {
            source,
            visible,
            active,
            style,
            marker,
            source_byte,
            preview_byte,
            graphemes,
            marks,
        }
    }

    fn utf16_len(&self) -> usize {
        self.source_byte.len() - 1
    }
}

/// The Markdown source as the preview shows it, and the mapping back.
///
/// Kept a line at a time so a keystroke rebuilds one line rather than the
/// document. Every lookup finds the line by binary search over the running
/// totals and then reads that line's own table, so it costs what it did before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewDocument {
    pub text: String,
    lines: Vec<PreviewLine>,
    /// What is marked inside each line (要件 7.3.2), the same order as `lines`.
    /// **Kept beside them rather than reached through them**: the engine slices
    /// a run of lines out per block, and a slice is what it can take.
    marks: Vec<Vec<Emphasis>>,
    /// The marker standing at the head of each line (要件 7.3.2), the same
    /// order as `lines`, and beside them for the same reason `marks` is.
    markers: Vec<Option<LineMarker>>,
    /// Running totals, one per line plus a final entry, so a lookup can binary
    /// search them. Rebuilt on every refresh, which is a pass over lines.
    utf16_starts: Vec<usize>,
    source_starts: Vec<usize>,
    preview_starts: Vec<usize>,
}

impl PreviewDocument {
    /// The preview with no active line, so every line shows its formatted form.
    ///
    /// The editor always knows which line the caret is on and goes through
    /// `refresh`; this shorthand exists for tests.
    #[cfg(test)]
    pub fn from_source(source: &str) -> Self {
        Self::from_source_with_active_line(source, None)
    }

    #[cfg(test)]
    pub fn from_source_with_active_line(source: &str, active_line_start: Option<usize>) -> Self {
        let mut preview = Self::default();
        preview.refresh(source, active_line_start);
        preview
    }

    /// Bring the preview up to date, rebuilding only the lines that changed.
    ///
    /// A line is rebuilt when its text changed, and when it became or stopped
    /// being the active line — moving the caret to another line changes the form
    /// of exactly two lines, and leaves every other line's table alone.
    pub fn refresh(&mut self, source: &str, active_line_start: Option<usize>) {
        let lines = source.split('\n').collect::<Vec<&str>>();
        let mut active_index = None;
        let mut line_start = 0;
        for (index, line) in lines.iter().enumerate() {
            if active_line_start == Some(line_start) {
                active_index = Some(index);
            }
            line_start += line.len() + 1;
        }

        // 要件 7.3.2: how every line is set, which is where a fence reaches
        // past its own line (`line_styles`).
        let styles = line_styles(source);
        let style_at = |index: usize| styles.get(index).copied().unwrap_or_default();
        let last = lines.len() - 1;
        let matches = |kept: &PreviewLine, index: usize, line: &str| {
            let has_break = index != last;
            let active = active_index == Some(index);
            kept.active == active
                && kept.style == style_at(index)
                && kept.source.len() == line.len() + usize::from(has_break)
                && kept.source.starts_with(line)
        };

        let shared_head = self
            .lines
            .iter()
            .enumerate()
            .zip(&lines)
            .take_while(|((index, kept), line)| matches(kept, *index, line))
            .count();
        // Compared from the far end as well, so inserting or removing a line
        // leaves the lines after it recognised rather than shifted out of place.
        let rest = lines.len().min(self.lines.len()) - shared_head;
        let shared_tail = (0..rest)
            .take_while(|back| {
                let index = lines.len() - 1 - back;
                let kept = &self.lines[self.lines.len() - 1 - back];
                matches(kept, index, lines[index])
            })
            .count();

        let changed = shared_head..lines.len() - shared_tail;
        let rebuilt = changed
            .clone()
            .map(|index| {
                let active = active_index == Some(index);
                PreviewLine::build(lines[index], active, style_at(index), index != last)
            })
            .collect::<Vec<PreviewLine>>();
        let removed = shared_head..self.lines.len() - shared_tail;
        self.lines.splice(removed, rebuilt);

        self.text.clear();
        self.marks.clear();
        self.markers.clear();
        self.utf16_starts.clear();
        self.source_starts.clear();
        self.preview_starts.clear();
        let mut utf16 = 0;
        let mut source_byte = 0;
        let mut preview_byte = 0;
        for line in &self.lines {
            self.utf16_starts.push(utf16);
            self.source_starts.push(source_byte);
            self.preview_starts.push(preview_byte);
            self.text.push_str(&line.visible);
            self.marks.push(line.marks.clone());
            self.markers.push(line.marker);
            utf16 += line.utf16_len();
            source_byte += line.source.len();
            preview_byte += line.visible.len();
        }
        self.utf16_starts.push(utf16);
        self.source_starts.push(source_byte);
        self.preview_starts.push(preview_byte);
    }

    /// What is marked inside each line (要件 7.3.2).
    pub fn marks(&self) -> &[Vec<Emphasis>] {
        &self.marks
    }

    /// The marker standing at the head of each line (要件 7.3.2).
    pub fn markers(&self) -> &[Option<LineMarker>] {
        &self.markers
    }

    pub fn utf16_len(&self) -> usize {
        self.utf16_starts.last().copied().unwrap_or(0)
    }

    /// The line holding a UTF-16 position, and the position within it.
    fn locate(&self, position: usize) -> (usize, usize) {
        let position = position.min(self.utf16_len());
        let index = self
            .utf16_starts
            .partition_point(|start| *start <= position)
            .saturating_sub(1)
            .min(self.lines.len().saturating_sub(1));
        (index, position - self.utf16_starts[index])
    }

    pub fn source_byte_at_utf16(&self, position: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let (index, within) = self.locate(position);
        self.source_starts[index] + self.lines[index].source_byte[within] as usize
    }

    pub fn utf16_at_source_byte(&self, source_byte: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let index = self
            .source_starts
            .partition_point(|start| *start < source_byte)
            .saturating_sub(1)
            .min(self.lines.len() - 1);
        let within = source_byte.saturating_sub(self.source_starts[index]);
        let line = &self.lines[index];
        let found = line
            .source_byte
            .partition_point(|mapped| (*mapped as usize) < within);
        (self.utf16_starts[index] + found).min(self.utf16_len())
    }

    pub fn preview_byte_at_utf16(&self, position: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let (index, within) = self.locate(position);
        self.preview_starts[index] + self.lines[index].preview_byte[within] as usize
    }

    pub fn previous_grapheme_position(&self, position: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let (index, within) = self.locate(position);
        let line = &self.lines[index];
        let found = line
            .graphemes
            .partition_point(|boundary| (*boundary as usize) < within);
        if found > 0 {
            return self.utf16_starts[index] + line.graphemes[found - 1] as usize;
        }
        // At the start of a line, the previous boundary is in the line before.
        match index.checked_sub(1) {
            Some(previous) => {
                let line = &self.lines[previous];
                let last = line.graphemes.last().copied().unwrap_or(0) as usize;
                self.utf16_starts[previous] + last
            }
            None => 0,
        }
    }

    pub fn next_grapheme_position(&self, position: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let (index, within) = self.locate(position);
        let line = &self.lines[index];
        let found = line
            .graphemes
            .partition_point(|boundary| (*boundary as usize) <= within);
        if found < line.graphemes.len() {
            return self.utf16_starts[index] + line.graphemes[found] as usize;
        }
        // Past the last boundary of this line, so the next one starts the next.
        self.utf16_starts
            .get(index + 1)
            .copied()
            .unwrap_or_else(|| self.utf16_len())
            .min(self.utf16_len())
    }
}

impl DocumentStats {
    /// Count the whole document.
    ///
    /// **The reference implementation.** The editor goes through
    /// [`DocumentCounts`], which keeps the same numbers a line at a time; this
    /// is the plain version it is checked against, and the only thing that
    /// still calls it is that check.
    #[cfg(test)]
    pub fn from_source(source: &str) -> Self {
        Self {
            logical_lines: logical_line_count(source),
            body_characters: visible_markdown_text(source).graphemes(true).count(),
            source_characters: source.graphemes(true).count(),
            longest_line_characters: longest_logical_line(source),
        }
    }
}

/// One logical line's contribution to everything counted across the document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LineCounts {
    /// The source line, kept so the next update can tell what changed.
    text: String,
    source_graphemes: usize,
    /// Graphemes of the line as the preview shows it.
    body_graphemes: usize,
    characters: usize,
    /// How the line was set when it was counted (要件 7.3.2). **Kept for the
    /// reason [`PreviewLine`] keeps it**: a fence opening above a line changes
    /// what the preview shows of it without changing a character of it — a
    /// literal line is counted as it was written, and a `> ` inside a fence is
    /// a character of code rather than a marker that comes off.
    style: LineStyle,
}

impl LineCounts {
    fn of(line: &str, style: LineStyle) -> Self {
        let mut visible = String::with_capacity(line.len());
        // The counts are about how much text there is, not how it is set.
        push_visible_line(line, style, &mut visible, &mut Vec::new());
        Self {
            source_graphemes: line.graphemes(true).count(),
            body_graphemes: visible.graphemes(true).count(),
            characters: line.chars().count(),
            style,
            text: line.to_owned(),
        }
    }
}

/// The document counted a line at a time, kept between keystrokes.
///
/// Everything the status bar shows, and the heading level every block is set
/// with, is a sum or a maximum over logical lines — and a keystroke changes one
/// line. Counting the whole document again cost 2.0ms of every keystroke,
/// nearly all of it walking grapheme clusters over text that had not changed
/// (技術検証 7.1).
///
/// Lines are compared from both ends, so inserting or removing one leaves the
/// lines on either side recognised rather than shifted out of place.
#[derive(Debug, Default)]
pub struct DocumentCounts {
    lines: Vec<LineCounts>,
    /// One entry per line, in step with `lines`, so the engine can be handed a
    /// slice without building one each time.
    line_styles: Vec<LineStyle>,
}

impl DocumentCounts {
    /// Bring the counts up to date with `source`, recounting only what changed.
    pub fn refresh(&mut self, source: &str) {
        let lines = source.split('\n').collect::<Vec<&str>>();
        // 要件 7.3.2: how every line is set, which is where a fence reaches
        // past its own line. A line whose text did not change may still be
        // counted differently because a fence opened above it, so the flag is
        // part of what makes a kept line still usable.
        let styles = line_styles(source);
        let style_at = |index: usize| styles.get(index).copied().unwrap_or_default();
        let matches = |kept: &LineCounts, index: usize, line: &str| {
            kept.text == *line && kept.style == style_at(index)
        };
        let shared_head = self
            .lines
            .iter()
            .enumerate()
            .zip(&lines)
            .take_while(|((index, kept), line)| matches(kept, *index, line))
            .count();
        let rest = lines.len().min(self.lines.len()) - shared_head;
        let shared_tail = (0..rest)
            .take_while(|back| {
                let index = lines.len() - 1 - back;
                let kept = &self.lines[self.lines.len() - 1 - back];
                matches(kept, index, lines[index])
            })
            .count();

        let changed = shared_head..lines.len() - shared_tail;
        let replacement = changed
            .map(|index| LineCounts::of(lines[index], style_at(index)))
            .collect::<Vec<LineCounts>>();
        let removed = shared_head..self.lines.len() - shared_tail;
        self.lines.splice(removed, replacement);

        self.line_styles = styles;
    }

    /// How each logical line is set, one entry per line of `split('\n')`.
    pub fn line_styles(&self) -> &[LineStyle] {
        &self.line_styles
    }

    /// The counts the status bar shows.
    ///
    /// A `\n` is its own grapheme cluster — no carriage return ever reaches the
    /// document (7.6) — so the breaks between lines are simply one each.
    pub fn stats(&self) -> DocumentStats {
        let breaks = self.lines.len().saturating_sub(1);
        DocumentStats {
            logical_lines: self.lines.len().max(1),
            body_characters: self
                .lines
                .iter()
                .map(|line| line.body_graphemes)
                .sum::<usize>()
                + breaks,
            source_characters: self
                .lines
                .iter()
                .map(|line| line.source_graphemes)
                .sum::<usize>()
                + breaks,
            longest_line_characters: self
                .lines
                .iter()
                .map(|line| line.characters)
                .max()
                .unwrap_or(0),
        }
    }
}

/// Characters in the longest logical line of `source`.
#[cfg(test)]
fn longest_logical_line(source: &str) -> usize {
    source
        .split('\n')
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
pub fn logical_line_count(source: &str) -> usize {
    if source.is_empty() {
        1
    } else {
        source.split('\n').count()
    }
}

#[cfg(test)]
pub fn visible_markdown_text(source: &str) -> String {
    visible_markdown_text_with_active_line(source, None)
}

/// The preview built in one pass over the whole document.
///
/// **A reference implementation.** The editor builds the preview a line at a
/// time through [`PreviewLine`]; this is the plain version that the refreshed
/// one is checked against, and the only thing that still calls it is that check.
#[cfg(test)]
fn visible_markdown_text_with_active_line(
    source: &str,
    active_line_start: Option<usize>,
) -> String {
    let styles = line_styles(source);
    let mut visible = String::with_capacity(source.len());
    let mut line_start = 0;

    for (index, line) in source.lines().enumerate() {
        if active_line_start == Some(line_start) {
            visible.push_str(line);
        } else {
            let style = styles.get(index).copied().unwrap_or_default();
            push_visible_line(line, style, &mut visible, &mut Vec::new());
        }
        visible.push('\n');
        line_start += line.len() + 1;
    }

    if !source.ends_with('\n') {
        visible.pop();
    }

    visible
}

/// One line as the preview shows it, with no trailing break.
///
/// The single place that decides what the preview removes. Both the preview
/// itself and the per-line counts behind the status bar go through it, so the
/// two cannot come to disagree about what "本文" means.
fn push_visible_line(
    line: &str,
    style: LineStyle,
    visible: &mut String,
    marks: &mut Vec<Emphasis>,
) {
    // **The style decides, here too.** An indented line is shown as written —
    // markers and all — unless the style says it is an item, and then it is a
    // nested one whose indent is the block's (要件 7.3.2). Reading the spaces
    // here instead would be a second opinion about what a line is, and the one
    // that loses: the pane sets what the style says.
    let indented = line.starts_with([' ', '\t']) && !style.kind.is_list();
    // 要件 7.3.2: the blockquote marker comes off whatever is under it — a rule
    // inside a quote is still a rule, and the quoting itself is the block's
    // indent (`BlockSpan::indent_steps`, 技術検証 7.1). **A box was tried at the
    // head of the line instead and taken out again**: it indented the first
    // line of a quoted paragraph and left every line it wrapped to flush with
    // the body, because a box reaches that head and no further.
    //
    // **The style decides, not the text.** A `> ` inside a fenced block is one
    // more character of code, and `quote_depth` is where that was settled.
    let content = if style.quote_depth > 0 {
        quote_content(line)
    } else {
        line
    };
    // A line inside a fence is literal for the same reason the inside of an
    // inline code span is (要件 7.3.2): **nothing in code is a marker**, so a
    // `**` there is two asterisks and stays two asterisks. A rule is literal
    // for the same reason — `***` is the line's own marks.
    if style.is_literal() || indented {
        // Indented text is literal, markers and all.
        visible.push_str(content);
        return;
    }

    let content = strip_heading_marker(content);
    // **A line long enough to be pathological is left literal.** Looking for
    // the closer of a marker that has none costs a scan to the end of the line,
    // so a line made mostly of unclosed markers costs the square of its length.
    // 要件 2.3 already calls a paragraph past this length exceptional; here the
    // cost of that is its markers showing, which loses nothing.
    if content.chars().count() > MARKED_LINE_LIMIT {
        visible.push_str(content);
        return;
    }
    let mut at = 0;
    push_marked(content, visible, marks, &mut at);
}

/// The longest line the preview works markers out for (要件 2.3, 7.3.2).
const MARKED_LINE_LIMIT: usize = 32_000;

/// The markers the preview hides, longest first (要件 7.3.2).
///
/// Longest first because `**` has to be tried before `*`: a two-character
/// marker read one character at a time is two empty ones.
fn markers() -> [(&'static str, Marks); 6] {
    let bold = Marks {
        bold: true,
        ..Marks::default()
    };
    let italic = Marks {
        italic: true,
        ..Marks::default()
    };
    let strike = Marks {
        strike: true,
        ..Marks::default()
    };
    let code = Marks {
        code: true,
        ..Marks::default()
    };
    [
        ("**", bold),
        ("__", bold),
        ("~~", strike),
        ("`", code),
        ("*", italic),
        ("_", italic),
    ]
}

/// Write out `content` with its markers hidden, recording what they enclosed.
///
/// **A marker that does not close is not a marker.** `2*3` keeps its asterisk,
/// because nothing later on the line closes it; that is the whole of why the
/// preview may hide a character at all. `at` is the UTF-16 position within the
/// line, which is what the marks are measured in.
fn push_marked(content: &str, visible: &mut String, marks: &mut Vec<Emphasis>, at: &mut u32) {
    let mut rest = content;
    let mut previous = None;
    while let Some(letter) = rest.chars().next() {
        if let Some((found, inner, after)) = opens_here(rest, previous) {
            let start = *at;
            if found.code {
                // **Nothing inside code is a marker.** `` `snake_case` `` is
                // the example that matters: an underscore in code is an
                // underscore.
                for character in inner.chars() {
                    visible.push(character);
                    *at += character.len_utf16() as u32;
                }
            } else {
                push_marked(inner, visible, marks, at);
            }
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: found,
            });
            previous = inner.chars().next_back();
            rest = after;
            continue;
        }
        visible.push(letter);
        *at += letter.len_utf16() as u32;
        previous = Some(letter);
        rest = &rest[letter.len_utf8()..];
    }
}

/// The marker `rest` begins with, what it encloses and what follows it.
///
/// `previous` is the character already written out, which decides the one rule
/// that is not about the marker itself: **`_` inside a word is not a marker**,
/// so `snake_case_name` keeps both of its underscores. `*` has no such rule,
/// which is what Markdown itself does.
fn opens_here<'a>(rest: &'a str, previous: Option<char>) -> Option<(Marks, &'a str, &'a str)> {
    for (marker, marks) in markers() {
        let Some(after_open) = rest.strip_prefix(marker) else {
            continue;
        };
        // A marker opens only when something that is not a space follows it:
        // `2 * 3` is arithmetic.
        if after_open.starts_with([' ', '\t']) {
            continue;
        }
        if marker.starts_with('_') && previous.is_some_and(char::is_alphanumeric) {
            continue;
        }
        let Some(close) = closing_at(after_open, marker) else {
            continue;
        };
        let inner = &after_open[..close];
        let after = &after_open[close + marker.len()..];
        return Some((marks, inner, after));
    }
    None
}

/// Where the marker that closes this one begins, if it is on the line at all.
///
/// A closing marker sits against what it closes — `*これ *` is not emphasis —
/// and a marker that closes immediately encloses nothing, so it is not one
/// either.
fn closing_at(rest: &str, marker: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = rest[from..].find(marker) {
        let at = from + found;
        let before = rest[..at].chars().next_back();
        let against = before.is_some_and(|letter| letter != ' ' && letter != '\t');
        if against {
            return Some(at);
        }
        from = at + marker.len();
    }
    None
}

fn strip_heading_marker(line: &str) -> &str {
    let marker_length = marker_length(line);
    if marker_length > 0 {
        line.get(marker_length..)
            .and_then(|rest| rest.strip_prefix(' '))
            .unwrap_or(line)
    } else {
        line
    }
}

/// The length of a heading marker at the start of `line`, or 0.
///
/// A marker only counts with a space after it, which is what
/// `strip_heading_marker` removes.
fn marker_length(line: &str) -> usize {
    let hashes = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    let followed_by_space = line[hashes..].starts_with(' ');
    if (1..=6).contains(&hashes) && followed_by_space {
        hashes
    } else {
        0
    }
}

/// The Markdown heading level of one source line, 0 for body text.
///
/// The preview removes the marker, so this is the only place the level survives
/// into what the engine lays out. It has to agree exactly with what
/// `visible_markdown_text_with_active_line` treats as a heading, or the vertical
/// pane sets a line at a size that does not match what it shows: an indented
/// line stays literal text, a blockquote marker comes off first, and a run of
/// hashes with no space after it is not a marker.
pub fn heading_level(line: &str) -> u8 {
    if line.starts_with([' ', '\t']) {
        return 0;
    }
    marker_length(quote_content(line)) as u8
}

/// What a line says once its blockquote marker is off, and whether it had one.
///
/// **One marker and no more**, which is what the preview boxes. Every reading
/// of a line goes through this, so none of them can come to a different
/// conclusion about where a line's content begins.
fn quoted(line: &str) -> Option<&str> {
    line.strip_prefix("> ").or_else(|| line.strip_prefix('>'))
}

/// The same, for the callers that do not care whether there was one.
fn quote_content(line: &str) -> &str {
    quoted(line).unwrap_or(line)
}

/// The fence a line opens or closes, if it is a fence at all (要件 7.3.2).
///
/// Three or more backticks or tildes at the very start of the line. A line that
/// begins with a space is literal already, so a fence there would be a fence
/// inside literal text; refusing it here is what agrees with the preview.
fn fence_marker(line: &str) -> Option<char> {
    let mut characters = line.chars();
    let first = characters.next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = 1 + characters.take_while(|letter| *letter == first).count();
    if run < 3 {
        return None;
    }
    Some(first)
}

/// `---`, `***` or `___` alone on a line (要件 7.3.2).
///
/// Three or more of one mark and nothing else. `***強調***` begins the same way
/// and is not a rule, which is the whole of why the rest of the line is looked
/// at rather than only its first three characters.
fn is_rule(content: &str) -> bool {
    let bare = content.trim_end();
    let Some(mark) = bare.chars().next() else {
        return false;
    };
    if !matches!(mark, '-' | '*' | '_') {
        return false;
    }
    bare.len() >= 3 && bare.chars().all(|letter| letter == mark)
}

/// The list marker a line begins with, if it has one (要件 7.3.2).
///
/// **A marker counts only with a space after it**, the rule a heading's hashes
/// already follow: `-1` is a negative number and `*text*` is emphasis.
fn list_kind(content: &str) -> Option<LineKind> {
    if let Some(rest) = content.strip_prefix(['-', '*', '+']) {
        let item = rest.strip_prefix(' ')?;
        return Some(task_kind(item).unwrap_or(LineKind::Bullet));
    }
    let digits = content.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &content[digits..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    if !rest.starts_with(' ') {
        return None;
    }
    Some(LineKind::Ordered)
}

/// The checkbox at the head of a task item, if the item begins with one
/// (要件 7.3.2).
fn task_kind(item: &str) -> Option<LineKind> {
    let inside = item.strip_prefix('[')?;
    let mark = inside.chars().next()?;
    let after = inside.get(mark.len_utf8()..)?.strip_prefix(']')?;
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    match mark {
        ' ' => Some(LineKind::Task { done: false }),
        'x' | 'X' => Some(LineKind::Task { done: true }),
        _ => None,
    }
}

/// The columns of white space at the head of `line`, and what follows them.
///
/// **A tab is four columns**, which is what it is worth to the eye in every
/// editor a writer is likely to have indented this text in. Nothing else here
/// depends on the number: it is only ever compared with another indent.
fn leading_indent(line: &str) -> (usize, &str) {
    let mut columns = 0;
    for (offset, character) in line.char_indices() {
        match character {
            ' ' => columns += 1,
            '\t' => columns += 4,
            _ => return (columns, &line[offset..]),
        }
    }
    (columns, "")
}

/// The deepest a list may be nested (要件 7.3.2).
///
/// Each level sets its item in by one more step, and a document that nested
/// forever would set its text past the far margin. Six is past anything a
/// person writes and reads.
const MAX_LIST_DEPTH: usize = 6;

/// The indents of the list levels open above the current line.
///
/// **The one thing besides a fence that a line cannot decide alone**
/// (技術検証 7.1). How deep an item is nested is decided by the item it sits
/// under, not by a number of spaces: writers indent by two, three or four and
/// mean the same thing by it, and a rule that fixed on one of those would set
/// the other two at the wrong depth. Comparing with the level above needs to
/// know what that level was.
#[derive(Default)]
pub struct ListLevels(Vec<usize>);

impl ListLevels {
    /// How deep an item indented `columns` sits, opening or closing levels to
    /// suit.
    fn depth_of(&mut self, columns: usize) -> u8 {
        while self.0.last().is_some_and(|open| columns < *open) {
            self.0.pop();
        }
        if self.0.last() != Some(&columns) && self.0.len() < MAX_LIST_DEPTH {
            self.0.push(columns);
        }
        (self.0.len().saturating_sub(1)) as u8
    }

    /// A line that is not an item and not indented ends every open level: the
    /// list is over. **A blank line does not** — an item may be followed by a
    /// blank line and go on (a loose list), and so may a paragraph inside one.
    fn ended_by(&mut self, columns: usize, rest: &str) {
        if columns == 0 && !rest.is_empty() {
            self.0.clear();
        }
    }
}

/// How one line outside every fence is set.
fn outside_fence(line: &str, levels: &mut ListLevels) -> LineStyle {
    let quote = quoted(line);
    let content = quote.unwrap_or(line);
    let (columns, body) = leading_indent(content);
    let kind = if is_rule(body) {
        LineKind::Rule
    } else {
        list_kind(body).unwrap_or_default()
    };
    // **The rule the preview reads a line by**: indented text is literal,
    // markers and all (`push_visible_line`). An indented item is the one thing
    // that is not — it is nested — and everything else indented stays as
    // written, so a line the pane sets is never a line the preview shows
    // verbatim.
    if columns > 0 && !kind.is_list() {
        levels.ended_by(columns, body);
        return LineStyle::default();
    }
    if kind.is_list() {
        let depth = levels.depth_of(columns);
        return LineStyle {
            heading_level: 0,
            kind,
            quote_depth: u8::from(quote.is_some()),
            list_depth: depth,
        };
    }
    levels.ended_by(columns, body);
    LineStyle {
        heading_level: heading_level(line),
        kind,
        quote_depth: u8::from(quote.is_some()),
        list_depth: 0,
    }
}

/// How much of `content` the marker at its head takes, in UTF-16 units.
///
/// Every part of every marker is one ASCII character — `list_kind` allows
/// nothing else — so units, bytes and characters are the same count here.
fn marker_len(content: &str, kind: LineKind) -> Option<u32> {
    let marker = match kind {
        LineKind::Bullet => 1,
        // `- [x]`, whose five characters `list_kind` has already checked.
        LineKind::Task { .. } => 5,
        LineKind::Ordered => content.chars().take_while(char::is_ascii_digit).count() + 1,
        _ => return None,
    };
    // **The space after the marker belongs to the marker.** It is the gap the
    // box stands in for, not the first character of the item.
    let rest = content.get(marker..).unwrap_or("");
    let space = usize::from(rest.starts_with(' '));
    u32::try_from(marker + space).ok()
}

/// The marker standing at the head of one line, as the preview shows it
/// (要件 7.3.2).
///
/// **Measured on the content rather than the source line.** The preview takes
/// the blockquote marker off, so a quoted item's marker begins the line the
/// pane shows, and a DirectWrite range is measured in that line's units. The
/// quoting itself is the block's indent and nothing to do with the head of the
/// line.
///
/// The kind comes from `style` instead of being worked out again, so this
/// cannot reach a different conclusion than the pane about what a line is.
fn line_marker(line: &str, style: LineStyle) -> Option<LineMarker> {
    let content = quote_content(line);
    let ornament = match style.kind {
        LineKind::Bullet => Ornament::Bullet,
        LineKind::Ordered => Ornament::Number,
        LineKind::Task { done: false } => Ornament::TaskOpen,
        LineKind::Task { done: true } => Ornament::TaskDone,
        // **A line that is all marks goes under one box.** A rule has a stroke
        // drawn across it and a fence has the block's ground reaching over it;
        // either way what stands in its place is a whole-line mark, and the
        // line keeps the room it takes (要件 7.3.2).
        LineKind::Rule | LineKind::Fence => Ornament::Hidden,
        _ => return None,
    };
    // Trailing spaces and all, which are part of what the line was written as
    // and nothing to look at.
    //
    // **A nested item's indent goes under the box with its marker** (要件
    // 7.3.2). What sets the item in is the block (`indent_steps`), so the
    // spaces the writer typed must take no room of their own — they would be
    // added to the indent rather than being it, and only on the item's first
    // line at that.
    let utf16_len = match ornament {
        Ornament::Hidden => content.encode_utf16().count() as u32,
        _ => {
            // Bytes as UTF-16 units, which they are: an indent is spaces and
            // tabs, and `marker_len` says the same about every marker.
            let (_, body) = leading_indent(content);
            let indent = (content.len() - body.len()) as u32;
            indent + marker_len(body, style.kind)?
        }
    };
    Some(LineMarker {
        utf16_len,
        ornament,
    })
}

/// How one line is set, given the fence the lines before it left open.
fn line_style(line: &str, fence: &mut Option<char>, levels: &mut ListLevels) -> LineStyle {
    match (*fence, fence_marker(line)) {
        (None, Some(opened)) => {
            *fence = Some(opened);
            LineStyle::of_kind(LineKind::Fence)
        }
        (Some(open), Some(close)) if open == close => {
            *fence = None;
            LineStyle::of_kind(LineKind::Fence)
        }
        // A run of tildes inside a backtick block closes nothing — it is one
        // more line of code.
        (Some(_), _) => LineStyle::of_kind(LineKind::Code),
        (None, None) => outside_fence(line, levels),
    }
}

/// How every logical line of `source` is set (要件 7.3.2).
///
/// **The one place a line's kind and its heading level are decided together**,
/// so the preview, the panes, the counts and the outline cannot come to hold
/// two opinions about a line. A hash inside a fenced block is not a heading,
/// and a fence is the only thing about a line that the lines before it decide —
/// everything else is read off the line itself, which is what lets a block
/// depend on nothing outside its own text.
///
/// One entry per `split('\n')` line, which is also one entry per line of the
/// preview: the preview emits exactly one line for each source line.
pub fn line_styles(source: &str) -> Vec<LineStyle> {
    let mut fence = None;
    let mut levels = ListLevels::default();
    source
        .split('\n')
        .map(|line| line_style(line, &mut fence, &mut levels))
        .collect()
}

/// One heading of a document's outline (要件 7.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// 1 through 6, the way Markdown counts them.
    pub level: u8,
    /// What the heading says, with its marker taken off.
    pub text: String,
    /// Where its line begins in the source, so a click can go there.
    pub at: usize,
}

/// Every heading in the source, in the order they appear (要件 7.7).
///
/// **The same rule the preview and the vertical pane use** — `heading_level` is
/// the one place a line is decided to be a heading, and the outline must not be
/// a second opinion about it: a line the outline listed but the pane set as
/// body text would be a heading nobody could see.
///
/// Cheap enough to do whenever the panel is drawn: one pass, and a line that
/// does not begin with a hash is dismissed on its first character.
pub fn outline(source: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut at = 0usize;
    let mut fence = None;
    let mut levels = ListLevels::default();
    for line in source.split('\n') {
        // Through `line_style` rather than `heading_level`, so a hash inside a
        // fenced block is as much not-a-heading here as it is in the pane.
        let level = line_style(line, &mut fence, &mut levels).heading_level;
        if level > 0 {
            headings.push(Heading {
                level,
                text: heading_text(line),
                at,
            });
        }
        at += line.len() + 1;
    }
    headings
}

/// What a heading line says once its marker is off.
///
/// A blockquote marker comes off first, the same way `heading_level` looks past
/// it; an empty heading keeps its hashes, because a row saying nothing is not a
/// row anybody can aim at.
fn heading_text(line: &str) -> String {
    let text = strip_heading_marker(quote_content(line)).trim();
    if text.is_empty() {
        line.trim().to_string()
    } else {
        text.to_string()
    }
}

/// The heading level of every logical line of `source`.
///
/// One entry per `split('\n')` line, which is also one entry per line of the
/// preview: the preview emits exactly one line for each source line, so both
/// panes read the same vector. It is per logical line, and a line's level
/// depends on that line alone, so a block's entries never depend on anything
/// before the block.
#[cfg(test)]
pub fn heading_levels(source: &str) -> Vec<u8> {
    line_styles(source)
        .iter()
        .map(|style| style.heading_level)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outline lists exactly the lines the panes set as headings, says
    /// what they say, and points at where they begin (要件 7.7).
    #[test]
    fn the_outline_agrees_with_what_a_pane_sets_as_a_heading() {
        let source = "# 第一部\n本文\n### 三の見出し\n\
                      #見出しでない\n  # 字下げ\n> ## 引用の中";

        let found = outline(source);

        let shape: Vec<(u8, &str)> = found
            .iter()
            .map(|heading| (heading.level, heading.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![(1, "第一部"), (3, "三の見出し"), (2, "引用の中")],
        );
        // The byte each one names is where its own line begins, and that line
        // is one `heading_level` agrees about.
        for heading in &found {
            let line = source[heading.at..].split('\n').next().unwrap_or("");
            assert_eq!(heading_level(line), heading.level);
        }
        let levels = heading_levels(source);
        assert_eq!(levels.iter().filter(|level| **level > 0).count(), 3);
        assert!(outline("見出しのない文書").is_empty());
    }

    #[test]
    fn counts_logical_lines() {
        assert_eq!(logical_line_count(""), 1);
        assert_eq!(logical_line_count("a"), 1);
        assert_eq!(logical_line_count("a\nb"), 2);
        assert_eq!(logical_line_count("a\n"), 2);
    }

    /// One line as the preview shows it, and what it marks.
    fn preview_of(line: &str) -> (String, Vec<Emphasis>) {
        let mut visible = String::new();
        let mut marks = Vec::new();
        push_visible_line(line, line_styles(line)[0], &mut visible, &mut marks);
        (visible, marks)
    }

    fn label(marks: Marks) -> &'static str {
        match marks {
            Marks { bold: true, .. } => "bold",
            Marks { italic: true, .. } => "italic",
            Marks { strike: true, .. } => "strike",
            Marks { code: true, .. } => "code",
            _ => "none",
        }
    }

    fn shape(marks: &[Emphasis]) -> Vec<(u32, u32, &'static str)> {
        marks
            .iter()
            .map(|mark| (mark.utf16_start, mark.utf16_len, label(mark.marks)))
            .collect()
    }

    /// **A marker that nothing closes is not a marker** (要件 7.3.2). This is
    /// what keeps `snake_case` and `2*3` intact, which the preview used to
    /// break by dropping every asterisk, underscore and backtick it saw.
    #[test]
    fn only_a_marker_that_closes_is_hidden() {
        assert_eq!(preview_of("2*3").0, "2*3");
        assert_eq!(preview_of("snake_case").0, "snake_case");
        assert_eq!(preview_of("snake_case_name").0, "snake_case_name");
        assert_eq!(preview_of("*未閉じ").0, "*未閉じ");
        // A marker has to sit against what it marks on both sides.
        assert_eq!(preview_of("* 箇条書き").0, "* 箇条書き");
        assert_eq!(preview_of("2 * 3 * 4").0, "2 * 3 * 4");
        assert!(preview_of("2*3").1.is_empty());
    }

    /// What each marker encloses is hidden and marked (要件 7.3.2).
    #[test]
    fn each_marker_marks_what_it_encloses() {
        let (text, marks) = preview_of("**太字**とふつう");
        assert_eq!(text, "太字とふつう");
        assert_eq!(shape(&marks), vec![(0, 2, "bold")]);

        let (text, marks) = preview_of("これは*斜体*です");
        assert_eq!(text, "これは斜体です");
        assert_eq!(shape(&marks), vec![(3, 2, "italic")]);

        let (text, marks) = preview_of("~~消した~~あと");
        assert_eq!(text, "消したあと");
        assert_eq!(shape(&marks), vec![(0, 3, "strike")]);
    }

    /// **Nothing inside code is a marker.** The underscore in a name written as
    /// code is an underscore (要件 7.3.2).
    #[test]
    fn code_holds_its_own_characters() {
        let (text, marks) = preview_of("`snake_case`を使う");

        assert_eq!(text, "snake_caseを使う");
        assert_eq!(shape(&marks), vec![(0, 10, "code")]);
    }

    /// Markers nest, and each stretch says what its own marker means.
    ///
    /// **Markers that end at the same place are not resolved** — `**太字と*斜体***`
    /// leaves an asterisk showing rather than working out which of the three
    /// closes what. Nothing is lost when that happens, which is what 要件 3 asks
    /// for; getting it right needs the delimiter stack CommonMark describes.
    #[test]
    fn markers_nest() {
        let (text, marks) = preview_of("**太字の*なか*だけ**");

        assert_eq!(text, "太字のなかだけ");
        // The inner one closes first, so it is recorded first.
        assert_eq!(shape(&marks), vec![(3, 2, "italic"), (0, 7, "bold")]);
    }

    #[test]
    fn strips_basic_markdown_for_body_count() {
        let visible = visible_markdown_text("# 見出し\n\n**本文**と`code`");
        assert_eq!(visible, "見出し\n\n本文とcode");
    }

    #[test]
    fn treats_markdown_markers_after_indentation_as_literal_text() {
        let visible = visible_markdown_text("    **本文**\n    # 見出し\n  > 引用");

        assert_eq!(visible, "    **本文**\n    # 見出し\n  > 引用");
    }

    #[test]
    fn exposes_markdown_only_on_the_active_line() {
        let source = "# 見出し\n**本文**と`code`\n> 引用";
        let second_line = "# 見出し\n".len();
        let preview = PreviewDocument::from_source_with_active_line(source, Some(second_line));

        assert_eq!(preview.text, "見出し\n**本文**と`code`\n引用");
        assert_eq!(
            preview.source_byte_at_utf16("見出し\n**".encode_utf16().count()),
            second_line + 2
        );
        assert_eq!(
            preview.utf16_at_source_byte(second_line + 2),
            "見出し\n**".encode_utf16().count()
        );
    }

    /// The level and the stripping have to agree line for line. Where they
    /// disagree, the vertical pane sets a line at a size that does not match the
    /// text it is showing.
    #[test]
    fn a_line_is_a_heading_exactly_when_its_marker_is_stripped() {
        let source = "# 見出し\n###### 深い見出し\n####### 深すぎる\n#マーカーのみ\n\
                      > # 引用の見出し\n    # 字下げ\n本文\n";

        let levels = heading_levels(source);
        let stripped = visible_markdown_text(source);

        assert_eq!(levels, vec![1, 6, 0, 0, 1, 0, 0, 0]);
        assert_eq!(
            stripped,
            "見出し\n深い見出し\n####### 深すぎる\n#マーカーのみ\n\
             引用の見出し\n    # 字下げ\n本文\n"
        );
        // One entry per line of the preview as well as of the source, which is
        // what lets both panes read the same vector.
        assert_eq!(levels.len(), stripped.split('\n').count());
    }

    /// What each logical line of a source is.
    fn kinds(source: &str) -> Vec<LineKind> {
        line_styles(source).iter().map(|style| style.kind).collect()
    }

    /// A fenced block is the one thing about a line that the lines before it
    /// decide (要件 7.3.2), so this is the test that the state is carried at
    /// all.
    #[test]
    fn a_fenced_block_runs_from_its_opening_fence_to_its_closing_one() {
        let source = "本文\n```rust\nlet x = 1;\n```\n本文\n";

        assert_eq!(
            kinds(source),
            vec![
                LineKind::Body,
                LineKind::Fence,
                LineKind::Code,
                LineKind::Fence,
                LineKind::Body,
                LineKind::Body,
            ]
        );
    }

    /// A tilde run inside a backtick block is one more line of code, and an
    /// opening fence that nobody closes swallows the rest of the document.
    #[test]
    fn a_fence_is_closed_only_by_its_own_mark() {
        let source = "```\n~~~\n# まだコード\n";

        assert_eq!(
            kinds(source),
            vec![
                LineKind::Fence,
                LineKind::Code,
                LineKind::Code,
                LineKind::Code,
            ]
        );
        assert_eq!(heading_levels(source), vec![0, 0, 0, 0]);
    }

    /// **Nothing inside a fence is a marker**, the same rule the inside of an
    /// inline code span already follows. A code block that showed its asterisks
    /// as bold would be showing something the writer did not write.
    #[test]
    fn nothing_inside_a_fence_is_a_marker() {
        let source = "```\n**そのまま**\n# 見出しではない\n```\n";

        assert_eq!(visible_markdown_text(source), source);
    }

    /// The outline and the panes have to hold one opinion about what a heading
    /// is: a row listed here that the pane sets as code is a row nobody can
    /// aim at.
    #[test]
    fn the_outline_skips_a_heading_inside_a_fence() {
        let source = "# 本物\n```\n# 偽物\n```\n# もう一つ\n";

        let listed = outline(source)
            .into_iter()
            .map(|heading| heading.text)
            .collect::<Vec<String>>();
        assert_eq!(listed, vec!["本物", "もう一つ"]);
    }

    /// A list marker counts only with a space after it, the rule the heading
    /// hashes already follow.
    #[test]
    fn a_list_marker_counts_only_with_a_space_after_it() {
        let source = "- 箇条書き\n* 箇条書き\n+ 箇条書き\n1. 番号\n2) 番号\n-1\n*強調*\n";

        assert_eq!(
            kinds(source),
            vec![
                LineKind::Bullet,
                LineKind::Bullet,
                LineKind::Bullet,
                LineKind::Ordered,
                LineKind::Ordered,
                LineKind::Body,
                LineKind::Body,
                LineKind::Body,
            ]
        );
    }

    #[test]
    fn a_task_is_a_bullet_whose_item_begins_with_a_box() {
        let source = "- [ ] まだ\n- [x] 済み\n- [X] 済み\n- [~] ただの箇条書き\n";

        assert_eq!(
            kinds(source),
            vec![
                LineKind::Task { done: false },
                LineKind::Task { done: true },
                LineKind::Task { done: true },
                LineKind::Bullet,
                LineKind::Body,
            ]
        );
    }

    /// `***強調***` begins the way a rule does, which is why the whole line is
    /// looked at rather than only its first three characters.
    #[test]
    fn a_rule_is_three_marks_and_nothing_else() {
        let source = "---\n***\n___\n--\n***強調***\n";

        assert_eq!(
            kinds(source),
            vec![
                LineKind::Rule,
                LineKind::Rule,
                LineKind::Rule,
                LineKind::Body,
                LineKind::Body,
                LineKind::Body,
            ]
        );
    }

    /// The blockquote marker comes off first, the same way it does for a
    /// heading, so what is under it still says what it is.
    #[test]
    fn a_quoted_line_still_says_what_it_is_under_the_marker() {
        let styles = line_styles("> - 引用の箇条書き\n> # 引用の見出し\n本文");

        assert_eq!(styles[0].kind, LineKind::Bullet);
        assert_eq!(styles[0].quote_depth, 1);
        assert_eq!(styles[1].heading_level, 1);
        assert_eq!(styles[1].quote_depth, 1);
        assert_eq!(styles[2].quote_depth, 0);
    }

    /// The box stands over the marker **and the space after it**: that space is
    /// the gap the box is standing in for, not the first character of the item.
    #[test]
    fn a_marker_covers_the_marker_and_the_space_after_it() {
        let cases = [
            ("- 箇条書き", 2, Ornament::Bullet),
            ("* 箇条書き", 2, Ornament::Bullet),
            ("1. 番号", 3, Ornament::Number),
            ("10. 番号", 4, Ornament::Number),
            ("- [ ] まだ", 6, Ornament::TaskOpen),
            ("- [x] 済み", 6, Ornament::TaskDone),
        ];

        for (line, utf16_len, ornament) in cases {
            let style = line_styles(line)[0];
            let expected = LineMarker {
                utf16_len,
                ornament,
            };
            assert_eq!(line_marker(line, style), Some(expected), "{line}");
        }
    }

    /// The preview takes the blockquote marker off, so a quoted item's marker
    /// begins the line the pane shows — and that is where the box has to be
    /// measured, not in the source line. **The quoting itself is the block's
    /// indent** and adds nothing at the head of the line.
    #[test]
    fn a_quoted_item_measures_its_marker_from_the_content() {
        let line = "> - 引用の箇条書き";
        let style = line_styles(line)[0];

        let marker = line_marker(line, style).expect("a bullet");
        assert_eq!(marker.utf16_len, 2);
        assert_eq!(marker.ornament, Ornament::Bullet);
    }

    /// **A line that is all marks goes under one box** — trailing spaces and
    /// all, which are part of what was written and nothing to look at. What
    /// stands in its place is a whole-line mark: a stroke across a rule, the
    /// block's ground over a fence.
    #[test]
    fn a_line_that_is_all_marks_goes_under_one_box() {
        for (line, utf16_len) in [("---", 3), ("***", 3), ("___   ", 6), ("> ---", 3)] {
            let style = line_styles(line)[0];
            let expected = LineMarker {
                utf16_len,
                ornament: Ornament::Hidden,
            };
            assert_eq!(line_marker(line, style), Some(expected), "{line}");
        }
    }

    /// The fence is one of them, and **only the fence**: the lines between two
    /// of them are the code itself and stay where they are.
    #[test]
    fn a_fence_goes_under_a_box_but_the_code_does_not() {
        let source = "```rust\nlet x = 1;\n```\n本文";
        let preview = PreviewDocument::from_source(source);
        let ornaments = preview
            .markers()
            .iter()
            .map(|marker| marker.map(|found| found.ornament))
            .collect::<Vec<Option<Ornament>>>();

        assert_eq!(
            ornaments,
            vec![Some(Ornament::Hidden), None, Some(Ornament::Hidden), None]
        );
        assert_eq!(preview.markers()[0].expect("a fence").utf16_len, 7);
    }

    /// **A rule is literal, for the reason code is.** `***` is the line's own
    /// marks and not emphasis around nothing; read as emphasis it leaves one
    /// asterisk where three were written, and then the box measured on the
    /// source is longer than the line the pane shows.
    #[test]
    fn a_rule_reaches_the_preview_exactly_as_it_was_written() {
        let preview = PreviewDocument::from_source("***\n___\n本文");

        assert_eq!(preview.text, "***\n___\n本文");
    }

    /// A `> ` inside a fenced block is one more character of code, so it is
    /// shown and the line is not quoted. The kind is what decides, not what
    /// the text looks like.
    #[test]
    fn a_quote_inside_a_fence_is_left_alone() {
        let source = "```\n> コード\n```";
        let preview = PreviewDocument::from_source(source);

        assert_eq!(preview.text, source);
        assert_eq!(preview.markers()[1], None);
        assert_eq!(line_styles(source)[1].quote_depth, 0);
    }

    #[test]
    fn a_line_with_nothing_at_its_head_has_no_marker() {
        for line in ["本文", "# 見出し", "> 引用"] {
            let style = line_styles(line)[0];
            assert_eq!(line_marker(line, style), None, "{line}");
        }
    }

    /// **Nothing stands over the marker of the line the caret is on.** A box
    /// hides the glyphs it covers, and on that line those are the ones being
    /// edited (要件 7.3.1) — the same rule that keeps its markers showing.
    #[test]
    fn the_active_line_has_no_marker_standing_over_it() {
        let source = "- 箇条書き\n- もう一行";
        let preview = PreviewDocument::from_source_with_active_line(source, Some(0));

        assert_eq!(preview.markers()[0], None);
        assert!(preview.markers()[1].is_some());
    }

    /// Moving the caret away has to give the line its box back, which is the
    /// refresh path rather than a fresh build.
    #[test]
    fn moving_off_a_line_gives_its_marker_back() {
        let source = "- 箇条書き\n- もう一行";
        let mut preview = PreviewDocument::default();

        preview.refresh(source, Some(0));
        assert_eq!(preview.markers()[0], None);

        preview.refresh(source, Some("- 箇条書き\n".len()));
        assert!(preview.markers()[0].is_some());
        assert_eq!(preview.markers()[1], None);
    }

    /// A fence reaches past its own line, so a line nobody touched may have to
    /// be counted again. **This is why a kept line is compared by more than its
    /// text** — without the flag, the line below keeps a body count taken when
    /// its markers were still markers.
    #[test]
    fn opening_a_fence_recounts_the_lines_it_swallows() {
        let mut counts = DocumentCounts::default();
        let before = "本文\n**強調**";
        counts.refresh(before);
        assert_eq!(counts.stats(), DocumentStats::from_source(before));

        let after = "```\n本文\n**強調**";
        counts.refresh(after);
        assert_eq!(counts.stats(), DocumentStats::from_source(after));
    }

    /// The incremental counts must agree with counting the document again, at
    /// every step of a series of edits.
    ///
    /// This is the whole safety of the arrangement: the reference is the
    /// implementation it replaced, and the sums are reconstructed from pieces
    /// that never see a line break, so the breaks are added back by a rule that
    /// is easy to get wrong and impossible to notice.
    #[test]
    fn counting_line_by_line_agrees_with_counting_the_document() {
        let start = "# 見出し\n\n本文**強調**です\n> 引用\n    字下げ*も*\n\
                     ```\nlet x = **1**;\n```\n";
        let mut source = String::from(start);
        let mut counts = DocumentCounts::default();
        counts.refresh(&source);
        assert_eq!(counts.stats(), DocumentStats::from_source(&source));

        // An edit of each shape, each starting from where the last left off.
        let edits: [(usize, &str); 6] = [
            (0, "追記"),
            (source.len(), "末尾に追加"),
            (4, "\n"),
            (0, "## "),
            (source.len(), "\n"),
            (2, "👨‍👩‍👧‍👦"),
        ];
        for (at, insertion) in edits {
            let at = at.min(source.len());
            let at = (0..=at)
                .rev()
                .find(|by| source.is_char_boundary(*by))
                .unwrap();
            source.insert_str(at, insertion);
            counts.refresh(&source);

            assert_eq!(
                counts.stats(),
                DocumentStats::from_source(&source),
                "after inserting {insertion:?} at {at}"
            );
            let levels = counts
                .line_styles()
                .iter()
                .map(|style| style.heading_level)
                .collect::<Vec<u8>>();
            assert_eq!(levels, heading_levels(&source));
        }

        // And back to nothing.
        source.clear();
        counts.refresh(&source);
        assert_eq!(counts.stats(), DocumentStats::from_source(&source));
    }

    #[test]
    fn measures_the_longest_logical_line() {
        let stats = DocumentStats::from_source("短い\n長いほうの行です\n中くらい\n");

        assert_eq!(
            stats.longest_line_characters,
            "長いほうの行です".chars().count()
        );
        assert_eq!(DocumentStats::from_source("").longest_line_characters, 0);
    }

    #[test]
    fn counts_a_grapheme_cluster_as_one_character() {
        let stats = DocumentStats::from_source("家族👨‍👩‍👧‍👦");

        assert_eq!(stats.source_characters, 3);
        assert_eq!(stats.body_characters, 3);
    }

    #[test]
    fn maps_preview_positions_past_markdown_markers() {
        let preview = PreviewDocument::from_source("# 見出し\n**本文**");

        assert_eq!(preview.text, "見出し\n本文");
        assert_eq!(preview.source_byte_at_utf16(0), "# ".len());
        assert_eq!(
            preview.source_byte_at_utf16("見出し\n".encode_utf16().count()),
            "# 見出し\n**".len()
        );
    }

    #[test]
    fn moves_the_preview_caret_by_grapheme_cluster() {
        let preview = PreviewDocument::from_source("A👨‍👩‍👧‍👦B");
        let family_end = "A👨‍👩‍👧‍👦".encode_utf16().count();

        assert_eq!(preview.next_grapheme_position(1), family_end);
        assert_eq!(preview.previous_grapheme_position(family_end), 1);
    }

    #[test]
    fn maps_utf16_positions_back_into_preview_bytes() {
        let preview = PreviewDocument::from_source("A𠮷B");

        assert_eq!(preview.preview_byte_at_utf16(0), 0);
        assert_eq!(preview.preview_byte_at_utf16(1), "A".len());
        assert_eq!(preview.preview_byte_at_utf16(3), "A𠮷".len());
        assert_eq!(preview.preview_byte_at_utf16(4), "A𠮷B".len());
    }

    /// The binary searches must agree with the linear scans they replaced, at
    /// every position of a document that mixes markdown, surrogate pairs and
    /// combining sequences.
    #[test]
    fn binary_search_lookups_match_a_linear_scan() {
        let source = "# 見出し\n\n**強調**と`code`と𠮷野家と👨‍👩‍👧‍👦\n> 引用行\n最終行";
        let preview = PreviewDocument::from_source(source);
        assert_lookups_match_a_linear_scan(&preview, source);
    }

    /// The same check after a series of edits, and with the active line moving.
    ///
    /// The mapping is kept a line at a time now, so what has to be shown is not
    /// only that a freshly built preview is right but that a **refreshed** one
    /// is: a line that was left alone has to still map to the right place after
    /// everything around it moved.
    #[test]
    fn refreshed_lookups_match_a_linear_scan() {
        let start = "# 見出し\n\n**強調**と`code`と𠮷野家と👨‍👩‍👧‍👦\n> 引用行\n\
                     ```\n**そのまま**\n```\n最終行";
        let mut source = String::from(start);
        let mut preview = PreviewDocument::default();
        let mut active = None;

        for step in 0..8 {
            preview.refresh(&source, active);
            assert_lookups_match_a_linear_scan(&preview, &source);
            assert_eq!(
                preview.text,
                visible_markdown_text_with_active_line(&source, active),
                "the refreshed text drifted at step {step}"
            );

            // A different shape of edit each time round, and the active line
            // moves with it so the two lines that change form are exercised.
            let at = match step % 4 {
                0 => 0,
                1 => source.len(),
                2 => source.len() / 2,
                _ => "# 見出し\n".len(),
            };
            let at = (0..=at.min(source.len()))
                .rev()
                .find(|by| source.is_char_boundary(*by))
                .unwrap();
            source.insert_str(
                at,
                if step % 2 == 0 {
                    "編集**の**行"
                } else {
                    "\n"
                },
            );
            active = source
                .char_indices()
                .map(|(byte, _)| byte)
                .find(|byte| *byte > at)
                .filter(|_| step % 3 != 0);
        }
    }

    /// Every lookup, at every position, against a scan that knows nothing about
    /// how the preview stores its mapping.
    ///
    /// Deliberately independent of the representation: the tables moved from
    /// the whole document to one per line, and this test did not have to change
    /// its mind about what the right answer is.
    fn assert_lookups_match_a_linear_scan(preview: &PreviewDocument, source: &str) {
        let boundaries = {
            let mut found = vec![0_usize];
            let mut boundary = 0;
            for grapheme in preview.text.graphemes(true) {
                boundary += grapheme.encode_utf16().count();
                found.push(boundary);
            }
            found
        };
        let mapped = (0..=preview.utf16_len())
            .map(|position| preview.source_byte_at_utf16(position))
            .collect::<Vec<usize>>();

        for position in 0..=preview.utf16_len() {
            let linear_preview_byte = {
                let target = position.min(preview.utf16_len());
                let mut utf16_cursor = 0;
                let mut found = preview.text.len();
                for (byte_offset, character) in preview.text.char_indices() {
                    if utf16_cursor >= target {
                        found = byte_offset;
                        break;
                    }
                    utf16_cursor += character.len_utf16();
                }
                found
            };
            assert_eq!(
                preview.preview_byte_at_utf16(position),
                linear_preview_byte,
                "preview byte mismatch at {position}"
            );

            let linear_previous = boundaries
                .iter()
                .copied()
                .rev()
                .find(|boundary| *boundary < position)
                .unwrap_or(0);
            assert_eq!(
                preview.previous_grapheme_position(position),
                linear_previous,
                "previous grapheme mismatch at {position}"
            );

            let linear_next = boundaries
                .iter()
                .copied()
                .find(|boundary| *boundary > position)
                .unwrap_or_else(|| preview.utf16_len());
            assert_eq!(
                preview.next_grapheme_position(position),
                linear_next,
                "next grapheme mismatch at {position}"
            );
        }

        for source_byte in 0..=source.len() {
            let linear = mapped
                .iter()
                .position(|offset| *offset >= source_byte)
                .unwrap_or_else(|| preview.utf16_len());
            assert_eq!(
                preview.utf16_at_source_byte(source_byte),
                linear.min(preview.utf16_len()),
                "utf16 lookup mismatch at source byte {source_byte}"
            );
        }
    }

    /// **How deep an item is nested is decided by the item it sits under**
    /// (要件 7.3.2). Writers indent by two, three or four columns and mean the
    /// same thing by it, so a rule that fixed on one of those would set the
    /// other two at the wrong depth — this document's own notes use two and its
    /// test data uses four.
    #[test]
    fn nesting_is_counted_from_the_level_above_not_from_a_number_of_spaces() {
        let by_two = line_styles("- 一\n  - 二\n    - 三\n");
        let by_four = line_styles("- 一\n    - 二\n        - 三\n");
        let depths = |styles: Vec<LineStyle>| {
            styles
                .iter()
                .take(3)
                .map(|style| style.list_depth)
                .collect::<Vec<u8>>()
        };

        assert_eq!(depths(by_two), vec![0, 1, 2]);
        assert_eq!(depths(by_four), vec![0, 1, 2]);
    }

    /// Coming back out closes the levels it passed, and a paragraph at the
    /// margin ends the list altogether. **A blank line does not** — an item may
    /// be followed by one and go on.
    #[test]
    fn coming_back_out_closes_the_levels_it_passed() {
        let styles = line_styles("- 一\n  - 二\n- 三\n\n  - 四\n本文\n  - 五\n");
        let depths = styles.iter().map(|s| s.list_depth).collect::<Vec<u8>>();

        // 一 二 三 (blank) 四 本文 五
        assert_eq!(depths[0], 0);
        assert_eq!(depths[1], 1);
        assert_eq!(depths[2], 0, "back at the margin");
        assert_eq!(depths[4], 1, "a blank line did not end the list");
        assert_eq!(depths[6], 0, "a paragraph at the margin did");
    }

    /// 要件 7.3.2: an indented line that is not an item is shown as written,
    /// markers and all. **Only an item is nested**, and the preview reads that
    /// from the style rather than from the spaces.
    #[test]
    fn an_indented_line_that_is_not_an_item_is_still_literal() {
        let styles = line_styles("- 一\n  続きの行です\n  - 二\n");

        assert_eq!(styles[1].kind, LineKind::Body);
        assert_eq!(styles[1].list_depth, 0);
        assert_eq!(styles[2].kind, LineKind::Bullet);
        assert_eq!(styles[2].list_depth, 1, "the level above is still open");
    }

    /// An item is set in one step for being an item and one more for each level
    /// it is under, and a quoted one is set in by both (要件 7.3.2).
    #[test]
    fn a_nested_item_asks_its_block_for_a_step_a_level() {
        let styles = line_styles("- 一\n  - 二\n> - 三\n");

        assert_eq!(styles[0].indent_steps(), 1);
        assert_eq!(styles[1].indent_steps(), 2);
        assert_eq!(styles[2].indent_steps(), 2, "quoted, and an item");
    }

    #[test]
    fn round_trips_every_position_between_source_and_preview() {
        let source = "# 見出し\n本文**強調**\n";
        let preview = PreviewDocument::from_source(source);

        for position in 0..=preview.utf16_len() {
            let source_byte = preview.source_byte_at_utf16(position);
            assert!(
                source.is_char_boundary(source_byte),
                "position {position} mapped into the middle of a source character"
            );
        }
    }
}
