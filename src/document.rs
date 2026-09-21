use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::text_blocks::{
    Beside, CommentSyntax, Emphasis, LineKind, LineMarker, LineStyle, Marks, Ornament, Picture,
    Pictures, TextScale, is_table_row, table_alignments, warichu_cells_x10,
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
    /// 段落の前後の行から持ち越した記号（書き手の判断 2026-09-15）。**`active`と同じ理由で持つ**——
    /// 前の行で開いた`**`は、この行の字を1つも変えずに、この行の組み方を変える。
    context: LineContext,
    /// この行を1行だけで読んだとき、開いて閉じなかった記号。行の字が変わらなければ読み直さない。
    unclosed: Vec<(usize, &'static str)>,
}

impl PreviewLine {
    /// Build one line's text and mapping.
    ///
    /// The body of this is the whole-document loop it replaced, narrowed to one
    /// line. A break belongs to the line before it, so a line is self-contained:
    /// no grapheme cluster and no mapping step ever crosses from one to the
    /// next.
    fn build(
        source_line: &str,
        active: bool,
        style: LineStyle,
        has_break: bool,
        reading: Reading,
        context: LineContext,
        unclosed: Vec<(usize, &'static str)>,
    ) -> Self {
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
            // 書き手の求め 2026-09-15: **記号は隠さず、ただし太字として見える。**
            marks = active_marks(source_line, style, reading, &context);
            // 追加要件 2026-09-15: **編集中の画像の行は、絵を残して記法をその下に見せる。**箱は行頭の`!`
            // だけにかぶせ、行の長さいっぱいの送りにする（`size_images`）——後ろの記法は次の行へ折り返す。
            if style.kind == LineKind::Image {
                marks = image_box(source_line, 1).into_iter().collect();
            }
        } else {
            push_visible_line_in(
                source_line,
                style,
                &mut visible,
                &mut marks,
                reading,
                &context,
            );
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
        let hidden_wiki = if active || style.kind.is_code() || !source.contains("[[") {
            Vec::new()
        } else {
            hidden_wiki_display_ranges(&source)
        };

        for (preview_offset, character) in visible.char_indices() {
            // The preview only ever deletes from the source, so the next
            // preview character is almost always sitting at the cursor already.
            let source_offset =
                next_visible_source_offset(&source, source_cursor, character, &hidden_wiki);
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
            context,
            unclosed,
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
    /// 前回どちらで組んだか（要件 E9）。
    ///
    /// **旗が変われば、取っておいた行は全部使えない。**行を取っておく条件は
    /// 「本文と組み方が同じなら数え直さない」で、記法を読むかどうかは
    /// **その行が何の字でできているか**を変える——`｜漢字《かんじ》`は
    /// 記法として6字、字として11字である。
    reading: Reading,
    /// 追加要件 2026-09-15: 画像の行に描く絵（鍵 → 画素）。`size_images`が入れる。
    pictures: Pictures,
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
        Self::from_source_as(source, active_line_start, Reading::all())
    }

    /// 同じことを、**記法を読むかどうかを言われて**する（要件 E9）。
    #[cfg(test)]
    pub fn from_source_as(
        source: &str,
        active_line_start: Option<usize>,
        reading: Reading,
    ) -> Self {
        let mut preview = Self::default();
        preview.refresh(source, active_line_start, reading);
        preview
    }

    /// 記法の読み方が前回と違えば、取っておいた行を捨てる（要件 E9）。
    ///
    /// **本文でも組み方でもない3つ目の理由。**行を取っておく条件（`matches`）は
    /// 本文と組み方と活性行を見ているので、旗が変わったことには気づけない
    /// ——気づけないまま使えば、切り替えても画面が変わらない。
    fn forget_if_read_differently(&mut self, reading: Reading) {
        if self.reading != reading {
            self.lines.clear();
            self.reading = reading;
        }
    }

    /// Bring the preview up to date, rebuilding only the lines that changed.
    ///
    /// A line is rebuilt when its text changed, and when it became or stopped
    /// being the active line — moving the caret to another line changes the form
    /// of exactly two lines, and leaves every other line's table alone.
    pub fn refresh(&mut self, source: &str, active_line_start: Option<usize>, reading: Reading) {
        self.forget_if_read_differently(reading);
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
        //
        // 書き手の決定 2026-09-11: **読むと決めた記号だけが印**——切った記号の行は
        // プレビューでも本文の1行で、記号は字として出る。
        let styles = line_styles_reading(source, reading);
        let style_at = |index: usize| styles.get(index).copied().unwrap_or_default();
        let last = lines.len() - 1;
        let same_text = |kept: &PreviewLine, index: usize, line: &str| {
            let has_break = index != last;
            kept.source.len() == line.len() + usize::from(has_break)
                && kept.source.starts_with(line)
        };
        // 書き手の判断 2026-09-15: 段落の中で改行をまたぐ記号。1行だけで読んだ閉じない記号は、
        // 字の変わらない行なら取っておいたものを使う（前後から揃っているところまで）。
        let text_head = self
            .lines
            .iter()
            .enumerate()
            .zip(&lines)
            .take_while(|((index, kept), line)| same_text(kept, *index, line))
            .count();
        let text_rest = lines.len().min(self.lines.len()) - text_head;
        let text_tail = (0..text_rest)
            .take_while(|back| {
                let index = lines.len() - 1 - back;
                same_text(
                    &self.lines[self.lines.len() - 1 - back],
                    index,
                    lines[index],
                )
            })
            .count();
        let unclosed = (0..lines.len())
            .map(|index| {
                if index < text_head {
                    self.lines[index].unclosed.clone()
                } else if index >= lines.len() - text_tail {
                    self.lines[self.lines.len() - (lines.len() - index)]
                        .unclosed
                        .clone()
                } else {
                    standalone_unclosed(lines[index], reading)
                }
            })
            .collect::<Vec<_>>();
        let contexts = paragraph_contexts(&lines, style_at, &unclosed, reading);
        let matches = |kept: &PreviewLine, index: usize, line: &str| {
            let active = active_index == Some(index);
            kept.active == active
                && kept.style == style_at(index)
                && kept.context == contexts[index]
                && same_text(kept, index, line)
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
                PreviewLine::build(
                    lines[index],
                    active,
                    style_at(index),
                    index != last,
                    reading,
                    contexts[index].clone(),
                    unclosed[index].clone(),
                )
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

    /// 画像の箱に大きさを入れる（追加要件 2026-09-15）。`size(image)`が読んだ絵と描く大きさ（幅, 高さ）を返す。
    ///
    /// **返さない（読めない）絵は箱を外す**——字は消していないので、記法がそのまま見える。編集中の行は
    /// `source_shown`（記法を見せたまま、その前に絵）。行を組み直すたびに0に戻るので、呼ぶ側は
    /// 組み直しのあと毎回呼ぶ。
    pub fn size_images(
        &mut self,
        mut size: impl FnMut(ImageRef<'_>) -> Option<(std::sync::Arc<Picture>, (u32, u32))>,
    ) {
        let mut pictures = std::collections::HashMap::new();
        for (index, line) in self.lines.iter().enumerate() {
            if !line
                .marks
                .iter()
                .any(|mark| matches!(mark.ornament, Some(Ornament::Image { .. })))
            {
                continue;
            }
            self.marks[index] = line
                .marks
                .iter()
                .filter_map(|mark| match mark.ornament {
                    Some(Ornament::Image { key, .. }) => image_of_line(&line.source)
                        .and_then(&mut size)
                        .map(|(picture, (width, height))| {
                            pictures.insert(key, picture);
                            Emphasis {
                                ornament: Some(Ornament::Image {
                                    key,
                                    width,
                                    height,
                                    source_shown: line.active,
                                }),
                                ..*mark
                            }
                        }),
                    _ => Some(*mark),
                })
                .collect();
        }
        // 同じ絵のままなら差し替えない（空のままの文書で`Arc`を作り続けない）。
        if *self.pictures != pictures {
            self.pictures = std::sync::Arc::new(pictures);
        }
    }

    /// 画像の行に描く絵（追加要件 2026-09-15）。
    pub fn pictures(&self) -> &Pictures {
        &self.pictures
    }

    /// What is marked inside each line (要件 7.3.2).
    pub fn marks(&self) -> &[Vec<Emphasis>] {
        &self.marks
    }

    /// Apply verified source destinations to both active paths and inactive labels.
    pub fn set_invalid_link_targets(&mut self, invalid: &[Range<usize>]) {
        for (index, (line, marks)) in self.lines.iter().zip(self.marks.iter_mut()).enumerate() {
            let offset = self.source_starts[index];
            let tokens = line_link_ranges(&line.source);
            for mark in marks.iter_mut().filter(|mark| mark.marks.link) {
                let source_at = line
                    .source_byte
                    .get(mark.utf16_start as usize)
                    .copied()
                    .unwrap_or(0) as usize;
                mark.marks.unresolved_link = tokens.iter().any(|(token, target, _)| {
                    token.contains(&source_at)
                        && invalid
                            .iter()
                            .any(|bad| *bad == (offset + target.start..offset + target.end))
                });
            }
        }
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
    /// 段落の前後から持ち越した記号と、1行だけで読んだときの閉じない記号（`PreviewLine`と同じ）。
    context: LineContext,
    unclosed: Vec<(usize, &'static str)>,
}

impl LineCounts {
    fn of(
        line: &str,
        style: LineStyle,
        reading: Reading,
        context: LineContext,
        unclosed: Vec<(usize, &'static str)>,
    ) -> Self {
        let mut visible = String::with_capacity(line.len());
        // The counts are about how much text there is, not how it is set.
        // **印は要る**（要件 7.8）：ルビの読みは本文に居残るので、どこからどこ
        // までが読みかを言えるのは印だけである。
        let mut marks = Vec::new();
        // 書き手の判断 2026-09-15: 行をまたぐ太字の記号は、画面と同じく本文に数えない。
        push_visible_line_in(line, style, &mut visible, &mut marks, reading, &context);
        Self {
            source_graphemes: line.graphemes(true).count(),
            body_graphemes: visible.graphemes(true).count(),
            ruby_graphemes: ruby_graphemes(&visible, &marks),
            characters: line.chars().count(),
            style,
            text: line.to_owned(),
            context,
            unclosed,
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
    /// 前回どちらで組んだか（要件 E9）。
    ///
    /// **旗が変われば、取っておいた行は全部使えない。**行を取っておく条件は
    /// 「本文と組み方が同じなら数え直さない」で、記法を読むかどうかは
    /// **その行が何の字でできているか**を変える——`｜漢字《かんじ》`は
    /// 記法として6字、字として11字である。
    reading: Reading,
}

impl DocumentCounts {
    /// Bring the counts up to date with `source`, recounting only what changed.
    pub fn refresh(&mut self, source: &str, reading: Reading) {
        // 要件 E9: **旗が変われば取っておいた行は全部使えない**（`PreviewDocument`の
        // ほうに同じ一文がある）。
        if self.reading != reading {
            self.lines.clear();
            self.reading = reading;
        }
        let lines = source.split('\n').collect::<Vec<&str>>();
        // 要件 7.3.2: how every line is set, which is where a fence reaches
        // past its own line. A line whose text did not change may still be
        // counted differently because a fence opened above it, so the flag is
        // part of what makes a kept line still usable.
        let styles = line_styles_reading(source, reading);
        let style_at = |index: usize| styles.get(index).copied().unwrap_or_default();
        // 書き手の判断 2026-09-15: 段落の中で改行をまたぐ記号（`PreviewDocument::refresh`と同じ）。
        let same_text = |kept: &LineCounts, line: &str| kept.text == *line;
        let text_head = self
            .lines
            .iter()
            .zip(&lines)
            .take_while(|(kept, line)| same_text(kept, line))
            .count();
        let text_rest = lines.len().min(self.lines.len()) - text_head;
        let text_tail = (0..text_rest)
            .take_while(|back| {
                same_text(
                    &self.lines[self.lines.len() - 1 - back],
                    lines[lines.len() - 1 - back],
                )
            })
            .count();
        let unclosed = (0..lines.len())
            .map(|index| {
                if index < text_head {
                    self.lines[index].unclosed.clone()
                } else if index >= lines.len() - text_tail {
                    self.lines[self.lines.len() - (lines.len() - index)]
                        .unclosed
                        .clone()
                } else {
                    standalone_unclosed(lines[index], reading)
                }
            })
            .collect::<Vec<_>>();
        let contexts = paragraph_contexts(&lines, style_at, &unclosed, reading);
        let matches = |kept: &LineCounts, index: usize, line: &str| {
            kept.text == *line && kept.style == style_at(index) && kept.context == contexts[index]
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
            .map(|index| {
                LineCounts::of(
                    lines[index],
                    style_at(index),
                    reading,
                    contexts[index].clone(),
                    unclosed[index].clone(),
                )
            })
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
///
/// **頭がちょうど行末なら、その行も入らない**（書き手の報告 2026-09-11：「選択範囲
/// より一行前から箇条書きになります」）。同じ話の裏側で、**行末に立てた頭では、
/// その行の字は1つも選ばれていない**。行の頭のすぐ近くを押すと、当たり判定は
/// **前の行の末尾**を返す（それが上の行を指す普通の振る舞いである）——そこを
/// 「その行も選ばれている」と読むと、書き手が見ていない行に印が付く。
pub fn selected_lines(source: &str, start: usize, end: usize) -> (usize, usize) {
    let (from, to) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    // 改行そのものの上に頭があれば、次の行から。**バイトで見る**——`from`は字の
    // 切れ目とは限らず、`&source[from..]`はそこで落ちる（改行はASCIIなので、
    // バイトが合えば改行そのものである）。
    let from = if to > from && source.as_bytes().get(from) == Some(&b'\n') {
        from + 1
    } else {
        from
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
/// **選び直す範囲は、行の字までで、後ろの改行は含めない**（書き手の報告
/// 2026-09-11：「キャレットが行末から次の行の先頭に移動します」）。改行は行と行の
/// あいだにあるもので、**それを選びに入れるとカーソルは次の行の頭へ出る**——動かした
/// のは行であって、書き手を次の行へ連れて行ったのではない。
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
            let chosen = (above_start, above_start + body(moved).len());
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
            let chosen = (head, head + body(moved).len());
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
                (start, start + body(moved).len())
            } else {
                (second, second + body(moved).len())
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
    // 書き手の決定 2026-09-12:「改行すると、半角スペースが行頭にはいっている
    // ようです」——**本文の行では、空白だけの字下げを写さない。**
    //
    // 行頭の空白はいちど出来ると、ここが次の行へ写し、その行がまた次へ写す。
    // **書き手が字下げとして打った覚えの無い1つ**（原因は別に追っている）が、
    // それで文書じゅうへ広がり、しかも**行頭に空白のある行は見出しにならない**
    // （`heading_level`）ので「#を打っても見出しにならない」まで連れてくる。
    //
    // **箇条書き・引用・項目の続きの段落はこれまでどおり継ぐ**——そこでの
    // 字下げは書き手が打ったものではなく、項目の形そのものだからである。
    let kept = if marker > 0 || quote > 0 || style.list_indent > 0 {
        &line[quote..quote + indent]
    } else {
        ""
    };
    // **中身の無い項目は、そこで終わる**（E3：「空の項目でEnterを押すと継続を
    // 終える」）。印を持たない行はここへ来ない——字下げだけの行でEnterが何も
    // しないと、効かない鍵に見える。
    // **項目の続きの段落でEnterを押したら、次の項目が出る**（書き手の決定
    // 2026-09-10：「Shift-ENTERの間はその段落が続いている感じです。つまり、
    // Shift+Enter、入力してShift+Enterとつづけて、Enterすると、次の箇条書きが
    // 始まる感じ」）。**中身があってもなくても同じ**——`Shift+Enter`が「まだ
    // この項目」と言う鍵で、`Enter`はいつでも「次の項目へ」である。
    //
    // 空いた行はそのまま残るので、項目と項目のあいだが一行空く。
    if marker == 0
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

/// 選んだ行を箇条書きにする、やめる（E10）。
///
/// **もう一度頼めば外れる**（書き手の選択 2026-09-10）。選んだ行がそろって
/// 頼まれた印を持っていれば、それは「もう箇条書きである」ということで、そこで
/// 押された同じ鍵が言っているのは「やめる」である。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListEdit {
    /// `Ctrl+Shift+8`——印の箇条書き。印の字は書き手が選ぶ（既定は`-`）。
    Bullet,
    /// `Ctrl+Shift+7`——番号の箇条書き。**選んだ範囲の中で1から**数える。
    Ordered,
    /// `Renumber`——**この行の番号から、下を数え直す**（E10の②）。印は付け替え
    /// ない。開始番号を変えるのは、先頭を手で打ち直してこれを頼むことである
    /// （書き手の選択 2026-09-11：数を訊く画面は作らない）。
    Renumber,
}

/// 選んだ行に箇条書きの印を付ける、外す（E10）。
///
/// **行の見方は`line_styles`のもの**（`styles`で受け取る）——画面が見出しとして
/// 組んでいる行に印を付けたら、画面と操作が別々のことを言う（E3の③と同じ）。
///
/// 触るのは**本文の行と、既に箇条書きの行だけ**（[`takes_marker`]）。
///
/// **番号は選んだ範囲の中で1から**、字下げの桁ごとに数える（入れ子は入れ子で
/// 1から）。範囲の外にいる番号は書き換えない——打っていないところは動かさない
/// （E3の③で書き手が選んだこと）。開始番号を変えるのは`Renumber`の仕事である。
///
/// **`None`は「何も変わらない」。**触れる行が1つも無ければ、呼ぶ側がそう言える。
pub fn list_edit(
    source: &str,
    styles: &[LineStyle],
    from: usize,
    to: usize,
    what: ListEdit,
    bullet: char,
) -> Option<(Range<usize>, String, (usize, usize))> {
    match what {
        ListEdit::Bullet => set_markers(source, styles, from, to, LineKind::Bullet, bullet),
        ListEdit::Ordered => set_markers(source, styles, from, to, LineKind::Ordered, bullet),
        ListEdit::Renumber => renumber_below(source, styles, from, to),
    }
}

/// 選んだ行の印を、頼まれた種類にそろえる／外す（[`list_edit`]）。
fn set_markers(
    source: &str,
    styles: &[LineStyle],
    from: usize,
    to: usize,
    wanted: LineKind,
    bullet: char,
) -> Option<(Range<usize>, String, (usize, usize))> {
    let (start, end) = selected_lines(source, from, to);
    let first = source[..start].matches('\n').count();
    let line_style = |offset: usize| styles.get(first + offset).copied().unwrap_or_default();
    // **外すのは、全部がもうその印のときだけ。**1行でも印の無い行が混じって
    // いれば、書き手が頼んでいるのは「そろえる」ほうである。
    let mut touched = false;
    let mut all_wanted = true;
    for (offset, line) in source[start..end].split_inclusive('\n').enumerate() {
        let style = line_style(offset);
        if !takes_marker(line, style) {
            continue;
        }
        touched = true;
        // **頼まれた記号になっていて、はじめて「もうその印である」**（書き手の決定
        // 2026-09-11：記号ごとに画面の印が違うので、記号は選ぶもの）。`*`の行の上で
        // 「`-`の箇条書き」を頼まれたら、**その記号にそろえる**のが頼まれたこと
        // ——`Ctrl+Shift+7`の「そろえる→外れる」と同じ階段で、次の一押しが外す。
        all_wanted &= style.kind == wanted && (wanted != LineKind::Bullet || wears(line, bullet));
    }
    if !touched {
        return None;
    }
    // **番号の連なりの上では、まずそろえる**（書き手の求め 2026-09-11）。書き手は
    // 「開始数字を変える」ためにこの鍵を探した——**`Ctrl+Shift+7`は「そろえる→
    // 外れる」の階段**である。振り直しても何も変わらなければ、次の一押しが外す。
    //
    // **印の鍵（`Ctrl+Shift+8`）はこの道を通らない。**印に数え直すところは無い。
    if all_wanted && wanted == LineKind::Ordered {
        if let Some(evened) = renumber_below(source, styles, from, to) {
            return Some(evened);
        }
    }
    let mut text = String::with_capacity(end - start);
    let kept = [from.clamp(start, end), to.clamp(start, end)];
    let mut shifts = [0isize; 2];
    // 字下げの桁ごとの数。**内側へ入れば積み、外側へ戻れば捨てる**——捨てたぶんは
    // もう一度入ったときに1から始まる（`renumber_around`と同じ数え方）。
    let mut counts: Vec<(usize, u64)> = Vec::new();
    let mut at = start;
    for (offset, line) in source[start..end].split_inclusive('\n').enumerate() {
        let style = line_style(offset);
        if !takes_marker(line, style) {
            text.push_str(line);
            at += line.len();
            continue;
        }
        let quote = line.len() - quote_content(line).len();
        let content = &line[quote..];
        let (columns, body) = leading_indent(content);
        let indent = content.len() - body.len();
        let marker = marker_len(body, style.kind).unwrap_or(0) as usize;
        while counts.last().is_some_and(|(held, _)| *held > columns) {
            counts.pop();
        }
        match counts.last_mut() {
            Some((held, count)) if *held == columns => *count += 1,
            _ => counts.push((columns, 1)),
        }
        let head = if all_wanted {
            String::new()
        } else if wanted == LineKind::Ordered {
            let number = counts.last().map_or(1, |(_, count)| *count);
            format!("{number}. ")
        } else {
            format!("{bullet} ")
        };
        // 足した印の後ろにいた位置は、その印のぶんだけ後ろへ出る。
        let delta = head.len() as isize - marker as isize;
        note_shift(&kept, &mut shifts, at + quote + indent + marker, delta);
        text.push_str(&line[..quote + indent]);
        text.push_str(&head);
        text.push_str(&body[marker..]);
        at += line.len();
    }
    if text == source[start..end] {
        return None;
    }
    let chosen = chosen_range(settle(kept, shifts, start, start, &text));
    Some((start..end, text, chosen))
}

/// この行の番号から、下の項目を数え直す（E10の②）。
///
/// **開始番号を変えるのは、先頭を打ち直してこれを頼むこと**（書き手の選択
/// 2026-09-11）。`5.`と打ち直した行にカーソルを置いてこれを頼めば、その下が
/// `6. 7. …`になる——数を訊く画面は無い。
///
/// **数え直すのはカーソルの行から下だけ。**上は書き手が打ったところであり、
/// 打っていないところは動かさない（E3の③と同じ）。
///
/// **連なりが切れるところで止まる。**箇条書きに属さない行——空行も本文の段落も
/// ——がリストの終わりで、そこを越えて遠くのリストを数えない（要件 7.3.2）。
///
/// **深さごとに数える。**内側の連なりはその連なりの先頭の番号から始まり、外側へ
/// 戻れば外側の続きになる（`renumber_around`と同じ数え方）。番号を持たない項目も
/// 1つと数え、書き換えはしない。
fn renumber_below(
    source: &str,
    styles: &[LineStyle],
    from: usize,
    to: usize,
) -> Option<(Range<usize>, String, (usize, usize))> {
    let (start, _) = selected_lines(source, from, to);
    let first = source[..start].matches('\n').count();
    let style_of = |index: usize| styles.get(index).copied().unwrap_or_default();
    // **数え始めるのは項目の行から。**空行の上でこれを頼まれても、どの番号から
    // 続けるのかを言っていない。
    if style_of(first).list_indent == 0 {
        return None;
    }
    let mut text = String::new();
    let kept = [from.max(start), to.max(start)];
    let mut shifts = [0isize; 2];
    // 深さごとの**次の番号と、その連なりの種類**。内へ入れば積み、外へ戻れば捨てる。
    let mut counts: Vec<(u8, char, u64)> = Vec::new();
    // **この連なりの深さと種類**（CommonMark §5.3）。ここが変わったら、そこから
    // 先はもう別のリストである。
    let base = style_of(first).list_indent;
    let mut at = start;
    let mut end = start;
    // **最後の項目までが、数え直した範囲。**連なりの後ろにある空行は数えたものの
    // 外である——選ばれたまま残るのは、番号を持てる行だけにする。
    let mut settled = 0;
    for (offset, line) in source[start..].split_inclusive('\n').enumerate() {
        let style = style_of(first + offset);
        if !still_in_list(line, style) {
            break;
        }
        // **数えるのは項目だけ。**あいだの空行も、項目の続きの段落も、番号を
        // 持たない——ここで深さを数えると、**空行が深さ0として数を捨てる**
        // （書き手の報告 2026-09-11：空行の下の項目が自分の番号から数え直され、
        // 何も変わらなかった）。
        if !style.kind.is_list() {
            text.push_str(line);
            at += line.len();
            continue;
        }
        let depth = style.list_indent;
        let kind = item_type(line, style).unwrap_or('-');
        let quote = line.len() - quote_content(line).len();
        let content = &line[quote..];
        let (_, body) = leading_indent(content);
        let indent = content.len() - body.len();
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        let written = || body[..digits].parse::<u64>().unwrap_or(1);
        while counts.last().is_some_and(|(held, ..)| *held > depth) {
            counts.pop();
        }
        // **記号が変われば、そこから別のリスト**（CommonMark §5.3、書き手の決定
        // 2026-09-11）。この連なりの深さで種類が変わったら、そこで止める
        // ——「連なりが切れるところで止まる」に、切れ目が1つ増えたのである。
        if depth == base
            && counts
                .iter()
                .any(|(held, was, _)| *held == depth && *was != kind)
        {
            break;
        }
        let number = match counts.last_mut() {
            Some((held, was, next)) if *held == depth && *was == kind => {
                let number = *next;
                *next += 1;
                number
            }
            // **新しい深さ、あるいは別の種類は、その先頭が書いている番号から。**
            // 打ち直した数がそのまま開始番号である。
            _ => {
                while counts.last().is_some_and(|(held, ..)| *held == depth) {
                    counts.pop();
                }
                let start = if style.kind == LineKind::Ordered {
                    written()
                } else {
                    1
                };
                counts.push((depth, kind, start + 1));
                start
            }
        };
        at += line.len();
        end = at;
        if style.kind != LineKind::Ordered {
            text.push_str(line);
            settled = text.len();
            continue;
        }
        let number = number.to_string();
        // 桁が変われば、その行の後ろにいる位置もずれる。
        let delta = number.len() as isize - digits as isize;
        let head = at - line.len() + quote + indent + digits;
        note_shift(&kept, &mut shifts, head, delta);
        text.push_str(&line[..quote + indent]);
        text.push_str(&number);
        text.push_str(&body[digits..]);
        settled = text.len();
    }
    text.truncate(settled);
    if text == source[start..end] {
        return None;
    }
    // **数え直した連なりが選ばれたまま。**どこまで数え直したかが画面に出るのと、
    // **もう一度押せば外れる**のが同じ一手になる（書き手の求め 2026-09-11の階段）
    // ——外すのは選ばれている行なので、選ばれていなければ1行しか外れない。
    //
    // **最後の改行は入れない**（[`line_edit`]と同じ理由）——入れるとカーソルが
    // 連なりの次の行の頭へ出る。
    let without_break = text.strip_suffix('\n').unwrap_or(&text).len();
    let chosen = (start, start + without_break);
    Some((start..end, text, chosen))
}

/// 挿入メニューのひな形（RFN01-38）。**本文へ記法を置く**もので、行の体裁を
/// 変える[`LineEdit`]／[`ListEdit`]とは別の群である。
///
/// どれも「選んだ字を包む」形なので、返すのは[`line_edit`]と同じ
/// 「置き換える範囲・そこへ入る字・そのあと選び直す範囲」。**選び直すのは
/// キャレット1つ**——次に打つのは読みかリンク先であって、囲んだ字ではない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertEdit {
    /// `[表示名](リンク先)`。**未選択はリンク先から書く**（書き手の選択
    /// 2026-09-21）——`](`の後ろに立つので、既存のリンク先補完がそのまま出る。
    MarkdownLink,
    /// `[[リンク先]]`。選んだ字をリンク先にする（書き手の選択 2026-09-21）。
    WikiLink,
    /// `[[リンク先|表示名]]`。選んだ字を表示名にする（同上）。
    WikiLinkAlias,
    /// `｜親文字《よみ》`。選んだ字を親文字にし、読みを書く所へ立つ。**全角の
    /// 縦線で書く**（書き手の選択 2026-09-21）——半角と縦線省略は読めるままだが、
    /// 書き出す形は1つにする。
    Ruby,
}

/// 挿入メニューのひな形を組む（RFN01-38）。
///
/// **`from..to`は書き手が選んだ範囲**（同じ所ならキャレット1つ）。選んだ向きに
/// よらず同じ結果にするので、頭と尻を入れ替えてから読む。
///
/// **形はここで決め、本文へ入れるのは[`crate::apply_span_edit`]に任せる**——文字数・
/// 読み取り専用・Viewerの検査と、取り消し1回の区切りは、打鍵と同じ道に1つだけ
/// ある（要件 7.1）。開きと閉じは**一度に置く**：閉じを先に用意しておくのは、
/// リンクの補完が後ろの閉じ括弧を見て重ねて書かないためである
/// （`link_completion.rs`）。
///
/// **`None`は「入れられない」。**範囲が本文の外を指すか、字の切れ目に乗って
/// いなければ入れない——数え違いのまま`&str`を切ると落ちる。
pub fn insert_edit(
    source: &str,
    from: usize,
    to: usize,
    what: InsertEdit,
) -> Option<(Range<usize>, String, (usize, usize))> {
    let (start, end) = if from <= to { (from, to) } else { (to, from) };
    if end > source.len() || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return None;
    }
    let picked = &source[start..end];
    let (text, after) = match what {
        InsertEdit::MarkdownLink => (
            format!("[{picked}]()"),
            "[".len() + picked.len() + "](".len(),
        ),
        InsertEdit::WikiLink => (format!("[[{picked}]]"), "[[".len() + picked.len()),
        InsertEdit::WikiLinkAlias => (format!("[[|{picked}]]"), "[[".len()),
        InsertEdit::Ruby => (format!("｜{picked}《》"), "｜".len() + picked.len()),
    };
    // **選び直す位置は新しい本文のものである**（[`line_edit`]と同じ）——置き換えた
    // 範囲の頭から数えるので、`apply_span_edit`がそのまま使える。
    let caret = start + after;
    Some((start..end, text, (caret, caret)))
}

/// この項目の**種類**——CommonMark §5.3 の「同じ種類の項目の並び」（書き手の決定
/// 2026-09-11：「記号を変えるとそこから別のリストが始まる。これはそうするべき」）。
///
/// **箇条書きなら印の字、番号なら区切りの字**（`.`か`)`）。`- あ`と`* い`は別の
/// リストであり、`1. あ`と`1) い`も別のリストである——他のツールはそこで連なりを
/// 切り、番号も数え直す。
///
/// **印を持たない行は`None`**（空行も、項目の続きの段落も、種類を持たない）。
fn item_type(line: &str, style: LineStyle) -> Option<char> {
    if !style.kind.is_list() {
        return None;
    }
    let content = quote_content(line);
    let (_, body) = leading_indent(content);
    if style.kind == LineKind::Ordered {
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        return body[digits..].chars().next();
    }
    body.chars().next()
}

/// この行はまだ箇条書きの連なりの中か（[`renumber_below`]・[`renumber_around`]）。
///
/// **空行は連なりを切らない**（書き手の報告 2026-09-11）。項目のあいだが空いていても
/// Markdownでは1つのリスト（緩いリスト）であり、`testdata/01_行属性.md`にも
/// 「空行をはさんでも深さは続きます」と書いてある。**後から箇条書きにする道
/// （[`set_markers`]）も空行を跨いで1から数える**ので、ここで切ると**同じ文書に
/// ついて2つの数え方**ができる——番号を振った直後に数え直しが「何も変わらない」と
/// 言うのがそれだった。
///
/// **切るのは、字の入った行がリストの外にいるとき。**そこで止めないと、遠く上の
/// リストの番号を継いでしまう。
fn still_in_list(line: &str, style: LineStyle) -> bool {
    style.list_indent > 0 || line.trim().is_empty()
}

/// 編集の前の位置に、1行ぶんのずれを積む（[`set_markers`]・[`renumber_below`]）。
///
/// **比べるのは編集の前の座標で、動かすのは最後に一度**（`renumber_around`と同じ
/// 形）。**1行ごとに動かしながら次の行の閾値と比べてはいけない**——積んだぶんだけ
/// 位置が前へ出るので、**行末に立っていた端が次の行の印の後ろまで送られる**
/// （書き手の報告 2026-09-11、Panic：送られた先が字の途中だとそこで落ちる）。
fn note_shift(positions: &[usize], shifts: &mut [isize; 2], at: usize, delta: isize) {
    let taken = delta.unsigned_abs();
    for (position, shift) in positions.iter().zip(shifts.iter_mut()) {
        if *position >= at {
            *shift += delta;
            continue;
        }
        // **消したぶんの中にいた位置は、その頭に集まる。**外した字下げの中に
        // 立っていたカーソルが、残った字下げの中に立ったままになることはない。
        if delta < 0 && *position > at - taken {
            *shift -= (*position - (at - taken)) as isize;
        }
    }
}

/// 積んだずれを当てて、書き換えたあとの位置にする（[`set_markers`]・
/// [`renumber_below`]・[`shift_indent`]）。
///
/// `at`は`text`が文書の中で始まる位置、`held`は**書き換えた範囲の頭**である。
///
/// **選んでいる範囲の頭は動かない。**`held`はその範囲の外側の境目なので、そこに
/// 立っていた端は足した印や字下げの**前**にいる——動かすと**先頭の行だけ印が
/// 選択から落ちる**（書き手の報告 2026-09-11：「先頭行の先頭文字(Numberなら1)だけ
/// 選択範囲から落ちます」）。
///
/// **カーソル1つだけなら動く。**そこには「範囲の頭」は無く、あるのは書き手が立って
/// いた字である——`Tab`で行頭のカーソルが字下げの後ろへ出るのは、書き手が見て
/// 通したことである（E3の④）。
///
/// **字の切れ目へ丸める。**位置はバイトで数えるので、丸めないと字の途中に立ちうる
/// ——そこを頭にした`&str`の借り方はどこで落ちてもおかしくない（6.7）。
fn settle(kept: [usize; 2], shifts: [isize; 2], at: usize, held: usize, text: &str) -> [usize; 2] {
    let selecting = kept[0] != kept[1];
    let mut moved = [at; 2];
    for ((position, shift), out) in kept.iter().zip(shifts).zip(moved.iter_mut()) {
        let moved = if selecting && *position == held {
            held
        } else {
            position.saturating_add_signed(shift)
        };
        let inside = moved.clamp(at, at + text.len()) - at;
        *out = at + crate::floor_char_boundary(text, inside);
    }
    moved
}

/// 2つの位置を、選び直す範囲にする（[`settle`]）。
fn chosen_range(moved: [usize; 2]) -> (usize, usize) {
    (moved[0].min(moved[1]), moved[0].max(moved[1]))
}

/// この行が、その記号で書かれているか（[`set_markers`]）。
///
/// **読むほうは`list_kind`が決める。**こちらは「もうその記号になっているか」だけを
/// 答える——頼まれた記号にそろえるのか、外すのかを分ける問いである。
fn wears(line: &str, bullet: char) -> bool {
    let content = quote_content(line);
    let (_, body) = leading_indent(content);
    body.starts_with(bullet)
}

/// 印を付け替えられる行（[`list_edit`]）。
///
/// **本文の行と、既に箇条書きの行だけ。**見出し・区切り線・表・コードは行その
/// ものが別の意味を持っていて、行頭に印を足せばその意味が壊れる——`# 章`は
/// `- # 章`になれば見出しでなくなり、コードは書いてあるとおりでなくなる。
///
/// **空の行は触らない**（`shift_indent`が空の行を下げないのと同じ理由）。中身の
/// 無い項目が増えるだけで、書き手はそれを消すことになる。
///
/// **タスクも触らない。**`- [ ]`は既に箇条書きで、印を付け替えれば箱が消える
/// ——済んだかどうかは書き手が付けた印であって、並べ方を変えた拍子に捨てて
/// よいものではない（E3の③の「済んだ印を写さない」と同じ根）。
fn takes_marker(line: &str, style: LineStyle) -> bool {
    if style.heading_level > 0 || style.kind.is_code() || style.kind.is_table() {
        return false;
    }
    if matches!(style.kind, LineKind::Rule | LineKind::Task { .. }) {
        return false;
    }
    !quote_content(line).trim().is_empty()
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
pub fn shift_indent(
    source: &str,
    from: usize,
    to: usize,
    deeper: bool,
    marks: BulletMarks,
) -> Option<Indented> {
    let (start, end) = selected_lines(source, from, to);
    let mut text = String::with_capacity(source.len());
    text.push_str(&source[..start]);
    let kept = [from, to];
    let mut shifts = [0isize; 2];
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
                note_shift(&kept, &mut shifts, body, INDENT_STEP.len() as isize);
                changed = true;
            }
        } else {
            let taken = outdent_width(rest);
            if taken > 0 {
                changed = true;
                note_shift(&kept, &mut shifts, body + taken, -(taken as isize));
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
    // **動かすのは最後に一度**（[`note_shift`]）。ここまでは編集の前の座標で
    // 数えてあるので、行末に立っていた端が次の行の字下げまで送られない。
    let mut moved = settle(kept, shifts, 0, start, &text);
    let renumbered = renumber_around(&text, start, &mut moved, marks);
    Some(Indented {
        text: renumbered,
        chosen: chosen_range(moved),
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
fn renumber_around(source: &str, at: usize, positions: &mut [usize], marks: BulletMarks) -> String {
    // **数え方は読み方と同じ**——印として読まない記号の行は項目ではないので、
    // 数にも入らない（同じ文書に2つの数え方があってはならない）。
    let styles = line_styles_as(source, marks);
    let lines: Vec<&str> = source.split('\n').collect();
    let here = source[..at.min(source.len())].matches('\n').count();
    // **空行は連なりを切らない**（[`still_in_list`]）。数え方は1つである。
    let inside = |index: usize| {
        let line = lines.get(index).copied().unwrap_or_default();
        still_in_list(line, styles.get(index).copied().unwrap_or_default())
    };
    if styles.get(here).copied().unwrap_or_default().list_indent == 0 {
        return source.to_owned();
    }
    let first = (0..=here).rev().take_while(|index| inside(*index)).last();
    let last = (here..lines.len())
        .take_while(|index| inside(*index))
        .last();
    let (Some(first), Some(last)) = (first, last) else {
        return source.to_owned();
    };
    // 深さごとの数と、その連なりの種類。**内側へ入れば積み、外側へ戻れば捨てる**
    // ——捨てたぶんはもう一度入ったときに1から始まる。**記号が変わればそこから
    // 別のリスト**（CommonMark §5.3）なので、種類も鍵の一部である。
    let mut counts: Vec<(u8, char, u64)> = Vec::new();
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
        let kind = item_type(line, style).unwrap_or('-');
        while counts.last().is_some_and(|(held, ..)| *held > depth) {
            counts.pop();
        }
        match counts.last_mut() {
            Some((held, was, count)) if *held == depth && *was == kind => *count += 1,
            // **同じ深さで種類が変われば、そこから別のリスト**（CommonMark §5.3）
            // ——`1. 甲 / 2. 乙 / 1) 丙`の丙は3つめではない。**その項目が書いている
            // 番号から**数える：打ったのは書き手で、ここが振り直すのはその下である。
            Some((held, ..)) if *held == depth => {
                counts.pop();
                let quote = line.len() - quote_content(line).len();
                let (_, body) = leading_indent(&line[quote..]);
                let digits = body.chars().take_while(char::is_ascii_digit).count();
                let start = if style.kind == LineKind::Ordered {
                    body[..digits].parse::<u64>().unwrap_or(1)
                } else {
                    1
                };
                counts.push((depth, kind, start));
            }
            // **内側へ入れば1から**（書き手が画面で通した数え方）。
            _ => counts.push((depth, kind, 1)),
        }
        let number = counts.last().map_or(1, |(.., count)| *count);
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

/// 原稿の記法をどう読むか（要件 E9、書き手の決定 2026-09-11）。
///
/// **読み方であって、組み方ではない。**紙のシートに置けば、同じ文書が2つのペインで
/// 別の本文になる（字数も食い違う）——だから窓が1つ持つ（Settings → General の
/// MARKUP）。**2つの旗は同じところから来て同じところへ行く**ので1つの値で運ぶ
/// ——片方だけ渡し忘れる道が無い。
///
/// ## ルビと傍点（要件 E9）
///
/// **Markdownの標準ではない。**`｜漢字《かんじ》`も`《《傍点》》`も青空文庫／なろう／
/// カクヨムの記法で、切れなければ「この編集器でしか正しく見えない書き方」を書き手に
/// 強いることになる（書き手の言葉）。
///
/// **切ったときは、記号をそのまま字として出す。**原稿のバイト列は元から変えて
/// いない——読むのをやめるだけで、`｜`も`《》`も本文の1字として組まれる。
///
/// **ルビと傍点は1つの旗で切る。**同じ記法の一族（`《》`を使い、同じ道具が書き、
/// 同じ理由でMarkdownに無い）で、片方だけ読む書き手は考えにくい
/// ——足りなければ書き手が2つに分けるよう言う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// ルビと傍点の記法を読むか（要件 E9）。
    pub ruby: bool,
    /// どの記号を箇条書きの印として読むか（書き手の決定 2026-09-11）。
    pub bullets: BulletMarks,
}

impl Reading {
    /// 記法を全部読む——**既定**（Markdownがそう言っており、切りたい書き手が
    /// 切る側である）。
    pub fn all() -> Self {
        Self {
            ruby: true,
            bullets: BulletMarks::all(),
        }
    }
}

impl Default for Reading {
    fn default() -> Self {
        Self::all()
    }
}

/// 箇条書きの印として読める記号（要件 7.3.2、書き手の決定 2026-09-11）。
///
/// **CommonMarkは3つを同格に認めている**ので、この編集器も同格に扱う——どれを
/// 読むかは書き手が1つずつ選ぶ（`-`も切れる：「\*も標準ということなら、扱いは
/// 同じであるべき」）。
pub const BULLET_MARKS: [char; 3] = ['-', '*', '+'];

/// そのうち、いま箇条書きとして読む記号（書き手の決定 2026-09-11）。
///
/// **読み方であって、組み方ではない。**切った記号は本文の1行になり、記号もそのまま
/// 字として出る——**原稿のバイト列は変えない**（ルビの旗と同じ作り）。字数まで変わる
/// ので、紙のシートではなく窓が1つ持つ（Settings → General の MARKUP）。
///
/// **全部切ることもできる。**そのとき箇条書きの印は1つも無く、`Ctrl+Shift+8`は
/// 何も入れられない——**選べるとはそういうこと**であって、編集器が1つ残して
/// 「これは切らせない」と言う筋は無い。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulletMarks([bool; BULLET_MARKS.len()]);

impl BulletMarks {
    /// 3つとも読む——**既定**（CommonMarkがそう言っている）。
    pub fn all() -> Self {
        Self([true; BULLET_MARKS.len()])
    }

    /// この記号を箇条書きとして読むか。
    pub fn reads(self, mark: char) -> bool {
        BULLET_MARKS
            .iter()
            .zip(self.0)
            .any(|(known, on)| *known == mark && on)
    }

    /// 読む記号を、`BULLET_MARKS`の並び順に。
    pub fn chars(self) -> impl Iterator<Item = char> {
        BULLET_MARKS
            .into_iter()
            .zip(self.0)
            .filter_map(|(mark, on)| on.then_some(mark))
    }

    /// **鍵が入れる字**——読む記号のうち、いちばん前のもの（書き手の選択
    /// 2026-09-11の案C）。1つも読まないなら`None`で、そのとき鍵は何もしない。
    pub fn first(self) -> Option<char> {
        self.chars().next()
    }

    /// 1つを入切した形。
    pub fn toggled(self, mark: char) -> Self {
        let mut marks = self.0;
        for (index, known) in BULLET_MARKS.iter().enumerate() {
            if *known == mark {
                marks[index] = !marks[index];
            }
        }
        Self(marks)
    }

    /// 設定ファイルが言っている形（読む記号を並べた字）。
    pub fn as_said(self) -> String {
        self.chars().collect()
    }

    /// その逆——**知らない字は読み飛ばす**。手で書いた設定ファイルが、Markdownで
    /// ない字を印にすることはない。
    pub fn from_said(said: &str) -> Self {
        let mut marks = [false; BULLET_MARKS.len()];
        for (index, known) in BULLET_MARKS.iter().enumerate() {
            marks[index] = said.contains(*known);
        }
        Self(marks)
    }
}

impl Default for BulletMarks {
    fn default() -> Self {
        Self::all()
    }
}

#[cfg(test)]
pub fn visible_markdown_text(source: &str) -> String {
    visible_markdown_text_as(source, Reading::all())
}

/// E13: export recognised Markdown as plain body text, keeping source selection coordinates.
pub fn plain_body_text(source: &str, ranges: &[(usize, usize)]) -> String {
    let mut preview = PreviewDocument::default();
    preview.refresh(source, None, Reading::all());
    let whole = [(0, source.len())];
    let ranges = if ranges.is_empty() {
        &whole[..]
    } else {
        ranges
    };
    ranges
        .iter()
        .map(|&(start, end)| {
            let mut text = String::new();
            for (index, line) in preview.lines.iter().enumerate() {
                if matches!(
                    line.style.kind,
                    LineKind::Fence | LineKind::Rule | LineKind::TableRule
                ) {
                    continue;
                }
                let mut utf16 = 0;
                let trimmed = line.visible.trim();
                let first = line.visible.len() - line.visible.trim_start().len();
                let last = first + trimmed.len();
                for (byte, ch) in line.visible.char_indices() {
                    let position = preview.source_starts[index] + line.source_byte[utf16] as usize;
                    let hidden = line.marker.is_some_and(|m| utf16 < m.utf16_len as usize)
                        || line.marks.iter().any(|mark| {
                            mark.ornament.is_some_and(Ornament::rides_beside_the_line)
                                && (mark.utf16_start as usize
                                    ..(mark.utf16_start + mark.utf16_len) as usize)
                                    .contains(&utf16)
                        });
                    utf16 += ch.len_utf16();
                    if position < start || position >= end || hidden {
                        continue;
                    }
                    if line.style.kind == LineKind::TableRow && ch == '|' {
                        if byte != first && byte + 1 != last {
                            text.push('\t');
                        }
                    } else {
                        text.push(ch);
                    }
                }
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 同じことを、**記法を読むかどうかを言われて**する（要件 E9）。
#[cfg(test)]
pub fn visible_markdown_text_as(source: &str, reading: Reading) -> String {
    visible_markdown_text_with_active_line(source, None, reading)
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
    reading: Reading,
) -> String {
    let styles = line_styles(source);
    let mut visible = String::with_capacity(source.len());
    let mut line_start = 0;
    let all = source.lines().collect::<Vec<_>>();
    let style_at = |index: usize| styles.get(index).copied().unwrap_or_default();
    let unclosed = all
        .iter()
        .map(|line| standalone_unclosed(line, reading))
        .collect::<Vec<_>>();
    let contexts = paragraph_contexts(&all, style_at, &unclosed, reading);

    for (index, line) in source.lines().enumerate() {
        if active_line_start == Some(line_start) {
            visible.push_str(line);
        } else {
            let style = style_at(index);
            let context = &contexts[index];
            push_visible_line_in(line, style, &mut visible, &mut Vec::new(), reading, context);
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
    reading: Reading,
) {
    // **The style decides, here too.** An indented line is shown as written —
    // markers and all — unless a list set it in (要件 7.3.2), which is either a
    // nested item or a paragraph continuing one. Reading the spaces here
    // instead would be a second opinion about what a line is, and the one that
    // loses: the pane sets what the style says.
    let indented = line.starts_with([' ', '\t']) && style.list_indent == 0;
    // 要件 7.3.2: the blockquote marker comes off whatever is under it — a rule
    // inside a quote is still a rule, and the quoting itself is the block's
    // indent (`BlockSpan::indent_cells`, 技術検証 7.1). **A box was tried at the
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

    // 追加要件 2026-09-15: **画像だけの行は、字を消さずに箱をかぶせる。**字は残っているので、
    // 絵が読めなかったときは箱を外すだけで記法がそのまま見える。
    if style.kind == LineKind::Image {
        let start = visible.len();
        visible.push_str(line);
        let length = visible[start..].encode_utf16().count() as u32;
        marks.extend(image_box(line, length));
        return;
    }
    let content = strip_heading_marker(content);
    // 要件 7.8（2026-09-16）: 行の頭の`［＃2字下げ］`は、その行の字下げとしてもう効いている
    // （`LineStyle::note_indent`）。**字としては消える**——ルビの縦線と同じで、指示は本文ではない。
    let content = if reading.ruby {
        note_indent_head(content).map_or(content, |(_, rest)| rest)
    } else {
        content
    };
    // 同じく、行の頭の`［＃地付き］`・`［＃地から2字上げ］`（`LineStyle::tail_cells`）。
    let content = if reading.ruby {
        note_tail_head(content).map_or(content, |(_, rest)| rest)
    } else {
        content
    };
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
    push_marked(content, visible, marks, &mut at, reading);
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
fn push_marked(
    content: &str,
    visible: &mut String,
    marks: &mut Vec<Emphasis>,
    at: &mut u32,
    reading: Reading,
) {
    push_marked_recording(content, visible, marks, at, reading, None);
}

/// [`push_marked`]に、**その行で開いて閉じなかった記号**を書き留める口を足したもの（書き手の判断
/// 2026-09-15：太字などは標準どおり、同じ段落の中なら改行をまたぐ）。書き留めるのは一番外側だけで、
/// 位置は`content`の中のバイト。
fn push_marked_recording(
    content: &str,
    visible: &mut String,
    marks: &mut Vec<Emphasis>,
    at: &mut u32,
    reading: Reading,
    mut unclosed: Option<&mut Vec<(usize, &'static str)>>,
) {
    let mut rest = content;
    let mut previous = None;
    while let Some(letter) = rest.chars().next() {
        // 要件 7.8: **傍点はルビより先に読む。**`《《強調》》`はルビの`《》`で
        // 始まるので、後から見ると「《強調《」という読みのおかしなルビとして
        // 当たってしまう。長いほうを先に訊く、というだけの順である。
        if reading.ruby
            && let Some((inner, after)) = dots_here(rest)
        {
            let start = *at;
            // 中は普通の本文なので、太字も斜体もそのまま入れ子になる。
            push_marked(inner, visible, marks, at, reading);
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: Marks {
                    beside: Beside::Dot,
                    ..Marks::default()
                },
                ornament: None,
            });
            previous = inner.chars().next_back();
            rest = after;
            continue;
        }
        // 要件 7.8（2026-09-17）: 左の注記（`［＃「東京」の左に「とうきょう」の注記］`）。
        // **傍点の注記より先に読む。**どちらも`［＃「`で始まるので、順に訊く以外の
        // 見分け方は無い。こちらも後ろを向いていて、指す語は`visible`の中にいる。
        //
        // 読みは**本文に居残ったまま箱で隠される**（ルビと同じ道、`Ornament::Ruby`に
        // その理由が書いてある）。親文字は箱の直前にいるとは限らないので、箱は
        // そこまでの距離を持つ（`Ornament::LeftNote`）。
        if reading.ruby
            && let Some((word, said, after)) = side_note_here(rest)
        {
            if let Some(found) = visible.rfind(word) {
                let base_start = visible[..found].encode_utf16().count() as u32;
                let base_utf16 = word.encode_utf16().count() as u32;
                let reading_start = *at;
                for character in said.chars() {
                    visible.push(character);
                    *at += character.len_utf16() as u32;
                }
                marks.push(Emphasis {
                    utf16_start: reading_start,
                    utf16_len: *at - reading_start,
                    marks: Marks::default(),
                    ornament: Some(Ornament::LeftNote {
                        back_utf16: reading_start - base_start,
                        base_utf16,
                    }),
                });
            }
            // 指す先が無ければ注記ごと消える（傍点の注記と同じ）。**原文には残っている。**
            rest = after;
            continue;
        }
        // 要件 7.8: 青空文庫の注記形式（`［＃「本当に」に傍点］`）。
        // **これだけが後ろを向いている。**注記は自分より前にある語を指すので、
        // いま書き出した`visible`の中をさかのぼって、その語に点を打つ。
        if reading.ruby
            && let Some((word, beside, after)) = dots_note_here(rest)
        {
            if let Some(found) = visible.rfind(word) {
                let start = visible[..found].encode_utf16().count() as u32;
                marks.push(Emphasis {
                    utf16_start: start,
                    utf16_len: word.encode_utf16().count() as u32,
                    marks: Marks {
                        beside,
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
        // 要件 7.8（2026-09-17）: 範囲を囲む注記——縦中横・割り注・文字の大きさ。
        //
        // **箱で隠すものと、旗を立てるものに分かれる。**縦中横と割り注は組み方そのもの
        // なので中の字を箱が覆い（描くときに覆った字を読み出す）、文字の大きさは太字と
        // 同じ旗なので、中は普通の本文としてそのまま入れ子になる。
        if reading.ruby
            && let Some((inner, note, after)) = range_note_here(rest)
        {
            let start = *at;
            match note {
                RangeNote::Upright | RangeNote::Warichu => {
                    // 箱の中は**書いてあるとおりの字**（`Ornament::Number`と同じ道）。
                    // ここで太字を読んでも、描くのは覆った字そのものなので効かない。
                    for character in inner.chars() {
                        visible.push(character);
                        *at += character.len_utf16() as u32;
                    }
                    marks.push(Emphasis {
                        utf16_start: start,
                        utf16_len: *at - start,
                        marks: Marks::default(),
                        ornament: Some(if note == RangeNote::Upright {
                            Ornament::Upright
                        } else {
                            Ornament::Warichu {
                                cells_x10: warichu_cells_x10(inner),
                            }
                        }),
                    });
                }
                RangeNote::Small | RangeNote::Large => {
                    let scale = if note == RangeNote::Small {
                        TextScale::Small
                    } else {
                        TextScale::Large
                    };
                    let from = marks.len();
                    push_marked(inner, visible, marks, at, reading);
                    // **中の走りにも大きさを配る。**ルビも縦中横も「その走りの大きさで組む」
                    // ので、内側の走りが本文の大きさのままだと、親文字だけが小さくなって
                    // 読みが元の大きさで残る。入れ子の注記は内側が勝つ（先に置かれている）。
                    for mark in &mut marks[from..] {
                        if mark.marks.scale == TextScale::Normal {
                            mark.marks.scale = scale;
                        }
                    }
                    marks.push(Emphasis {
                        utf16_start: start,
                        utf16_len: *at - start,
                        marks: Marks {
                            scale,
                            ..Marks::default()
                        },
                        ornament: None,
                    });
                }
            }
            previous = inner.chars().next_back().or(previous);
            rest = after;
            continue;
        }
        // 要件 7.8: ルビ。`｜親《よみ》`と、親が漢字の連なりで明らかなときの
        // `漢字《かんじ》`。**縦線は消え、読みは居残って箱で隠れる**
        // （`Ornament::Ruby`にその理由が書いてある）。
        if reading.ruby
            && let Some((base, said, after, already_shown)) = ruby_here(rest, visible)
        {
            let base_start = *at;
            for character in base.chars() {
                visible.push(character);
                *at += character.len_utf16() as u32;
            }
            // **親文字は縦線の側から来るとは限らない。**`漢字《かんじ》`では
            // 親はもう`visible`に出ているので、そのぶんを数えに足す。
            let base_utf16 = already_shown + (*at - base_start);
            let reading_start = *at;
            for character in said.chars() {
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
        if let Some((shown, target, after)) = link_here(rest, previous) {
            let start = *at;
            // Retain the original destination for resolution; shortening the
            // implicit display name never rewrites the source target.
            let unresolved_link = false;
            // Emphasis inside the shown text is still emphasis: `[**太字**](x)`
            // is a bold link, and this is the same recursion that nests one
            // marker inside another.
            if rest.starts_with("[[") && !rest[..rest.len() - after.len()].contains('|') {
                // An implicit filename is literal text, not emphasis markup.
                let (name, fragment) = wiki_display_parts(target);
                visible.push_str(name);
                visible.push_str(fragment);
                *at += (name.encode_utf16().count() + fragment.encode_utf16().count()) as u32;
            } else {
                push_marked(shown, visible, marks, at, reading);
            }
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: *at - start,
                marks: Marks {
                    link: true,
                    unresolved_link,
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
                push_marked(inner, visible, marks, at, reading);
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
        // 開けるのに閉じる相手が行に無い記号。**字として残す**のは今までどおりで、あとで段落の
        // 次の行が閉じるかを見るために、位置だけを書き留める（`paragraph_contexts`）。
        if let Some(record) = unclosed.as_deref_mut()
            && let Some(marker) = unclosed_opener(rest, previous)
        {
            record.push((content.len() - rest.len(), marker));
            for character in marker.chars() {
                visible.push(character);
                *at += character.len_utf16() as u32;
            }
            previous = marker.chars().next_back();
            rest = &rest[marker.len()..];
            continue;
        }
        visible.push(letter);
        *at += letter.len_utf16() as u32;
        previous = Some(letter);
        rest = &rest[letter.len_utf8()..];
    }
}

/// 開ける形をしている記号（[`opens_here`]と同じ決まり：後ろが空白でなく、`_`は語の中では開かない）。
/// 閉じる相手がいるかは見ない——[`opens_here`]が`None`を返したあとに訊く。
fn unclosed_opener(rest: &str, previous: Option<char>) -> Option<&'static str> {
    markers()
        .into_iter()
        .map(|(marker, _)| marker)
        .find(|marker| {
            rest.strip_prefix(marker).is_some_and(|after| {
                !after.is_empty()
                    && !after.starts_with([' ', '\t'])
                    && !(marker.starts_with('_') && previous.is_some_and(char::is_alphanumeric))
            })
        })
}

/// 段落の前の行から持ち越した記号（書き手の判断 2026-09-15：**太字などは標準どおり、同じ段落の中なら
/// 改行をまたぐ**。閉じなければ太字にしない）。
///
/// `prefix`はこの行の頭で開いたままの記号（外側から）、`suffix`はこの行の終わりでまだ閉じていない記号
/// （内側から）。**どちらも、段落の中でいずれ閉じるものだけ**——閉じない記号は字のままである。
/// 行は、この記号を前後に仮に足した形で読む（[`with_context`]）。読み方は1行のときと同じものを使う。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LineContext {
    prefix: Vec<&'static str>,
    suffix: Vec<&'static str>,
}

impl LineContext {
    fn is_empty(&self) -> bool {
        self.prefix.is_empty() && self.suffix.is_empty()
    }
}

/// 持ち越した記号を仮に足した行。閉じる記号は行末の空白の前に置く（空白の後ろでは閉じない）。
fn with_context(line: &str, context: &LineContext) -> String {
    let body = line.trim_end();
    let mut text = String::with_capacity(line.len() + 8);
    text.extend(context.prefix.iter().copied());
    text.push_str(body);
    text.extend(context.suffix.iter().copied());
    text.push_str(&line[body.len()..]);
    text
}

/// 段落として続く本文の行か。見出し・箇条書き・引用・表・コード・字下げ・空行はそこで段落が切れる
/// （CommonMarkでもそれらは別のブロックである）。
fn joins_paragraph(line: &str, style: LineStyle) -> bool {
    style.kind == LineKind::Body
        && style.quote_depth == 0
        && style.list_indent == 0
        && !style.is_literal()
        && !line.trim().is_empty()
        && !line.starts_with([' ', '\t'])
        && heading_level(line) == 0
}

/// その行を1行だけで読んだとき、開いて閉じなかった記号（位置と記号）。記号の字が無い行は読まない。
fn standalone_unclosed(line: &str, reading: Reading) -> Vec<(usize, &'static str)> {
    if !line.contains(['*', '_', '~', '`']) || line.chars().count() > MARKED_LINE_LIMIT {
        return Vec::new();
    }
    let mut found = Vec::new();
    push_marked_recording(
        line,
        &mut String::new(),
        &mut Vec::new(),
        &mut 0,
        reading,
        Some(&mut found),
    );
    found
}

/// 各行が段落の前後から持ち越す記号（[`LineContext`]）。`unclosed`は各行を1行だけで読んだときの
/// 閉じなかった記号——持ち越す記号が無い段落は、これが全部空なので何も読まない。
fn paragraph_contexts(
    lines: &[&str],
    style_at: impl Fn(usize) -> LineStyle,
    unclosed: &[Vec<(usize, &'static str)>],
    reading: Reading,
) -> Vec<LineContext> {
    let mut contexts = vec![LineContext::default(); lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if !joins_paragraph(lines[index], style_at(index)) {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && joins_paragraph(lines[index], style_at(index)) {
            index += 1;
        }
        let run = start..index;
        if run.len() < 2 || unclosed[run.clone()].iter().all(Vec::is_empty) {
            continue;
        }
        // 1回目：行ごとに、頭で開いたままの記号と、閉じた行を数える。
        struct Open {
            marker: &'static str,
            closes: bool,
        }
        let mut opens: Vec<Open> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut at_start: Vec<Vec<usize>> = Vec::with_capacity(run.len());
        for line in run.clone() {
            at_start.push(stack.clone());
            let text = lines[line];
            let found = if stack.is_empty() {
                unclosed[line].clone()
            } else if text.contains(['*', '_', '~', '`']) {
                let prefix: String = stack.iter().map(|open| opens[*open].marker).collect();
                let mut found = Vec::new();
                push_marked_recording(
                    &format!("{prefix}{text}"),
                    &mut String::new(),
                    &mut Vec::new(),
                    &mut 0,
                    reading,
                    Some(&mut found),
                );
                // 仮に足した記号は、足した長さの中にある。そこから外へ出た位置を行の位置に戻す。
                let mut virtual_at = Vec::new();
                let mut offset = 0;
                for open in &stack {
                    virtual_at.push((offset, *open));
                    offset += opens[*open].marker.len();
                }
                let mut next = Vec::new();
                for (position, marker) in found {
                    if position < offset {
                        if let Some((_, open)) = virtual_at.iter().find(|(at, _)| *at == position) {
                            next.push(*open);
                        }
                    } else {
                        opens.push(Open {
                            marker,
                            closes: false,
                        });
                        next.push(opens.len() - 1);
                    }
                }
                for open in &stack {
                    if !next.contains(open) {
                        opens[*open].closes = true;
                    }
                }
                stack = next;
                continue;
            } else {
                // 記号の字が無い行は、何も閉じず、何も開けない。
                continue;
            };
            for (_, marker) in found {
                opens.push(Open {
                    marker,
                    closes: false,
                });
                stack.push(opens.len() - 1);
            }
        }
        // 2回目：閉じる記号だけを、各行の頭と終わりに持ち越す。
        let closing = |held: &Vec<usize>| {
            held.iter()
                .filter(|open| opens[**open].closes)
                .map(|open| opens[*open].marker)
                .collect::<Vec<_>>()
        };
        for (offset, line) in run.clone().enumerate() {
            let prefix = closing(&at_start[offset]);
            let mut suffix = at_start.get(offset + 1).map(&closing).unwrap_or_default();
            suffix.reverse();
            contexts[line] = LineContext { prefix, suffix };
        }
    }
    contexts
}

/// 画像だけの行が指す絵（追加要件 2026-09-15、書き手）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRef<'a> {
    /// 書いてあるとおりの行き先（`<…>`は外す）。解決は呼ぶ側（文書の置き場所を知っている）。
    pub target: &'a str,
    /// Obsidianの`|300`（`300x200`の幅）。無ければ元の大きさ。
    pub width: Option<u32>,
}

/// 行が画像だけか、そうなら指す絵（追加要件 2026-09-15）。
///
/// `![説明](画像.png)`・`![説明|300](画像.png)`・`![説明](<画像 a.png> "題")`・`![[画像.png]]`・
/// `![[画像.png|300]]`。**行き先は画像の拡張子で終わるものだけ**——`![[ノート]]`は別の文書の埋め込みで、
/// 絵ではない。`http://`などの外の場所は読まない（取りに行かない）。
pub fn image_of_line(line: &str) -> Option<ImageRef<'_>> {
    let body = line.trim();
    let size = |option: &str| {
        option
            .trim()
            .split('x')
            .next()
            .and_then(|width| width.parse::<u32>().ok())
            .filter(|width| *width > 0)
    };
    let (target, width) = if let Some(inner) = body
        .strip_prefix("![[")
        .and_then(|rest| rest.strip_suffix("]]"))
    {
        match inner.split_once('|') {
            Some((target, option)) => (target.trim(), size(option)),
            None => (inner.trim(), None),
        }
    } else {
        let rest = body.strip_prefix("![")?;
        let (alt, after) = rest.split_once("](")?;
        let inner = after.strip_suffix(')')?;
        let target = match inner.split_once(" \"") {
            Some((target, _)) => target,
            None => inner,
        }
        .trim();
        let target = target
            .strip_prefix('<')
            .and_then(|target| target.strip_suffix('>'))
            .unwrap_or(target);
        (
            target,
            alt.rsplit_once('|').and_then(|(_, option)| size(option)),
        )
    };
    let lower = target.to_ascii_lowercase();
    let picture = [
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".tif", ".tiff", ".ico", ".jxr", ".heic",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension));
    (picture && !target.is_empty() && !target.contains("://")).then_some(ImageRef { target, width })
}

/// 画像の行の幅の指定を`width`にした行（追加要件 2026-09-16：絵の大きさをマウスで変える）。
///
/// 指定があれば置き換え、無ければ足す。`300x200`の高さは捨てる——縦横比は絵が保つ。行の前後の
/// 空白はそのまま。画像の行でなければ`None`。
pub fn with_image_width(line: &str, width: u32) -> Option<String> {
    image_of_line(line)?;
    let head = line.len() - line.trim_start().len();
    let body = line.trim();
    let tail = &line[head + body.len()..];
    let body = if let Some(inner) = body
        .strip_prefix("![[")
        .and_then(|rest| rest.strip_suffix("]]"))
    {
        let target = inner.split_once('|').map_or(inner, |(target, _)| target);
        format!("![[{target}|{width}]]")
    } else {
        let (alt, after) = body.strip_prefix("![")?.split_once("](")?;
        let sized = |option: &str| {
            option
                .trim()
                .split('x')
                .next()
                .is_some_and(|width| width.parse::<u32>().is_ok_and(|width| width > 0))
        };
        let alt = match alt.rsplit_once('|') {
            Some((text, option)) if sized(option) => text,
            _ => alt,
        };
        format!("![{alt}|{width}]({after}")
    };
    Some(format!("{}{body}{tail}", &line[..head]))
}

/// 絵を指す鍵：行き先と幅から決まる（同じ書き方なら同じ鍵）。
pub fn image_key(image: ImageRef<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    image.target.hash(&mut hasher);
    image.width.hash(&mut hasher);
    hasher.finish() | 1
}

/// 画像の行に立てる箱（大きさはまだ0、`PreviewDocument::size_images`が入れる）。
fn image_box(line: &str, utf16_len: u32) -> Option<Emphasis> {
    let image = image_of_line(line)?;
    Some(Emphasis {
        utf16_start: 0,
        utf16_len,
        marks: Marks::default(),
        ornament: Some(Ornament::Image {
            key: image_key(image),
            width: 0,
            height: 0,
            source_shown: false,
        }),
    })
}

/// 持ち越した記号を足して1行を読む。**足した記号が対にならなかった**（空白の並びなどで閉じられ
/// なかった）ときは、足した字が画面に出てしまうので、持ち越しは無かったことにして読み直す。
fn push_visible_line_in(
    line: &str,
    style: LineStyle,
    visible: &mut String,
    marks: &mut Vec<Emphasis>,
    reading: Reading,
    context: &LineContext,
) {
    if context.is_empty() {
        push_visible_line(line, style, visible, marks, reading);
        return;
    }
    let start = visible.len();
    let kept_marks = marks.len();
    push_visible_line(&with_context(line, context), style, visible, marks, reading);
    if !deletes_only(&visible[start..], line) {
        visible.truncate(start);
        marks.truncate(kept_marks);
        push_visible_line(line, style, visible, marks, reading);
    }
}

/// `visible`が`source`から字を消しただけのものか（前から順に拾えるか）。
fn deletes_only(visible: &str, source: &str) -> bool {
    let mut rest = source.chars();
    visible
        .chars()
        .all(|wanted| rest.any(|found| found == wanted))
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

/// 範囲を囲む注記（要件 7.8、2026-09-17、書き手「組版の表現拡大」①③）。
///
/// **開きと閉じで挟むものだけがここにいる。**後ろから前を指す注記（`［＃「…」に傍点］`）は
/// [`dots_note_here`]、行そのものの体裁（`［＃ここから2字下げ］`）は[`format_note`]である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeNote {
    /// `［＃縦中横］…［＃縦中横終わり］`——自動の規則が拾わないものを1マスに正立させる。
    Upright,
    /// `［＃割り注］…［＃割り注終わり］`——1行の中に半分の大きさで2行。
    Warichu,
    /// `［＃小さな文字］…［＃小さな文字終わり］`。
    Small,
    /// `［＃大きな文字］…［＃大きな文字終わり］`。
    Large,
}

/// 開き・閉じ・種類。**長い言い方から先に並べる**必要はない——開きは`］`まで含めて
/// 比べるので、「小さな文字」と「文字」のような取りこぼしが起きない。
const RANGE_NOTES: [(&str, &str, RangeNote); 4] = [
    ("［＃縦中横］", "［＃縦中横終わり］", RangeNote::Upright),
    ("［＃割り注］", "［＃割り注終わり］", RangeNote::Warichu),
    (
        "［＃小さな文字］",
        "［＃小さな文字終わり］",
        RangeNote::Small,
    ),
    (
        "［＃大きな文字］",
        "［＃大きな文字終わり］",
        RangeNote::Large,
    ),
];

/// `rest`の頭にある範囲の注記——中身、種類、注記の後ろ（2026-09-17）。
///
/// **閉じないものは注記ではない。**太字の`**`と同じ規則で、閉じない`［＃縦中横］`は
/// 字のまま本文に残る——原文にある指示が画面から消えたまま何も起きない、を避ける道である。
/// 空の`［＃縦中横］［＃縦中横終わり］`も組む相手がいないので記法ではない。
fn range_note_here(rest: &str) -> Option<(&str, RangeNote, &str)> {
    if !rest.starts_with("［＃") {
        return None;
    }
    for (open, close, kind) in RANGE_NOTES {
        let Some(after) = rest.strip_prefix(open) else {
            continue;
        };
        let Some(at) = after.find(close) else {
            continue;
        };
        if at == 0 {
            continue;
        }
        return Some((&after[..at], kind, &after[at + close.len()..]));
    }
    None
}

/// 左の注記（`［＃「東京」の左に「とうきょう」の注記］`、要件 7.8、2026-09-17）。
/// 指す語、左に出す字、注記の後ろ。
///
/// **傍点の注記と同じく後ろを向いている**ので、どこに出すかは呼び出し側が`visible`を
/// さかのぼって決める。ここは書式を読むだけ。
fn side_note_here(rest: &str) -> Option<(&str, &str, &str)> {
    let after_open = rest.strip_prefix("［＃「")?;
    let close = after_open.find("」の左に「")?;
    if close == 0 {
        return None;
    }
    let word = &after_open[..close];
    let after_word = &after_open[close + "」の左に「".len()..];
    let end = after_word.find("」の注記］")?;
    if end == 0 {
        return None;
    }
    Some((
        word,
        &after_word[..end],
        &after_word[end + "」の注記］".len()..],
    ))
}

/// 青空文庫の注記形式の傍点・傍線（`［＃「本当に」に傍点］`、要件 7.8）。
/// 印を打つ語、印の種類、注記の後ろ。
///
/// **注記は後ろから前を指す**ので、返すのは語そのものである——どこに打つかは
/// 呼び出し側が`visible`をさかのぼって決める。ここは書式を読むだけ。
///
/// 種類は青空文庫の言い方をそのまま読む（2026-09-16、線の種類は2026-09-17）：
/// 傍点・ゴマ傍点・丸傍点・白丸傍点・二重丸傍点・×傍点と、傍線・二重傍線・波線・
/// 鎖線・破線。**知らない注記
/// （`［＃改ページ］`など）は読まない**：消してしまうと、原文にある指示が画面から
/// 消えたまま何も起きないことになる。
fn dots_note_here(rest: &str) -> Option<(&str, Beside, &str)> {
    let after_open = rest.strip_prefix("［＃「")?;
    // **1つの注記の中で閉じる。**探すのは最初の`］`までで、そこに種類が無ければ
    // この注記は傍点のものではない——先の別の注記の`］`まで届くと、そのあいだの
    // 本文ごと語として飲み込んでしまう（2026-09-17、左の注記を足して分かった）。
    let inside = after_open
        .find('］')
        .map_or(after_open, |at| &after_open[..at + '］'.len_utf8()]);
    // 長い言い方から先に見る——「丸傍点」は「傍点」でも終わるので、短いほうから
    // 当てると種類が落ちる。
    let kinds = [
        ("」に二重丸傍点］", Beside::Double),
        ("」に白丸傍点］", Beside::Open),
        ("」にゴマ傍点］", Beside::Sesame),
        ("」に丸傍点］", Beside::Solid),
        ("」に×傍点］", Beside::Cross),
        ("」に傍点］", Beside::Dot),
        ("」に二重傍線］", Beside::DoubleLine),
        ("」に波線］", Beside::WaveLine),
        ("」に鎖線］", Beside::ChainLine),
        ("」に破線］", Beside::DashLine),
        ("」に傍線］", Beside::Line),
    ];
    let (close, note, beside) = kinds
        .iter()
        .filter_map(|(note, beside)| inside.find(note).map(|at| (at, *note, *beside)))
        .min_by_key(|(at, note, _)| (*at, std::cmp::Reverse(note.len())))?;
    if close == 0 {
        return None;
    }
    Some((
        &after_open[..close],
        beside,
        &after_open[close + note.len()..],
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
        .filter(|mark| mark.ornament.is_some_and(Ornament::rides_beside_the_line))
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
            push_visible_line(line, style, &mut visible, &mut marks, Reading::all());
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

/// Default Wiki display: a filename stem and its unchanged heading fragment.
fn wiki_display_parts(target: &str) -> (&str, &str) {
    let (path, fragment) = target
        .find('#')
        .map_or((target, ""), |at| (&target[..at], &target[at..]));
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    // Keep dotfiles and extensionless names intact; remove only the last suffix.
    let stem = name
        .rfind('.')
        .filter(|&at| at > 0)
        .map_or(name, |at| &name[..at]);
    (stem, fragment)
}

/// Link tokens on one logical line, excluding code, escaped syntax and images.
fn line_link_ranges(source: &str) -> Vec<(Range<usize>, Range<usize>, bool)> {
    let mut links = Vec::new();
    let mut rest = source;
    let mut previous = None;
    while !rest.is_empty() {
        if let Some((_, _, after)) = rest.strip_prefix('!').and_then(|s| link_here(s, None)) {
            rest = after;
            previous = Some(']');
            continue;
        }
        if let Some((_, _, after)) = opens_here(rest, previous).filter(|(marks, _, _)| marks.code) {
            rest = after;
            previous = Some('`');
            continue;
        }
        if let Some((_, target, after)) = link_here(rest, previous) {
            let start = source.len() - rest.len();
            let end = source.len() - after.len();
            let target_start = target.as_ptr() as usize - source.as_ptr() as usize;
            links.push((
                start..end,
                target_start..target_start + target.len(),
                rest.starts_with("[["),
            ));
            rest = after;
            previous = Some(']');
            continue;
        }
        let ch = rest.chars().next().unwrap();
        let len = ch.len_utf8()
            + if ch == '\\' {
                rest[ch.len_utf8()..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
            } else {
                0
            };
        rest = &rest[len..];
        previous = Some(ch);
    }
    links
}

fn next_visible_source_offset(
    source: &str,
    cursor: usize,
    character: char,
    hidden: &[Range<usize>],
) -> usize {
    source[cursor..]
        .char_indices()
        .map(|(relative, found)| (cursor + relative, found))
        .find(|(offset, found)| {
            *found == character
                && !hidden
                    .get(hidden.partition_point(|range| range.end <= *offset))
                    .is_some_and(|range| range.contains(offset))
        })
        .map_or(cursor, |(offset, _)| offset)
}

fn hidden_wiki_display_ranges(source: &str) -> Vec<Range<usize>> {
    let mut hidden = Vec::new();
    for (token, target, wiki) in line_link_ranges(source) {
        if !wiki {
            continue;
        }
        if source.as_bytes().get(target.end) == Some(&b'|') {
            hidden.push(token.start..target.end + 1);
        } else {
            let (name, fragment) = wiki_display_parts(&source[target.clone()]);
            let name_start = name.as_ptr() as usize - source.as_ptr() as usize;
            hidden.push(token.start..name_start);
            let fragment_start = if fragment.is_empty() {
                target.end
            } else {
                target.end - fragment.len()
            };
            hidden.push(name_start + name.len()..fragment_start);
        }
        hidden.push(token.end - 2..token.end);
    }
    hidden
}

/// Source ranges of link destinations, preserving their original spelling.
/// Consumers decide whether a target is local; parsing never resolves or reads files.
pub fn link_target_ranges(source: &str) -> Vec<(Range<usize>, bool)> {
    let styles = line_styles_as(source, BulletMarks::default());
    let mut offset = 0;
    let mut result = Vec::new();
    for (index, line) in source.split_inclusive('\n').enumerate() {
        if !styles.get(index).is_some_and(|style| style.kind.is_code()) {
            result.extend(
                line_link_ranges(line)
                    .into_iter()
                    .map(|(_, target, wiki)| (offset + target.start..offset + target.end, wiki)),
            );
        }
        offset += line.len();
    }
    result
}

/// Parse a complete ordinary or Wiki link without rewriting its destination.
/// Explicit labels are returned verbatim; implicit Wiki display is shortened
/// only when rendering. Images and unmatched opening brackets are not links.
fn link_here<'a>(rest: &'a str, previous: Option<char>) -> Option<(&'a str, &'a str, &'a str)> {
    if previous == Some('!') {
        return None;
    }
    if let Some(inner_and_rest) = rest.strip_prefix("[[") {
        let (inner, after) = inner_and_rest.split_once("]]")?;
        // `[[note|shown]]` shows the second half; `[[note]]` shows the note.
        let (target, shown) = inner.split_once('|').unwrap_or((inner, inner));
        return (!shown.is_empty()).then_some((shown, target, after));
    }
    let inner_and_rest = rest.strip_prefix('[')?;
    let (shown, after_close) = inner_and_rest.split_once("](")?;
    // The shown text may hold brackets of its own, but not a `](` — the first
    // one closes the link, which is what Markdown itself does.
    let (target, after) = after_close.split_once(')')?;
    (!shown.is_empty()).then_some((shown, target, after))
}

/// A link at a source byte. Parsing shares the preview grammar and never reads disk.
pub fn link_target_at(source: &str, byte: usize) -> Option<(&str, bool)> {
    let (start, end) = line_span(source, byte);
    let line_number = source[..start].bytes().filter(|&b| b == b'\n').count();
    if line_styles_as(source, BulletMarks::default())
        .get(line_number)?
        .kind
        .is_code()
    {
        return None;
    }
    let mut rest = &source[start..end];
    let mut at = start;
    let mut previous = None;
    while !rest.is_empty() {
        if let Some((_, _, after)) = rest.strip_prefix('!').and_then(|s| link_here(s, None)) {
            at += rest.len() - after.len();
            rest = after;
            previous = Some(']');
            continue;
        }
        if let Some((marks, _, after)) = opens_here(rest, previous).filter(|(m, _, _)| m.code) {
            let _ = marks;
            at += rest.len() - after.len();
            rest = after;
            previous = Some('`');
            continue;
        }
        if let Some((_, target, after)) = link_here(rest, previous) {
            let next = at + rest.len() - after.len();
            if (at..next).contains(&byte) {
                return Some((target, rest.starts_with("[[")));
            }
            at = next;
            rest = after;
            previous = Some(']');
            continue;
        }
        let ch = rest.chars().next()?;
        let mut len = ch.len_utf8();
        if ch == '\\' {
            len += rest[len..].chars().next().map_or(0, char::len_utf8);
        }
        rest = &rest[len..];
        at += len;
        previous = Some(ch);
    }
    None
}

/// Resolve an explicit local path. Wiki names never trigger a folder search.
pub fn link_path(
    target: &str,
    wiki: bool,
    source_file: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let target = target
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(target.trim());
    if target.is_empty() || target.contains(['\n', '\r', '#']) || target.contains("://") {
        return None;
    }
    let path = std::path::Path::new(target);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else if wiki || target.contains(':') || target.starts_with(['/', '\\']) {
        None
    } else {
        Some(source_file?.parent()?.join(path))
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
fn list_kind(content: &str, marks: BulletMarks) -> Option<LineKind> {
    // 書き手の決定 2026-09-11: **読むと決めた記号だけが印である。**切った記号の行は
    // 本文の1行で、記号もそのまま字として出る（原稿は変えていない）。
    if let Some(mark) = content.chars().next().filter(|mark| marks.reads(*mark)) {
        let rest = &content[mark.len_utf8()..];
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
fn outside_fence(line: &str, levels: &mut ListLevels, marks: BulletMarks) -> LineStyle {
    let quote = quoted(line);
    let content = quote.unwrap_or(line);
    let (columns, body) = leading_indent(content);
    let kind = if is_rule(body) {
        LineKind::Rule
    } else {
        list_kind(body, marks).unwrap_or_default()
    };
    let quote_depth = u8::from(quote.is_some());
    if kind.is_list() {
        return LineStyle {
            heading_level: 0,
            kind,
            quote_depth,
            comment: CommentSyntax::None,
            list_indent: levels.depth_of(columns) + 1,
            note_indent: 0,
            tail_cells: None,
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
            note_indent: 0,
            tail_cells: None,
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
        note_indent: 0,
        tail_cells: None,
    }
}

/// 体裁の注記が言う字下げ（要件 7.8、2026-09-16、書き手「組版の表現拡大」）。
///
/// 青空文庫の言い方をそのまま読む：`［＃ここから2字下げ］`から`［＃ここで字下げ終わり］`までと、
/// 行の頭に置く`［＃2字下げ］`（その行だけ）。**数字は半角でも全角でもよい**——テキストで配られる
/// 原稿はどちらも使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteIndent {
    /// ここから、この字数だけ下げる。
    From(u8),
    /// ここで終わり。
    End,
}

/// 行末へ寄せる注記（要件 7.8、2026-09-16）。`［＃地付き］`と`［＃地から2字上げ］`を、
/// 行の頭に置く形で読む。返すのは**行の終わりから空ける字数**（地付きは0）と、注記を外した残り。
///
/// 署名や結び、詩の行末ぞろえに使う。範囲の形（`［＃ここから地付き］`）は、要ると言われてから。
pub fn note_tail_head(line: &str) -> Option<(u8, &str)> {
    let rest = line.strip_prefix("［＃")?;
    let close = rest.find("］")?;
    let inner = &rest[..close];
    let after = &rest[close + "］".len()..];
    if inner == "地付き" {
        return Some((0, after));
    }
    let count = inner.strip_prefix("地から")?.strip_suffix("字上げ")?;
    Some((note_count(count)?, after))
}

/// 行の全体が字下げの注記なら、それ。指示だけの行は本文ではない（[`LineKind::Note`]）。
pub fn note_indent_of(line: &str) -> Option<NoteIndent> {
    let body = line.trim();
    if body == "［＃ここで字下げ終わり］" {
        return Some(NoteIndent::End);
    }
    let inner = body.strip_prefix("［＃")?.strip_suffix("］")?;
    let count = inner.strip_prefix("ここから")?.strip_suffix("字下げ")?;
    Some(NoteIndent::From(note_count(count)?))
}

/// 行の頭に置く`［＃2字下げ］`。字数と、注記を外した残り。
pub fn note_indent_head(line: &str) -> Option<(u8, &str)> {
    let rest = line.strip_prefix("［＃")?;
    let close = rest.find("］")?;
    let count = rest[..close].strip_suffix("字下げ")?;
    // 「ここから」は範囲の側の言い方で、1行の指示ではない。
    if count.starts_with("ここから") {
        return None;
    }
    let count = note_count(count)?;
    Some((count, &rest[close + "］".len()..]))
}

/// 注記の中の字数。半角と全角の数字を読む（1桁で足りる——2桁下げる原稿は無い）。
fn note_count(text: &str) -> Option<u8> {
    let digits = text
        .chars()
        .map(|letter| match letter {
            '0'..='9' => Some(letter as u8 - b'0'),
            '０'..='９' => Some(letter as u32 as u8 - '０' as u32 as u8),
            _ => None,
        })
        .collect::<Option<Vec<u8>>>()?;
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    Some(digits.iter().fold(0u8, |total, digit| total * 10 + digit))
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

/// 表の行のセルの字の範囲（文書のバイト）と、行の頭と終わり（改行を除く）（書き手の求め 2026-09-15、
/// 表のまま編集）。表の行でなければ`None`。
///
/// **セルの字は前後の余白を除いたもの**——升目に描かれるのはそこだけで、`|`と余白はどこにも描かれない。
/// 行を閉じる`|`が残す空のセルは数えない（升目にも無い）。途中の空のセルは、`|`の前の位置を持つ。
fn table_row_cells(
    source: &str,
    styles: &[LineStyle],
    byte: usize,
) -> Option<(usize, usize, Vec<Range<usize>>)> {
    let (line_start, line_end) = line_span(source, byte);
    let line = source[line_start..line_end]
        .strip_suffix('\n')
        .unwrap_or(&source[line_start..line_end]);
    let index = source[..line_start].matches('\n').count();
    if styles.get(index)?.kind != LineKind::TableRow {
        return None;
    }
    let found = crate::text_blocks::table_cells(line);
    let last = found.len().saturating_sub(1);
    let cells = found
        .iter()
        .enumerate()
        .filter_map(|(at, cell)| {
            let text = &line[cell.byte_start..cell.byte_end];
            if at == last && text.trim().is_empty() {
                return None;
            }
            let lead = text.len() - text.trim_start().len();
            let start = line_start + cell.byte_start + lead;
            Some(start..start + text.trim().len())
        })
        .collect::<Vec<_>>();
    Some((line_start, line_start + line.len(), cells))
}

/// 表の行で、←→が跨ぐ先（書き手の求め 2026-09-15：表のまま編集）。
///
/// `byte`が**描かれない場所**（`|`とその前後の余白）にあれば、動いた向きで次に立てる場所を返す——
/// 次のセルの頭、前のセルの終わり、行の外なら隣の行（そこがまた表の行なら、呼ぶ側がもう一度訊く）。
/// セルの字の中と両端なら`None`（そこは止まり場所である）。
pub fn table_step(source: &str, styles: &[LineStyle], byte: usize, forward: bool) -> Option<usize> {
    let (start, end, cells) = table_row_cells(source, styles, byte)?;
    let (first, last) = (cells.first()?, cells.last()?);
    if cells
        .iter()
        .any(|cell| cell.start <= byte && byte <= cell.end)
    {
        return None;
    }
    Some(if forward {
        match cells.iter().find(|cell| cell.start > byte) {
            Some(next) => next.start,
            None if end < source.len() => end + 1,
            None => last.end,
        }
    } else {
        match cells.iter().rev().find(|cell| cell.end < byte) {
            Some(previous) => previous.end,
            None if start > 0 => start - 1,
            None => first.start,
        }
    })
}

/// 表の中の`Tab`／`Shift+Tab`の行き先（書き手の求め 2026-09-15）：次／前のセルの頭。
///
/// 行の終わりのセルからは次の表の行の最初のセルへ（区切り行は跨ぐ）、表の終わりのセルでは動かない。
/// 表の行でなければ`None`——`Tab`は今までどおり字下げか字を入れる。
pub fn table_tab(source: &str, styles: &[LineStyle], byte: usize, back: bool) -> Option<usize> {
    let (start, end, cells) = table_row_cells(source, styles, byte)?;
    // いまのセル：`byte`より前で始まる最後のセル（`|`の後ろの余白は次のセル）。
    let here = cells
        .iter()
        .rposition(|cell| cell.start <= byte)
        .unwrap_or(0);
    if !back && here + 1 < cells.len() {
        return Some(cells[here + 1].start);
    }
    if back && byte > cells[here].start {
        return Some(cells[here].start);
    }
    if back && here > 0 {
        return Some(cells[here - 1].start);
    }
    // 隣の表の行へ。区切り行（表の行ではない）は跨ぎ、表の外に出たら動かない。
    let mut line = if back { start } else { end };
    loop {
        if back {
            if line == 0 {
                return Some(byte);
            }
            line = line_span(source, line - 1).0;
        } else {
            if line >= source.len() {
                return Some(byte);
            }
            line += 1;
        }
        let index = source[..line].matches('\n').count();
        match styles.get(index).map(|style| style.kind) {
            Some(LineKind::TableRow) => {
                let (_, _, row) = table_row_cells(source, styles, line)?;
                let cell = if back { row.last() } else { row.first() };
                return Some(cell.map_or(line, |cell| cell.start));
            }
            Some(LineKind::TableRule) => {
                if !back {
                    line = line_span(source, line).1.saturating_sub(1);
                }
            }
            _ => return Some(byte),
        }
    }
}

/// 編集中の行の書式（書き手の求め 2026-09-15：「記号は隠さず、ただし太字として見える方がいい」）。
///
/// **読み方は整形表示と同じ**（[`push_visible_line`]）で、組んだ行の印を原文の位置へ写す——読み方を
/// 2つ持てば、いつか編集中と整形後で太字の範囲が食い違う。写すのは字の書式（太字・斜体・取消線・
/// コード・リンク・傍点・注釈）だけで、**字を覆う箱（ルビの読みなど）は写さない**：編集中は記号も読みも
/// 見せる。印は記号の内側の字に付き、記号そのものは地の字のまま——どこまでが記法かが見える。
fn active_marks(
    line: &str,
    style: LineStyle,
    reading: Reading,
    context: &LineContext,
) -> Vec<Emphasis> {
    let mut shown = String::new();
    let mut formatted = Vec::new();
    push_visible_line_in(line, style, &mut shown, &mut formatted, reading, context);
    if formatted.is_empty() {
        return formatted;
    }
    // 組んだ行のUTF-16位置 → 原文のUTF-16位置。整形は原文から字を消すだけなので、`build`と同じく
    // 前から順に拾えば当たる。
    let mut map = Vec::with_capacity(shown.encode_utf16().count() + 1);
    let mut cursor = 0;
    let mut cursor_utf16 = 0u32;
    let hidden_wiki = if style.kind.is_code() || !line.contains("[[") {
        Vec::new()
    } else {
        hidden_wiki_display_ranges(line)
    };
    for character in shown.chars() {
        let remaining = &line[cursor..];
        let skipped = next_visible_source_offset(line, cursor, character, &hidden_wiki) - cursor;
        cursor_utf16 += remaining[..skipped].encode_utf16().count() as u32;
        cursor += skipped;
        for unit in 0..character.len_utf16() as u32 {
            map.push(cursor_utf16 + unit);
        }
        cursor += character.len_utf8();
        cursor_utf16 += character.len_utf16() as u32;
    }
    map.push(cursor_utf16);
    let mut marks: Vec<Emphasis> = formatted
        .into_iter()
        .filter(|emphasis| emphasis.ornament.is_none())
        .filter_map(|emphasis| {
            let start = *map.get(emphasis.utf16_start as usize)?;
            let end = emphasis.utf16_start + emphasis.utf16_len;
            // 終わりは「最後の字の次」。最後の字の位置から数え直し、閉じる記号を含めない。
            let end = if emphasis.utf16_len == 0 {
                start
            } else {
                map.get(end as usize - 1).map(|last| last + 1)?
            };
            Some(Emphasis {
                utf16_start: start,
                utf16_len: end.saturating_sub(start),
                ..emphasis
            })
        })
        .collect();
    // The active line exposes the source path, not the shortened preview label.
    // Mark the whole destination from parser spans. Explicit aliases retain
    // their own mapped formatting; brackets and the alias separator stay plain.
    if !style.kind.is_code() {
        for (_, target, _) in line_link_ranges(line) {
            let start = line[..target.start].encode_utf16().count() as u32;
            let len = line[target.clone()].encode_utf16().count() as u32;
            marks.retain(|mark| {
                !mark.marks.link || !(start..start + len).contains(&mark.utf16_start)
            });
            marks.push(Emphasis {
                utf16_start: start,
                utf16_len: len,
                marks: Marks {
                    link: true,
                    unresolved_link: false,
                    ..Marks::default()
                },
                ornament: None,
            });
        }
    }
    marks
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
    if style.heading_level > 0 {
        let content = quote_content(line);
        let body = strip_heading_marker(content);
        return Some(LineMarker {
            utf16_len: (line.len() - body.len()) as u32,
            ornament: Ornament::Markup,
        });
    }
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
        // 要件 7.8（2026-09-16）: 体裁の注記だけの行も同じ——指示は本文ではない。
        LineKind::Rule | LineKind::Fence | LineKind::Note | LineKind::PageBreak => Ornament::Hidden,
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
    reading: Reading,
    note_indent: &mut u8,
) -> LineStyle {
    let marks = reading.bullets;
    // 要件 7.8（2026-09-16）: 体裁の注記。**フェンスの中は本文ではない**ので読まない。
    if reading.ruby && fence.is_none() {
        match note_indent_of(line) {
            Some(NoteIndent::From(count)) => {
                *note_indent = count;
                *table = None;
                return LineStyle::of_kind(LineKind::Note);
            }
            Some(NoteIndent::End) => {
                *note_indent = 0;
                *table = None;
                return LineStyle::of_kind(LineKind::Note);
            }
            None => {}
        }
        // 要件 7.8・要件 7.10（2026-09-16）: 改ページ。**画面では紙を切らない**ので、
        // ここに切れ目があることを見せる行になる。
        if line.trim() == "［＃改ページ］" {
            *table = None;
            return LineStyle {
                note_indent: *note_indent,
                ..LineStyle::of_kind(LineKind::PageBreak)
            };
        }
    }
    // 行の頭の`［＃2字下げ］`は、その行だけ。範囲の中にいればそこへ足す。
    let head_indent = if reading.ruby && fence.is_none() {
        note_indent_head(line).map(|(count, _)| count)
    } else {
        None
    };
    // 行の頭の`［＃地付き］`・`［＃地から2字上げ］`も、その行だけ。
    let tail = if reading.ruby && fence.is_none() {
        note_tail_head(line).map(|(count, _)| count)
    } else {
        None
    };
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
            let style = outside_fence(line, levels, marks);
            // 追加要件 2026-09-15: 画像だけの行。本文の行（引用・箇条書き・字下げの中ではない）に限る。
            if style.kind == LineKind::Body
                && style.quote_depth == 0
                && style.list_indent == 0
                && !line.starts_with([' ', '\t'])
                && image_of_line(line).is_some()
            {
                LineStyle {
                    kind: LineKind::Image,
                    ..style
                }
            } else {
                style
            }
        }
    };
    // Anything a fence decides ends whatever table was open: a table's rows are
    // bars at the margin, and a fenced line is code whatever it is made of.
    *table = None;
    LineStyle {
        note_indent: *note_indent + head_indent.unwrap_or(0),
        tail_cells: tail,
        ..style
    }
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
#[cfg(test)]
pub fn line_styles(source: &str) -> Vec<LineStyle> {
    line_styles_as(source, BulletMarks::all())
}

/// 同じことを、**どの記号を印として読むかを言われて**する（書き手の決定 2026-09-11）。
pub fn line_styles_as(source: &str, marks: BulletMarks) -> Vec<LineStyle> {
    line_styles_reading(
        source,
        Reading {
            ruby: true,
            bullets: marks,
        },
    )
}

/// 同じことを、**記法をどこまで読むかを言われて**する（要件 E9・要件 7.8）。
/// 体裁の注記（字下げ）はルビと同じ旗で切れる——同じ記法の一族である。
pub fn line_styles_reading(source: &str, reading: Reading) -> Vec<LineStyle> {
    let mut fence = None;
    let mut levels = ListLevels::default();
    let mut table = None;
    // 字下げの範囲は行をまたいで続く（フェンスと同じ道）。
    let mut note_indent = 0;
    let mut lines = source.split('\n').peekable();
    let mut styles = Vec::new();
    while let Some(line) = lines.next() {
        // The line after this one, and an empty one past the last: a row of
        // bars at the end of the document has no delimiter row under it and is
        // not a table.
        let next = lines.peek().copied().unwrap_or_default();
        styles.push(line_style(
            line,
            next,
            &mut fence,
            &mut levels,
            &mut table,
            reading,
            &mut note_indent,
        ));
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
    let mut note_indent = 0;
    let mut lines = source.split('\n').peekable();
    while let Some(line) = lines.next() {
        let next = lines.peek().copied().unwrap_or_default();
        // Through `line_style` rather than `heading_level`, so a hash inside a
        // fenced block is as much not-a-heading here as it is in the pane.
        // **アウトラインは見出しだけを見る。**どの記号を印として読むかは、ここの
        // 答えを変えない（`- 項目`は見出しではない）ので、全部読む側で通す。
        let style = line_style(
            line,
            next,
            &mut fence,
            &mut levels,
            &mut table,
            Reading::all(),
            &mut note_indent,
        );
        let level = style.heading_level;
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
        preview.refresh("```rust\nlet a = 1; // 説明\n```\n", None, Reading::all());
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
        preview.refresh("```python\nlet a = 1; // 説明\n```\n", None, Reading::all());
        assert_eq!(commented(&preview), 0, "the line is code again");

        preview.refresh("```なにか\nlet a = 1; // 説明\n```\n", None, Reading::all());
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
        // **改行は選びに入れない**（書き手の報告 2026-09-11）——入れるとカーソルが
        // 次の行の頭へ出る。
        assert_eq!(picked, "二");
        // 先頭の行は前へ行けない。
        assert!(edited(source, (0, 0), LineEdit::MoveBefore).is_none());
    }

    /// E3の②: 後の行と入れ替える。**末尾の行は後へ行けない。**
    #[test]
    fn a_line_changes_places_with_the_one_after_it() {
        let source = "一\n二\n三\n";

        let (next, picked) = edited(source, (0, 0), LineEdit::MoveAfter).expect("動かせる");

        assert_eq!(next, "二\n一\n三\n");
        assert_eq!(picked, "一");
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
        assert_eq!(picked, "二");

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
        assert_eq!(picked, "二\n三");
    }

    /// E3の②・E10（書き手の報告 2026-09-11）: **頭がちょうど行末なら、その行も
    /// 入らない。**「選択範囲より一行前から箇条書きになります」——行の頭の近くを
    /// 押すと当たり判定は前の行の末尾を返すので、そこを「その行も選ばれている」と
    /// 読むと、書き手が見ていない行に印が付く。
    #[test]
    fn a_selection_that_starts_at_a_line_end_leaves_that_line_alone() {
        let source = "一\n二\n三\n";
        let first_end = "一".len();

        // 「一」の行末から「二」の途中まで——選ばれているのは「二」だけ。
        assert_eq!(
            selected_lines(source, first_end, "一\n二".len()),
            ("一\n".len(), "一\n二\n".len())
        );

        // 空の行でも同じ（押した先が空行の改行だった、というのが報告の形）。
        let blank = "あ\n\nい\nう\n";
        let at = "あ\n".len();
        assert_eq!(
            selected_lines(blank, at, blank.len()),
            ("あ\n\n".len(), blank.len())
        );

        // **カーソル1つだけなら、その行。**行末に立っているカーソルは、その行にいる。
        assert_eq!(
            selected_lines(source, first_end, first_end),
            (0, "一\n".len())
        );
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
        assert_eq!(picked, "一");

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

    /// E3の③、書き手の決定 2026-09-12: **本文の行の字下げは継がない。**
    /// 行頭の空白は写せば写すほど広がり、広がった先の行は見出しにならない。
    #[test]
    fn an_indented_body_line_does_not_carry_its_indent() {
        assert_eq!(
            continued("    続きの段落", "    続きの段落".len()),
            Continuation::Insert("\n".to_owned())
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
        // 効かない鍵に見える。**継ぐものは無い**（書き手の決定 2026-09-12）ので、
        // ただの改行になる。
        assert_eq!(continued("    ", 4), Continuation::Insert("\n".to_owned()));
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

        let deeper =
            shift_indent(source, second, second, true, BulletMarks::all()).expect("下げられる");
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
        let back = shift_indent(&next, second, second, false, BulletMarks::all()).expect("戻せる");
        assert_eq!(back.text, source);
    }

    /// 挿入のひな形を当てて、出来上がる本文と、そのあとキャレットが立つ所を見る
    /// （RFN01-38）。
    fn inserted(source: &str, at: (usize, usize), what: InsertEdit) -> Option<(String, usize)> {
        let (region, text, chosen) = insert_edit(source, at.0, at.1, what)?;
        let mut next = source.to_owned();
        next.replace_range(region, &text);
        assert_eq!(
            chosen.0, chosen.1,
            "挿入のあとに選ばれているのはキャレット1つ"
        );
        Some((next, chosen.0))
    }

    /// RFN01-38: **未選択ならリンク先から書く。**`](`の後ろに立つので、既存の
    /// リンク先補完がそのまま出る（書き手の選択 2026-09-21）。
    #[test]
    fn a_link_written_from_nothing_starts_at_its_target() {
        let (next, caret) = inserted("前後", (3, 3), InsertEdit::MarkdownLink).expect("入る");

        assert_eq!(next, "前[]()後");
        // `[`の直後ではなく`(`の直後——次に打つのはリンク先である。
        assert_eq!(caret, "前[".len() + "](".len());
    }

    /// RFN01-38: **選んだ字は表示名になり、キャレットはリンク先へ。**
    #[test]
    fn a_link_keeps_the_chosen_text_for_its_own_name() {
        let (next, caret) = inserted("前東京後", (3, 9), InsertEdit::MarkdownLink).expect("入る");

        assert_eq!(next, "前[東京]()後");
        assert_eq!(caret, "前[東京](".len());
    }

    /// RFN01-38: **Wikiリンクは選んだ字をリンク先にする。**続けて`#`を打てるよう、
    /// キャレットは字の後ろ（`]]`の前）に立つ。
    #[test]
    fn a_wiki_link_puts_the_chosen_text_in_the_target() {
        let (next, caret) = inserted("前東京後", (3, 9), InsertEdit::WikiLink).expect("入る");
        assert_eq!(next, "前[[東京]]後");
        assert_eq!(caret, "前[[東京".len());

        let (empty, caret) = inserted("前後", (3, 3), InsertEdit::WikiLink).expect("入る");
        assert_eq!(empty, "前[[]]後");
        assert_eq!(caret, "前[[".len());
    }

    /// RFN01-38: **別名付きは選んだ字を表示名にする。**書くのはリンク先なので、
    /// キャレットは`[[`の直後（`|`の前）に立つ。
    #[test]
    fn an_aliased_wiki_link_keeps_the_chosen_text_for_its_name() {
        let (next, caret) = inserted("前東京後", (3, 9), InsertEdit::WikiLinkAlias).expect("入る");

        assert_eq!(next, "前[[|東京]]後");
        assert_eq!(caret, "前[[".len());
    }

    /// RFN01-38: **ルビは全角の縦線で書く**（書き手の選択 2026-09-21）。選択なしは
    /// 親文字から、選択ありは読みから書く。
    #[test]
    fn ruby_is_written_with_the_full_width_bar() {
        let (empty, caret) = inserted("前後", (3, 3), InsertEdit::Ruby).expect("入る");
        assert_eq!(empty, "前｜《》後");
        assert_eq!(caret, "前｜".len());

        let (next, caret) = inserted("前東京後", (3, 9), InsertEdit::Ruby).expect("入る");
        assert_eq!(next, "前｜東京《》後");
        assert_eq!(caret, "前｜東京".len());
    }

    /// RFN01-38: **選択の向きは結果を変えない。**逆向きに引いても同じ字が同じ形に
    /// なり、キャレットは同じ所に立つ（書き手の選択 2026-09-21）。
    #[test]
    fn the_direction_of_the_selection_does_not_change_the_template() {
        let forward = inserted("前東京後", (3, 9), InsertEdit::Ruby).expect("入る");
        let backward = inserted("前東京後", (9, 3), InsertEdit::Ruby).expect("入る");

        assert_eq!(forward, backward);
    }

    /// RFN01-38: **字の切れ目に乗っていない範囲は入れない。**数え違いのまま
    /// 切るより、入れないと言うほうがよい。
    #[test]
    fn a_range_that_is_not_on_a_character_boundary_is_refused() {
        let source = "東京";

        assert!(insert_edit(source, 1, 3, InsertEdit::WikiLink).is_none());
        assert!(insert_edit(source, 0, source.len() + 1, InsertEdit::WikiLink).is_none());
        assert!(insert_edit(source, 0, source.len(), InsertEdit::WikiLink).is_some());
    }

    /// [`list_edit`]を、その文書の行の見方で呼ぶ（試験）。
    fn listed(
        source: &str,
        from: usize,
        to: usize,
        what: ListEdit,
        bullet: char,
    ) -> Option<(Range<usize>, String, (usize, usize))> {
        list_edit(source, &line_styles(source), from, to, what, bullet)
    }

    /// E10: **選んだ行が箇条書きになる。**もう一度頼めば外れる（書き手の選択
    /// 2026-09-10）。
    #[test]
    fn the_chosen_lines_take_a_marker_and_give_it_back() {
        let source = "あああ\nいいい\nううう\n";

        let (region, text, chosen) =
            listed(source, 0, source.len(), ListEdit::Bullet, '-').expect("付けられる");
        assert_eq!(region, 0..source.len());
        assert_eq!(text, "- あああ\n- いいい\n- ううう\n");
        // **選んだところは選ばれたまま、足した印も内側**（書き手の報告 2026-09-11）。
        // 続けて`Tab`を押せばそのまま入れ子になる。
        assert_eq!(chosen, (0, text.len()));

        // 全部がもうその印なら、同じ鍵が外す。
        let (_, back, _) =
            listed(&text, chosen.0, chosen.1, ListEdit::Bullet, '-').expect("外せる");
        assert_eq!(back, source);
    }

    /// E10: **番号は選んだ範囲の中で1から**、字下げの桁ごとに数える。範囲の外の
    /// 番号は書き換えない——打っていないところは動かさない（E3の③と同じ）。
    #[test]
    fn numbers_count_from_one_inside_what_was_chosen() {
        let source = "あああ\n    いいい\nううう\n9. そのまま\n";
        let chosen_end = "あああ\n    いいい\nううう\n".len();

        let (_, text, _) =
            listed(source, 0, chosen_end, ListEdit::Ordered, '-').expect("付けられる");

        assert_eq!(text, "1. あああ\n    1. いいい\n2. ううう\n");
    }

    /// E10: **触るのは本文の行と、既に箇条書きの行だけ。**見出し・タスク・空の行は
    /// そのまま残り、コードだけの範囲では何も起きない。
    #[test]
    fn a_heading_a_task_and_an_empty_line_keep_what_they_are() {
        let source = "# 見出し\n\n- [ ] やること\nあああ\n";

        let (_, text, _) =
            listed(source, 0, source.len(), ListEdit::Bullet, '-').expect("本文の行がある");
        assert_eq!(text, "# 見出し\n\n- [ ] やること\n- あああ\n");

        let fenced = "```\nコード\n```\n";
        assert_eq!(
            listed(fenced, 0, fenced.len(), ListEdit::Bullet, '-'),
            None,
            "コードだけなら触れる行が無い"
        );
    }

    /// E10: **印は引用の`>`の後ろに入り、別の印は置き換わる**（`shift_indent`が
    /// 字下げを`>`の後ろへ入れるのと同じ場所）。
    #[test]
    fn a_marker_goes_inside_the_quote_and_replaces_another() {
        let source = "> あああ\n1. いいい\n";

        let (_, text, _) =
            listed(source, 0, source.len(), ListEdit::Bullet, '*').expect("付けられる");

        assert_eq!(text, "> * あああ\n* いいい\n");
    }

    /// E10（書き手の報告 2026-09-11、Panic）: **行末に立っていた端が、次の行の
    /// 印の後ろまで送られてはいけない。**
    ///
    /// 1行ごとに動かした位置を次の行の閾値と比べていたので、積んだぶんだけ前へ
    /// ずれて見え、余分に送られていた。送られた先が字の途中だと、そこで
    /// `source_line_start`が落ちる（診断ログ`panic … not a char boundary`）。
    #[test]
    fn a_position_at_the_end_of_a_line_is_not_carried_into_the_next() {
        // **1行目を短くしておく**（`abc`）。次の行の閾値が近い位置ほど、積んだぶんで
        // 追い越しやすい——長い行では起きないので、ここが起きる形である。
        let source = "abc\n明日は\n昨日は\n";
        let inside = "ab".len();

        let (_, text, chosen) =
            listed(source, inside, source.len(), ListEdit::Bullet, '-').expect("付けられる");

        assert_eq!(text, "- abc\n- 明日は\n- 昨日は\n");
        // 1行目の印の2バイトだけ後ろへ——2行目の印は、この位置より後ろにある。
        assert_eq!(chosen.0, inside + "- ".len());
        assert!(text.is_char_boundary(chosen.0), "字の切れ目に立っている");
    }

    /// E10・E3の④（書き手の報告 2026-09-11）: **選んだ範囲の頭は動かない。**
    /// 足した印も字下げも、選ばれている側の内側に入る——先頭の行だけ印が選択から
    /// 落ちていた。**カーソル1つだけなら動く**（E3の④で通したとおり）。
    #[test]
    fn what_was_added_at_the_head_stays_inside_the_selection() {
        let source = "あああ\nいいい\n";

        // 印を足す側。
        let (_, _, chosen) =
            listed(source, 0, source.len(), ListEdit::Bullet, '-').expect("付けられる");
        assert_eq!(chosen.0, 0, "先頭の印も選ばれている");

        // 字下げの側（同じ`settle`を通る）。
        let deeper =
            shift_indent(source, 0, source.len(), true, BulletMarks::all()).expect("下げられる");
        assert_eq!(deeper.chosen.0, 0, "先頭の字下げも選ばれている");

        // カーソル1つだけなら、字下げの後ろへ出る。
        let caret = shift_indent(source, 0, 0, true, BulletMarks::all()).expect("下げられる");
        assert_eq!(caret.chosen, (INDENT_STEP.len(), INDENT_STEP.len()));
    }

    /// 書き手の決定 2026-09-11: **どれを箇条書きの印として読むかは、3つとも選べる。**
    /// `-`も切れる——「`*`も標準ということなら、扱いは同じであるべき」。
    #[test]
    fn each_mark_can_be_read_or_not() {
        let source = "- ハイフン\n* アスタリスク\n+ プラス\n";

        let all = line_styles_as(source, BulletMarks::all());
        assert!(all[..3].iter().all(|style| style.kind == LineKind::Bullet));

        // `-`だけ読む——Obsidianなどが入れる字だけの原稿と同じ見え方になる。
        let hyphen = BulletMarks::from_said("-");
        let only_hyphen = line_styles_as(source, hyphen);
        assert_eq!(only_hyphen[0].kind, LineKind::Bullet);
        assert_eq!(only_hyphen[1].kind, LineKind::Body);
        assert_eq!(only_hyphen[2].kind, LineKind::Body);

        // **`-`も切れる。**扱いは3つとも同じである。
        let starred = BulletMarks::from_said("*+");
        let without_hyphen = line_styles_as(source, starred);
        assert_eq!(without_hyphen[0].kind, LineKind::Body);
        assert_eq!(without_hyphen[1].kind, LineKind::Bullet);
        assert_eq!(without_hyphen[2].kind, LineKind::Bullet);

        // **全部切ることもできる。**そのとき箇条書きの印は1つも無い。
        let none = BulletMarks::from_said("");
        assert!(
            line_styles_as(source, none)[..3]
                .iter()
                .all(|style| style.kind == LineKind::Body)
        );
        assert_eq!(none.first(), None, "入れる字が無い");
    }

    /// 書き手の決定 2026-09-11: **読まない印には、箱も立たない。**プレビューでも
    /// 本文の1行で、記号はそのまま字として出る——**原稿のバイト列は変えていない。**
    #[test]
    fn a_mark_that_is_not_read_gets_no_box_in_the_preview() {
        let source = "- ハイフン\n* アスタリスク\n";
        let hyphen = Reading {
            ruby: true,
            bullets: BulletMarks::from_said("-"),
        };

        let all = PreviewDocument::from_source_as(source, None, Reading::all());
        let only_hyphen = PreviewDocument::from_source_as(source, None, hyphen);

        assert!(all.markers()[1].is_some(), "読むなら箱が立つ");
        assert!(only_hyphen.markers()[1].is_none(), "読まないなら立たない");
        assert!(only_hyphen.markers()[0].is_some(), "読む`-`は立つ");
        assert_eq!(all.text, only_hyphen.text, "本文は1字も変わらない");
    }

    /// 書き手の決定 2026-09-11: **入れるのは、読む記号のいちばん前のもの**（案C）。
    #[test]
    fn the_key_writes_a_mark_that_will_be_read() {
        assert_eq!(BulletMarks::all().first(), Some('-'));
        assert_eq!(BulletMarks::from_said("*+").first(), Some('*'));
        assert_eq!(BulletMarks::from_said("+").first(), Some('+'));
        assert_eq!(BulletMarks::from_said("").first(), None);
        // 設定ファイルの知らない字は読み飛ばす。
        assert_eq!(BulletMarks::from_said("・-").as_said(), "-");
    }

    /// E10の③（書き手の選択 2026-09-11、案C）: **頼まれた記号にそろえる→外す。**
    /// 記号ごとに画面の印が違うので、**どの記号で書くかは選ぶもの**である。
    #[test]
    fn the_asked_for_mark_is_what_the_lines_end_up_wearing() {
        let starred = "* 一つめ\n* 二つめ\n";

        // `-`の箇条書きを頼まれた——`*`の行はその記号にそろう。
        let (_, hyphened, chosen) =
            listed(starred, 0, starred.len(), ListEdit::Bullet, '-').expect("そろえられる");
        assert_eq!(hyphened, "- 一つめ\n- 二つめ\n");

        // もう一度同じことを頼まれれば、もうそろっているので外れる。
        let (_, off, _) =
            listed(&hyphened, chosen.0, chosen.1, ListEdit::Bullet, '-').expect("外せる");
        assert_eq!(off, "一つめ\n二つめ\n");

        // **読むほうは3つとも同格のまま**（`*`の行も箇条書きである）。
        assert_eq!(line_styles(starred)[0].kind, LineKind::Bullet);
    }

    /// E10（書き手の報告 2026-09-11）: **空行を挟んだ項目も1つの連なり。**
    ///
    /// 書き手が選んだのは空行で区切られた行の並びで、**印を付ける側は1から順に
    /// 番号を振った**のに、**数え直す側は空行で止まって「何も変わらない」と言った**
    /// ——同じ文書について2つの数え方があった。
    #[test]
    fn items_across_a_blank_line_are_one_run() {
        let source = "あああ\n\nいいい\n\nううう\n";

        // 印を付ける側——空行は飛ばし、番号は1から続く。
        let (_, made, _) = listed(source, 0, source.len(), ListEdit::Ordered, '-').expect("付く");
        assert_eq!(made, "1. あああ\n\n2. いいい\n\n3. ううう\n");

        // 数え直す側——先頭を`5.`に打ち直せば、空行を跨いで下が続く。
        let typed = made.replacen("1. ", "5. ", 1);
        let (region, evened, _) = listed(&typed, 0, 0, ListEdit::Renumber, '-').expect("そろう");
        assert_eq!(evened, "5. あああ\n\n6. いいい\n\n7. ううう\n");
        // 連なりの後ろの空行は、数え直した範囲の外。
        assert_eq!(region.end, typed.len());
    }

    /// E10（書き手の求め 2026-09-11）: **`Ctrl+Shift+7`は「そろえる→外れる」の
    /// 階段。**開始数字を変えるのは、先頭を打ち直してこの鍵を押すことである。
    #[test]
    fn the_numbered_key_evens_the_run_before_it_takes_it_off() {
        let source = "5. あああ\n2. いいい\n3. ううう\n";

        // 一回目——先頭の数字から下がそろい、数え直した連なりが選ばれる。
        let (region, evened, chosen) =
            listed(source, 0, 0, ListEdit::Ordered, '-').expect("そろえられる");
        assert_eq!(evened, "5. あああ\n6. いいい\n7. ううう\n");
        assert_eq!(region, 0..source.len());
        // 最後の改行は選びに入れない（カーソルを次の行へ出さない）。
        assert_eq!(chosen, (0, evened.len() - "\n".len()));

        // 二回目——もう変わらないので、選ばれている行の印が外れる。
        let (_, off, _) =
            listed(&evened, chosen.0, chosen.1, ListEdit::Ordered, '-').expect("外せる");
        assert_eq!(off, "あああ\nいいい\nううう\n");
    }

    /// E10の④（書き手の決定 2026-09-11、CommonMark §5.3）: **記号を変えると、そこから
    /// 別のリストが始まる。**他のツールはそこで連なりを切り、番号も数え直す
    /// ——この編集器も同じにする。
    #[test]
    fn changing_the_mark_starts_a_new_list() {
        // 区切りが`.`から`)`へ変わる——丙は3つめではなく、その連なりの1つめ。
        let mixed = "1. 甲\n2. 乙\n1) 丙\n5) 丁\n";

        // `Tab`の数え直し（`renumber_around`）：丙は自分の番号のまま、丁が続く。
        let last = mixed.find("5) 丁").expect("ある");
        let deeper = shift_indent(mixed, last, last, true, BulletMarks::all()).expect("下げられる");
        assert_eq!(deeper.text, "1. 甲\n2. 乙\n1) 丙\n    1) 丁\n");

        // `Renumber`：種類の変わるところで止まる。**丙と丁には触れない。**
        let typed = "5. 甲\n2. 乙\n1) 丙\n5) 丁\n";
        let (region, text, _) = listed(typed, 0, 0, ListEdit::Renumber, '-').expect("数え直せる");
        assert_eq!(text, "5. 甲\n6. 乙\n");
        assert_eq!(region.end, "5. 甲\n2. 乙\n".len(), "丙から先は別のリスト");
    }

    /// E10の④: **印の字が変われば、箇条書きでも別のリスト**（§5.3）。番号を持たない
    /// 項目も1つと数えるので、そこが切れることは数にも効く。
    #[test]
    fn a_different_bullet_is_a_different_list_too() {
        let mixed = "- 甲\n* 乙\n";

        assert_eq!(item_type("- 甲", line_styles(mixed)[0]), Some('-'));
        assert_eq!(item_type("* 乙", line_styles(mixed)[1]), Some('*'));
        // 番号の区切りも同じ問いで答える。
        let ordered = "1. 甲\n2) 乙\n";
        assert_eq!(item_type("1. 甲", line_styles(ordered)[0]), Some('.'));
        assert_eq!(item_type("2) 乙", line_styles(ordered)[1]), Some(')'));
        // 印を持たない行は種類を持たない。
        assert_eq!(item_type("本文", LineStyle::default()), None);
    }

    /// E10の②: **この行の番号から、下を数え直す**（書き手の選択 2026-09-11）。
    /// 上の行は触らず、連なりの外で止まる。
    #[test]
    fn renumbering_starts_at_the_number_that_was_typed() {
        let source = "1. 一\n5. 二\n1. 三\n本文\n9. 別のリスト\n";
        let second = "1. 一\n".len();

        let (region, text, _) =
            listed(source, second, second, ListEdit::Renumber, '-').expect("数え直せる");

        assert_eq!(text, "5. 二\n6. 三\n");
        assert_eq!(region, second.."1. 一\n5. 二\n1. 三\n".len());
    }

    /// E10の②: **深さごとに数える。**内側はその連なりの先頭の番号から始まり、
    /// 外へ戻れば外側の続きになる（`renumber_around`と同じ数え方）。
    #[test]
    fn renumbering_counts_each_depth_on_its_own() {
        let source = "1. 一\n    3. 内側\n    9. 内側の次\n5. 外へ戻る\n";

        let (_, text, _) = listed(source, 0, 0, ListEdit::Renumber, '-').expect("数え直せる");

        assert_eq!(text, "1. 一\n    3. 内側\n    4. 内側の次\n2. 外へ戻る\n");
    }

    /// E10の②: **箇条書きの外では何も起きない。**呼ぶ側がそう言えるように`None`。
    #[test]
    fn renumbering_outside_a_list_does_nothing() {
        let source = "本文です\n1. 一\n";

        assert_eq!(listed(source, 0, 0, ListEdit::Renumber, '-'), None);
    }

    /// E10（書き手の報告 2026-09-11、Panic）: **どこを選んでも、選び直す範囲は
    /// 字の切れ目に立つ。**
    ///
    /// 本物の原稿（`testdata/01_行属性.md`——見出し・箇条書き・番号・タスク・引用・
    /// コード・入れ子が全部ある）の上を、**後ろ向きの選択も含めて**総当たりする。
    /// 落ちた場所は`source_line_start`だったが、**原因は数え方**なので、確かめる
    /// のはここである。
    #[test]
    fn every_selection_settles_on_a_character_boundary() {
        let source = include_str!("../testdata/01_行属性.md");
        let styles = line_styles(source);
        let boundary = |byte: usize| crate::floor_char_boundary(source, byte.min(source.len()));

        for what in [ListEdit::Bullet, ListEdit::Ordered, ListEdit::Renumber] {
            for head in (0..source.len()).step_by(5) {
                // 前向き・後ろ向き・カーソルだけの3つ。行末も行頭も通る。
                for tail in [head, head + 37, head.saturating_sub(83)] {
                    let (from, to) = (boundary(head), boundary(tail));
                    let edit = list_edit(source, &styles, from, to, what, '-');
                    let Some((region, text, chosen)) = edit else {
                        continue;
                    };
                    let mut next = source.to_owned();
                    next.replace_range(region, &text);
                    assert!(
                        next.is_char_boundary(chosen.0) && next.is_char_boundary(chosen.1),
                        "{what:?} {from}..{to} が字の途中へ立った（{chosen:?}）"
                    );
                }
            }
        }
    }

    /// E3の④（E10で見つけた同じ傷、2026-09-11）: **`Tab`でも、行末に立っていた端が
    /// 次の行の字下げまで送られてはいけない。**数えるのは編集の前の座標である。
    #[test]
    fn indenting_does_not_carry_a_line_end_into_the_next_line() {
        let source = "abc\n明日は\n";
        let inside = "ab".len();

        let deeper = shift_indent(source, inside, source.len(), true, BulletMarks::all())
            .expect("下げられる");

        assert_eq!(deeper.text, "    abc\n    明日は\n");
        // 1行目の字下げの4桁だけ後ろへ——2行目の字下げは、この位置より後ろ。
        assert_eq!(deeper.chosen.0, inside + INDENT_STEP.len());
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
        let deeper =
            shift_indent(source, second, second, true, BulletMarks::all()).expect("下げられる");
        assert_eq!(deeper.text, "1. 一\n    1. 二\n2. 三\n");

        // 3つめも内側へ——内側の連なりの続きになる。
        let third = third + INDENT_STEP.len();
        let deeper =
            shift_indent(&deeper.text, third, third, true, BulletMarks::all()).expect("下げられる");
        assert_eq!(deeper.text, "1. 一\n    1. 二\n    2. 三\n");

        // 戻せば、外の連なりの続きへ。
        let back =
            shift_indent(&deeper.text, third, third, false, BulletMarks::all()).expect("戻せる");
        assert_eq!(back.text, "1. 一\n    1. 二\n2. 三\n");
    }

    /// E3の④（2026-09-11に直した）: **空行は連なりを切らない。**
    ///
    /// **もとは「空行を挟めば別のリスト」と書いてあった**が、それは要件のどこにも
    /// 無い決まりで、Markdown自身は逆を言う——項目のあいだが空いていても1つの
    /// リスト（緩いリスト）である。**切らないほうを正にした**のは、後から箇条書きに
    /// する道（E10）が空行を跨いで1から数えるからで、**同じ文書に2つの数え方が
    /// あってはならない**（書き手の報告 2026-09-11：番号を振った直後に数え直しが
    /// 「何も変わらない」と言った）。
    #[test]
    fn a_blank_line_does_not_end_the_run_of_numbers() {
        let source = "1. 甲\n2. 乙\n\n1. 丙\n2. 丁\n";
        let last = source.find("2. 丁").expect("ある");

        let deeper =
            shift_indent(source, last, last, true, BulletMarks::all()).expect("下げられる");

        // 丙は3つめの項目である——空行の前の2つから続いている。
        assert_eq!(deeper.text, "1. 甲\n2. 乙\n\n3. 丙\n    1. 丁\n");
    }

    /// E3の④: 選んだ行はまとめて。**空の行は下げない**——そこで箇条書きが切れる。
    #[test]
    fn every_selected_line_moves_together_except_the_empty_ones() {
        let source = "一\n\n二\n";

        let deeper =
            shift_indent(source, 0, source.len(), true, BulletMarks::all()).expect("下げられる");

        assert_eq!(deeper.text, "    一\n\n    二\n");
    }

    /// E3の④: **字下げは引用の`>`の後ろ。**前に入れると引用そのものが崩れる。
    #[test]
    fn an_indent_goes_after_the_quote_marker() {
        let source = "> - 項目\n";

        let deeper = shift_indent(source, 0, 0, true, BulletMarks::all()).expect("下げられる");

        assert_eq!(deeper.text, ">     - 項目\n");
        assert_eq!(line_styles(&deeper.text)[0].quote_depth, 1);
    }

    /// E3の④: **外せる字下げが無ければ、何も起きない**（`None`）。
    #[test]
    fn a_line_at_the_margin_has_nothing_to_give_back() {
        assert_eq!(
            shift_indent("項目\n", 0, 0, false, BulletMarks::all()),
            None
        );
        // タブ1つは一段とみなす（他の道具で書かれた原稿）。
        let taken = shift_indent("\t項目\n", 0, 0, false, BulletMarks::all()).expect("外せる");
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

    /// E3の③（書き手の決定 2026-09-10）: **項目の続きの段落でEnterを押すと、
    /// 次の項目が出る。**`Shift+Enter`が「まだこの項目」と言う鍵で、`Enter`は
    /// いつでも「次の項目へ」——中身があってもなくても同じである。
    #[test]
    fn a_paragraph_under_an_item_opens_the_next_item() {
        let source = "1. aaa\n   ";
        let at = source.len();

        // **改行して次の項目**——空いた行はそのまま残るので、一行空く。
        assert_eq!(
            enter_continuation(source, &line_styles(source), at, false),
            Continuation::Insert("\n2. ".to_owned())
        );
        // **書きかけの段落からでも同じ。**Shift+Enterを重ねて書いた続きの行で
        // Enterを押した書き手は、その項目を書き終えている。
        let written = "1. aaa\n   bbb";
        assert_eq!(
            enter_continuation(written, &line_styles(written), written.len(), false),
            Continuation::Insert("\n2. ".to_owned())
        );
        // Shift+Enterのほうは、段落を続ける。
        assert_eq!(
            enter_continuation(written, &line_styles(written), written.len(), true),
            Continuation::Insert("\n   ".to_owned())
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

        // **字下げも継がない**（書き手の決定 2026-09-12）：リストの外の字下げは
        // 項目の形ではなく、ただ行頭に空白があるだけの本文である。
        assert_eq!(
            enter_continuation(source, &line_styles(source), source.len(), false),
            Continuation::Insert("\n".to_owned())
        );
    }

    /// 書き手の決定 2026-09-12: **項目の続きの段落は、これまでどおり継ぐ。**
    /// そこでの字下げは書き手が打ったものではなく、項目の形そのものである。
    #[test]
    fn a_paragraph_under_an_item_still_carries_its_indent() {
        let source = "- 項目\n  続きの段落";

        assert_eq!(
            enter_continuation(source, &line_styles(source), source.len(), true),
            Continuation::Insert("\n  ".to_owned())
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
        preview_of_as(line, Reading::all())
    }

    /// 同じことを、**記法を読むかどうかを言われて**（要件 E9）。
    fn preview_of_as(line: &str, reading: Reading) -> (String, Vec<Emphasis>) {
        let mut visible = String::new();
        let mut marks = Vec::new();
        push_visible_line(
            line,
            line_styles(line)[0],
            &mut visible,
            &mut marks,
            reading,
        );
        (visible, marks)
    }

    fn label(marks: Marks) -> &'static str {
        match marks {
            Marks { bold: true, .. } => "bold",
            Marks { italic: true, .. } => "italic",
            Marks { strike: true, .. } => "strike",
            Marks { code: true, .. } => "code",
            Marks { beside, .. } if !beside.is_none() => "dots",
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

    /// E9: **切ったら、記号をそのまま字として出す。**
    ///
    /// `｜漢字《かんじ》`も`《《傍点》》`も`［＃「…」に傍点］`もMarkdownの標準では
    /// ないので、切れなければ「この編集器でしか正しく見えない書き方」を書き手に
    /// 強いることになる。**原稿のバイト列は元から変えていない**——読むのをやめる
    /// だけで、`｜`も`《》`も本文の1字として組まれる。
    #[test]
    fn the_notation_shows_as_letters_when_it_is_not_read() {
        for line in [
            "｜漢字《かんじ》を書く",
            "|漢字《かんじ》を書く",
            "漢字《かんじ》を書く",
            "これは《《本当に》》おかしい",
            "これは本当におかしい［＃「本当に」に傍点］",
        ] {
            let (visible, marks) = preview_of_as(
                line,
                Reading {
                    ruby: false,
                    ..Reading::all()
                },
            );
            assert_eq!(visible, line, "そのまま出る");
            assert!(marks.is_empty(), "{line}: {marks:?}");
        }
    }

    /// E9: **入っていれば、いままでどおり読む。**同じ行の答えが旗で分かれる、
    /// というのがこの設定の全部である。
    #[test]
    fn the_notation_is_still_read_when_it_is_on() {
        let (visible, marks) = preview_of_as("｜漢字《かんじ》を書く", Reading::all());
        assert_eq!(visible, "漢字《かんじ》を書く");
        assert!(
            marks
                .iter()
                .any(|mark| matches!(mark.ornament, Some(Ornament::Ruby { .. })))
        );

        let (_, dots) = preview_of_as("これは《《本当に》》おかしい", Reading::all());
        assert!(dots.iter().any(|mark| mark.marks.beside == Beside::Dot));
    }

    /// 要件 7.8（2026-09-16、書き手「組版の表現拡大」）: 体裁の注記が字下げを言う。
    #[test]
    fn a_note_indents_the_lines_it_covers() {
        let source = "地の文。\n［＃ここから2字下げ］\n手紙の一行目。\n手紙の二行目。\n                      ［＃ここで字下げ終わり］\n地の文へ戻る。\n［＃1字下げ］この行だけ。\n";
        let styles = line_styles(source);
        let indents = styles
            .iter()
            .map(|style| (style.kind, style.indent_cells()))
            .collect::<Vec<_>>();
        assert_eq!(
            indents,
            vec![
                (LineKind::Body, 0),
                (LineKind::Note, 0),
                (LineKind::Body, 2),
                (LineKind::Body, 2),
                (LineKind::Note, 0),
                (LineKind::Body, 0),
                (LineKind::Body, 1),
                (LineKind::Body, 0),
            ]
        );
        // 指示の行は字を隠す（`---`やフェンスと同じ全行の箱）。行そのものは残る。
        let preview = PreviewDocument::from_source(source);
        assert!(
            preview.markers()[1].is_some(),
            "指示の行が隠れていない: {:?}",
            preview.markers()[1]
        );
        // 行の頭の`［＃1字下げ］`は字としては消える。
        assert_eq!(
            preview.text.lines().nth(6),
            Some("この行だけ。"),
            "{}",
            preview.text
        );
        // 地付きと改ページ（2026-09-16）。
        let source =
            "地の文。\n［＃地付き］署名。\n［＃地から2字上げ］結び。\n［＃改ページ］\n次の章。\n";
        let styles = line_styles(source);
        assert_eq!(styles[1].tail_cells, Some(0), "地付き");
        assert_eq!(styles[2].tail_cells, Some(2), "地から2字上げ");
        assert_eq!(styles[3].kind, LineKind::PageBreak);
        let preview = PreviewDocument::from_source(source);
        assert_eq!(preview.text.lines().nth(1), Some("署名。"));
        assert_eq!(preview.text.lines().nth(2), Some("結び。"));
        assert!(preview.markers()[3].is_some(), "改ページの行が隠れていない");

        // 読み方を切れば、どれも本文の字のまま——字下げもしない。
        let plain = line_styles_reading(
            source,
            Reading {
                ruby: false,
                ..Reading::all()
            },
        );
        assert!(plain.iter().all(|style| style.indent_cells() == 0));
        assert!(plain.iter().all(|style| style.kind != LineKind::Note));
    }

    /// 要件 7.8（2026-09-16、書き手「組版の表現拡大」）: 注記は印の種類を名指す。
    #[test]
    fn a_note_names_which_mark_goes_beside_the_word() {
        let beside = |line: &str| {
            let (visible, marks) = preview_of(line);
            let beside = marks
                .iter()
                .map(|mark| mark.marks.beside)
                .find(|beside| !beside.is_none());
            (visible, beside)
        };
        assert_eq!(
            beside("彼は来た［＃「来た」に傍点］"),
            ("彼は来た".to_owned(), Some(Beside::Dot))
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に丸傍点］").1,
            Some(Beside::Solid)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に白丸傍点］").1,
            Some(Beside::Open)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に二重丸傍点］").1,
            Some(Beside::Double)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」にゴマ傍点］").1,
            Some(Beside::Sesame)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に×傍点］").1,
            Some(Beside::Cross)
        );
        assert_eq!(beside("彼は来た［＃「来た」に傍線］").1, Some(Beside::Line));
        // 線の種類（2026-09-17、書き手「組版の表現拡大」②）。
        assert_eq!(
            beside("彼は来た［＃「来た」に二重傍線］").1,
            Some(Beside::DoubleLine)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に波線］").1,
            Some(Beside::WaveLine)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に鎖線］").1,
            Some(Beside::ChainLine)
        );
        assert_eq!(
            beside("彼は来た［＃「来た」に破線］").1,
            Some(Beside::DashLine)
        );
        // 知らない注記は本文のまま。
        assert_eq!(
            beside("彼は来た［＃改ページ］"),
            ("彼は来た［＃改ページ］".to_owned(), None)
        );
        // 読み方を切れば、注記も字のまま出る。
        let (visible, marks) = preview_of_as(
            "彼は来た［＃「来た」に丸傍点］",
            Reading {
                ruby: false,
                ..Reading::all()
            },
        );
        assert_eq!(visible, "彼は来た［＃「来た」に丸傍点］");
        assert!(marks.iter().all(|mark| mark.marks.beside.is_none()));
    }

    /// 要件 7.8（2026-09-17、書き手「組版の表現拡大」②）: **右にルビ、左に注。**
    ///
    /// 注記は文のどこからでも前の語を名指すので、箱は親文字までの距離を持つ
    /// （`Ornament::LeftNote`）。読みはルビと同じく本文に居残り、箱が隠す。
    #[test]
    fn a_note_can_stand_on_the_left_of_a_word() {
        let (visible, marks) =
            preview_of("彼は東京へ行った［＃「東京」の左に「とうきょう」の注記］");

        // 注記の記法は消え、左に出す字だけが居残る。
        assert_eq!(visible, "彼は東京へ行ったとうきょう");
        let note = marks
            .iter()
            .find_map(|mark| match mark.ornament {
                Some(Ornament::LeftNote {
                    back_utf16,
                    base_utf16,
                }) => Some((mark.utf16_start, mark.utf16_len, back_utf16, base_utf16)),
                _ => None,
            })
            .expect("左の注記が読まれていない");
        // 読みは8文字目から5字。親文字「東京」は2文字目からの2字なので、距離は6。
        assert_eq!(note, (8, 5, 6, 2));

        // 同じ語に右のルビと左の注を両方。
        let (visible, marks) =
            preview_of("｜東京《とうきょう》［＃「東京」の左に「とうけい」の注記］");
        assert_eq!(visible, "東京《とうきょう》とうけい");
        assert_eq!(
            marks
                .iter()
                .filter(|mark| mark.ornament.is_some_and(Ornament::rides_beside_the_line))
                .count(),
            2
        );

        // 指す先が無ければ注記ごと消える（傍点の注記と同じ）。
        assert_eq!(
            preview_of("彼は来た［＃「京都」の左に「きょうと」の注記］").0,
            "彼は来た"
        );
    }

    /// 要件 7.8（2026-09-17、書き手「組版の表現拡大」①③）: 範囲を囲む注記。
    ///
    /// **縦中横と割り注は箱が字を覆い**（描くときに読み出す）、**文字の大きさは旗**
    /// （中は普通の本文としてそのまま入れ子になる）。
    #[test]
    fn a_range_note_says_how_its_own_stretch_is_set() {
        let ornament = |line: &str| {
            let (visible, marks) = preview_of(line);
            (visible, marks.iter().find_map(|mark| mark.ornament))
        };

        assert_eq!(
            ornament("第［＃縦中横］10［＃縦中横終わり］章"),
            ("第10章".to_owned(), Some(Ornament::Upright))
        );
        // 自動の規則が拾わない字も、名指せば立つ。
        assert_eq!(
            ornament("［＃縦中横］ABC［＃縦中横終わり］").1,
            Some(Ornament::Upright)
        );
        // 割注は中の字数で場所を取る。「注記です」は4マス、半分ずつで1マスぶん。
        assert_eq!(
            ornament("本文［＃割り注］注記です［＃割り注終わり］の続き"),
            (
                "本文注記ですの続き".to_owned(),
                Some(Ornament::Warichu { cells_x10: 10 })
            )
        );

        // 文字の大きさは箱ではなく旗——中の太字もそのまま読む。
        let (visible, marks) =
            preview_of("ここは［＃小さな文字］**細かい**話［＃小さな文字終わり］です");
        assert_eq!(visible, "ここは細かい話です");
        assert!(marks.iter().any(|mark| mark.marks.bold));
        assert_eq!(
            marks
                .iter()
                .map(|mark| mark.marks.scale)
                .find(|scale| *scale != TextScale::Normal),
            Some(TextScale::Small)
        );
        assert_eq!(
            preview_of("［＃大きな文字］大見得［＃大きな文字終わり］")
                .1
                .iter()
                .map(|mark| mark.marks.scale)
                .find(|scale| *scale != TextScale::Normal),
            Some(TextScale::Large)
        );

        // **閉じないものは注記ではない。**字のまま本文に残る。
        assert_eq!(
            ornament("［＃縦中横］10章"),
            ("［＃縦中横］10章".to_owned(), None)
        );
        // 読み方を切れば、どれも字のまま。
        assert_eq!(
            preview_of_as(
                "第［＃縦中横］10［＃縦中横終わり］章",
                Reading {
                    ruby: false,
                    ..Reading::all()
                },
            )
            .0,
            "第［＃縦中横］10［＃縦中横終わり］章"
        );
    }

    /// E9: **旗が変われば、取っておいた行は全部使えない。**行を取っておく条件は
    /// 本文と組み方と活性行で、そこに読み方は入っていない——気づけないまま使えば
    /// 「切り替えても画面が変わらない」になる。
    #[test]
    fn changing_how_the_notation_is_read_rebuilds_what_was_kept() {
        let source = "｜漢字《かんじ》を書く
";
        let mut preview = PreviewDocument::default();
        preview.refresh(source, None, Reading::all());
        assert_eq!(
            preview.text,
            "漢字《かんじ》を書く
"
        );
        preview.refresh(
            source,
            None,
            Reading {
                ruby: false,
                ..Reading::all()
            },
        );
        assert_eq!(preview.text, source);
        preview.refresh(source, None, Reading::all());
        assert_eq!(
            preview.text,
            "漢字《かんじ》を書く
"
        );

        // 数え方も同じ——**記法をやめれば`｜`も本文の1字**である。
        let mut counts = DocumentCounts::default();
        counts.refresh(source, Reading::all());
        let read = counts.stats();
        counts.refresh(
            source,
            Reading {
                ruby: false,
                ..Reading::all()
            },
        );
        let literal = counts.stats();
        assert!(literal.body_characters > read.body_characters);
        assert_eq!(literal.ruby_characters, 0);
        // **記法をやめれば、本文は原文そのもの**——削るものが1字も無い。
        assert_eq!(literal.body_characters, literal.source_characters);
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

    /// 書き手の判断 2026-09-15: **太字などは標準どおり、同じ段落の中なら改行をまたぐ。閉じなければ太字にしない。**
    ///
    /// 空行・見出し・箇条書きで段落が切れれば、またがない。閉じる記号を打った瞬間に、前の行まで太字になる。
    #[test]
    fn emphasis_spans_the_lines_of_one_paragraph() {
        let bold_over = |preview: &PreviewDocument, line: usize, text: &str| {
            let shown = preview.text.split('\n').nth(line).unwrap();
            let start = shown[..shown.find(text).unwrap()].encode_utf16().count() as u32;
            let len = text.encode_utf16().count() as u32;
            preview.marks()[line].iter().any(|mark| {
                mark.marks.bold
                    && mark.utf16_start <= start
                    && start + len <= mark.utf16_start + mark.utf16_len
            })
        };
        // 3行にまたがる太字：記号は消え、3行とも太字。
        let source = "前の段落\n\n**ここから\n途中\nここまで**の後\n";
        let preview = PreviewDocument::from_source(source);
        assert_eq!(preview.text, "前の段落\n\nここから\n途中\nここまでの後\n");
        assert!(bold_over(&preview, 2, "ここから"));
        assert!(bold_over(&preview, 3, "途中"));
        assert!(bold_over(&preview, 4, "ここまで"));
        assert!(!bold_over(&preview, 4, "の後"));

        // 閉じなければ、字のまま（後ろの行も太字にならない）。
        let open = "**閉じない\n次の行\n";
        let preview = PreviewDocument::from_source(open);
        assert_eq!(preview.text, open);
        assert!(
            preview
                .marks()
                .iter()
                .all(|marks| marks.iter().all(|mark| !mark.marks.bold))
        );

        // 空行・見出し・箇条書きで段落が切れれば、またがない。
        for broken in ["**前\n\n後**\n", "**前\n# 見出し**\n", "**前\n- 項目**\n"] {
            let preview = PreviewDocument::from_source(broken);
            assert!(
                preview.text.starts_with("**前"),
                "{broken:?} → {:?}",
                preview.text
            );
        }

        // 取消線の中に太字、行をまたいで入れ子（`*`と`**`が並ぶ入れ子は、この読み方では区別しない）。
        let nested = "~~取消の**太字\nまだ太字**取消~~\n";
        let preview = PreviewDocument::from_source(nested);
        assert_eq!(preview.text, "取消の太字\nまだ太字取消\n");
        assert!(bold_over(&preview, 1, "まだ太字"));

        // 編集中の行：記号は見えたまま、持ち越した太字で組む。
        let second = "**ここから\n".len();
        let editing = "**ここから\n途中の行\nここまで**\n";
        let preview = PreviewDocument::from_source_with_active_line(editing, Some(second));
        assert!(preview.text.contains("\n途中の行\n"));
        assert!(bold_over(&preview, 1, "途中の行"));

        // 1字ずつ打って閉じたとき、取っておいた行も組み直される。
        let mut preview = PreviewDocument::from_source("**ここから\n途中\nここまで\n");
        assert!(!bold_over(&preview, 1, "途中"));
        preview.refresh("**ここから\n途中\nここまで**\n", None, Reading::all());
        assert_eq!(preview.text, "ここから\n途中\nここまで\n");
        assert!(bold_over(&preview, 1, "途中"));
        assert_eq!(
            preview.text,
            visible_markdown_text_with_active_line(
                "**ここから\n途中\nここまで**\n",
                None,
                Reading::all()
            )
        );
        // ステータスバーの本文の字数も、またいだ記号を数えない（1行ずつ数えるほうと素朴なほうが一致する）。
        let mut counts = DocumentCounts::default();
        counts.refresh("**ここから\n途中\nここまで\n", Reading::all());
        counts.refresh("**ここから\n途中\nここまで**\n", Reading::all());
        let stats = counts.stats();
        assert_eq!(
            stats,
            DocumentStats::from_source("**ここから\n途中\nここまで**\n")
        );
        assert_eq!(
            stats.body_characters,
            "ここから\n途中\nここまで\n".chars().count()
        );
    }

    /// 書き手の求め 2026-09-15（表のまま編集）: ←→は`|`と余白を跨いでセルからセルへ、`Tab`は次のセルの頭へ。
    #[test]
    fn the_caret_walks_a_table_cell_by_cell() {
        let source = "前\n| 名前 | 役割 |\n| --- | --- |\n| a |  | c |\n後";
        let styles = line_styles(source);
        let at = |needle: &str| source.find(needle).unwrap();
        let name = at("名前");
        let name_end = name + "名前".len();
        let role = at("役割");
        // セルの中と両端は止まり場所。
        assert_eq!(table_step(source, &styles, name + 3, true), None);
        assert_eq!(table_step(source, &styles, name_end, true), None);
        // 端の次（余白）からは、隣のセルの頭／前のセルの終わりへ。
        assert_eq!(table_step(source, &styles, name_end + 1, true), Some(role));
        assert_eq!(table_step(source, &styles, role - 1, false), Some(name_end));
        // 行の頭の`|`は、前へ行けば前の行の終わり、後ろへ行けば最初のセル。
        let row = at("| 名前");
        assert_eq!(table_step(source, &styles, row, true), Some(name));
        assert_eq!(table_step(source, &styles, row, false), Some(row - 1));
        // 行を閉じる`|`の後ろへ進めば、次の行の頭。
        let role_end = role + "役割".len();
        assert_eq!(
            table_step(source, &styles, role_end + 1, true),
            Some(at("| --- |"))
        );
        // 途中の空のセルにも立てる（`|  |`の2つめの空白の後ろ）。
        let empty = at("|  |") + 3;
        assert_eq!(table_step(source, &styles, empty, true), None);
        // 表の外は知らない。
        assert_eq!(table_step(source, &styles, 0, true), None);

        // Tab：次のセル、行の終わりからは区切り行を跨いで次の行の最初のセル、表の終わりでは動かない。
        assert_eq!(table_tab(source, &styles, name, false), Some(role));
        assert_eq!(table_tab(source, &styles, role, false), Some(at("a |")));
        let c = at("c |");
        assert_eq!(table_tab(source, &styles, c, false), Some(c));
        // Shift+Tab：セルの途中ならそのセルの頭、頭なら前のセル、行の頭なら前の表の行の最後のセル。
        assert_eq!(table_tab(source, &styles, role + 3, true), Some(role));
        assert_eq!(table_tab(source, &styles, role, true), Some(name));
        assert_eq!(table_tab(source, &styles, at("a |"), true), Some(role));
        assert_eq!(table_tab(source, &styles, 0, false), None, "not in a table");
    }

    /// 書き手の求め 2026-09-15: **編集中の行も、記号を見せたまま太字などの書式で組む。**
    ///
    /// 印は原文の位置の、記号の内側の字に付く。ルビの読みを覆う箱は編集中には付けない（読みも見せる）。
    #[test]
    fn the_active_line_keeps_its_emphasis_with_the_markers_showing() {
        let source = "前の行\n本文**太字**と*斜体*、~~消し~~、`code`、｜漢字《かんじ》\n";
        let second_line = "前の行\n".len();
        let preview = PreviewDocument::from_source_with_active_line(source, Some(second_line));
        let line = "本文**太字**と*斜体*、~~消し~~、`code`、｜漢字《かんじ》";
        assert!(preview.text.contains(line), "the markers stay");
        let at = |needle: &str| line[..line.find(needle).unwrap()].encode_utf16().count() as u32;
        let marks = &preview.marks()[1];
        let found = |start: u32, len: u32| {
            marks
                .iter()
                .find(|mark| mark.utf16_start == start && mark.utf16_len == len)
                .map(|mark| mark.marks)
        };
        assert!(
            found(at("太字"), 2).is_some_and(|marks| marks.bold),
            "{marks:?}"
        );
        assert!(
            found(at("斜体"), 2).is_some_and(|marks| marks.italic),
            "{marks:?}"
        );
        assert!(
            found(at("消し"), 2).is_some_and(|marks| marks.strike),
            "{marks:?}"
        );
        assert!(
            found(at("code"), 4).is_some_and(|marks| marks.code),
            "{marks:?}"
        );
        assert!(
            marks.iter().all(|mark| mark.ornament.is_none()),
            "no box hides the ruby reading while editing"
        );
        // 離れれば、今までどおり記号の無い整形表示。
        let preview = PreviewDocument::from_source_with_active_line(source, None);
        assert!(preview.text.contains("本文太字と斜体、消し、code、"));
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

        preview.refresh(source, Some(0), Reading::all());
        assert_eq!(
            preview.markers()[0].expect("箱はある").ornament,
            Ornament::Markup
        );

        preview.refresh(source, Some("- 箇条書き\n".len()), Reading::all());
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
        counts.refresh(before, Reading::all());
        assert_eq!(counts.stats(), DocumentStats::from_source(before));

        let after = "```\n本文\n**強調**";
        counts.refresh(after, Reading::all());
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
        counts.refresh(&source, Reading::all());
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
            counts.refresh(&source, Reading::all());

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
        counts.refresh(&source, Reading::all());
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
        counts.refresh(source, Reading::all());

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
            preview.refresh(&source, active, Reading::all());
            assert_lookups_match_a_linear_scan(&preview, &source);
            assert_eq!(
                preview.text,
                visible_markdown_text_with_active_line(&source, active, Reading::all()),
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

    /// An item is set in one step (2字) for being an item and one more for each
    /// level it is under, and a quoted one is set in by both (要件 7.3.2).
    #[test]
    fn a_nested_item_asks_its_block_for_a_step_a_level() {
        let styles = line_styles("- 一\n  - 二\n> - 三\n");

        assert_eq!(styles[0].indent_cells(), 2);
        assert_eq!(styles[1].indent_cells(), 4);
        assert_eq!(styles[2].indent_cells(), 4, "quoted, and an item");
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

    /// 追加要件 2026-09-15: **画像だけの行**が絵の行になる。行き先は画像の拡張子で終わるものだけで、
    /// 幅の指定（`|300`）はObsidianの2つの書き方のどちらからも読む。文中の画像・外のURL・引用や
    /// 箇条書きの中は、記法のまま。
    #[test]
    fn only_a_line_that_is_one_image_becomes_a_picture() {
        let image = |line| image_of_line(line).map(|image| (image.target, image.width));
        assert_eq!(image("![説明](img/a.png)"), Some(("img/a.png", None)));
        assert_eq!(image("  ![説明|300](a.JPG)  "), Some(("a.JPG", Some(300))));
        assert_eq!(
            image("![説明](<写真 1.png> \"題\")"),
            Some(("写真 1.png", None))
        );
        assert_eq!(image("![[a.png]]"), Some(("a.png", None)));
        assert_eq!(image("![[a.png|300x200]]"), Some(("a.png", Some(300))));
        assert_eq!(image("![[ノート]]"), None);
        assert_eq!(image("![説明](https://example.com/a.png)"), None);
        assert_eq!(image("文の中の![説明](a.png)"), None);

        let preview =
            PreviewDocument::from_source("![説明](a.png)\n> ![説明](a.png)\n- ![説明](a.png)\n");
        let pictures = preview
            .marks()
            .iter()
            .map(|marks| {
                marks
                    .iter()
                    .any(|mark| mark.ornament.is_some_and(Ornament::is_image))
            })
            .collect::<Vec<_>>();
        assert_eq!(pictures[..3], [true, false, false]);
    }

    /// 追加要件 2026-09-16: 絵の大きさを変えたら、幅の指定を書き換える（無ければ足す）。
    #[test]
    fn resizing_a_picture_writes_its_width() {
        let resized = |line| with_image_width(line, 240);
        assert_eq!(
            resized("![説明](a.png)").as_deref(),
            Some("![説明|240](a.png)")
        );
        assert_eq!(resized("![](a.png)").as_deref(), Some("![|240](a.png)"));
        assert_eq!(
            resized("  ![説明|300](<写真 1.png> \"題\") ").as_deref(),
            Some("  ![説明|240](<写真 1.png> \"題\") ")
        );
        assert_eq!(
            resized("![a|b](a.png)").as_deref(),
            Some("![a|b|240](a.png)")
        );
        assert_eq!(resized("![[a.png]]").as_deref(), Some("![[a.png|240]]"));
        assert_eq!(
            resized("![[a.png|300x200]]").as_deref(),
            Some("![[a.png|240]]")
        );
        assert_eq!(resized("![[ノート]]"), None);
        for line in ["![説明](a.png)", "![[a.png|300x200]]"] {
            let written = resized(line).unwrap();
            assert_eq!(
                image_of_line(&written).map(|image| image.width),
                Some(Some(240))
            );
        }
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

    #[test]
    fn wiki_preview_uses_filename_but_preserves_aliases_targets_and_source() {
        for (source, expected, target) in [
            ("[[folder/Note.md]]", "Note", "folder/Note.md"),
            (
                r"[[folder\原稿😀.txt#見出し]]",
                "原稿😀#見出し",
                r"folder\原稿😀.txt#見出し",
            ),
            (
                "[[../chapter/Name.part.md]]",
                "Name.part",
                "../chapter/Name.part.md",
            ),
            (
                "[[folder/NoExtension]]",
                "NoExtension",
                "folder/NoExtension",
            ),
            ("[[folder/.hidden]]", ".hidden", "folder/.hidden"),
            ("[[#Heading]]", "#Heading", "#Heading"),
            (
                "[[folder/my_note_file.md]]",
                "my_note_file",
                "folder/my_note_file.md",
            ),
            ("[[folder/Note.md|表示.md]]", "表示.md", "folder/Note.md"),
            ("[[folder/Note.md|**別名**]]", "別名", "folder/Note.md"),
        ] {
            let preview = PreviewDocument::from_source(source);
            assert_eq!(preview.text, expected, "{source}");
            assert_eq!(preview.lines[0].source, source);
            assert_eq!(
                link_target_at(source, preview.source_byte_at_utf16(0)),
                Some((target, true))
            );
            assert_eq!(
                PreviewDocument::from_source_with_active_line(source, Some(0)).text,
                source
            );
        }
        assert_eq!(
            visible_markdown_text("[folder/Note.md](folder/Note.md)"),
            "folder/Note.md"
        );
        assert_eq!(
            visible_markdown_text("`[[folder/Note.md]]`"),
            "[[folder/Note.md]]"
        );
    }

    #[test]
    fn shortened_wiki_mapping_skips_matching_directory_and_extension_characters() {
        let source = "前 [[原稿😀/原稿😀.md#md見出し]] 後";
        let preview = PreviewDocument::from_source(source);
        assert_eq!(preview.text, "前 原稿😀#md見出し 後");
        let filename = source.rfind("原稿😀").unwrap();
        assert_eq!(preview.source_byte_at_utf16(2), filename);
        let hash = source.find('#').unwrap();
        assert_eq!(preview.source_byte_at_utf16(6), hash);
        assert_eq!(preview.source_byte_at_utf16(7), hash + 1);
        assert_eq!(preview.utf16_at_source_byte(filename), 2);
        for at in 0..=preview.utf16_len() {
            assert!(source.is_char_boundary(preview.source_byte_at_utf16(at)));
        }
        let alias = "[[同名/同名.md|同名]]";
        assert_eq!(
            PreviewDocument::from_source(alias).source_byte_at_utf16(0),
            alias.rfind("同名").unwrap()
        );
    }

    #[test]
    fn active_link_marks_cover_the_entire_source_destination() {
        for source in [
            "前😀 [[原稿😀/原稿😀.md#見出し]] 後",
            "前😀 [[原稿😀/原稿😀.md#見出し|**原稿😀**]] 後",
            r"前 [[D:\原稿\次.md#節|別名]] 後",
            "前 [表示](dir/target.md#heading) 後",
        ] {
            let active = PreviewDocument::from_source_with_active_line(source, Some(0));
            assert_eq!(active.text, source);
            let (_, target, _) = line_link_ranges(source).into_iter().next().unwrap();
            let expected_start = source[..target.start].encode_utf16().count() as u32;
            let expected_len = source[target.clone()].encode_utf16().count() as u32;
            let path_mark = active.marks()[0]
                .iter()
                .find(|mark| {
                    mark.marks.link
                        && mark.utf16_start == expected_start
                        && mark.utf16_len == expected_len
                })
                .unwrap();
            assert!(
                !path_mark.marks.unresolved_link,
                "unknown paths use normal link color"
            );
            for mark in active.marks()[0].iter().filter(|mark| mark.marks.link) {
                if !source.contains("[[") {
                    continue;
                }
                assert!(mark.utf16_start >= expected_start);
                // The opening brackets must never receive link color.
                assert!(
                    mark.utf16_start + mark.utf16_len
                        <= source[..source.rfind("]]").unwrap()].encode_utf16().count() as u32
                );
            }
            assert_eq!(
                link_target_at(source, target.start),
                Some((&source[target], source.contains("[[")))
            );
        }
    }

    #[test]
    fn active_wiki_alias_and_following_marks_keep_exact_source_positions() {
        let source = "😀 [[同名/同名.md|**同名😀**]] **後😀** [表示](next.md)";
        let active = PreviewDocument::from_source_with_active_line(source, Some(0));
        for shown in ["同名😀", "後😀"] {
            let byte = source.find(shown).unwrap();
            assert!(active.marks()[0].iter().any(|mark| mark.marks.bold
                && mark.utf16_start == source[..byte].encode_utf16().count() as u32
                && mark.utf16_len == shown.encode_utf16().count() as u32));
        }
        let ordinary = source.find("表示").unwrap();
        assert!(active.marks()[0].iter().any(|mark| mark.marks.link
            && mark.utf16_start == source[..ordinary].encode_utf16().count() as u32
            && mark.utf16_len == 2));
        let preview = PreviewDocument::from_source(source);
        assert_eq!(preview.text, "😀 同名😀 後😀 表示");
        assert_eq!(
            preview.source_byte_at_utf16(3),
            source.find("同名😀").unwrap()
        );
    }

    #[test]
    fn target_ranges_exclude_code_images_and_escapes_and_keep_original_targets() {
        let source = "[[dir/原稿.md#節|表示]] [text](dir/Other.md) `[[code]]` ![[image.png]] \\[[escaped]]\n```\n[[fenced]]\n```\n[[last.md]]";
        let found: Vec<_> = link_target_ranges(source)
            .into_iter()
            .map(|(range, wiki)| (&source[range], wiki))
            .collect();
        assert_eq!(
            found,
            [
                ("dir/原稿.md#節", true),
                ("dir/Other.md", false),
                ("last.md", true)
            ]
        );
    }

    #[test]
    fn verified_invalid_wiki_links_are_distinct_from_links_footnotes_and_code() {
        let source = "[[不明|**表示名😀**]] [通常](章.md) [^注] `[[コード]]` ![[画像]]";
        let mut preview = PreviewDocument::from_source(source);
        assert!(
            !preview.marks()[0]
                .iter()
                .any(|span| span.marks.unresolved_link)
        );
        preview.set_invalid_link_targets(&[link_target_ranges(source)[0].0.clone()]);
        let marks = &preview.marks()[0];
        let unresolved: Vec<_> = marks
            .iter()
            .filter(|span| span.marks.unresolved_link)
            .collect();
        assert_eq!(unresolved.len(), 1);
        assert!(unresolved[0].marks.link);
        assert_eq!(unresolved[0].utf16_start, 0);
        assert_eq!(unresolved[0].utf16_len, 5);
        assert!(
            marks
                .iter()
                .any(|span| span.marks.link && !span.marks.unresolved_link)
        );
        assert_eq!(
            visible_markdown_text(source),
            "表示名😀 通常 [注] [[コード]] ![[画像]]"
        );
        assert!(
            !PreviewDocument::from_source("[[未完").marks()[0]
                .iter()
                .any(|span| span.marks.unresolved_link)
        );
    }

    #[test]
    fn link_validity_applies_to_active_paths_and_inactive_aliases_without_changing_text() {
        for source in [
            "😀 [[dir/target.md]] [other](other.md)",
            "😀 [[dir/target.md|**表示😀**]] [other](other.md)",
            "😀 [表示](dir/target.md#heading) [other](other.md)",
        ] {
            let bad = link_target_ranges(source)[0].0.clone();
            for active in [None, Some(0)] {
                let mut preview = PreviewDocument::from_source_with_active_line(source, active);
                let visible = preview.text.clone();
                assert!(preview.marks()[0].iter().all(|m| !m.marks.unresolved_link));
                preview.set_invalid_link_targets(std::slice::from_ref(&bad));
                assert!(preview.marks()[0].iter().any(|m| m.marks.unresolved_link));
                assert!(
                    preview.marks()[0]
                        .iter()
                        .any(|m| m.marks.link && !m.marks.unresolved_link)
                );
                assert_eq!(preview.text, visible);
                preview.refresh(source, active, Reading::all());
                preview.set_invalid_link_targets(std::slice::from_ref(&bad));
                assert!(preview.marks()[0].iter().any(|m| m.marks.unresolved_link));
                preview.set_invalid_link_targets(&[]);
                assert!(preview.marks()[0].iter().all(|m| !m.marks.unresolved_link));
                assert_eq!(preview.text, visible);
            }
        }
    }

    #[test]
    fn link_clicks_share_the_preview_grammar() {
        let source = "前😀 [**表示名**](章/原稿.md) [[不明|別名]] `[[コード]]` ![[画像]]";
        assert_eq!(
            link_target_at(source, source.find("表示名").unwrap()),
            Some(("章/原稿.md", false))
        );
        assert_eq!(
            link_target_at(source, source.find("別名").unwrap()),
            Some(("不明", true))
        );
        assert_eq!(link_target_at(source, source.find("コード").unwrap()), None);
        assert_eq!(link_target_at(source, source.find("画像").unwrap()), None);
        assert_eq!(link_target_at("```\n[[コード]]\n```", 7), None);
        assert_eq!(link_target_at("[[未完", 3), None);
        assert_eq!(link_target_at("\\[説明](章.md)", 3), None);
    }

    #[test]
    fn local_link_paths_do_not_guess_wiki_names_or_unsaved_bases() {
        use std::path::{Path, PathBuf};
        let source = Path::new(r"D:\原稿\本文.md");
        assert_eq!(
            link_path("章/次.md", false, Some(source)),
            Some(source.parent().unwrap().join("章/次.md"))
        );
        assert_eq!(
            link_path(r"D:\原稿\次.md", true, None),
            Some(PathBuf::from(r"D:\原稿\次.md"))
        );
        assert_eq!(link_path("次", true, Some(source)), None);
        assert_eq!(link_path("次.md", false, None), None);
        assert_eq!(link_path("https://example.com", false, Some(source)), None);
        assert_eq!(link_path("#見出し", false, Some(source)), None);
        let preview = PreviewDocument::from_source(r"[[D:\原稿\次.md|次]]");
        assert!(
            preview.marks()[0]
                .iter()
                .any(|m| m.marks.link && !m.marks.unresolved_link)
        );
    }

    #[test]
    fn body_copy_removes_decoration_but_preserves_code_and_unknown_markup() {
        let source = "# 見出し\n**太字**と｜漢字《かんじ》と[表示](章.md)\n- 項目\n```text\n**コード**\n```\n![画像](画像.png)\n";
        assert_eq!(
            plain_body_text(source, &[]),
            "見出し\n太字と漢字と表示\n項目\n**コード**\n![画像](画像.png)\n"
        );
    }

    #[test]
    fn body_copy_selection_uses_the_whole_documents_markup_context() {
        let source = "前 **太字😀** と [[ノート|別名]] 後";
        let start = source.find("太字").unwrap();
        let end = source.find("** と").unwrap();
        assert_eq!(plain_body_text(source, &[(start, end)]), "太字😀");
        let alias = source.find("別名").unwrap();
        assert_eq!(
            plain_body_text(source, &[(alias, alias + "別名".len())]),
            "別名"
        );
        assert_eq!(
            plain_body_text(source, &[(start, end), (alias, alias + "別名".len())]),
            "太字😀\n別名"
        );
    }

    #[test]
    fn body_copy_tables_use_tabs_and_omit_the_rule() {
        assert_eq!(
            plain_body_text("| A | B |\n|---|---|\n| 一 | 二 |\n", &[]),
            " A \t B \n 一 \t 二 \n"
        );
    }

    #[test]
    fn body_copy_manual_fixture_matches_expected_text() {
        assert_eq!(
            plain_body_text(include_str!("../testdata/23_本文だけコピー.md"), &[]),
            include_str!("../testdata/23_本文だけコピー_期待結果.txt")
        );
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
