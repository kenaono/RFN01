#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOptions {
    pub characters_per_column: usize,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            characters_per_column: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerticalColumnModel {
    pub graphemes: Vec<String>,
    pub source_start: usize,
    pub source_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerticalLayoutModel {
    pub columns: Vec<VerticalColumnModel>,
}

pub fn layout_vertical(source: &str, options: LayoutOptions) -> VerticalLayoutModel {
    let capacity = options.characters_per_column.max(1);
    let mut columns = Vec::new();
    let mut graphemes = Vec::with_capacity(capacity);
    let mut source_start = 0;
    let mut last_end = 0;

    for (byte_offset, grapheme) in source.grapheme_indices(true) {
        if graphemes.is_empty() {
            source_start = byte_offset;
        }

        if matches!(grapheme, "\n" | "\r\n") {
            if !graphemes.is_empty() {
                columns.push(VerticalColumnModel {
                    graphemes: std::mem::take(&mut graphemes),
                    source_start,
                    source_end: byte_offset,
                });
            }
            last_end = byte_offset + grapheme.len();
            continue;
        }

        graphemes.push(grapheme.to_owned());
        last_end = byte_offset + grapheme.len();

        if graphemes.len() == capacity {
            columns.push(VerticalColumnModel {
                graphemes: std::mem::take(&mut graphemes),
                source_start,
                source_end: last_end,
            });
            graphemes = Vec::with_capacity(capacity);
        }
    }

    if !graphemes.is_empty() {
        columns.push(VerticalColumnModel {
            graphemes,
            source_start,
            source_end: last_end,
        });
    }

    VerticalLayoutModel { columns }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_utf8_source_ranges_to_columns() {
        let layout = layout_vertical(
            "日本語abc",
            LayoutOptions {
                characters_per_column: 3,
            },
        );

        assert_eq!(layout.columns.len(), 2);
        assert_eq!(layout.columns[0].graphemes, vec!["日", "本", "語"]);
        assert_eq!(layout.columns[0].source_start, 0);
        assert_eq!(layout.columns[0].source_end, 9);
        assert_eq!(layout.columns[1].graphemes, vec!["a", "b", "c"]);
        assert_eq!(layout.columns[1].source_start, 9);
        assert_eq!(layout.columns[1].source_end, 12);
    }

    #[test]
    fn starts_a_new_column_after_a_line_break() {
        let layout = layout_vertical("甲乙\n丙丁", LayoutOptions::default());

        assert_eq!(layout.columns.len(), 2);
        assert_eq!(layout.columns[0].graphemes, vec!["甲", "乙"]);
        assert_eq!(layout.columns[1].graphemes, vec!["丙", "丁"]);
    }

    #[test]
    fn keeps_an_emoji_sequence_in_one_layout_cell() {
        let layout = layout_vertical(
            "A👨‍👩‍👧‍👦B",
            LayoutOptions {
                characters_per_column: 2,
            },
        );

        assert_eq!(layout.columns[0].graphemes, vec!["A", "👨‍👩‍👧‍👦"]);
        assert_eq!(layout.columns[1].graphemes, vec!["B"]);
    }
}
use unicode_segmentation::UnicodeSegmentation;
