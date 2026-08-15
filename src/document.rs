use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentStats {
    pub logical_lines: usize,
    pub body_characters: usize,
    pub source_characters: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewDocument {
    pub text: String,
    /// UTF-16 position in `text` to byte offset in the Markdown source.
    /// Monotonically non-decreasing, so every lookup is a binary search.
    utf16_to_source_byte: Vec<usize>,
    /// UTF-16 position in `text` to byte offset in `text`, also non-decreasing.
    utf16_to_preview_byte: Vec<usize>,
    /// Grapheme cluster boundaries in `text`, in UTF-16 units. Sorted.
    grapheme_utf16_boundaries: Vec<usize>,
}

impl PreviewDocument {
    /// The preview with no active line, so every line shows its formatted form.
    ///
    /// The editor always knows which line the caret is on and goes through
    /// `from_source_with_active_line`; this shorthand exists for tests.
    #[cfg(test)]
    pub fn from_source(source: &str) -> Self {
        Self::from_source_with_active_line(source, None)
    }

    pub fn from_source_with_active_line(source: &str, active_line_start: Option<usize>) -> Self {
        let text = visible_markdown_text_with_active_line(source, active_line_start);
        let utf16_length = text.encode_utf16().count();
        let mut utf16_to_source_byte = vec![source.len(); utf16_length + 1];
        let mut utf16_to_preview_byte = vec![text.len(); utf16_length + 1];
        let mut source_cursor = 0;
        let mut utf16_cursor = 0;

        for (preview_byte, character) in text.char_indices() {
            // The preview only ever deletes from the source, so the next preview
            // character is almost always sitting at the cursor already.
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
            let preview_end = preview_byte + character.len_utf8();
            let utf16_units = character.len_utf16();

            for boundary in 0..utf16_units {
                utf16_to_source_byte[utf16_cursor + boundary] = source_offset;
            }
            // A position inside a surrogate pair resolves to the byte after the
            // pair, which is what the caret and preedit insertion rely on.
            utf16_to_preview_byte[utf16_cursor] = preview_byte;
            for boundary in 1..=utf16_units {
                utf16_to_preview_byte[utf16_cursor + boundary] = preview_end;
            }
            utf16_cursor += utf16_units;
            utf16_to_source_byte[utf16_cursor] = source_end;
            source_cursor = source_end;
        }

        if text.is_empty() {
            utf16_to_source_byte[0] = source.len();
            utf16_to_preview_byte[0] = 0;
        }

        let mut grapheme_utf16_boundaries = Vec::with_capacity(utf16_length / 2 + 1);
        grapheme_utf16_boundaries.push(0);
        let mut boundary = 0;
        for grapheme in text.graphemes(true) {
            boundary += grapheme.encode_utf16().count();
            grapheme_utf16_boundaries.push(boundary);
        }

        Self {
            text,
            utf16_to_source_byte,
            utf16_to_preview_byte,
            grapheme_utf16_boundaries,
        }
    }

    pub fn utf16_len(&self) -> usize {
        self.utf16_to_source_byte.len().saturating_sub(1)
    }

    pub fn source_byte_at_utf16(&self, position: usize) -> usize {
        self.utf16_to_source_byte[position.min(self.utf16_len())]
    }

    pub fn utf16_at_source_byte(&self, source_byte: usize) -> usize {
        self.utf16_to_source_byte
            .partition_point(|mapped| *mapped < source_byte)
            .min(self.utf16_len())
    }

    pub fn preview_byte_at_utf16(&self, position: usize) -> usize {
        self.utf16_to_preview_byte[position.min(self.utf16_len())]
    }

    pub fn previous_grapheme_position(&self, position: usize) -> usize {
        let index = self
            .grapheme_utf16_boundaries
            .partition_point(|boundary| *boundary < position);
        if index == 0 {
            0
        } else {
            self.grapheme_utf16_boundaries[index - 1]
        }
    }

    pub fn next_grapheme_position(&self, position: usize) -> usize {
        let index = self
            .grapheme_utf16_boundaries
            .partition_point(|boundary| *boundary <= position);
        self.grapheme_utf16_boundaries
            .get(index)
            .copied()
            .unwrap_or_else(|| self.utf16_len())
    }
}

impl DocumentStats {
    pub fn from_source(source: &str) -> Self {
        Self {
            logical_lines: logical_line_count(source),
            body_characters: visible_markdown_text(source).graphemes(true).count(),
            source_characters: source.graphemes(true).count(),
        }
    }
}

pub fn logical_line_count(source: &str) -> usize {
    if source.is_empty() {
        1
    } else {
        source.split('\n').count()
    }
}

pub fn visible_markdown_text(source: &str) -> String {
    visible_markdown_text_with_active_line(source, None)
}

fn visible_markdown_text_with_active_line(
    source: &str,
    active_line_start: Option<usize>,
) -> String {
    let mut visible = String::with_capacity(source.len());
    let mut line_start = 0;

    for line in source.lines() {
        if active_line_start == Some(line_start) {
            visible.push_str(line);
            visible.push('\n');
            line_start += line.len() + 1;
            continue;
        }

        let content_start = line
            .find(|character: char| !matches!(character, ' ' | '\t'))
            .unwrap_or(line.len());
        if content_start > 0 {
            visible.push_str(line);
            visible.push('\n');
            line_start += line.len() + 1;
            continue;
        }

        let content = line
            .strip_prefix("> ")
            .or_else(|| line.strip_prefix('>'))
            .unwrap_or(line);
        let content = strip_heading_marker(content);

        for character in content.chars() {
            if !matches!(character, '*' | '_' | '`') {
                visible.push(character);
            }
        }
        visible.push('\n');
        line_start += line.len() + 1;
    }

    if !source.ends_with('\n') {
        visible.pop();
    }

    visible
}

fn strip_heading_marker(line: &str) -> &str {
    let marker_length = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&marker_length) {
        line.get(marker_length..)
            .and_then(|rest| rest.strip_prefix(' '))
            .unwrap_or(line)
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_logical_lines() {
        assert_eq!(logical_line_count(""), 1);
        assert_eq!(logical_line_count("a"), 1);
        assert_eq!(logical_line_count("a\nb"), 2);
        assert_eq!(logical_line_count("a\n"), 2);
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

            let linear_previous = preview
                .grapheme_utf16_boundaries
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

            let linear_next = preview
                .grapheme_utf16_boundaries
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
            let linear = preview
                .utf16_to_source_byte
                .iter()
                .position(|mapped| *mapped >= source_byte)
                .unwrap_or_else(|| preview.utf16_len());
            assert_eq!(
                preview.utf16_at_source_byte(source_byte),
                linear.min(preview.utf16_len()),
                "utf16 lookup mismatch at source byte {source_byte}"
            );
        }
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
