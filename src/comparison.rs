//! Bounded line comparison. Ranges are UTF-8 byte boundaries; an empty range
//! marks where text exists only in the other version. Nothing edits a buffer.
use std::ops::Range;

#[derive(Default, Debug)]
pub struct Difference {
    pub ranges: Vec<Range<usize>>,
    /// Complete changed line spans on both sides, for aligned comparison views.
    pub pairs: Vec<(Range<usize>, Range<usize>)>,
    pub grouped: bool,
}

const MAX_CELLS: usize = 1_000_000;

pub fn compare(left: &str, right: &str) -> Difference {
    let a: Vec<_> = left.split_inclusive('\n').collect();
    let b: Vec<_> = right.split_inclusive('\n').collect();
    let offsets = |lines: &[&str]| {
        let mut at = 0;
        let mut result = vec![0];
        for line in lines {
            at += line.len();
            result.push(at);
        }
        result
    };
    let aa = offsets(&a);
    let bb = offsets(&b);
    let mut first = 0;
    while first < a.len().min(b.len()) && a[first] == b[first] {
        first += 1;
    }
    let (mut end_a, mut end_b) = (a.len(), b.len());
    while end_a > first && end_b > first && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let (n, m) = (end_a - first, end_b - first);
    let mut result = Difference::default();
    let mut add = |i: usize, j: usize, x: usize, y: usize| {
        let l = &left[aa[i]..aa[x]];
        let r = &right[bb[j]..bb[y]];
        if l == r {
            return;
        }
        result.pairs.push((aa[i]..aa[x], bb[j]..bb[y]));
        // Trim equal scalars inside a block, staying on UTF-8 boundaries.
        let prefix: usize = l
            .chars()
            .zip(r.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        let suffix: usize = l[prefix..]
            .chars()
            .rev()
            .zip(r[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        result.ranges.push(aa[i] + prefix..aa[x] - suffix);
    };
    if n.saturating_add(1).saturating_mul(m.saturating_add(1)) > MAX_CELLS {
        add(first, first, end_a, end_b);
        result.grouped = true;
        return result;
    }
    let stride = m + 1;
    // Intern lines once; the bounded DP then compares integers even when
    // paragraphs are long or repeated. HashMap still checks key equality.
    let mut ids = std::collections::HashMap::new();
    let mut intern = |line| {
        let next = ids.len();
        *ids.entry(line).or_insert(next)
    };
    let a_ids: Vec<_> = a[first..end_a].iter().map(|line| intern(*line)).collect();
    let b_ids: Vec<_> = b[first..end_b].iter().map(|line| intern(*line)).collect();
    let mut lengths = vec![0u32; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lengths[i * stride + j] = if a_ids[i] == b_ids[j] {
                1 + lengths[(i + 1) * stride + j + 1]
            } else {
                lengths[(i + 1) * stride + j].max(lengths[i * stride + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut pending = None;
    while i < n || j < m {
        if i < n && j < m && a_ids[i] == b_ids[j] {
            if let Some((x, y)) = pending.take() {
                add(first + x, first + y, first + i, first + j);
            }
            i += 1;
            j += 1;
        } else {
            pending.get_or_insert((i, j));
            if i < n && (j == m || lengths[(i + 1) * stride + j] >= lengths[i * stride + j + 1]) {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    if let Some((x, y)) = pending {
        add(first + x, first + y, first + i, first + j);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn japanese_edits_are_local_and_on_character_boundaries() {
        let a = "朝。\n猫が眠る。\n同じ\n夜。\n";
        let b = "朝。\n犬が眠る。\n同じ\n晩。\n";
        let diff = compare(a, b);
        assert_eq!(
            diff.ranges
                .iter()
                .map(|r| &a[r.clone()])
                .collect::<Vec<_>>(),
            vec!["猫", "夜"]
        );
        assert!(!diff.grouped);
    }
    #[test]
    fn insertion_and_deletion_preserve_the_unchanged_lines() {
        assert_eq!(compare("a\nc\n", "a\nb\nc\n").ranges, vec![2..2]);
        assert_eq!(compare("a\nb\nc\n", "a\nc\n").ranges, vec![2..4]);
        assert!(compare("a\na\n", "a\na\n").ranges.is_empty());
    }
    #[test]
    fn empty_text_final_newline_and_emoji_are_compared() {
        assert_eq!(compare("", "字").ranges, vec![0..0]);
        assert_eq!(compare("字", "").ranges, vec![0..3]);
        assert_eq!(compare("字\n", "字").ranges, vec![3..4]);
        assert_eq!(compare("😀猫", "😀犬").ranges, vec![4..7]);
        assert!(compare("", "").ranges.is_empty());
    }
    #[test]
    fn large_replacements_are_bounded_and_reported_as_grouped() {
        let a = "a\n".repeat(2000);
        let b = "b\n".repeat(2000);
        let diff = compare(&a, &b);
        assert!(diff.grouped);
        assert_eq!(diff.ranges.len(), 1);
    }
}
