//! Finding and replacing inside one document (要件 7.7).
//!
//! **Pure text arithmetic.** Nothing here knows about panes, carets or windows:
//! a haystack, a needle and a position in bytes. That is what makes the rules
//! about wrapping around, about searching backwards and about replacing every
//! match testable without a window, which is the bargain `text_blocks.rs` and
//! `pane_layout.rs` take as well.
//!
//! Byte positions are safe to hand back as they are. UTF-8 is
//! self-synchronising, so a needle can only ever be found at a character
//! boundary — a match never starts in the middle of a character.
//!
//! **Upper and lower case are the same letter** (要件 7.7): a writer looking
//! for `windows` means the sentence that begins with it too. The folding is
//! ASCII's and nothing more, which is what keeps the byte positions honest —
//! `A` and `a` are one byte each, so a match is the same length as the needle
//! and the text around it does not move. Full case folding does not have that
//! property (`İ` lowercases into two characters), and neither of the pairs a
//! Japanese document actually contains — ａ and Ａ, ア and あ — are ones a
//! search box should quietly treat as equal.

/// Where the next match is, from a position, wrapping around the end.
///
/// `from` is where the search begins: forwards, that is the first byte the
/// match may start at; backwards, the first byte it must end before. **The
/// document is a ring**: a search that reaches the end carries on from the
/// start, so `Ctrl+F` never simply stops working halfway down a document.
///
/// An empty needle matches nothing. There is no position in a document that is
/// "where the nothing is", and a search box the writer has not typed into yet is
/// the common case.
pub fn next_match(
    source: &str,
    needle: &str,
    from: usize,
    forwards: bool,
) -> Option<(usize, usize)> {
    if needle.is_empty() || needle.len() > source.len() {
        return None;
    }
    // **A byte that is not a character boundary is not an error here.** It
    // arrives from a caret that has been moved by an edit elsewhere, and
    // `str::get` answers `None` for it — which, before this, sent the search
    // back to the top of the document and made 次へ find the same match for
    // ever. Forwards the position moves on to the next character, backwards
    // back to the start of the one it is in: either way, no match is skipped.
    let from = if forwards {
        boundary_at_or_after(source, from)
    } else {
        boundary_at_or_before(source, from)
    };
    let found = if forwards {
        found_at(source, needle, from).or_else(|| found_at(source, needle, 0))
    } else {
        found_before(source, needle, from).or_else(|| found_before(source, needle, source.len()))
    };
    found.map(|at| (at, at + needle.len()))
}

/// Where the needle is, at or after `from`, ignoring the case of ASCII letters.
///
/// **The scan is over bytes and the answer is a byte position**, which is only
/// safe because the folding leaves lengths alone: every byte the needle matches
/// is either the same byte or the same letter in the other case, so the matched
/// text is exactly as long as the needle.
fn found_at(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    if pin.is_empty() || pin.len() > hay.len() {
        return None;
    }
    let last = hay.len() - pin.len();
    let mut at = from;
    while at <= last {
        // The boundary is asked about first because it is the cheap half, and
        // because it is the one that would matter if this ever folded anything
        // outside ASCII. As it stands a match cannot begin inside a character:
        // a byte in the middle of one is `0x80..=0xBF`, and no needle begins
        // with one of those.
        if haystack.is_char_boundary(at) && hay[at..at + pin.len()].eq_ignore_ascii_case(pin) {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// Where the last match that ends at or before `before` is.
fn found_before(haystack: &str, needle: &str, before: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    let end = before.min(hay.len());
    if pin.is_empty() || pin.len() > end {
        return None;
    }
    let mut at = end - pin.len();
    loop {
        if haystack.is_char_boundary(at) && hay[at..at + pin.len()].eq_ignore_ascii_case(pin) {
            return Some(at);
        }
        if at == 0 {
            return None;
        }
        at -= 1;
    }
}

/// The first character boundary at or after `at`.
fn boundary_at_or_after(source: &str, at: usize) -> usize {
    let mut at = at.min(source.len());
    while !source.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The last character boundary at or before `at`.
fn boundary_at_or_before(source: &str, at: usize) -> usize {
    let mut at = at.min(source.len());
    while !source.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Whether the text between two byte positions is the needle.
///
/// **The one place that answers "is this a match?"** (追加要件 2026-09-09).
/// 一件置換は「いま選ばれているものが検索で見つけたものか」を訊いてから
/// 置き換える。それを`==`で訊いていたので、`alpha`で見つけた`Alpha`は
/// 置換されずに次へ飛んでいた——**見つける規則と置き換える規則が2つあった**。
/// 畳み方は[`next_match`]・[`count`]・[`replace_all`]と同じASCIIのそれで、
/// ここを通せば4つが1つの規則を共有する。
///
/// 範囲が文字の途中から始まっていたり、長さが針と違えば一致ではない
/// ——畳んでも長さが変わらないのがASCIIの折り畳みの性質なので、
/// 長さの違いはそれだけで答えになる。
pub fn is_match(source: &str, needle: &str, start: usize, end: usize) -> bool {
    if needle.is_empty() || end.saturating_sub(start) != needle.len() {
        return false;
    }
    source
        .get(start..end)
        .is_some_and(|found| found.eq_ignore_ascii_case(needle))
}

/// How many times the needle is in the document.
///
/// Matches that would overlap are not counted twice, the same rule
/// `str::matches` follows: the search for the next one starts after the last.
pub fn count(source: &str, needle: &str) -> usize {
    let mut total = 0;
    let mut from = 0;
    while let Some(at) = found_at(source, needle, from) {
        total += 1;
        from = at + needle.len();
    }
    total
}

/// Every match replaced, and how many there were.
///
/// **The replacement is never searched again**, so replacing "a" with "aa"
/// finishes rather than filling the document: the next search starts after
/// what was just written.
///
/// **What is written is what the writer typed**, whatever case the match was
/// in. A replacement that copied the case of each match would be guessing at
/// which of three or four conventions was meant, and would be wrong about
/// proper nouns either way.
pub fn replace_all(source: &str, needle: &str, replacement: &str) -> (String, usize) {
    if needle.is_empty() {
        return (source.to_owned(), 0);
    }
    let mut out = String::with_capacity(source.len());
    let mut from = 0;
    let mut replaced = 0;
    while let Some(at) = found_at(source, needle, from) {
        out.push_str(&source[from..at]);
        out.push_str(replacement);
        from = at + needle.len();
        replaced += 1;
    }
    out.push_str(&source[from..]);
    (out, replaced)
}

/// The one span that differs between two versions of a document.
///
/// Used to describe a replace-all to everything that follows an edit: the undo
/// history, and the carets other panes are holding (要件 7.6). Taking the whole
/// document as the change would work but would drag every position in every
/// other pane to the top, so the common head and tail are cut away first.
///
/// Returns where the change begins, how many bytes went out and how many came
/// in. Two identical texts give a change of nothing at that position.
pub fn changed_span(old: &str, new: &str) -> (usize, usize, usize) {
    let head = common_head(old, new);
    let tail = common_tail(&old[head..], &new[head..]);
    (head, old.len() - head - tail, new.len() - head - tail)
}

/// How many bytes the two share from the start, ending at a character
/// boundary.
fn common_head(old: &str, new: &str) -> usize {
    let mut head = 0;
    for (one, two) in old.char_indices().zip(new.char_indices()) {
        if one != two {
            break;
        }
        head = one.0 + one.1.len_utf8();
    }
    head
}

/// How many bytes the two share from the end, ending at a character boundary.
fn common_tail(old: &str, new: &str) -> usize {
    let mut tail = 0;
    let mut one = old.chars().rev();
    let mut two = new.chars().rev();
    loop {
        let (Some(left), Some(right)) = (one.next(), two.next()) else {
            break;
        };
        if left != right || tail + left.len_utf8() > old.len().min(new.len()) {
            break;
        }
        tail += left.len_utf8();
    }
    tail
}

/// Where one match is, as a line to put in a list (要件 7.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Which logical line the match is on, counting from 1 — what the status
    /// bar calls a line, not a byte.
    pub line: usize,
    /// Where the match starts in the source, so the caret can be put on it.
    pub at: usize,
    /// The line the match is on, short enough to sit in a narrow pane.
    pub preview: String,
}

/// How much of a line a result shows.
const PREVIEW_CHARACTERS: usize = 60;

/// Every match in one document, with the line each sits on (要件 7.7).
///
/// **Every match, not the next one.** A folder-wide search is a list to read
/// down rather than a place to stand, so this neither wraps around nor begins
/// anywhere in particular.
///
/// `limit` bounds what one file contributes: a generated file that matches on
/// every line must not be able to fill the pane on its own.
pub fn hits_in(source: &str, needle: &str, limit: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    let mut line_start = 0usize;
    for (number, line) in source.split('\n').enumerate() {
        let mut from = 0usize;
        // Every match on the line, not the first: a line that says the word
        // twice is two places to go.
        while let Some(at) = found_at(line, needle, from) {
            hits.push(Hit {
                line: number + 1,
                at: line_start + at,
                preview: shortened(line),
            });
            if hits.len() >= limit {
                return hits;
            }
            from = at + needle.len();
        }
        // The newline that ended the line counts too, or every line after the
        // first would be reported one byte early.
        line_start += line.len() + 1;
    }
    hits
}

/// A line with its indentation dropped and its tail cut off.
///
/// **Cut by characters, not by bytes**, or a Japanese line would be cut through
/// the middle of one.
fn shortened(line: &str) -> String {
    let line = line.trim();
    let mut out: String = line.chars().take(PREVIEW_CHARACTERS).collect();
    if line.chars().nth(PREVIEW_CHARACTERS).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "春の海 ひねもすのたり のたりかな";

    #[test]
    fn finds_the_next_match_from_where_the_caret_is() {
        let at = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(next_match(SOURCE, "のたり", 0, true), Some((at, at + 9)));
        assert_eq!(
            next_match(SOURCE, "のたり", at + 1, true),
            Some((second, second + 9)),
        );
    }

    /// **The document is a ring.** A search that reaches the end carries on
    /// from the start, in both directions.
    #[test]
    fn a_search_wraps_around_the_end() {
        let first = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(
            next_match(SOURCE, "のたり", second + 1, true),
            Some((first, first + 9)),
            "past the last match, back to the first",
        );
        assert_eq!(
            next_match(SOURCE, "のたり", 0, false),
            Some((second, second + 9)),
            "backwards from the start, round to the last",
        );
    }

    #[test]
    fn searching_backwards_finds_the_match_before_the_caret() {
        let first = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(
            next_match(SOURCE, "のたり", second, false),
            Some((first, first + 9)),
        );
    }

    /// A position that is not a character boundary is not an error: it arrives
    /// from a caret an edit elsewhere has moved. **It must never send the
    /// search back to the top**, which is what made 次へ find the same match
    /// for ever.
    #[test]
    fn a_position_inside_a_character_still_searches_from_there() {
        let first = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        // One byte into the first match's own first character.
        let inside = first + 1;
        assert!(!SOURCE.is_char_boundary(inside));
        assert_eq!(
            next_match(SOURCE, "のたり", inside, true),
            Some((second, second + 9)),
        );
        assert_eq!(
            next_match(SOURCE, "のたり", inside, false),
            Some((second, second + 9)),
            "backwards from inside it, round to the last",
        );
    }

    #[test]
    fn upper_and_lower_case_are_the_same_letter() {
        let source = "Windows 11 と windows の WINDOWS";
        let first = next_match(source, "windows", 0, true).expect("the first one");
        assert_eq!(first, (0, 7));
        let second = next_match(source, "WINDOWS", first.1, true).expect("the second one");
        assert_eq!(&source[second.0..second.1], "windows");
        assert_eq!(count(source, "windows"), 3);
    }

    #[test]
    fn only_ascii_letters_are_folded() {
        // Full-width Ａ and half-width a are different characters, and a
        // Japanese document is full of pairs a search must not treat as equal.
        assert_eq!(next_match("Ａ", "a", 0, true), None);
        assert_eq!(next_match("ア", "あ", 0, true), None);
        // And a needle that is not ASCII at all still finds itself.
        assert_eq!(
            next_match("春はあけぼの", "あけぼの", 0, true),
            Some((6, 18))
        );
    }

    #[test]
    fn a_replacement_is_written_as_it_was_typed() {
        // Both matches go, and neither takes its own case with it.
        let (replaced, count) = replace_all("Cat and cat", "CAT", "dog");
        assert_eq!(replaced, "dog and dog");
        assert_eq!(count, 2);
    }

    #[test]
    fn a_search_backwards_folds_case_as_well() {
        let source = "cat と CAT";
        let found = next_match(source, "Cat", source.len(), false).expect("the last one");
        assert_eq!(&source[found.0..found.1], "CAT");
    }

    /// **見つけたものは、そのまま置換できる**（追加要件 2026-09-09）。
    /// 一件置換は「選ばれているのは検索が見つけたものか」を訊いてから置き換える
    /// ので、この問いが検索より厳しいと、見つかったのに置換されない一致ができる。
    #[test]
    fn what_the_search_finds_is_what_a_replace_calls_a_match() {
        let source = "Alpha alpha ALPHA";
        let mut from = 0;
        let mut matched = 0;
        while let Some((start, end)) = next_match(source, "alpha", from, true) {
            assert!(
                is_match(source, "alpha", start, end),
                "found {:?} but would not replace it",
                &source[start..end]
            );
            matched += 1;
            from = end;
            if from >= source.len() {
                break;
            }
        }
        assert_eq!(matched, 3);
        assert_eq!(count(source, "alpha"), 3);
        assert_eq!(replace_all(source, "alpha", "beta").1, 3);
    }

    /// Anything that is not exactly one match is not one: a longer span, a
    /// shorter one, a start inside a character, and the empty needle a search
    /// box has before it is typed into.
    #[test]
    fn a_span_that_is_not_the_needle_is_not_a_match() {
        let source = "あかalphaあか";

        assert!(!is_match(source, "alpha", 6, 12), "one byte too long");
        assert!(!is_match(source, "alpha", 6, 10), "too short");
        assert!(
            !is_match(source, "alpha", 5, 10),
            "starts inside a character"
        );
        assert!(!is_match(source, "", 6, 6), "nothing matches");
        assert!(is_match(source, "ALPHA", 6, 11));
    }

    #[test]
    fn a_needle_that_is_not_there_is_not_found() {
        assert_eq!(next_match(SOURCE, "冬の海", 0, true), None);
        assert_eq!(next_match(SOURCE, "", 0, true), None, "nothing matches");
        assert_eq!(next_match("", "あ", 0, true), None);
        assert_eq!(count(SOURCE, "のたり"), 2);
        assert_eq!(count(SOURCE, ""), 0);
    }

    /// A match can only start at a character boundary, so a byte position
    /// handed back is always one a slice can begin at.
    #[test]
    fn a_match_never_starts_inside_a_character() {
        let source = "あいうえお";
        let (start, end) = next_match(source, "うえ", 0, true).expect("is there");

        assert!(source.is_char_boundary(start));
        assert!(source.is_char_boundary(end));
        assert_eq!(&source[start..end], "うえ");
    }

    /// **What goes in is never searched again**, so a replacement containing
    /// the needle finishes rather than running away.
    #[test]
    fn replacing_every_match_does_not_search_what_it_wrote() {
        let (text, replaced) = replace_all("ああ", "あ", "ああ");

        assert_eq!(text, "ああああ");
        assert_eq!(replaced, 2);
        assert_eq!(replace_all("ああ", "", "い"), ("ああ".to_owned(), 0));
    }

    /// A replace-all is described to the rest of the editor as the one span
    /// that changed, so that carets in other panes move by what actually moved
    /// (要件 7.6) rather than being dragged to the top of the document.
    #[test]
    fn a_replacement_is_described_as_the_span_that_changed() {
        let old = "前置き のたり 後書き";
        let new = "前置き ゆらり 後書き";

        let (at, removed, inserted) = changed_span(old, new);

        // **The smallest span that differs**, so the shared 「り」 at the end
        // is left out of it — a caret sitting on it in another pane does not
        // move.
        assert_eq!(&old[at..at + removed], "のた");
        assert_eq!(&new[at..at + inserted], "ゆら");
    }

    #[test]
    fn two_identical_texts_changed_nothing() {
        assert_eq!(changed_span("同じ", "同じ"), (6, 0, 0));
    }

    /// Text added at one end leaves the other end alone.
    #[test]
    fn a_change_at_one_end_keeps_the_other_end_still() {
        assert_eq!(changed_span("bc", "abc"), (0, 0, 1));
        assert_eq!(changed_span("ab", "abc"), (2, 0, 1));
        assert_eq!(changed_span("abc", "ac"), (1, 1, 0));
    }

    /// A folder-wide search reports every match, on which line, and with the
    /// line to show for it.
    #[test]
    fn every_match_is_reported_with_its_line() {
        let source = "はじめに\n第一章の章\n\nおわりに";

        let hits = hits_in(source, "章", 50);

        let lines: Vec<usize> = hits.iter().map(|hit| hit.line).collect();
        assert_eq!(lines, vec![2, 2]);
        // Line 1 is 12 bytes and its newline one more, so line 2 begins at 13;
        // 章 sits 6 bytes into it and again at 12.
        let at: Vec<usize> = hits.iter().map(|hit| hit.at).collect();
        assert_eq!(at, vec![19, 25]);
        assert_eq!(hits[0].preview, "第一章の章");
        // Standing on a reported byte finds that match and no other.
        for hit in &hits {
            assert_eq!(
                next_match(source, "章", hit.at, true),
                Some((hit.at, hit.at + 3)),
            );
        }
        assert!(hits_in(source, "", 50).is_empty());
        assert!(hits_in(source, "見つからない語", 50).is_empty());
    }

    /// One file cannot fill the pane on its own.
    #[test]
    fn one_file_gives_up_after_the_limit() {
        let source = "あ\nあ\nあ\nあ\nあ";

        let hits = hits_in(source, "あ", 3);

        assert_eq!(hits.len(), 3);
        assert_eq!(hits[2].line, 3);
    }

    /// A result line is trimmed and cut to a length a narrow pane can show.
    #[test]
    fn a_long_line_is_cut_where_a_character_ends() {
        let long = "あ".repeat(PREVIEW_CHARACTERS + 10);
        let source = format!("    {long}");

        let hits = hits_in(&source, "あ", 50);

        let preview = &hits[0].preview;
        assert_eq!(preview.chars().count(), PREVIEW_CHARACTERS + 1);
        assert!(preview.ends_with('…'));
        // Trimmed, so the indentation does not eat the width.
        assert!(preview.starts_with('あ'));
    }
}
