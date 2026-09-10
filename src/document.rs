use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::text_blocks::{
    CommentSyntax, Emphasis, LineKind, LineMarker, LineStyle, Marks, Ornament, is_table_row,
    table_alignments,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentStats {
    pub logical_lines: usize,
    pub body_characters: usize,
    /// そのうち、ルビの読みが占めるぶん（要件 7.8・要件 10）。
    ///
    /// **足したものではなく、引けるものとして持つ。**要件 7.8 は「本文文字数が
    /// ルビを数えるかどうかは設定で選べる（初期値は数えない）」と言っている
    /// ——両方を持っておけば、**設定が変わっても数え直しが要らない**。
    /// 数え直しは1打鍵ぶんの仕事（`DocumentCounts`）なので、設定を切り替える
    /// たびに全文を歩くのは、この機能が求めていることに対して重すぎる。
    pub ruby_characters: usize,
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
        let marker;
        if active {
            // The line the caret is on is shown as it was written, markers and
            // all (要件 7.3.1).
            visible.push_str(source_line);
            // **行頭の記号は溝へぶら下げる**（書き手の報告 2026-09-10：「入力中に
            // 右に大きくズレて戻る」）。原文で出すだけだと、記号が幅を持つのに
            // 段下げは記号が隠れている前提のままなので、**その行だけ本文が記号の
            // 幅ぶん右にあり、離れると左へ戻る。**箱は字を隠すのではなく、
            // 幅を取らせないためにある——覆った字はそのまま溝に描かれる
            // （`Ornament::Markup`）ので、記号は見えたままである。
            marker = active_markup(source_line, style);
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

    /// The line shown as its own source, if the caret is on one (要件 7.3.1).
    ///
    /// **The preview's own record of it**, rather than the byte offset the
    /// caller passed in: that offset is into the document, and what everything
    /// downstream indexes by is the line.
    pub fn active_line(&self) -> Option<usize> {
        self.lines.iter().position(|line| line.active)
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
            ruby_characters: ruby_graphemes_in(source),
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
    /// そのうち、ルビの読みのぶん（要件 7.8）。
    ruby_graphemes: usize,
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
        // **印は要る**（要件 7.8）：ルビの読みは本文に居残るので、どこからどこ
        // までが読みかを言えるのは印だけである。
        let mut marks = Vec::new();
        push_visible_line(line, style, &mut visible, &mut marks);
        Self {
            source_graphemes: line.graphemes(true).count(),
            body_graphemes: visible.graphemes(true).count(),
            ruby_graphemes: ruby_graphemes(&visible, &marks),
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
            ruby_characters: self
                .lines
                .iter()
                .map(|line| line.ruby_graphemes)
                .sum::<usize>(),
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

/// 要件 10: which logical line the caret is on, and how far into that line it
/// is. **Both counted from 1**, which is how every editor states a place and
/// how the writer will read it back against another one.
///
/// **The line is the file's, not the screen's.** A paragraph that wraps over
/// six screen lines is one line here — the same line the count beside it is of
/// (要件 10). The column is counted in the characters the writer sees, so an
/// emoji written from four scalars moves it by one.
///
/// A byte that lands inside a character falls back to that character's head,
/// for the same reason the selection count does: naming a place must never be
/// the thing that brings the app down.
pub fn caret_place(source: &str, byte: usize) -> (usize, usize) {
    let byte = crate::floor_char_boundary(source, byte.min(source.len()));
    let head = source[..byte].rfind('\n').map_or(0, |newline| newline + 1);
    let line = source[..head].matches('\n').count() + 1;
    let column = source[head..byte].graphemes(true).count() + 1;
    (line, column)
}

/// 打たれた行番号を読む——`128`と`12:5`（E4、書き手の選択 2026-09-10）。
///
/// **ステータスバーが`Ln 12, Col 5`と出しているので、桁も受ける。**画面が言って
/// いる形をそのまま打てるほうがよく、桁を言わなければ行頭に着く。
///
/// **全角の数字と`：`も受ける。**IMEを立てたまま帯へ来た書き手が`１２`と打つのは
/// 打ち間違いではない——`半角で打ち直してください`と言うために、この機能はある
/// わけではない。
///
/// `None`は「番号として読めない」。0行・0桁も`None`である——[`caret_place`]が
/// 1から数えているのだから、0はこの編集器のどこにも無い位置である。
pub fn read_place(input: &str) -> Option<(usize, Option<usize>)> {
    let plain: String = input
        .trim()
        .chars()
        .map(|character| match character {
            // 全角の数字と、全角のコロン。
            '０'..='９' => char::from(b'0' + (character as u32 - '０' as u32) as u8),
            '：' => ':',
            other => other,
        })
        .collect();
    let (line, column) = match plain.split_once(':') {
        Some((line, column)) => (line, Some(column)),
        None => (plain.as_str(), None),
    };
    let line = read_number(line)?;
    let column = match column {
        Some(column) => Some(read_number(column)?),
        None => None,
    };
    Some((line, column))
}

/// 1以上の数として読めるか。**空白は挟まっていてよい**（`12 : 5`）。
fn read_number(input: &str) -> Option<usize> {
    let number: usize = input.trim().parse().ok()?;
    (number >= 1).then_some(number)
}

/// 行と桁が指すバイト位置——[`caret_place`]の逆（E4）。
///
/// **行が無ければ`None`。**書き手の選択（2026-09-10）：越えた番号で末尾へ
/// 連れて行くのではなく、動かずに言う。E4は「他の人や別の道具から『何行目』と
/// 示された箇所へ行くため」の機能なので、**無い行への着地は「手元の原稿が違う」
/// という知らせを消してしまう。**
///
/// **桁は行の終わりで止まる。**行より長い桁は打ち間違いか、別の道具の数え方で
/// あって、次の行へこぼれてよいものではない。桁を言わなければ行頭。
///
/// 返すのは必ず字の切れ目である（桁は書記素で数える、[`caret_place`]と同じ）
/// ——**サロゲートや結合文字の途中へは入らない。**
pub fn place_of(source: &str, line: usize, column: Option<usize>) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let mut head = 0;
    for _ in 1..line {
        head += source[head..].find('\n')? + 1;
    }
    let tail = source[head..]
        .find('\n')
        .map_or(source.len(), |newline| head + newline);
    let Some(column) = column else {
        return Some(head);
    };
    let mut at = head;
    for grapheme in source[head..tail].graphemes(true).take(column - 1) {
        at += grapheme.len();
    }
    Some(at)
}

/// ダブルクリックが選ぶ範囲——**その位置の語**（E3）。
///
/// **語の切れ目は`Alt+F`／`Alt+B`の規則とそろえる**（E3がそう言っている）。
/// [`is_word`]が「句読点でも空白でもない字」なので、日本語では句読点までのひと続きが
/// 語になる——`黒猫が鳴いた。`で`黒猫が鳴いた`。**規則を2つ持たない**ことのほうが、
/// 語の切り方の精度より大事である：`Alt+F`が止まらないところでダブルクリックが
/// 切れたら、書き手はどちらが本当かを覚えなければならない。
///
/// **行はまたがない。**改行は語の一部ではなく、行末で押した書き手が次の行まで
/// 選ぶのは、選びたかったものより多い。
///
/// **行末（字の無いところ）で押されたら、手前の語。**打ち終えた語の後ろを押すのは、
/// その語を指しているのと同じことである。
pub fn word_around(source: &str, byte: usize) -> (usize, usize) {
    let byte = crate::floor_char_boundary(source, byte.min(source.len()));
    let head = source[..byte].rfind('\n').map_or(0, |newline| newline + 1);
    let tail = source[byte..]
        .find('\n')
        .map_or(source.len(), |newline| byte + newline);
    let line = &source[head..tail];
    let at = byte - head;
    let here = line[at..].chars().next().map(letter_kind);
    let before = line[..at].chars().next_back().map(letter_kind);
    // **押された字そのものの頭を渡してもらう**（`PaneHit::letter`、書き手の報告
    // 2026-09-10：「英語では単語選択にならない感じ」）。カーソルの置き場所は字と
    // 字の境目なので、**英語のように字が細いと、押した点は語の後ろの境目へ寄る**
    // ——`cat`の右半分を押した書き手は空白ではなく`cat`を指しているのに、境目で
    // 訊けば空白と答えることになる。日本語は字が広いぶん、これが起きにくかった。
    let kind = match (here, before) {
        (Some(kind), _) => kind,
        // 行末（字の無いところ）で押されたら、手前の語。打ち終えた語の後ろを
        // 押すのは、その語を指しているのと同じことである。
        (None, Some(kind)) => kind,
        // 空の行には選ぶものが無い。
        (None, None) => return (byte, byte),
    };
    let mut start = at.min(line.len());
    let mut end = start;
    for character in line[..start].chars().rev() {
        if letter_kind(character) != kind {
            break;
        }
        start -= character.len_utf8();
    }
    for character in line[end..].chars() {
        if letter_kind(character) != kind {
            break;
        }
        end += character.len_utf8();
    }
    (head + start, head + end)
}

/// 語・空白・記号の3つ（[`word_around`]）。
///
/// **空白と記号を分けてある。**`。`を押して、その前後の空白まで選ばれるのは
/// 「押したもの」より多い。語の側の規則は[`is_word`]そのままである。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LetterKind {
    Word,
    Space,
    Mark,
}

fn letter_kind(character: char) -> LetterKind {
    if is_word(character) {
        LetterKind::Word
    } else if character.is_whitespace() {
        LetterKind::Space
    } else {
        LetterKind::Mark
    }
}

/// 論理行のバイト範囲——**改行を含む**（E3）。
///
/// 行番号を押して選ぶのも、行を動かす・複製する・消すのも、この範囲である。
/// **改行まで持つ**のは、行を動かすことが「字を動かす」ではなく「行を動かす」で
/// あるから：改行を置いていくと、動かした先で2行が1行になる。
///
/// 最後の行に改行が無ければ、そこで終わる。
pub fn line_span(source: &str, byte: usize) -> (usize, usize) {
    let byte = crate::floor_char_boundary(source, byte.min(source.len()));
    let head = source[..byte].rfind('\n').map_or(0, |newline| newline + 1);
    let end = source[head..]
        .find('\n')
        .map_or(source.len(), |newline| head + newline + 1);
    (head, end)
}

/// 行そのものへの編集——動かす・複製する・消す（E3の②）。
///
/// **上下ではなく、文書順の前と後**（書き手の選択 2026-09-10）。E3が「縦書きでは
/// 画面の上下ではなく文書順の前／後として説明する」と言っているとおりで、`Alt+↑`は
/// どちらの向きの面でも「前の行と入れ替える」である——縦書きで画面に合わせると
/// `Alt+←`が要件 11.2 の閲覧履歴とぶつかる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEdit {
    /// `Alt+↑`——前の行と入れ替える。
    MoveBefore,
    /// `Alt+↓`——後の行と入れ替える。
    MoveAfter,
    /// `Shift+Alt+↑`——前へ写す。
    CopyBefore,
    /// `Shift+Alt+↓`——後へ写す。
    CopyAfter,
    /// `Ctrl+Shift+K`——行ごと消す。
    Drop,
}

/// 選ばれているぶんが覆う論理行（E3の②）。
///
/// **カーソルだけでも1行。**行の編集は行に対する操作なので、選ばれていなければ
/// カーソルのある行がその1行である。
///
/// **終わりがちょうど行頭なら、その行は入らない。**選択の終わりは「そこまで」で
/// あって、次の行を指しているのではない——3行目の頭で止めた選択が3行目ごと
/// 動いたら、書き手は選んでいないものを動かされたことになる。
pub fn selected_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let (from, to) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let (first, first_end) = line_span(source, from);
    let (last_start, last_end) = line_span(source, to);
    let end = if to == last_start && to > first {
        last_start
    } else {
        last_end.max(first_end)
    };
    (first, end)
}

/// 行の編集の結果——**書き換える範囲と、そこへ入る字と、選び直す範囲**（E3の②）。
///
/// **`None`は「できない」。**先頭の行を前へ、末尾の行を後へは動かせない
/// ——黙って何もしないのではなく、呼ぶ側がそう言えるように`None`で返す。
///
/// **改行は行と行のあいだにある。**組み直しは本文だけを並べ、あいだに改行を1つずつ
/// 置く——最後の改行は、元の範囲が持っていたときだけ付ける。**末尾に改行の無い
/// 文書で最後の行を動かしても、改行が増えたり減ったりしない。**
pub fn line_edit(
    source: &str,
    span: (usize, usize),
    what: LineEdit,
) -> Option<(Range<usize>, String, (usize, usize))> {
    let (start, end) = span;
    let moved = &source[start..end];
    match what {
        LineEdit::MoveBefore => {
            if start == 0 {
                return None;
            }
            let (above_start, _) = line_span(source, start - 1);
            let above = &source[above_start..start];
            let region = above_start..end;
            let text = rejoin(&[moved, above], source[region.clone()].ends_with('\n'));
            let chosen = (above_start, above_start + body(moved).len() + 1);
            Some((region, text, chosen))
        }
        LineEdit::MoveAfter => {
            if end >= source.len() {
                return None;
            }
            let (_, below_end) = line_span(source, end);
            let below = &source[end..below_end];
            let region = start..below_end;
            let ends_with_newline = source[region.clone()].ends_with('\n');
            let text = rejoin(&[below, moved], ends_with_newline);
            let head = start + body(below).len() + 1;
            let chosen = (
                head,
                head + body(moved).len() + usize::from(ends_with_newline),
            );
            Some((region, text, chosen))
        }
        LineEdit::CopyBefore | LineEdit::CopyAfter => {
            let region = start..end;
            let ends_with_newline = moved.ends_with('\n');
            let text = rejoin(&[moved, moved], ends_with_newline);
            let second = start + body(moved).len() + 1;
            // **写したほうが選ばれる。**押し続けた書き手の手元では、写しが次の
            // 写しの元になる——`Shift+Alt+↓`を3回押せば3つ増える。
            let chosen = if what == LineEdit::CopyBefore {
                (start, second)
            } else {
                (
                    second,
                    second + body(moved).len() + usize::from(ends_with_newline),
                )
            };
            Some((region, text, chosen))
        }
        LineEdit::Drop => {
            // **末尾の行を消すときは、その手前の改行も。**行だけ消して改行を
            // 置いていくと、空の行が1つ増える。
            let region = if end == source.len() && start > 0 {
                start - 1..end
            } else {
                start..end
            };
            let at = region.start;
            Some((region, String::new(), (at, at)))
        }
    }
}

/// 行の本文——末尾の改行を落としたもの（[`line_edit`]）。
fn body(block: &str) -> &str {
    block.strip_suffix('\n').unwrap_or(block)
}

/// 行の並びを組み直す（[`line_edit`]）。
///
/// **改行はあいだに1つずつ、最後は言われたとおり。**
fn rejoin(blocks: &[&str], newline_at_end: bool) -> String {
    let mut out = String::new();
    for (at, block) in blocks.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        out.push_str(body(block));
    }
    if newline_at_end {
        out.push('\n');
    }
    out
}

/// Enterが継ぐもの（E3の③）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Continuation {
    /// 改行と、その後ろへ入る頭——字下げ・引用の`>`・箇条書きの印。
    /// **そのまま打ったのと同じ道を通る**ので、取り消しも別の面の追従も変わらない。
    Insert(String),
    /// 中身の無い項目でEnter——**行頭から`upto`までを`keep`に置き換える**。
    ///
    /// **改行は入らない。**書き手が終えたいのは箇条書きであって、行ではない。
    ///
    /// 残すものは鍵で変わる（書き手の選択 2026-09-10）：
    ///
    /// - **Enterは段ごと捨てる。**何も残らず、素の行頭へ戻る——箇条書きを
    ///   終えて本文へ戻る、いちばん多い用事がいちばん短い手数で済む。
    /// - **Shift+Enterは印だけ捨てる。**カーソルは項目の本文が始まっていた列に
    ///   残るので、その項目に属する段落としてそのまま書き続けられる。
    Clear { upto: usize, keep: String },
}

/// Enterを押したときに継ぐもの（E3の③）。
///
/// **行の見方は`line_styles`のもの**（`styles`で受け取る）。箇条書きかどうかを
/// ここで決め直すと、画面が箇条書きとして組んでいる行をEnterが本文として扱う、
/// という食い違いが起きる——**規則は1つ**である（②の語の切れ目と同じ考え方）。
///
/// **`Enter`は箇条書きを進め、`Shift+Enter`は段落を続ける**（書き手の決定
/// 2026-09-10）。`1. aaa`でShift+Enterを押した書き手が欲しいのは`2.`ではなく、
/// **`aaa`の下から続く段落**である——印は継がず、本文の列まで空白だけを継ぐ。
/// そこで`Enter`を押せば、その行が`2.`になって箇条書きへ戻る。
///
/// **頭の中で押されたEnterは、ただの改行**——行を押し下げたいだけの書き手に、
/// 印を写して返さない。
pub fn enter_continuation(
    source: &str,
    styles: &[LineStyle],
    caret: usize,
    soft: bool,
) -> Continuation {
    let (line_start, line_end) = line_span(source, caret);
    let line = source[line_start..line_end]
        .strip_suffix('\n')
        .unwrap_or(&source[line_start..line_end]);
    let index = source[..line_start].matches('\n').count();
    let style = styles.get(index).copied().unwrap_or_default();
    let at = caret.saturating_sub(line_start).min(line.len());
    let quote = line.len() - quote_content(line).len();
    let content = &line[quote..];
    let (_, body) = leading_indent(content);
    let indent = content.len() - body.len();
    let marker = marker_len(body, style.kind).unwrap_or(0) as usize;
    let head = quote + indent + marker;
    if at < head {
        return Continuation::Insert("\n".to_owned());
    }
    let kept = &line[quote..quote + indent];
    // **中身の無い項目は、そこで終わる**（E3：「空の項目でEnterを押すと継続を
    // 終える」）。印を持たない行はここへ来ない——字下げだけの行でEnterが何も
    // しないと、効かない鍵に見える。
    // **空の継続行でEnterを押したら、改行して次の項目が出る**（書き手の決定
    // 2026-09-10：「続けてEnterすると改行して2.になる感じ。つまり、一行空く感じ」）。
    // Shift+Enterで作った段落の行に何も書かなかったのだから、書き手はもう段落では
    // なく次の項目を書こうとしている——**空いた行はそのまま残る**ので、項目と項目の
    // あいだが一行空く。
    if line[head..].trim().is_empty()
        && marker == 0
        && quote == 0
        && style.list_indent > 0
        && !soft
        && let Some((item_head, next)) = item_above(source, styles, index, style.list_indent)
    {
        return Continuation::Insert(format!("\n{item_head}{next}"));
    }
    if line[head..].trim().is_empty() && (marker > 0 || quote > 0) {
        // **Enterは段ごと捨てて、素の行頭へ**（書き手の選択 2026-09-10）。
        // 箇条書きを終えて本文へ戻るのがいちばん多い用事で、字下げが残っていると
        // 次の行がその字下げを継いでいく。
        //
        // **Shift+Enterは印だけ捨てる。**印の幅は空白で埋めるので、書き始める場所は
        // 動かない——その項目の本文が始まっていた列である。**引用の`>`は埋めない**：
        // 引用に「本文の列」は無く、終えた書き手が戻るのは本文そのものである。
        // 印は`marker_len`が数えたASCIIなので、バイトの数がそのまま桁の数になる。
        let keep = if soft {
            let mut keep = String::with_capacity(kept.len() + marker);
            keep.push_str(kept);
            for _ in 0..marker {
                keep.push(' ');
            }
            keep
        } else {
            String::new()
        };
        return Continuation::Clear { upto: head, keep };
    }
    let mut next = String::from("\n");
    next.push_str(&line[..quote]);
    next.push_str(kept);
    if soft {
        // **Shift+Enterは印を継がない**（書き手の決定 2026-09-10）。継ぐのは
        // 本文が始まっている列までの空白で、そこから項目の続きの段落を書ける。
        for _ in 0..marker {
            next.push(' ');
        }
    } else {
        next.push_str(&continued_marker(body, style.kind, marker));
    }
    Continuation::Insert(next)
}

/// この継続行が属する項目の、行頭の字と次の印（[`enter_continuation`]）。
///
/// **同じ深さの、いちばん近い項目。**継続行は項目と同じ`list_indent`を持つので
/// （要件 7.3.2）、そこを遡って最初に見つかる項目がその行の親である。
///
/// **リストの外まで遡らない。**深さが浅くなった行はもうこの項目の連なりではない
/// ——そこで止めないと、遠く上のリストの番号を継いでしまう。
fn item_above(
    source: &str,
    styles: &[LineStyle],
    from: usize,
    depth: u8,
) -> Option<(String, String)> {
    let lines: Vec<&str> = source.split('\n').take(from).collect();
    for (index, line) in lines.iter().enumerate().rev() {
        let style = styles.get(index).copied().unwrap_or_default();
        if style.list_indent < depth {
            return None;
        }
        if !style.kind.is_list() || style.list_indent != depth {
            continue;
        }
        let quote = line.len() - quote_content(line).len();
        let content = &line[quote..];
        let (_, body) = leading_indent(content);
        let indent = content.len() - body.len();
        let marker = marker_len(body, style.kind).unwrap_or(0) as usize;
        return Some((
            line[..quote + indent].to_owned(),
            continued_marker(body, style.kind, marker),
        ));
    }
    None
}

/// 次の行が持つ印（[`enter_continuation`]）。
///
/// - 番号は**1つ進める**（書き手の選択 2026-09-10）。**下の行は書き換えない**
///   ——途中に行を挿したときに下を振り直すのは、打っていないところが動くことである。
/// - タスクは**空の箱で継ぐ**。済んだ印を写すのは、まだしていないことを済んだと
///   言うことになる。
/// - 区切り線や表・コードは印を持たない（`marker_len`が`None`を返す）。
fn continued_marker(body: &str, kind: LineKind, marker: usize) -> String {
    match kind {
        LineKind::Bullet => body[..marker.min(body.len())].to_owned(),
        LineKind::Task { .. } => {
            let bullet = body.chars().next().unwrap_or('-');
            format!("{bullet} [ ] ")
        }
        LineKind::Ordered => {
            let digits = body.chars().take_while(char::is_ascii_digit).count();
            let number: u64 = body[..digits].parse().unwrap_or(0);
            let delimiter = body[digits..].chars().next().unwrap_or('.');
            format!("{}{delimiter} ", number.saturating_add(1))
        }
        _ => String::new(),
    }
}

/// 字下げの一段——`Tab`が足し、`Shift+Tab`が外す幅（E3の④）。
///
/// **空白で書く。**タブ文字は幅が読み手の道具で変わるので、原稿の中では
/// 「何桁下げたか」が書き手の見たとおりにならない。入れ子の深さは桁数から
/// 決まる（`ListLevels`）ので、そこが揺れるのは困る。
///
/// **一段は1つだけ。**本文へ入れる`Tab`も、箇条書きを入れ子にする`Tab`も同じ幅で
/// ある——選んだものによって下がる量が変わったら、書き手は同じ鍵に2つの意味を
/// 覚えることになる（`main`の`TAB_INDENT`がこれを指している）。
pub const INDENT_STEP: &str = "    ";

/// 字下げを一段深く／浅くした結果（E3の④）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Indented {
    /// **書き換えたあとの文書ぜんぶ。**字下げと番号の振り直しが同じ一手で起きる
    /// ので、変わった範囲は呼ぶ側が`changed_span`で取る——2つの編集に分けると、
    /// 取り消しが2回に割れる。
    pub text: String,
    /// 書き換えたあとの選択。長さが無ければ、そこに立つカーソル。
    pub chosen: (usize, usize),
}

/// 選んだ行の字下げを一段深く／浅くする（E3の④）。
///
/// **箇条書きなら、それが入れ子の深さになる。**深さは書き手が下げた桁数から
/// 決まる（`ListLevels`）ので、字下げを足すことと入れ子にすることは同じ操作である
/// ——`Tab`で内側の項目になり、`Shift+Tab`で戻る（書き手の求め 2026-09-10）。
///
/// **字下げは引用の`>`の後ろに入る。**`> - 項目`の`>`は行がどこにあるかを言う印で、
/// その前に空白を入れると引用そのものが崩れる。
///
/// **浅くできる行が1つも無ければ`None`。**何も起きないことを、呼ぶ側が知れる。
pub fn shift_indent(source: &str, from: usize, to: usize, deeper: bool) -> Option<Indented> {
    let (start, end) = selected_lines(source, from, to);
    let mut text = String::with_capacity(source.len());
    text.push_str(&source[..start]);
    let mut moved = [from, to];
    let mut changed = false;
    let mut at = start;
    for line in source[start..end].split_inclusive('\n') {
        let quote = line.len() - quote_content(line).len();
        let body = at + quote;
        text.push_str(&line[..quote]);
        let rest = &line[quote..];
        if deeper {
            // **空の行は下げない。**選んだ範囲に挟まった空行まで下げると、
            // そこで箇条書きが切れる（空行は字下げを持たない行である）。
            if rest.trim_end_matches('\n').is_empty() {
                text.push_str(rest);
            } else {
                text.push_str(INDENT_STEP);
                text.push_str(rest);
                shift_positions(&mut moved, body, INDENT_STEP.len() as isize);
                changed = true;
            }
        } else {
            let taken = outdent_width(rest);
            if taken > 0 {
                changed = true;
                shift_positions(&mut moved, body + taken, -(taken as isize));
            }
            text.push_str(&rest[taken..]);
        }
        at += line.len();
    }
    if !changed {
        return None;
    }
    text.push_str(&source[end..]);
    // **番号は階層ごとに数え直す**（書き手の報告 2026-09-10：「TABを打って入れ子に
    // なると、次の番号は1からです。Shift+TABで元の箇条書きに復帰すると、番号が元の
    // 箇条書きの番号を継続します」）。深さを変えたのだから、その連なりの数え方も
    // 変わっている——**同じ一手の中で直す**ので、取り消しは1回で戻る。
    let renumbered = renumber_around(&text, start, &mut moved);
    Some(Indented {
        text: renumbered,
        chosen: (moved[0].min(moved[1]), moved[0].max(moved[1])),
    })
}

/// `at`を含む箇条書きの連なりの番号を、階層ごとに数え直す（E3の④）。
///
/// **深さごとに1から。**内側へ入れば1から始まり、外側へ戻ればその深さの続きから
/// ——Wordの感覚であり、書き手が求めたものである。
///
/// **連なりは空行で切れる**（要件 7.3.2：空行は字下げを持たない行なので、
/// 項目・空行・項目は2つのリストである）。そこで止めないと、遠く上のリストの
/// 番号を継いでしまう。
///
/// 番号を持つ行だけが書き換わる——`-`や`- [x]`は数えるが、書き換えない。
fn renumber_around(source: &str, at: usize, positions: &mut [usize]) -> String {
    let styles = line_styles(source);
    let lines: Vec<&str> = source.split('\n').collect();
    let here = source[..at.min(source.len())].matches('\n').count();
    let inside = |index: usize| styles.get(index).copied().unwrap_or_default().list_indent > 0;
    if !inside(here) {
        return source.to_owned();
    }
    let first = (0..=here).rev().take_while(|index| inside(*index)).last();
    let last = (here..lines.len())
        .take_while(|index| inside(*index))
        .last();
    let (Some(first), Some(last)) = (first, last) else {
        return source.to_owned();
    };
    // 深さごとの数。**内側へ入れば積み、外側へ戻れば捨てる**——捨てたぶんは
    // もう一度入ったときに1から始まる。
    let mut counts: Vec<(u8, u64)> = Vec::new();
    let mut out = String::with_capacity(source.len());
    let mut moved: Vec<isize> = vec![0; positions.len()];
    let mut at = 0;
    for (index, line) in lines.iter().enumerate() {
        let head = at;
        at += line.len() + 1;
        if index < first || index > last {
            push_line(&mut out, line, index + 1 < lines.len());
            continue;
        }
        let style = styles.get(index).copied().unwrap_or_default();
        if !style.kind.is_list() {
            push_line(&mut out, line, index + 1 < lines.len());
            continue;
        }
        let depth = style.list_indent;
        while counts.last().is_some_and(|(held, _)| *held > depth) {
            counts.pop();
        }
        match counts.last_mut() {
            Some((held, count)) if *held == depth => *count += 1,
            _ => counts.push((depth, 1)),
        }
        let number = counts.last().map_or(1, |(_, count)| *count);
        if style.kind != LineKind::Ordered {
            push_line(&mut out, line, index + 1 < lines.len());
            continue;
        }
        let quote = line.len() - quote_content(line).len();
        let content = &line[quote..];
        let (_, body) = leading_indent(content);
        let indent = content.len() - body.len();
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        let written = number.to_string();
        // 数字の桁が変われば、その行の後ろにいる位置もずれる。
        let delta = written.len() as isize - digits as isize;
        if delta != 0 {
            let digits_end = head + quote + indent + digits;
            for (position, shift) in positions.iter().zip(moved.iter_mut()) {
                if *position >= digits_end {
                    *shift += delta;
                }
            }
        }
        out.push_str(&line[..quote + indent]);
        out.push_str(&written);
        out.push_str(&body[digits..]);
        if index + 1 < lines.len() {
            out.push('\n');
        }
    }
    for (position, shift) in positions.iter_mut().zip(moved) {
        *position = position.saturating_add_signed(shift);
    }
    out
}

/// 1行と、その後ろの改行（[`renumber_around`]）。
fn push_line(out: &mut String, line: &str, has_break: bool) {
    out.push_str(line);
    if has_break {
        out.push('\n');
    }
}

/// 一段ぶん外せる字下げの幅（[`shift_indent`]）。
///
/// **タブ1つか、空白を一段まで。**書き手が他の道具でタブを入れた原稿も開くので、
/// そこは1文字で一段とみなす。
fn outdent_width(line: &str) -> usize {
    if line.starts_with('\t') {
        return 1;
    }
    line.chars()
        .take(INDENT_STEP.len())
        .take_while(|character| *character == ' ')
        .count()
}

/// 位置を、`at`より後ろにあるぶんだけずらす（[`shift_indent`]）。
///
/// **`at`そのものは動く側**——行頭に立っていたカーソルは、足した字下げの後ろへ出る。
/// 縮めるときは、消えた範囲の中にいた位置がその頭に集まる。
fn shift_positions(positions: &mut [usize], at: usize, delta: isize) {
    for position in positions {
        if *position < at {
            continue;
        }
        *position = if delta >= 0 {
            *position + delta as usize
        } else {
            position
                .saturating_sub((-delta) as usize)
                .max(at - (-delta) as usize)
        };
    }
}

/// Whether a character is inside a word, for 要件 11.4's `Alt+F` and `Alt+B`.
///
/// **Everything that is not punctuation or space.** 要件 11.4 says the move
/// goes 「句読点または空白まで」 in Japanese, and Japanese puts no spaces between
/// words — so the run that ends at a punctuation mark or a space *is* the word.
/// **The same rule serves English**, where the spaces do that work instead, so
/// there is no second rule to pick between and no guess about which language a
/// line is in.
///
/// `_` counts as inside a word, so `snake_case` is one. This editor is written
/// about code often enough for that to be worth the one exception, and a writer
/// of prose never meets it.
fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The byte an `Alt+F` lands on (要件 11.4).
///
/// **Past whatever is not a word, then past the word itself**, so the caret
/// arrives at the far side of the next word rather than at its near side. That
/// is what makes a run of presses walk forwards one word at a time instead of
/// stopping twice at every word. The end of the text stops it.
pub fn next_word_boundary(text: &str, from: usize) -> usize {
    let mut at = crate::floor_char_boundary(text, from.min(text.len()));
    let mut inside = false;
    for character in text[at..].chars() {
        if is_word(character) {
            inside = true;
        } else if inside {
            break;
        }
        at += character.len_utf8();
    }
    at
}

/// The byte an `Alt+B` lands on (要件 11.4).
///
/// The mirror of [`next_word_boundary`]: back over whatever is not a word, then
/// back over the word, landing at its near side.
pub fn previous_word_boundary(text: &str, from: usize) -> usize {
    let mut at = crate::floor_char_boundary(text, from.min(text.len()));
    let mut inside = false;
    for character in text[..at].chars().rev() {
        if is_word(character) {
            inside = true;
        } else if inside {
            break;
        }
        at -= character.len_utf8();
    }
    at
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
    // markers and all — unless a list set it in (要件 7.3.2), which is either a
    // nested item or a paragraph continuing one. Reading the spaces here
    // instead would be a second opinion about what a line is, and the one that
    // loses: the pane sets what the style says.
    let indented = line.starts_with([' ', '\t']) && style.list_indent == 0;
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
        let start = visible.len();
        visible.push_str(content);
        // 要件 7.3.2: **the one thing said about the inside of code.** The line
        // is literal — nothing is taken off it and nothing stands over it — and
        // this only says which part of it the reader may skip.
        if let Some(at) = comment_start(&visible[start..], style.comment) {
            let length = content.encode_utf16().count() as u32 - at;
            marks.push(Emphasis {
                utf16_start: at,
                utf16_len: length,
                marks: Marks {
                    comment: true,
                    ..Marks::default()
                },
                ornament: None,
            });
        }
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
    // 要件 7.3.2: a callout says on its first line what kind it is. **The label
    // is kept as a word**, because the preview only ever deletes from the
    // source (技術検証 4.12) — a title cannot be put on the page that was not
    // written on it. What it can do is take the brackets off and set the word
    // the writer typed, inside the quote's own bar and indent.
    let content = match callout_label(content, style) {
        Some((label, rest)) => {
            let start = at;
            for character in label.chars() {
                visible.push(character);
                at += character.len_utf16() as u32;
            }
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: at - start,
                marks: Marks {
                    bold: true,
                    ..Marks::default()
                },
                ornament: None,
            });
            rest
        }
        None => content,
    };
    push_marked(content, visible, marks, &mut at);
}

/// The kind a callout announces on its first line, and what follows it
/// (要件 7.3.2).
///
/// `> [!NOTE]` and `> [!WARNING] 見出し`. **Only inside a quote**, which is what
/// the notation is built on: a callout is a quote that says what it is for, and
/// the bar and the indent it is drawn in are the quote's.
///
/// The label is given back **as the writer typed it** — brackets off, nothing
/// added, nothing translated. Anything else would be a word on the page that is
/// not in the document.
fn callout_label(content: &str, style: LineStyle) -> Option<(&str, &str)> {
    if style.quote_depth == 0 {
        return None;
    }
    let inner = content.strip_prefix("[!")?;
    let (label, rest) = inner.split_once(']')?;
    // A label is a word, and a `[!` with a bracket somewhere later in a
    // sentence is not one. **Letters only**, which is every kind Obsidian
    // defines and every kind anyone writes.
    let named = !label.is_empty() && label.chars().all(|c| c.is_ascii_alphabetic());
    named.then_some((label, rest))
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
        // 要件 7.8: **傍点はルビより先に読む。**`《《強調》》`はルビの`《》`で
        // 始まるので、後から見ると「《強調《」という読みのおかしなルビとして
        // 当たってしまう。長いほうを先に訊く、というだけの順である。
        if let Some((inner, after)) = dots_here(rest) {
            let start = *at;
            // 中は普通の本文なので、太字も斜体もそのまま入れ子になる。
            push_marked(inner, visible, marks, at);
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: Marks {
                    dots: true,
                    ..Marks::default()
                },
                ornament: None,
            });
            previous = inner.chars().next_back();
            rest = after;
            continue;
        }
        // 要件 7.8: 青空文庫の注記形式（`［＃「本当に」に傍点］`）。
        // **これだけが後ろを向いている。**注記は自分より前にある語を指すので、
        // いま書き出した`visible`の中をさかのぼって、その語に点を打つ。
        if let Some((word, after)) = dots_note_here(rest) {
            if let Some(found) = visible.rfind(word) {
                let start = visible[..found].encode_utf16().count() as u32;
                marks.push(Emphasis {
                    utf16_start: start,
                    utf16_len: word.encode_utf16().count() as u32,
                    marks: Marks {
                        dots: true,
                        ..Marks::default()
                    },
                    ornament: None,
                });
            }
            // 指す先が無くても注記そのものは消える。**原文には残っている**ので
            // 失われるものは無く、本文に`［＃…］`が出続けるほうが読みにくい。
            rest = after;
            continue;
        }
        // 要件 7.8: ルビ。`｜親《よみ》`と、親が漢字の連なりで明らかなときの
        // `漢字《かんじ》`。**縦線は消え、読みは居残って箱で隠れる**
        // （`Ornament::Ruby`にその理由が書いてある）。
        if let Some((base, reading, after, already_shown)) = ruby_here(rest, visible) {
            let base_start = *at;
            for character in base.chars() {
                visible.push(character);
                *at += character.len_utf16() as u32;
            }
            // **親文字は縦線の側から来るとは限らない。**`漢字《かんじ》`では
            // 親はもう`visible`に出ているので、そのぶんを数えに足す。
            let base_utf16 = already_shown + (*at - base_start);
            let reading_start = *at;
            for character in reading.chars() {
                visible.push(character);
                *at += character.len_utf16() as u32;
            }
            marks.push(Emphasis {
                utf16_start: reading_start,
                utf16_len: *at - reading_start,
                marks: Marks::default(),
                ornament: Some(Ornament::Ruby { base_utf16 }),
            });
            previous = base.chars().next_back().or(previous);
            rest = after;
            continue;
        }
        // 要件 7.3.2: a link shows what it was given to show. **Before the
        // paired markers**, because what a link hides is not a pair around the
        // text — `[` opens it, `](…)` closes it, and the part between them is
        // the only part meant to be read.
        // **Before the link**, because `[^1]` begins the way a link does and
        // means something else. A link needs `](` after its text and a footnote
        // needs a caret before it, so the two never both match — but reading
        // the caret first is what says so at a glance.
        if let Some((name, after)) = footnote_here(rest) {
            let start = *at;
            visible.push('[');
            *at += 1;
            for character in name.chars() {
                visible.push(character);
                *at += character.len_utf16() as u32;
            }
            visible.push(']');
            *at += 1;
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: Marks {
                    link: true,
                    ..Marks::default()
                },
                ornament: None,
            });
            previous = Some(']');
            rest = after;
            continue;
        }
        if let Some((shown, after)) = link_here(rest, previous) {
            let start = *at;
            // Emphasis inside the shown text is still emphasis: `[**太字**](x)`
            // is a bold link, and this is the same recursion that nests one
            // marker inside another.
            push_marked(shown, visible, marks, at);
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: Marks {
                    link: true,
                    ..Marks::default()
                },
                ornament: None,
            });
            previous = shown.chars().next_back();
            rest = after;
            continue;
        }
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
                ornament: None,
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

/// 傍点の`《《…》》`（要件 7.8、カクヨム式）。中身と、その後ろ。
///
/// **開いて閉じるものだけが記法である**——太字の`**`と同じ規則で、閉じない
/// `《《`はただの括弧として本文に残る。空の`《《》》`も記法ではない（点を打つ
/// 相手がいない）。
fn dots_here(rest: &str) -> Option<(&str, &str)> {
    let after_open = rest.strip_prefix("《《")?;
    let close = after_open.find("》》")?;
    if close == 0 {
        return None;
    }
    Some((&after_open[..close], &after_open[close + "》》".len()..]))
}

/// 青空文庫の注記形式の傍点（`［＃「本当に」に傍点］`、要件 7.8）。
/// 点を打つ語と、注記の後ろ。
///
/// **注記は後ろから前を指す**ので、返すのは語そのものである——どこに打つかは
/// 呼び出し側が`visible`をさかのぼって決める。ここは書式を読むだけ。
///
/// 「傍点」以外の注記（`［＃改ページ］`など）は読まない。**知らない注記は
/// 本文として残す**：消してしまうと、原文にある指示が画面から消えたまま
/// 何も起きないことになる。
fn dots_note_here(rest: &str) -> Option<(&str, &str)> {
    let after_open = rest.strip_prefix("［＃「")?;
    let close = after_open.find("」に傍点］")?;
    if close == 0 {
        return None;
    }
    Some((
        &after_open[..close],
        &after_open[close + "」に傍点］".len()..],
    ))
}

/// ルビ（要件 7.8、青空文庫／なろう式）。
///
/// 返すのは**これから書き出す親文字**、`《》`込みの読み、その後ろ、そして
/// **すでに`visible`に出ている親文字の長さ**（UTF-16単位）の4つ。
/// 形が2つあるので、どちらから来ても同じ組を返すためにこの形にしてある。
///
/// - `｜親文字《よみ》`（半角`|`も受ける——なろうが両方読む）。縦線が親文字の
///   始まりを言うので、親は何の字でもよい。**縦線だけが消える。**
/// - `漢字《かんじ》`。親は`《`の直前にある漢字の連なりで、そこはもう書き出して
///   ある。**漢字が前に無ければルビではない**——`《`は日本語の本文では引用符
///   としても使われるので、これがそれと分ける唯一の規則である。
///
/// **読みが空ならルビではない。**`《》`だけが残っても組む字が無い。
fn ruby_here<'a>(rest: &'a str, visible: &str) -> Option<(&'a str, &'a str, &'a str, u32)> {
    if let Some(after_bar) = rest.strip_prefix('｜').or_else(|| rest.strip_prefix('|')) {
        let open = after_bar.find('《')?;
        let base = &after_bar[..open];
        // 縦線が2本続くのは親文字の切れ目の言い直しで、ルビの親ではない。
        // 表の行の`|`もここで落ちる。
        if base.is_empty() || base.contains(['｜', '|']) {
            return None;
        }
        let (reading, after) = reading_at(&after_bar[open..])?;
        return Some((base, reading, after, 0));
    }
    if !rest.starts_with('《') || rest.starts_with("《《") {
        return None;
    }
    let (reading, after) = reading_at(rest)?;
    let back = trailing_kanji(visible);
    if back == 0 {
        return None;
    }
    Some(("", reading, after, back))
}

/// `《…》`を`《》`込みで切り出す。入れ子は読まない——ルビの読みは字の並びで
/// あって、その中にもう一段の記法は無い。
fn reading_at(rest: &str) -> Option<(&str, &str)> {
    let after_open = rest.strip_prefix('《')?;
    let close = after_open.find('》')?;
    if close == 0 || after_open[..close].contains('《') {
        return None;
    }
    let end = '《'.len_utf8() + close + '》'.len_utf8();
    Some((&rest[..end], &rest[end..]))
}

/// 末尾に続いている漢字の長さ（UTF-16単位）。0なら漢字で終わっていない。
///
/// **々も漢字の側に数える**（「人々《ひとびと》」）。ひらがな・カタカナは
/// 数えない——`親《おや》`のように送り仮名まで巻き込むと、書き手が縦線で
/// 言い直すしかなくなる。それが縦線のある理由だが、**要らないときに要求する
/// 記法は、要件7.8が採った互換性の意味を薄くする**。
fn trailing_kanji(visible: &str) -> u32 {
    let mut counted = 0;
    for letter in visible.chars().rev() {
        if !is_kanji(letter) {
            break;
        }
        counted += letter.len_utf16() as u32;
    }
    counted
}

/// CJK統合漢字（拡張Aまで）と、繰り返しの`々`。
fn is_kanji(letter: char) -> bool {
    matches!(letter, '\u{3005}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}')
}

/// ルビの読みが占める書記素の数（要件 7.8）。
///
/// **`《》`込みで数える。**画面に出ていないのは読みの字だけではなく、それを
/// 囲む括弧もである——本文として読む人にはどちらも見えていない。
fn ruby_graphemes(visible: &str, marks: &[Emphasis]) -> usize {
    marks
        .iter()
        .filter(|mark| matches!(mark.ornament, Some(Ornament::Ruby { .. })))
        .map(|mark| {
            let start = byte_at_utf16_in(visible, mark.utf16_start);
            let end = byte_at_utf16_in(visible, mark.utf16_start + mark.utf16_len);
            visible[start..end].graphemes(true).count()
        })
        .sum()
}

/// 文書全体のルビの読みの数（要件 7.8）。
///
/// **[`DocumentStats::from_source`]と同じ立場**——1行ずつ数える
/// [`DocumentCounts`]の答え合わせに使う、素朴なほうの実装である。
#[cfg(test)]
fn ruby_graphemes_in(source: &str) -> usize {
    let styles = line_styles(source);
    source
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let style = styles.get(index).copied().unwrap_or_default();
            let mut visible = String::with_capacity(line.len());
            let mut marks = Vec::new();
            push_visible_line(line, style, &mut visible, &mut marks);
            ruby_graphemes(&visible, &marks)
        })
        .sum()
}

/// `visible`の中の、UTF-16位置に当たるバイト位置。
///
/// 印はUTF-16で測ってあり、書記素を数えるにはバイトが要る。**範囲の外は
/// 末尾に丸める**——切れ端の印を渡されても落ちない側へ。
fn byte_at_utf16_in(visible: &str, position: u32) -> usize {
    let mut units = 0;
    for (byte, letter) in visible.char_indices() {
        if units >= position {
            return byte;
        }
        units += letter.len_utf16() as u32;
    }
    visible.len()
}

/// The footnote `rest` begins with: what it shows, and what follows it
/// (要件 7.3.2).
///
/// `[^1]` in the middle of a sentence, and `[^1]:` at the head of the line that
/// defines it. **The caret comes off and the brackets stay**: the preview only
/// ever deletes (技術検証 4.12), so `1` on its own would be a number nobody
/// could tell from a number, and `[1]` is what a footnote has looked like in
/// print for as long as there have been footnotes.
///
/// The name may be anything without a space or a bracket, which is what
/// Markdown allows: `[^あ]` and `[^note-1]` are both footnotes.
fn footnote_here(rest: &str) -> Option<(&str, &str)> {
    let inner = rest.strip_prefix("[^")?;
    let (name, after) = inner.split_once(']')?;
    let named = !name.is_empty() && !name.contains([' ', '[', ']']);
    named.then_some((name, after))
}

/// The link `rest` begins with: the text it shows, and what follows it.
///
/// Two shapes, and one rule they share with every other marker — **a link that
/// does not close is not a link**, so an unmatched `[` is a bracket and stays
/// one.
///
/// - `[shown](where)`, the ordinary link. 要件 7.3.2 formats it; where it
///   points is 要件 7.3.1's business (`Ctrl+click`) and not shown.
/// - `[[note]]` and `[[note|shown]]`, the internal link. **Kept and formatted
///   even though going to one is out of scope** (要件定義 §14): not breaking a
///   notation and being able to decide where it points are different things.
///
/// **An image is not a link.** `![説明](画像.png)` keeps its markup, because
/// 要件 7.3.3 asks for the image to be shown as a name — and a link that read
/// as ordinary text would hide the one thing that says it is a picture.
fn link_here<'a>(rest: &'a str, previous: Option<char>) -> Option<(&'a str, &'a str)> {
    if previous == Some('!') {
        return None;
    }
    if let Some(inner_and_rest) = rest.strip_prefix("[[") {
        let (inner, after) = inner_and_rest.split_once("]]")?;
        // `[[note|shown]]` shows the second half; `[[note]]` shows the note.
        let shown = inner.split_once('|').map_or(inner, |(_, shown)| shown);
        return (!shown.is_empty()).then_some((shown, after));
    }
    let inner_and_rest = rest.strip_prefix('[')?;
    let (shown, after_close) = inner_and_rest.split_once("](")?;
    // The shown text may hold brackets of its own, but not a `](` — the first
    // one closes the link, which is what Markdown itself does.
    let (_, after) = after_close.split_once(')')?;
    (!shown.is_empty()).then_some((shown, after))
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

/// A fence that is open, and what it said the block is (要件 7.3.2).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fence {
    /// Which of the two markers opened it; only the same one closes it.
    marker: char,
    comment: CommentSyntax,
}

/// What starts a comment in the language an opening fence names (要件 7.3.2).
///
/// **The writer's word, not a guess at the contents.** A fence with nothing
/// after it says nothing about what is in it, and a language nobody listed here
/// is left alone — which is the safe way round: the cost of not knowing is that
/// code looks like code, and the cost of guessing wrong is a colour over
/// something that is not a comment.
fn comment_syntax(line: &str) -> CommentSyntax {
    let named = line.trim_start_matches(['`', '~']).trim();
    // `rust,ignore` and `python title="x"` both name the language first.
    let named = named
        .split([' ', '\t', ',', ';', '{'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match named.as_str() {
        "rust" | "rs" | "c" | "cc" | "cpp" | "c++" | "h" | "hpp" | "cs" | "csharp" | "java"
        | "js" | "javascript" | "mjs" | "jsx" | "ts" | "typescript" | "tsx" | "go" | "swift"
        | "kotlin" | "kt" | "scala" | "php" | "dart" | "zig" | "glsl" | "jsonc" => {
            CommentSyntax::Slashes
        }
        "python" | "py" | "ruby" | "rb" | "sh" | "bash" | "zsh" | "shell" | "console" | "fish"
        | "yaml" | "yml" | "toml" | "perl" | "pl" | "r" | "make" | "makefile" | "cmake"
        | "dockerfile" | "docker" | "nim" | "elixir" | "ex" | "powershell" | "ps1" | "conf" => {
            CommentSyntax::Hash
        }
        "sql" | "lua" | "haskell" | "hs" | "elm" | "ada" => CommentSyntax::Dashes,
        "lisp" | "clojure" | "clj" | "scheme" | "elisp" | "asm" | "nasm" | "ini" => {
            CommentSyntax::Semicolon
        }
        "tex" | "latex" | "erlang" | "erl" | "matlab" | "octave" => CommentSyntax::Percent,
        _ => CommentSyntax::None,
    }
}

/// Where a comment begins in one line of code, in UTF-16 units, if it begins.
///
/// **Quoted stretches are skipped, and nothing else is looked at.** A URL in a
/// string is how a rule this simple shows itself — `"https://…"` coloured from
/// the slashes — and stepping over quotes is the whole of what it takes to
/// avoid the one mistake a reader would notice. What is left uncaught is the
/// rest of what a lexer would know: a `#` inside a shell word, a marker inside
/// a raw string, a language whose strings are not `'` or `"`. 要件 4.2 says
/// this editor does not read code, and this is the line that draws.
fn comment_start(line: &str, syntax: CommentSyntax) -> Option<u32> {
    let marker = syntax.marker()?;
    let mut at = 0u32;
    let mut quote: Option<char> = None;
    let mut characters = line.char_indices();
    while let Some((byte, letter)) = characters.next() {
        match quote {
            Some(open) => {
                if letter == '\\' {
                    // The escaped character is one more character of string,
                    // whatever it is.
                    if let Some((_, escaped)) = characters.next() {
                        at += letter.len_utf16() as u32 + escaped.len_utf16() as u32;
                        continue;
                    }
                } else if letter == open {
                    quote = None;
                }
            }
            None => {
                if line[byte..].starts_with(marker) {
                    return Some(at);
                }
                if letter == '"' || letter == '\'' {
                    quote = Some(letter);
                }
            }
        }
        at += letter.len_utf16() as u32;
    }
    None
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

    /// How far in a line that continues an item is set, and `None` for one that
    /// continues nothing (要件 7.3.2).
    ///
    /// **The deepest level it is indented past.** A writer lines a continuation
    /// up under the text of the item it belongs to, so the item it belongs to
    /// is the last one that begins to the left of it. Being indented further
    /// than that changes nothing: it lines up with the same item's text, which
    /// is where it is set.
    fn continuing(&self, columns: usize) -> Option<u8> {
        let depth = self.0.iter().rposition(|open| *open < columns)?;
        u8::try_from(depth + 1).ok()
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
    let quote_depth = u8::from(quote.is_some());
    if kind.is_list() {
        return LineStyle {
            heading_level: 0,
            kind,
            quote_depth,
            comment: CommentSyntax::None,
            list_indent: levels.depth_of(columns) + 1,
        };
    }
    // 要件 7.3.2: a paragraph written under an item belongs to it and is set in
    // with it. **Not an item itself** — it has no marker, and none is drawn for
    // it; what it has is the same indent, so that the writer's own lining-up on
    // the page is what appears on it.
    if let Some(indent) = levels.continuing(columns)
        && matches!(kind, LineKind::Body)
    {
        return LineStyle {
            heading_level: 0,
            kind,
            quote_depth,
            comment: CommentSyntax::None,
            list_indent: indent,
        };
    }
    // **The rule the preview reads a line by**: indented text that continues
    // nothing is literal, markers and all (`push_visible_line`). Nothing here
    // may call such a line a list or a rule, or the pane would set a line the
    // preview shows verbatim.
    if columns > 0 {
        levels.ended_by(columns, body);
        return LineStyle::default();
    }
    levels.ended_by(columns, body);
    LineStyle {
        heading_level: heading_level(line),
        kind,
        quote_depth,
        comment: CommentSyntax::None,
        list_indent: 0,
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
/// 編集中の行で、**描かれない行頭の空白**（要件 7.3.1、書き手の報告 2026-09-10）。
///
/// 記号は箱の下にあり、墨は前後の空白を落として溝に描かれる（`marker_ink`）
/// ——入れ子の字下げは**どこにも描かれない**ので、そこに立ったカーソルは記号の頭に
/// 見える。書き手には「その間、止まっているように見えます」となる。
///
/// **描かれない字は、カーソルの止まり場所ではない。**返した範囲を、←と→は一息に
/// 跨ぐ（`main`の`move_pane_caret`）。
///
/// 引用の`>`は描かれる（落とすのは空白だけ）ので、ここには入らない。
pub fn hidden_indent(source: &str, styles: &[LineStyle], caret: usize) -> Option<Range<usize>> {
    let (line_start, line_end) = line_span(source, caret);
    let line = source[line_start..line_end]
        .strip_suffix('\n')
        .unwrap_or(&source[line_start..line_end]);
    let index = source[..line_start].matches('\n').count();
    let style = styles.get(index).copied().unwrap_or_default();
    let marker = active_markup(line, style)?;
    let hidden = line.len() - line.trim_start().len();
    // 箱の外まで跨がない——覆われているぶんだけが隠れている。
    let hidden = hidden.min(marker.utf16_len as usize);
    (hidden > 0).then(|| line_start..line_start + hidden)
}

/// 編集中の行で、行頭の記号が座る箱（要件 7.3.1、書き手の報告 2026-09-10）。
///
/// **隠すためではなく、幅を取らせないための箱。**覆った字はそのまま溝に描かれる
/// （`Ornament::Markup`）ので、記号は原文のまま見えている——変わるのは、その字が
/// 本文の流れから外れて溝に立つことだけである。おかげで**本文の位置が、その行に
/// カーソルがあるかどうかで動かない。**
///
/// 覆うのは、組み上がりの行で消えているものと同じ範囲——引用の`>`、字下げの空白、
/// 箇条書きの印。`line_marker`が数えるのは`>`を落としたあとなので、そのぶんを
/// 足し直す（原文の行には`>`が残っている）。
///
/// **区切り線とフェンスは覆わない。**あれは行そのものが記号で、溝に立てるものが
/// 無い——原文で出ている行を、二度描くことになる。
fn active_markup(line: &str, style: LineStyle) -> Option<LineMarker> {
    // `> `はASCIIなので、バイトの数がそのままUTF-16の数である。
    let quote = (line.len() - quote_content(line).len()) as u32;
    match line_marker(line, style) {
        // **区切り線とフェンスは覆わない。**あれは行そのものが記号で、溝に立てる
        // ものが無い——原文で出ている行を、二度描くことになる。
        Some(hidden) if matches!(hidden.ornament, Ornament::Hidden) => None,
        Some(hidden) => Some(LineMarker {
            utf16_len: hidden.utf16_len + quote,
            ornament: Ornament::Markup,
        }),
        // 印を持たない引用の行。**`>`も組み上がりでは消える印である。**
        None => (quote > 0).then_some(LineMarker {
            utf16_len: quote,
            ornament: Ornament::Markup,
        }),
    }
}

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
        //
        // **A table's delimiter row is not here**, though it is all marks too:
        // its box has to be as wide as the table, and how wide that is only the
        // measured cells say. It is built where they are measured, so that no
        // two places have an opinion about the box over that line (技術検証 7.7).
        LineKind::Rule | LineKind::Fence => Ornament::Hidden,
        // 要件 7.3.2: the white space a writer typed to line a continuation up
        // under its item. **The style is what says it is that** — the same
        // spaces under nothing are text, and shown as text.
        LineKind::Body if style.list_indent > 0 => Ornament::Indent,
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
    // Bytes as UTF-16 units, which they are: an indent is spaces and tabs, and
    // `marker_len` says the same about every marker.
    let (_, body) = leading_indent(content);
    let indent = (content.len() - body.len()) as u32;
    let utf16_len = match ornament {
        Ornament::Hidden => content.encode_utf16().count() as u32,
        Ornament::Indent => indent,
        _ => indent + marker_len(body, style.kind)?,
    };
    // Nothing to cover. A continuation that is not indented at all does not
    // arise — being indented is how it was recognised — but a box of no length
    // is a range DirectWrite has no use for either way.
    (utf16_len > 0).then_some(LineMarker {
        utf16_len,
        ornament,
    })
}

/// Where a line stands in a table the lines before it opened (要件 7.3.2).
///
/// **The second thing besides a fence that a line cannot decide alone**
/// (技術検証 7.1). A row of bars is a table only when a delimiter row follows
/// the first one — otherwise it is a sentence with bars in it — and the rows
/// after that one are rows because the delimiter row said so.
#[derive(Clone, Copy)]
enum TablePlace {
    /// The header row has been read and the delimiter row comes next.
    Delimiter,
    /// Inside the rows under the delimiter row.
    Body,
}

/// How a line a table reaches is set, and `None` for one no table reaches
/// (要件 7.3.2).
///
/// `next` is the line after this one, and it is read in one case only: deciding
/// whether a row of bars opens a table. **Nothing else here looks ahead**, and
/// nothing looks further than one line.
fn table_line(
    line: &str,
    next: &str,
    levels: &mut ListLevels,
    table: &mut Option<TablePlace>,
) -> Option<LineStyle> {
    let kind = match *table {
        // The delimiter row itself. It is one because the header row was only
        // read as a header on the strength of it.
        Some(TablePlace::Delimiter) => {
            *table = Some(TablePlace::Body);
            LineKind::TableRule
        }
        Some(TablePlace::Body) if is_table_row(line) => LineKind::TableRow,
        None if is_table_row(line) && table_alignments(next).is_some() => {
            *table = Some(TablePlace::Delimiter);
            LineKind::TableRow
        }
        _ => {
            *table = None;
            return None;
        }
    };
    // A table stands at the margin, so it ends every list open above it, the
    // way any other unindented line does.
    levels.ended_by(0, line);
    Some(LineStyle::of_kind(kind))
}

/// How one line is set, given the fence and the table the lines before it left
/// open.
fn line_style(
    line: &str,
    next: &str,
    fence: &mut Option<Fence>,
    levels: &mut ListLevels,
    table: &mut Option<TablePlace>,
) -> LineStyle {
    let style = match (*fence, fence_marker(line)) {
        (None, Some(opened)) => {
            *fence = Some(Fence {
                marker: opened,
                comment: comment_syntax(line),
            });
            LineStyle::of_kind(LineKind::Fence)
        }
        (Some(open), Some(close)) if open.marker == close => {
            *fence = None;
            LineStyle::of_kind(LineKind::Fence)
        }
        // A run of tildes inside a backtick block closes nothing — it is one
        // more line of code.
        (Some(open), _) => LineStyle {
            comment: open.comment,
            ..LineStyle::of_kind(LineKind::Code)
        },
        (None, None) => {
            if let Some(style) = table_line(line, next, levels, table) {
                return style;
            }
            outside_fence(line, levels)
        }
    };
    // Anything a fence decides ends whatever table was open: a table's rows are
    // bars at the margin, and a fenced line is code whatever it is made of.
    *table = None;
    style
}

/// How every logical line of `source` is set (要件 7.3.2).
///
/// **The one place a line's kind and its heading level are decided together**,
/// so the preview, the panes, the counts and the outline cannot come to hold
/// two opinions about a line. A hash inside a fenced block is not a heading.
///
/// **Two things about a line are decided by the lines around it** — the fence
/// it is inside, and the table it belongs to (`TablePlace`). Everything else is
/// read off the line itself, which is what lets a block depend on nothing
/// outside its own text; and both exceptions end a block rather than crossing
/// one, so a block still holds whole ones of each (`split_blocks`).
///
/// One entry per `split('\n')` line, which is also one entry per line of the
/// preview: the preview emits exactly one line for each source line.
pub fn line_styles(source: &str) -> Vec<LineStyle> {
    let mut fence = None;
    let mut levels = ListLevels::default();
    let mut table = None;
    let mut lines = source.split('\n').peekable();
    let mut styles = Vec::new();
    while let Some(line) = lines.next() {
        // The line after this one, and an empty one past the last: a row of
        // bars at the end of the document has no delimiter row under it and is
        // not a table.
        let next = lines.peek().copied().unwrap_or_default();
        styles.push(line_style(line, next, &mut fence, &mut levels, &mut table));
    }
    styles
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
    let mut table = None;
    let mut lines = source.split('\n').peekable();
    while let Some(line) = lines.next() {
        let next = lines.peek().copied().unwrap_or_default();
        // Through `line_style` rather than `heading_level`, so a hash inside a
        // fenced block is as much not-a-heading here as it is in the pane.
        let level = line_style(line, next, &mut fence, &mut levels, &mut table).heading_level;
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

    /// 要件 7.3.2: what the fence says the block is, and how little of it is
    /// read. The language is the first word, whatever follows it.
    #[test]
    fn the_fence_names_the_language() {
        assert_eq!(comment_syntax("```rust"), CommentSyntax::Slashes);
        assert_eq!(comment_syntax("```rust,ignore"), CommentSyntax::Slashes);
        assert_eq!(comment_syntax("~~~ PYTHON "), CommentSyntax::Hash);
        assert_eq!(
            comment_syntax("```python title=\"a b\""),
            CommentSyntax::Hash
        );
        assert_eq!(comment_syntax("```sql"), CommentSyntax::Dashes);
        // **Nothing is coloured unless the writer said what the block is**, and
        // a language nobody listed is left alone rather than guessed at.
        assert_eq!(comment_syntax("```"), CommentSyntax::None);
        assert_eq!(comment_syntax("```なにか"), CommentSyntax::None);
    }

    /// 要件 7.3.2: **a marker inside a string is not a comment.** The one
    /// mistake a reader would notice is a URL coloured from its slashes, and
    /// stepping over quotes is the whole of what it takes.
    #[test]
    fn a_marker_inside_a_string_starts_nothing() {
        let syntax = CommentSyntax::Slashes;
        assert_eq!(comment_start("let a = 1; // 説明", syntax), Some(11));
        assert_eq!(
            comment_start("let a = \"https://example.com\";", syntax),
            None
        );
        // And the comment after the string is still found.
        assert_eq!(
            comment_start("let a = \"http://x\"; // 説明", syntax),
            Some(20)
        );
        // An escaped quote does not end the string.
        assert_eq!(comment_start("let a = \"\\\"//\"; ", syntax), None);
        assert_eq!(comment_start("let a = 1;", syntax), None);
        // A hash language is the same rule with a different marker.
        assert_eq!(comment_start("a = 1  # 説明", CommentSyntax::Hash), Some(7));
        // And nothing at all is looked for where nothing starts a comment.
        assert_eq!(comment_start("# 説明", CommentSyntax::None), None);
    }

    /// 要件 7.3.2: the mark reaches the preview, over the comment and no more.
    #[test]
    fn a_comment_in_a_fenced_block_is_marked() {
        let source = "```rust\nlet a = 1; // 説明\n```\n";
        let preview = PreviewDocument::from_source(source);
        let marked = preview
            .marks()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.iter().any(|mark| mark.marks.comment))
            .collect::<Vec<_>>();
        assert_eq!(marked.len(), 1, "one line carries a comment");
        let (index, line) = marked[0];
        assert_eq!(index, 1, "the line inside the fence, not the fence");
        let mark = line.iter().find(|mark| mark.marks.comment).expect("marked");
        assert_eq!(mark.utf16_start, 11);
        assert_eq!(mark.utf16_len, "// 説明".encode_utf16().count() as u32);
    }

    /// **And the language reaches the lines under it**: changing what the fence
    /// says re-marks text that did not itself change.
    ///
    /// This is the property `LineStyle` carries the syntax for. The preview
    /// keeps a line while its style is what it was, so a syntax kept anywhere
    /// else would leave these lines marked as they were before.
    #[test]
    fn renaming_the_language_re_marks_the_lines_under_it() {
        let mut preview = PreviewDocument::default();
        preview.refresh("```rust\nlet a = 1; // 説明\n```\n", None);
        let commented = |preview: &PreviewDocument| {
            preview
                .marks()
                .iter()
                .flatten()
                .filter(|mark| mark.marks.comment)
                .count()
        };
        assert_eq!(commented(&preview), 1);

        // `//` is nothing in a language whose comments begin with `#`.
        preview.refresh("```python\nlet a = 1; // 説明\n```\n", None);
        assert_eq!(commented(&preview), 0, "the line is code again");

        preview.refresh("```なにか\nlet a = 1; // 説明\n```\n", None);
        assert_eq!(commented(&preview), 0, "and an unnamed block says nothing");
    }

    /// 要件 11.4 asks for the far side of the next word, so a run of presses
    /// walks forwards rather than stopping twice at each one.
    #[test]
    fn a_word_move_lands_past_the_word_it_crossed() {
        let text = "hello world";

        assert_eq!(next_word_boundary(text, 0), 5);
        assert_eq!(next_word_boundary(text, 5), 11);
        assert_eq!(next_word_boundary(text, 11), 11);
    }

    #[test]
    fn a_backward_word_move_lands_at_the_near_side() {
        let text = "hello world";

        assert_eq!(previous_word_boundary(text, 11), 6);
        assert_eq!(previous_word_boundary(text, 6), 0);
        assert_eq!(previous_word_boundary(text, 0), 0);
    }

    /// **句読点または空白まで** (要件 11.4). Japanese writes no spaces, so the
    /// punctuation is what ends the run — and it is the same rule as English's.
    #[test]
    fn a_japanese_word_ends_at_its_punctuation() {
        let text = "これは、テストです。";

        assert_eq!(next_word_boundary(text, 0), 9, "これは");
        assert_eq!(next_word_boundary(text, 9), 27, "、を越えてテストです");
        assert_eq!(
            previous_word_boundary(text, text.len()),
            12,
            "テストですの頭"
        );
    }

    /// The move never lands inside a character, whichever way it went.
    #[test]
    fn a_word_move_lands_on_a_character_boundary() {
        let text = "あa、い b";

        for from in 0..=text.len() {
            let forwards = next_word_boundary(text, from);
            let backwards = previous_word_boundary(text, from);
            assert!(text.is_char_boundary(forwards), "forwards from {from}");
            assert!(text.is_char_boundary(backwards), "backwards from {from}");
        }
    }

    /// `snake_case` is one word: the underscore is inside it.
    #[test]
    fn an_underscore_holds_a_name_together() {
        let text = "let snake_case = 1";

        assert_eq!(next_word_boundary(text, 4), 14);
    }

    /// 要件 10: **a caret names its line and its column from 1**, and the line
    /// it names is the file's — the empty line between two paragraphs is a line
    /// like any other.
    #[test]
    fn a_caret_names_its_line_and_column_from_one() {
        let source = "一行目\n\n三行目です\n";
        assert_eq!(caret_place(source, 0), (1, 1));
        assert_eq!(caret_place(source, "一行".len()), (1, 3));
        assert_eq!(caret_place(source, "一行目\n".len()), (2, 1));
        assert_eq!(caret_place(source, "一行目\n\n".len()), (3, 1));
        assert_eq!(caret_place(source, "一行目\n\n三行目です".len()), (3, 6));
        // Past the last newline is the head of the line after it, which is
        // where the caret sits when the writer has just pressed Enter.
        assert_eq!(caret_place(source, source.len()), (4, 1));
    }

    /// 要件 10: **a character the writer sees is one column**, whatever it took
    /// to write it. The count beside it in the bar is of the same thing.
    #[test]
    fn a_grapheme_of_many_scalars_moves_the_column_by_one() {
        let family = "👨\u{200d}👩\u{200d}👧";
        let source = format!("{family}あ\n");
        assert_eq!(caret_place(&source, family.len()), (1, 2));
        assert_eq!(caret_place(&source, source.len() - 1), (1, 3));
    }

    /// A byte inside a character names that character's head rather than
    /// panicking (要件 10).
    #[test]
    fn a_byte_inside_a_character_names_its_head() {
        assert_eq!(caret_place("あい", 1), (1, 1));
        assert_eq!(caret_place("あい", 4), (1, 2));
        assert_eq!(caret_place("", 9), (1, 1));
    }

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

    /// E4: 打たれた番号を読む。**画面が出している形をそのまま打てる。**
    #[test]
    fn a_typed_place_reads_as_a_line_and_a_column() {
        assert_eq!(read_place("128"), Some((128, None)));
        assert_eq!(read_place("  12 : 5 "), Some((12, Some(5))));
        // IMEを立てたまま打った全角も同じ番号である。
        assert_eq!(read_place("１２：５"), Some((12, Some(5))));
        assert_eq!(read_place(""), None);
        assert_eq!(read_place("さいご"), None);
        // 1から数える編集器に0行目は無い。
        assert_eq!(read_place("0"), None);
        assert_eq!(read_place("12:0"), None);
        assert_eq!(read_place("-3"), None);
    }

    /// E4: 行と桁の指す位置は、[`caret_place`]がそこで言うことと同じ。
    #[test]
    fn a_place_and_the_caret_agree_about_where_it_is() {
        let source = "一行目\n二行目です\n三行目";

        for (line, column) in [(1, 1), (2, 3), (3, 2)] {
            let at = place_of(source, line, Some(column)).expect("その行はある");
            assert_eq!(caret_place(source, at), (line, column));
        }
        // 桁を言わなければ行頭。
        assert_eq!(place_of(source, 2, None), Some("一行目\n".len()));
    }

    /// E4: **無い行は`None`**——越えた番号で末尾へ連れて行かない
    /// （書き手の選択 2026-09-10）。
    #[test]
    fn a_line_that_is_not_there_is_not_a_place() {
        let source = "一行目\n二行目";

        assert_eq!(place_of(source, 3, None), None);
        assert_eq!(place_of(source, 0, None), None);
        assert!(place_of(source, 2, None).is_some());
        // 最後の行に改行が無くても、その行はある。
        assert_eq!(place_of("一行だけ", 1, None), Some(0));
        assert_eq!(place_of("", 1, None), Some(0));
    }

    /// E4: **桁は行の終わりで止まり、字の途中へは入らない。**
    #[test]
    fn a_column_stops_at_the_end_of_its_own_line() {
        let source = "あい\n家族👨‍👩‍👧です";

        // 行より長い桁は行末まで。次の行へはこぼれない。
        assert_eq!(place_of(source, 1, Some(9)), Some("あい".len()));
        // 4つのスカラーで書かれた絵文字も1桁ぶん。
        let at = place_of(source, 2, Some(4)).expect("その行はある");
        assert_eq!(&source["あい\n".len()..at], "家族👨‍👩‍👧");
        assert!(source.is_char_boundary(at));
    }

    /// E3: ダブルクリックが選ぶのは、`Alt+F`／`Alt+B`と同じ切れ目の語。
    #[test]
    fn a_double_click_takes_the_word_the_walk_would_stop_at() {
        let source = "黒猫が鳴いた。white cat";

        // 日本語は句読点まで。
        assert_eq!(word_around(source, 3), (0, "黒猫が鳴いた".len()));
        // その句読点そのものを押せば、記号だけ。
        let mark = "黒猫が鳴いた".len();
        assert_eq!(word_around(source, mark), (mark, mark + "。".len()));
        // 英語は語ごと。`Alt+F`が止まるところと同じである。
        let white = source.find("white").expect("ある");
        assert_eq!(word_around(source, white + 2), (white, white + 5));
        // 空白は空白だけ（前後の語まで取らない）。**押された字で決まる**ので、
        // 語の隣の空白でも語を巻き込まない。
        let space = white + 5;
        assert_eq!(word_around(source, space), (space, space + 1));
        assert_eq!(word_around(source, space + 1), (space + 1, source.len()));
        let spaces = "a  b";
        assert_eq!(word_around(spaces, 2), (1, 3));
    }

    /// E3: **行はまたがない。**行末で押したら手前の語。
    #[test]
    fn a_double_click_stays_on_its_own_line() {
        let source = "一行目\n二行目";
        let first_end = "一行目".len();

        assert_eq!(word_around(source, first_end), (0, first_end));
        // 空の行には選ぶものが無い。
        assert_eq!(word_around("\n\n", 1), (1, 1));
    }

    /// E3: 行の範囲は改行を含む——行を動かすとき、改行を置いていくと2行が1行になる。
    #[test]
    fn a_line_span_carries_its_own_line_break() {
        let source = "一\n二\n三";

        assert_eq!(line_span(source, 0), (0, "一\n".len()));
        assert_eq!(line_span(source, 4), ("一\n".len(), "一\n二\n".len()));
        // 最後の行に改行は無い。
        assert_eq!(
            line_span(source, source.len()),
            ("一\n二\n".len(), source.len())
        );
    }

    /// 行の編集を当てて、出来上がる本文と選び直される範囲を見る（E3の②）。
    fn edited(source: &str, at: (usize, usize), what: LineEdit) -> Option<(String, String)> {
        let span = selected_lines(source, at.0, at.1);
        let (region, text, chosen) = line_edit(source, span, what)?;
        let mut next = source.to_owned();
        next.replace_range(region, &text);
        let picked = next[chosen.0..chosen.1].to_owned();
        Some((next, picked))
    }

    /// E3の②: 前の行と入れ替える。**選ばれているのは動いた行のまま**なので、
    /// もう一度押せばさらに前へ行く。
    #[test]
    fn a_line_changes_places_with_the_one_before_it() {
        let source = "一\n二\n三\n";
        let second = "一\n".len();

        let (next, picked) =
            edited(source, (second, second), LineEdit::MoveBefore).expect("動かせる");

        assert_eq!(next, "二\n一\n三\n");
        assert_eq!(picked, "二\n");
        // 先頭の行は前へ行けない。
        assert!(edited(source, (0, 0), LineEdit::MoveBefore).is_none());
    }

    /// E3の②: 後の行と入れ替える。**末尾の行は後へ行けない。**
    #[test]
    fn a_line_changes_places_with_the_one_after_it() {
        let source = "一\n二\n三\n";

        let (next, picked) = edited(source, (0, 0), LineEdit::MoveAfter).expect("動かせる");

        assert_eq!(next, "二\n一\n三\n");
        assert_eq!(picked, "一\n");
        assert!(edited(source, (source.len(), source.len()), LineEdit::MoveAfter).is_none());
    }

    /// E3の②: **末尾に改行の無い文書でも、改行は増えも減りもしない。**
    #[test]
    fn moving_the_last_line_neither_gains_nor_loses_a_line_break() {
        let source = "一\n二";
        let second = "一\n".len();

        let (up, picked) =
            edited(source, (second, second), LineEdit::MoveBefore).expect("動かせる");
        assert_eq!(up, "二\n一");
        assert_eq!(picked, "二\n");

        let (down, picked) = edited(source, (0, 0), LineEdit::MoveAfter).expect("動かせる");
        assert_eq!(down, "二\n一");
        assert_eq!(picked, "一");
    }

    /// E3の②: いくつも選んでいれば、そのぶんが1つの塊として動く。
    #[test]
    fn every_line_the_selection_touches_moves_together() {
        let source = "一\n二\n三\n四\n";
        // 「二」の途中から「三」の途中まで。
        let at = ("一\n".len() + 1, "一\n二\n".len() + 1);

        let (next, picked) = edited(source, at, LineEdit::MoveAfter).expect("動かせる");

        assert_eq!(next, "一\n四\n二\n三\n");
        assert_eq!(picked, "二\n三\n");
    }

    /// E3の②: **終わりがちょうど行頭なら、その行は入らない。**
    #[test]
    fn a_selection_that_stops_at_a_line_head_leaves_that_line_alone() {
        let source = "一\n二\n三\n";

        assert_eq!(selected_lines(source, 0, "一\n".len()), (0, "一\n".len()));
    }

    /// E3の②: 写しは前へも後へも。**写したほうが選ばれる**ので、押し続ければ増える。
    #[test]
    fn a_copied_line_is_the_one_left_selected() {
        let source = "一\n二\n";

        let (next, picked) = edited(source, (0, 0), LineEdit::CopyAfter).expect("写せる");
        assert_eq!(next, "一\n一\n二\n");
        assert_eq!(picked, "一\n");

        // 末尾の行（改行なし）を写しても、末尾に改行は生えない。
        let last = "一\n".len();
        let (next, _) = edited("一\n二", (last, last), LineEdit::CopyBefore).expect("写せる");
        assert_eq!(next, "一\n二\n二");
    }

    /// E3の②: 消すと行ごと消える。**末尾の行では、その手前の改行も。**
    #[test]
    fn dropping_a_line_takes_its_line_break_with_it() {
        let second = "一\n".len();
        assert_eq!(
            edited("一\n二\n三\n", (second, second), LineEdit::Drop)
                .expect("消せる")
                .0,
            "一\n三\n"
        );
        // 末尾の行。改行を置いていくと空の行が増える。
        assert_eq!(
            edited("一\n二", (second, second), LineEdit::Drop)
                .expect("消せる")
                .0,
            "一"
        );
        // 1行しかない文書は、空になる。
        assert_eq!(edited("一", (0, 0), LineEdit::Drop).expect("消せる").0, "");
    }

    /// Enterの継ぎ方を、その行の見方ごと当てる（E3の③）。
    fn continued(line: &str, at: usize) -> Continuation {
        enter_continuation(line, &line_styles(line), at, false)
    }

    /// Shift+Enterのほう——印は継がず、本文の列までの空白だけを継ぐ。
    fn continued_softly(line: &str, at: usize) -> Continuation {
        enter_continuation(line, &line_styles(line), at, true)
    }

    /// E3の③: 箇条書きは継ぐ。**番号は1つ進む**（書き手の選択 2026-09-10）。
    #[test]
    fn a_list_item_carries_its_marker_to_the_next_line() {
        assert_eq!(
            continued("- 一つめ", "- 一つめ".len()),
            Continuation::Insert("\n- ".to_owned())
        );
        assert_eq!(
            continued("  * 一つめ", "  * 一つめ".len()),
            Continuation::Insert("\n  * ".to_owned())
        );
        assert_eq!(
            continued("9. 九つめ", "9. 九つめ".len()),
            Continuation::Insert("\n10. ".to_owned())
        );
        // 済んだ印は写さない——まだしていないことを済んだと言うことになる。
        assert_eq!(
            continued("- [x] 済んだ", "- [x] 済んだ".len()),
            Continuation::Insert("\n- [ ] ".to_owned())
        );
        // 引用も継ぐ。
        assert_eq!(
            continued("> 引用", "> 引用".len()),
            Continuation::Insert("\n> ".to_owned())
        );
    }

    /// E3の③: 印の無い行は、**字下げだけ**を継ぐ。
    #[test]
    fn an_indented_line_keeps_its_indent() {
        assert_eq!(
            continued("    続きの段落", "    続きの段落".len()),
            Continuation::Insert("\n    ".to_owned())
        );
        assert_eq!(
            continued("本文", "本文".len()),
            Continuation::Insert("\n".to_owned())
        );
    }

    /// E3の③: **入れ子の項目でも、字下げは行の見方ごと正しく残る**（書き手の報告
    /// 2026-09-10：「字下げは残っていないように見えます」——診断ログでは、試された
    /// 行が`- `／`1. `／`> `で、**そもそも字下げを持っていなかった**）。
    ///
    /// 1行だけを見る`continued`と違い、こちらは文書の中の行——`line_styles`は
    /// 入れ子の深さを前の行から決めるので、そこも通して確かめる。
    #[test]
    fn a_nested_item_keeps_the_indent_it_was_written_at() {
        let source = "- 一つめ\n  - ";
        let styles = line_styles(source);

        assert_eq!(styles[1].kind, LineKind::Bullet);
        // Shift+Enterなら、字下げ2つ＋印の幅2つ＝本文が始まっていた列。
        assert_eq!(
            enter_continuation(source, &styles, source.len(), true),
            Continuation::Clear {
                upto: 4,
                keep: "    ".to_owned()
            }
        );
        // Enterは段ごと捨てる——入れ子でも素の行頭へ。
        assert_eq!(
            enter_continuation(source, &styles, source.len(), false),
            Continuation::Clear {
                upto: 4,
                keep: String::new()
            }
        );
        // 行頭の項目でも、Shift+Enterの本文の列は印の幅のぶん右にある。
        assert_eq!(
            enter_continuation("- ", &line_styles("- "), 2, true),
            Continuation::Clear {
                upto: 2,
                keep: "  ".to_owned()
            }
        );
    }

    /// E3の③: **中身の無い項目でEnterは、印だけ消す**（書き手の選択 2026-09-10）。
    /// 字下げは残り、改行は入らない。
    #[test]
    fn an_empty_item_ends_the_list_instead_of_growing_it() {
        // **Enterは段ごと捨てる**（書き手の選択 2026-09-10）。素の行頭へ戻る。
        assert_eq!(
            continued("- ", 2),
            Continuation::Clear {
                upto: 2,
                keep: String::new()
            }
        );
        assert_eq!(
            continued("  - ", 4),
            Continuation::Clear {
                upto: 4,
                keep: String::new()
            }
        );
        // **Shift+Enterは印だけ捨てる**——カーソルは項目の本文の列に残る。
        assert_eq!(
            continued_softly("  - ", 4),
            Continuation::Clear {
                upto: 4,
                keep: "    ".to_owned()
            }
        );
        // 番号は幅が広いぶん、本文の列も右にある。
        assert_eq!(
            continued_softly("10. ", 4),
            Continuation::Clear {
                upto: 4,
                keep: "    ".to_owned()
            }
        );
        // 引用の印も印である。**こちらは埋めない**——引用に「本文の列」は無い。
        assert_eq!(
            continued("> ", 2),
            Continuation::Clear {
                upto: 2,
                keep: String::new()
            }
        );
        // **印を持たない行は、ここへ来ない**——字下げだけの行でEnterが何もしないと、
        // 効かない鍵に見える。
        assert_eq!(
            continued("    ", 4),
            Continuation::Insert("\n    ".to_owned())
        );
    }

    /// E3の③: **頭の中で押されたEnterは、ただの改行。**行を押し下げたいだけの
    /// 書き手に、印を写して返さない。
    #[test]
    fn an_enter_inside_the_head_is_only_a_line_break() {
        assert_eq!(
            continued("- 一つめ", 0),
            Continuation::Insert("\n".to_owned())
        );
        assert_eq!(
            continued("  - 一つめ", 3),
            Continuation::Insert("\n".to_owned())
        );
        // 印の直後から先は、継ぐ側。
        assert_eq!(
            continued("- 一つめ", 2),
            Continuation::Insert("\n- ".to_owned())
        );
    }

    /// E3の④: `Tab`は行を一段下げ、`Shift+Tab`が戻す。**箇条書きなら入れ子になる。**
    #[test]
    fn a_tab_makes_the_item_a_nested_one() {
        let source = "- 一つめ\n- 二つめ\n";
        let second = "- 一つめ\n".len();

        let deeper = shift_indent(source, second, second, true).expect("下げられる");
        let next = deeper.text.clone();
        assert_eq!(next, "- 一つめ\n    - 二つめ\n");
        // 入れ子として読める（深さが1つ増える）。
        let styles = line_styles(&next);
        assert!(styles[1].list_indent > styles[0].list_indent);
        // カーソルは同じ字の前に残る——字下げのぶんだけ後ろへ。
        assert_eq!(
            deeper.chosen,
            (second + INDENT_STEP.len(), second + INDENT_STEP.len())
        );

        // `Shift+Tab`で戻る。
        let back = shift_indent(&next, second, second, false).expect("戻せる");
        assert_eq!(back.text, source);
    }

    /// E3の④（書き手の報告 2026-09-10）: **番号は階層ごとに1から。**内側へ入れば
    /// 1から始まり、外へ戻ればその深さの続きから——`Tab`と`Shift+Tab`が同じ一手で
    /// 数え直す。
    #[test]
    fn numbers_start_over_inside_and_carry_on_outside() {
        let source = "1. 一\n2. 二\n3. 三\n";
        let second = "1. 一\n".len();
        let third = "1. 一\n2. 二\n".len();

        // 2つめを内側へ——そこは1から、外の3つめは2へ繰り上がる。
        let deeper = shift_indent(source, second, second, true).expect("下げられる");
        assert_eq!(deeper.text, "1. 一\n    1. 二\n2. 三\n");

        // 3つめも内側へ——内側の連なりの続きになる。
        let third = third + INDENT_STEP.len();
        let deeper = shift_indent(&deeper.text, third, third, true).expect("下げられる");
        assert_eq!(deeper.text, "1. 一\n    1. 二\n    2. 三\n");

        // 戻せば、外の連なりの続きへ。
        let back = shift_indent(&deeper.text, third, third, false).expect("戻せる");
        assert_eq!(back.text, "1. 一\n    1. 二\n2. 三\n");
    }

    /// E3の④: **数え直しは空行で切れる**（要件 7.3.2：空行を挟むリストは別の
    /// 連なり）。上の連なりの番号を継いでこない。
    #[test]
    fn renumbering_stops_at_the_blank_line_between_two_lists() {
        let source = "1. 甲\n2. 乙\n\n1. 丙\n2. 丁\n";
        let last = source.find("2. 丁").expect("ある");

        let deeper = shift_indent(source, last, last, true).expect("下げられる");

        assert_eq!(deeper.text, "1. 甲\n2. 乙\n\n1. 丙\n    1. 丁\n");
    }

    /// E3の④: 選んだ行はまとめて。**空の行は下げない**——そこで箇条書きが切れる。
    #[test]
    fn every_selected_line_moves_together_except_the_empty_ones() {
        let source = "一\n\n二\n";

        let deeper = shift_indent(source, 0, source.len(), true).expect("下げられる");

        assert_eq!(deeper.text, "    一\n\n    二\n");
    }

    /// E3の④: **字下げは引用の`>`の後ろ。**前に入れると引用そのものが崩れる。
    #[test]
    fn an_indent_goes_after_the_quote_marker() {
        let source = "> - 項目\n";

        let deeper = shift_indent(source, 0, 0, true).expect("下げられる");

        assert_eq!(deeper.text, ">     - 項目\n");
        assert_eq!(line_styles(&deeper.text)[0].quote_depth, 1);
    }

    /// E3の④: **外せる字下げが無ければ、何も起きない**（`None`）。
    #[test]
    fn a_line_at_the_margin_has_nothing_to_give_back() {
        assert_eq!(shift_indent("項目\n", 0, 0, false), None);
        // タブ1つは一段とみなす（他の道具で書かれた原稿）。
        let taken = shift_indent("\t項目\n", 0, 0, false).expect("外せる");
        assert_eq!(taken.text, "項目\n");
    }

    /// E3（書き手の報告 2026-09-10）: **描かれない行頭の空白は、カーソルの
    /// 止まり場所ではない。**入れ子の項目の字下げは箱の下にあってどこにも
    /// 描かれないので、そこに立ったカーソルは記号の頭に見える。
    #[test]
    fn the_hidden_indent_of_a_nested_item_is_not_a_place_to_stand() {
        let source = "1. 親\n    1. 子";
        let styles = line_styles(source);
        let line_start = "1. 親\n".len();

        // 入れ子の行頭の4桁が隠れている。
        assert_eq!(
            hidden_indent(source, &styles, line_start + 2),
            Some(line_start..line_start + 4)
        );
        // 字下げの無い項目には、隠れている空白が無い。
        assert_eq!(hidden_indent(source, &styles, 1), None);
        // 引用の`>`は描かれるので、跨ぐものではない。
        let quoted = "> - 項目";
        assert_eq!(hidden_indent(quoted, &line_styles(quoted), 1), None);
    }

    /// E3の③（書き手の決定 2026-09-10）: **Shift+Enterは項目の続きの段落。**
    /// `1. aaa`で押した書き手が欲しいのは`2.`ではなく、`aaa`の下から続く行である。
    #[test]
    fn a_soft_break_continues_the_item_as_a_paragraph() {
        let source = "1. aaa";

        assert_eq!(
            enter_continuation(source, &line_styles(source), source.len(), true),
            // 印は継がず、本文の列（`1. `の3桁）まで空白を継ぐ。
            Continuation::Insert("\n   ".to_owned())
        );
        // 素のEnterは箇条書きを進める。
        assert_eq!(
            enter_continuation(source, &line_styles(source), source.len(), false),
            Continuation::Insert("\n2. ".to_owned())
        );
    }

    /// E3の③（書き手の決定 2026-09-10）: **空の継続行でEnterを押すと、改行して
    /// 次の項目が出る。**Shift+Enterで作った段落に何も書かなかったのだから、
    /// 書き手はもう次の項目を書こうとしている——空いた行は残るので一行空く。
    #[test]
    fn an_empty_paragraph_under_an_item_opens_the_next_item() {
        let source = "1. aaa\n   ";
        let at = source.len();

        // **改行して次の項目**——空いた行はそのまま残るので、一行空く。
        assert_eq!(
            enter_continuation(source, &line_styles(source), at, false),
            Continuation::Insert("\n2. ".to_owned())
        );
        // 箇条書きの中でも、深さの合う項目を継ぐ。
        let nested = "1. 親\n    1. 子\n       ";
        assert_eq!(
            enter_continuation(nested, &line_styles(nested), nested.len(), false),
            Continuation::Insert("\n    2. ".to_owned())
        );
        // Shift+Enterはそのまま改行——段落を続けたい書き手の鍵である。
        assert_eq!(
            enter_continuation(source, &line_styles(source), at, true),
            Continuation::Insert("\n   ".to_owned())
        );
    }

    /// E3の③: **リストの外までは遡らない。**深さの合う項目が上に無ければ、
    /// ただの字下げた行として改行する。
    #[test]
    fn an_indented_line_outside_a_list_stays_a_plain_line() {
        let source = "本文\n   ";

        assert_eq!(
            enter_continuation(source, &line_styles(source), source.len(), false),
            Continuation::Insert("\n   ".to_owned())
        );
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
            Marks { dots: true, .. } => "dots",
            _ => "none",
        }
    }

    /// ルビの走りだけを、`(読みの位置, 読みの長さ, 親文字の長さ)`で。
    fn ruby_shape(marks: &[Emphasis]) -> Vec<(u32, u32, u32)> {
        marks
            .iter()
            .filter_map(|mark| match mark.ornament {
                Some(Ornament::Ruby { base_utf16 }) => {
                    Some((mark.utf16_start, mark.utf16_len, base_utf16))
                }
                _ => None,
            })
            .collect()
    }

    fn shape(marks: &[Emphasis]) -> Vec<(u32, u32, &'static str)> {
        marks
            .iter()
            .map(|mark| (mark.utf16_start, mark.utf16_len, label(mark.marks)))
            .collect()
    }

    /// 要件 7.8: **縦線が親文字の始まりを言う。**消えるのは縦線だけで、
    /// 読みは本文に居残ったまま箱で隠れる——だから描くときに読む字がある。
    #[test]
    fn a_ruby_bar_names_where_the_base_begins() {
        let (visible, marks) = preview_of("｜地の文《じのぶん》を書く");

        assert_eq!(visible, "地の文《じのぶん》を書く");
        // 読みは3文字目から`《じのぶん》`の6単位、親文字は3単位。
        assert_eq!(ruby_shape(&marks), vec![(3, 6, 3)]);
    }

    /// 半角の`|`も受ける（なろうが両方読む）。表の行の`|`は、読みが続かない
    /// のでルビにならない。
    #[test]
    fn a_half_width_bar_is_a_ruby_bar_too() {
        assert_eq!(preview_of("|地の文《じのぶん》").0, "地の文《じのぶん》");
        assert_eq!(preview_of("| a | b |").0, "| a | b |");
        assert_eq!(ruby_shape(&preview_of("| a | b |").1), vec![]);
    }

    /// 要件 7.8: 親が漢字の連なりで明らかなときは縦線が要らない。
    /// **漢字が前に無ければルビではない**——`《》`は本文の引用符でもある。
    #[test]
    fn kanji_before_a_reading_is_the_base_without_a_bar() {
        let (visible, marks) = preview_of("彼は漢字《かんじ》を見た");

        assert_eq!(visible, "彼は漢字《かんじ》を見た");
        assert_eq!(ruby_shape(&marks), vec![(4, 5, 2)]);
        // ひらがなの後ろは親文字にならないので、これはただの括弧。
        assert_eq!(ruby_shape(&preview_of("ここで《ちゅうい》").1), vec![]);
        assert_eq!(preview_of("ここで《ちゅうい》").0, "ここで《ちゅうい》");
    }

    /// **閉じない《はルビではない**——太字の`**`と同じ規則である。
    #[test]
    fn a_reading_that_does_not_close_is_not_ruby() {
        assert_eq!(ruby_shape(&preview_of("漢字《かんじ").1), vec![]);
        assert_eq!(ruby_shape(&preview_of("漢字《》").1), vec![], "空の読み");
        assert_eq!(ruby_shape(&preview_of("｜親《").1), vec![], "縦線だけ");
        assert_eq!(preview_of("漢字《かんじ").0, "漢字《かんじ");
    }

    /// 要件 7.8: 傍点はカクヨム式の`《《…》》`。**ルビより先に読む**ので、
    /// `《強調《`という読みのおかしなルビにはならない。
    #[test]
    fn double_brackets_are_emphasis_dots() {
        let (visible, marks) = preview_of("彼は《《本当に》》来た");

        assert_eq!(visible, "彼は本当に来た");
        assert_eq!(shape(&marks), vec![(2, 3, "dots")]);
        assert_eq!(ruby_shape(&marks), vec![], "ルビとして当たっていない");
    }

    /// 青空文庫の注記形式（書き手の決定、2026-09-09）。
    /// **これだけが後ろを向いている**——注記は自分より前にある語を指す。
    #[test]
    fn an_aozora_note_puts_dots_on_the_word_before_it() {
        let (visible, marks) = preview_of("彼は本当に来た［＃「本当に」に傍点］");

        assert_eq!(visible, "彼は本当に来た");
        assert_eq!(shape(&marks), vec![(2, 3, "dots")]);
    }

    /// 指す先が無い注記も消える。**原文には残っている**ので失うものは無く、
    /// 本文に`［＃…］`が出続けるほうが読みにくい。知らない注記は本文のまま。
    #[test]
    fn a_note_pointing_at_nothing_still_comes_off() {
        assert_eq!(preview_of("彼は来た［＃「本当に」に傍点］").0, "彼は来た");
        assert_eq!(
            preview_of("彼は来た［＃改ページ］").0,
            "彼は来た［＃改ページ］"
        );
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

    /// 要件 7.3.2: a row of bars is a table only when a delimiter row follows
    /// the first one, and the rows after it are rows because that row said so.
    #[test]
    fn a_delimiter_row_is_what_opens_a_table() {
        assert_eq!(
            kinds("| 見出し | 見出し |\n| --- | ---: |\n| 一 | 二 |\n本文\n"),
            vec![
                LineKind::TableRow,
                LineKind::TableRule,
                LineKind::TableRow,
                LineKind::Body,
                LineKind::Body,
            ]
        );
    }

    /// **A sentence with bars in it is a sentence.** Nothing under a row of
    /// bars that no delimiter row follows is a table (要件 7.3.2).
    #[test]
    fn bars_alone_are_not_a_table() {
        assert_eq!(
            kinds("| 一 | 二 |\n| 三 | 四 |\n"),
            vec![LineKind::Body, LineKind::Body, LineKind::Body]
        );
    }

    /// A table is bars at the margin, so a fenced line is code whatever it is
    /// made of (要件 7.3.2).
    #[test]
    fn a_fence_ends_a_table() {
        assert_eq!(
            kinds("| 一 |\n| --- |\n```\n| 二 |\n```\n| 三 |\n"),
            vec![
                LineKind::TableRow,
                LineKind::TableRule,
                LineKind::Fence,
                LineKind::Code,
                LineKind::Fence,
                LineKind::Body,
                LineKind::Body,
            ]
        );
    }

    /// 要件 7.3.2: **no box for a table comes from here.** A row's bars and the
    /// delimiter row both need widths that only the measured cells say, so both
    /// are built where the measuring happens (技術検証 7.7) — and a line with
    /// two opinions about the box over it is a line drawn twice.
    #[test]
    fn a_table_gets_no_box_from_the_head_of_its_lines() {
        let styles = line_styles("| 一 |\n| --- |\n| 二 |\n");

        assert_eq!(line_marker("| 一 |", styles[0]), None, "a row of cells");
        assert_eq!(line_marker("| --- |", styles[1]), None, "the delimiter row");
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

    /// **カーソルのある行の記号は、溝にぶら下がる**（要件 7.3.1、書き手の報告
    /// 2026-09-10）。箱は字を隠すためではなく、幅を取らせないためにある
    /// ——覆った字はそのまま溝に描かれるので、記号は原文のまま見えている。
    /// おかげで本文の位置が、その行を触っているかどうかで動かない。
    #[test]
    fn the_active_line_hangs_its_markup_in_the_gutter() {
        let source = "- 箇条書き\n- もう一行";
        let preview = PreviewDocument::from_source_with_active_line(source, Some(0));

        // 触っている行は原文のまま——`- `は本文の中にある。
        assert!(preview.text.starts_with("- 箇条書き"));
        assert_eq!(
            preview.markers()[0].expect("箱はある").ornament,
            Ornament::Markup
        );
        // 離れている行は、いつもの印。
        assert_eq!(
            preview.markers()[1].expect("箱はある").ornament,
            Ornament::Bullet
        );
    }

    /// Moving the caret away has to give the line its box back, which is the
    /// refresh path rather than a fresh build.
    #[test]
    fn moving_off_a_line_gives_its_marker_back() {
        let source = "- 箇条書き\n- もう一行";
        let mut preview = PreviewDocument::default();

        preview.refresh(source, Some(0));
        assert_eq!(
            preview.markers()[0].expect("箱はある").ornament,
            Ornament::Markup
        );

        preview.refresh(source, Some("- 箇条書き\n".len()));
        assert_eq!(
            preview.markers()[0].expect("箱はある").ornament,
            Ornament::Bullet
        );
        assert_eq!(
            preview.markers()[1].expect("箱はある").ornament,
            Ornament::Markup
        );
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

    /// 要件 7.8・要件 10: **ルビは別に数える。**両方持っているので、設定を
    /// 切り替えても数え直しは起きない——引き算が変わるだけである。
    #[test]
    fn ruby_is_counted_apart_from_the_body() {
        let stats = DocumentStats::from_source("彼は｜漢字《かんじ》を見た");

        // 本文に居残っているので`body`はルビ込み、`ruby`が`《かんじ》`の5字。
        assert_eq!(stats.ruby_characters, "《かんじ》".chars().count());
        assert_eq!(
            stats.body_characters - stats.ruby_characters,
            "彼は漢字を見た".chars().count()
        );
        // 縦線は本文にも数に残らない。
        assert_eq!(
            stats.source_characters,
            "彼は｜漢字《かんじ》を見た".chars().count()
        );
        // ルビの無い行は0。
        assert_eq!(
            DocumentStats::from_source("彼は漢字を見た").ruby_characters,
            0
        );
    }

    /// 1行ずつ数えるほう（`DocumentCounts`）と素朴なほうが、ルビについても
    /// 同じ答えを出す。**この2つが食い違うと、打鍵のたびに数が揺れる。**
    #[test]
    fn the_line_at_a_time_count_agrees_about_ruby() {
        let source = "｜漢字《かんじ》
傍点は《《ここ》》
人々《ひとびと》の話
";
        let mut counts = DocumentCounts::default();
        counts.refresh(source);

        assert_eq!(counts.stats(), DocumentStats::from_source(source));
        assert!(counts.stats().ruby_characters > 0);
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
                .map(|style| style.list_indent)
                .collect::<Vec<u8>>()
        };

        // One step for being an item, one more for each level it is under.
        assert_eq!(depths(by_two), vec![1, 2, 3]);
        assert_eq!(depths(by_four), vec![1, 2, 3]);
    }

    /// Coming back out closes the levels it passed, and a paragraph at the
    /// margin ends the list altogether. **A blank line does not** — an item may
    /// be followed by one and go on.
    #[test]
    fn coming_back_out_closes_the_levels_it_passed() {
        let styles = line_styles("- 一\n  - 二\n- 三\n\n  - 四\n本文\n  - 五\n");
        let depths = styles.iter().map(|s| s.list_indent).collect::<Vec<u8>>();

        // 一 二 三 (blank) 四 本文 五
        assert_eq!(depths[0], 1);
        assert_eq!(depths[1], 2);
        assert_eq!(depths[2], 1, "back at the margin");
        assert_eq!(depths[4], 2, "a blank line did not end the list");
        assert_eq!(depths[6], 1, "a paragraph at the margin did");
    }

    /// 要件 7.3.2: a paragraph written under an item belongs to it and is set
    /// in with it — **without becoming an item**. It has no marker and none is
    /// drawn for it; what it has is the same indent.
    #[test]
    fn a_line_that_continues_an_item_is_set_in_with_it() {
        let styles = line_styles("- 一\n  続きの行です\n  - 二\n    その続き\n");

        assert_eq!(styles[1].kind, LineKind::Body, "not an item");
        assert_eq!(styles[1].list_indent, 1, "set in with 一");
        assert_eq!(styles[2].kind, LineKind::Bullet);
        assert_eq!(styles[2].list_indent, 2);
        assert_eq!(styles[3].list_indent, 2, "set in with 二");
    }

    /// **The deepest level it is indented past**, so being indented further
    /// than the item's own text changes nothing: it lines up with the same
    /// item. And indented text under no list at all is still literal.
    #[test]
    fn a_continuation_belongs_to_the_last_item_that_begins_left_of_it() {
        let far = line_styles("- 一\n  - 二\n          深く下げた続き\n");
        let alone = line_styles("本文\n\n    字下げされただけの行\n");

        assert_eq!(far[2].list_indent, 2, "still 二's, however far in");
        assert_eq!(alone[2].list_indent, 0, "there is no item above it");
        assert_eq!(alone[2].kind, LineKind::Body);
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

    /// 要件 7.3.2: a link shows what it was given to show, and where it points
    /// is not on the page. **Both shapes**, because the internal one is kept
    /// and formatted even though going to one is out of scope (§14) — not
    /// breaking a notation and being able to decide where it points are
    /// different things.
    #[test]
    fn a_link_shows_what_it_was_given_to_show() {
        assert_eq!(visible_markdown_text("[説明](章/一.md)"), "説明");
        assert_eq!(visible_markdown_text("[[ノート名]]"), "ノート名");
        assert_eq!(visible_markdown_text("[[ノート名|表示名]]"), "表示名");
        assert_eq!(visible_markdown_text("見る[説明](x)前後"), "見る説明前後");
    }

    /// **A link that does not close is not a link**, which is the rule every
    /// other marker follows too: an unmatched bracket is a bracket.
    #[test]
    fn an_unclosed_link_is_left_as_written() {
        assert_eq!(visible_markdown_text("[説明"), "[説明");
        assert_eq!(visible_markdown_text("[説明](章"), "[説明](章");
        assert_eq!(visible_markdown_text("[[ノート名]"), "[[ノート名]");
        assert_eq!(visible_markdown_text("配列[0]と[1]"), "配列[0]と[1]");
    }

    /// **An image is not a link** (要件 7.3.3). It keeps its markup, because
    /// the image is shown as a name — and a link that read as ordinary text
    /// would hide the one thing that says it is a picture.
    #[test]
    fn an_image_keeps_its_markup() {
        assert_eq!(
            visible_markdown_text("![説明](画像.png)"),
            "![説明](画像.png)"
        );
        assert_eq!(visible_markdown_text("![[画像.png]]"), "![[画像.png]]");
    }

    /// The shown text is marked as a link, and emphasis inside it still counts:
    /// `[**太字**](x)` is a bold link.
    #[test]
    fn what_a_link_shows_is_marked_as_one() {
        let preview = PreviewDocument::from_source("[**太字**](x)\n");
        let marks = &preview.marks()[0];

        assert!(marks.iter().any(|span| span.marks.link && !span.marks.bold));
        assert!(marks.iter().any(|span| span.marks.bold && !span.marks.link));
    }

    /// 要件 7.3.2: a callout says what kind it is, and **the label is kept as a
    /// word**. The preview only ever deletes from the source, so a title cannot
    /// be put on the page that was not written on it — what it can do is take
    /// the brackets off and set the word the writer typed.
    #[test]
    fn a_callout_keeps_its_label_as_a_word() {
        assert_eq!(visible_markdown_text("> [!NOTE]"), "NOTE");
        assert_eq!(visible_markdown_text("> [!WARNING] 注意"), "WARNING 注意");
        // Only inside a quote: a callout is a quote that says what it is for.
        assert_eq!(visible_markdown_text("[!NOTE]"), "[!NOTE]");
        // A label is a word. A bracket later in a quoted sentence is not one.
        assert_eq!(visible_markdown_text("> [!] と書いた"), "[!] と書いた");
    }

    /// 要件 7.3.2: **the caret comes off and the brackets stay.** `1` on its own
    /// would be a number nobody could tell from a number, and `[1]` is what a
    /// footnote has looked like in print for as long as there have been
    /// footnotes.
    #[test]
    fn a_footnote_keeps_its_brackets_and_loses_its_caret() {
        assert_eq!(visible_markdown_text("本文[^1]です"), "本文[1]です");
        assert_eq!(visible_markdown_text("[^note-1]: 中身"), "[note-1]: 中身");
        assert_eq!(visible_markdown_text("[^あ]"), "[あ]");
        // Unclosed, or not a name: left as written, like every other marker.
        assert_eq!(visible_markdown_text("[^1"), "[^1");
        assert_eq!(visible_markdown_text("[^]"), "[^]");
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
