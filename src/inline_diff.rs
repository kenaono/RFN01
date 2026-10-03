//! RFN01-67 PR 2b（書き手と決めた 2026-10-03）: 1列の差分（Git Repository の右の列）。
//!
//! 画面を知らない。行の対は[`crate::comparison::compare`]（左右の比較の画面と同じもの）から取り、
//! ここでは1列に並べ直すことと、**対になった行どうしを字で比べる**ことだけをする。原稿は1段落が
//! 1行なので、行が変わったと言うだけでは段落のどこが変わったか分からない（書き手と見本で確かめた）。
//!
//! - 変わった所の前後`context`行だけを出し、そのあいだは`Kind::Gap`の1行に詰める。
//! - 消した行と足した行は、ブロックの中で頭から順に対にして字で比べる。対にならない行は塗らない
//!   （行の地の色だけで足りる）。
//! - 字の比較にも上限を置く（`MAX_CELLS`）。超えたら頭と尻の一致だけで、そのあいだを塗る。

use crate::comparison;

/// 字の比較で表を作ってよい大きさ（字数×字数）。
const MAX_CELLS: usize = 400_000;
/// 対の行が「同じ行を直したもの」と言える、残った字の割合（短い方の字数に対して）。これより
/// 少なければ別の行を書いたのであって、字を塗ると行のほとんどが塗られて読めない。
const SIMILAR: f32 = 0.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Removed,
    Added,
    /// 詰めた変わらない行。
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    /// 1から数えた行番号。
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// 行の字を、塗るか（`true`）どうかで区切ったもの。改行は含まない。
    pub parts: Vec<(String, bool)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unified {
    pub lines: Vec<Line>,
    pub removed: usize,
    pub added: usize,
}

fn trim_newline(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or(line)
}

/// 行の頭の位置（バイト）から何行目か。
fn line_index(starts: &[usize], at: usize) -> usize {
    starts.partition_point(|start| *start < at)
}

enum Step<'a> {
    Same(usize, usize, &'a str),
    Block(Vec<(usize, &'a str)>, Vec<(usize, &'a str)>),
}

pub fn unified(old: &str, new: &str, context: usize) -> Unified {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let starts = |lines: &[&str]| {
        let mut at = 0;
        let mut result = Vec::with_capacity(lines.len());
        for line in lines {
            result.push(at);
            at += line.len();
        }
        result
    };
    let (sa, sb) = (starts(&a), starts(&b));
    let diff = comparison::compare(old, new);
    let mut steps = Vec::new();
    let (mut i, mut j) = (0, 0);
    for (left, right) in &diff.pairs {
        let (from_a, to_a) = (line_index(&sa, left.start), line_index(&sa, left.end));
        let (from_b, to_b) = (line_index(&sb, right.start), line_index(&sb, right.end));
        while i < from_a && j < from_b {
            steps.push(Step::Same(i, j, a[i]));
            i += 1;
            j += 1;
        }
        steps.push(Step::Block(
            (from_a..to_a).map(|k| (k, a[k])).collect(),
            (from_b..to_b).map(|k| (k, b[k])).collect(),
        ));
        (i, j) = (to_a, to_b);
    }
    while i < a.len() && j < b.len() {
        steps.push(Step::Same(i, j, a[i]));
        i += 1;
        j += 1;
    }
    // 変わった所の前後`context`行だけを残す。
    let changed: Vec<bool> = steps.iter().map(|s| matches!(s, Step::Block(..))).collect();
    let near = |at: usize| {
        let from = at.saturating_sub(context);
        let to = (at + context + 1).min(changed.len());
        changed[from..to].iter().any(|c| *c)
    };
    let mut result = Unified::default();
    let mut skipped = false;
    for (at, step) in steps.into_iter().enumerate() {
        match step {
            Step::Same(x, y, text) => {
                if !near(at) {
                    skipped = true;
                    continue;
                }
                if std::mem::take(&mut skipped) {
                    result.lines.push(gap());
                }
                result.lines.push(Line {
                    kind: Kind::Context,
                    old: Some(x + 1),
                    new: Some(y + 1),
                    parts: vec![(trim_newline(text).to_owned(), false)],
                });
            }
            Step::Block(removed, added) => {
                if std::mem::take(&mut skipped) {
                    result.lines.push(gap());
                }
                result.removed += removed.len();
                result.added += added.len();
                let paired = removed.len().min(added.len());
                let marks: Vec<(Vec<(String, bool)>, Vec<(String, bool)>)> = (0..paired)
                    .map(|k| characters(trim_newline(removed[k].1), trim_newline(added[k].1)))
                    .collect();
                for (k, (x, text)) in removed.iter().enumerate() {
                    result.lines.push(Line {
                        kind: Kind::Removed,
                        old: Some(x + 1),
                        new: None,
                        parts: marks
                            .get(k)
                            .map(|(left, _)| left.clone())
                            .unwrap_or_else(|| vec![(trim_newline(text).to_owned(), false)]),
                    });
                }
                for (k, (y, text)) in added.iter().enumerate() {
                    result.lines.push(Line {
                        kind: Kind::Added,
                        old: None,
                        new: Some(y + 1),
                        parts: marks
                            .get(k)
                            .map(|(_, right)| right.clone())
                            .unwrap_or_else(|| vec![(trim_newline(text).to_owned(), false)]),
                    });
                }
            }
        }
    }
    if skipped && !result.lines.is_empty() {
        result.lines.push(gap());
    }
    result
}

fn gap() -> Line {
    Line {
        kind: Kind::Gap,
        old: None,
        new: None,
        parts: vec![("⋯".to_owned(), false)],
    }
}

/// 2つの行を字で比べ、それぞれの行を「塗るか」で区切る。
fn characters(left: &str, right: &str) -> (Vec<(String, bool)>, Vec<(String, bool)>) {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    let (mut keep_a, mut keep_b) = (vec![false; a.len()], vec![false; b.len()]);
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    for k in 0..prefix {
        keep_a[k] = true;
        keep_b[k] = true;
    }
    for k in 0..suffix {
        keep_a[a.len() - 1 - k] = true;
        keep_b[b.len() - 1 - k] = true;
    }
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (n, m) = (ma.len(), mb.len());
    if n > 0 && m > 0 && (n + 1).saturating_mul(m + 1) <= MAX_CELLS {
        // 中ほどは最長共通部分列で、残す字を決める。
        let stride = m + 1;
        let mut lengths = vec![0u32; (n + 1) * stride];
        for x in (0..n).rev() {
            for y in (0..m).rev() {
                lengths[x * stride + y] = if ma[x] == mb[y] {
                    1 + lengths[(x + 1) * stride + y + 1]
                } else {
                    lengths[(x + 1) * stride + y].max(lengths[x * stride + y + 1])
                };
            }
        }
        let (mut x, mut y) = (0, 0);
        while x < n && y < m {
            if ma[x] == mb[y] {
                keep_a[prefix + x] = true;
                keep_b[prefix + y] = true;
                x += 1;
                y += 1;
            } else if lengths[(x + 1) * stride + y] >= lengths[x * stride + y + 1] {
                x += 1;
            } else {
                y += 1;
            }
        }
    }
    let kept = keep_a.iter().filter(|k| **k).count();
    let shorter = a.len().min(b.len());
    if shorter > 0 && (kept as f32) < SIMILAR * shorter as f32 {
        return (
            vec![(left.to_owned(), false)],
            vec![(right.to_owned(), false)],
        );
    }
    (runs(&a, &keep_a), runs(&b, &keep_b))
}

/// 字を「塗る（残さない）か」で続けてまとめる。
fn runs(chars: &[char], keep: &[bool]) -> Vec<(String, bool)> {
    let mut parts: Vec<(String, bool)> = Vec::new();
    for (c, kept) in chars.iter().zip(keep) {
        let mark = !kept;
        match parts.last_mut() {
            Some((text, last)) if *last == mark => text.push(*c),
            _ => parts.push((c.to_string(), mark)),
        }
    }
    if parts.is_empty() {
        parts.push((String::new(), false));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(parts: &[(String, bool)]) -> Vec<&str> {
        parts
            .iter()
            .filter(|(_, mark)| *mark)
            .map(|(text, _)| text.as_str())
            .collect()
    }

    /// 書き手と見本で確かめた形：1段落が1行の原稿で、段落の中の変わった字だけを塗る。
    #[test]
    fn a_paragraph_shows_only_the_characters_that_changed() {
        let old = "雨は朝から降っていた。\n猫は窓辺で眠っていた。風がカーテンを揺らす。\n";
        let new = "雨は朝から降っていた。\n猫は窓辺で眠っている。風がカーテンを静かに揺らす。\n";
        let diff = unified(old, new, 3);
        assert_eq!((diff.removed, diff.added), (1, 1));
        let kinds: Vec<Kind> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [Kind::Context, Kind::Removed, Kind::Added]);
        assert_eq!(marked(&diff.lines[1].parts), ["た"]);
        assert_eq!(marked(&diff.lines[2].parts), ["る", "静かに"]);
        assert_eq!(diff.lines[1].old, Some(2));
        assert_eq!(diff.lines[2].new, Some(2));
        let joined: String = diff.lines[2]
            .parts
            .iter()
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(joined, "猫は窓辺で眠っている。風がカーテンを静かに揺らす。");
    }

    #[test]
    fn far_unchanged_lines_are_folded_into_a_gap() {
        let old: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let new = old.replace("10\n", "十\n");
        let diff = unified(&old, &new, 3);
        let kinds: Vec<Kind> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds.first(), Some(&Kind::Gap));
        assert_eq!(kinds.last(), Some(&Kind::Gap));
        // 前後3行ずつ＋消した行と足した行。
        assert_eq!(kinds.iter().filter(|k| **k == Kind::Context).count(), 6);
        assert_eq!(diff.lines[1].old, Some(7));
    }

    #[test]
    fn added_and_removed_files_have_one_empty_side() {
        let diff = unified("", "一\n二\n", 3);
        assert_eq!((diff.removed, diff.added), (0, 2));
        assert!(diff.lines.iter().all(|l| l.kind == Kind::Added));
        assert!(diff.lines.iter().all(|l| marked(&l.parts).is_empty()));
        let diff = unified("一\n", "", 3);
        assert_eq!((diff.removed, diff.added), (1, 0));
        assert!(unified("同じ\n", "同じ\n", 3).lines.is_empty());
    }

    #[test]
    fn unpaired_lines_in_a_block_are_not_marked() {
        let diff = unified("夜になった。\n", "夜が来た。\n灯りがともる。\n", 3);
        assert_eq!((diff.removed, diff.added), (1, 2));
        assert_eq!(marked(&diff.lines[0].parts), ["になっ"]);
        assert_eq!(marked(&diff.lines[1].parts), ["が来"]);
        assert!(marked(&diff.lines[2].parts).is_empty());
    }

    /// 書き直した別の行は字で比べない（塗ると行のほとんどが塗られて読めない）。
    #[test]
    fn unrelated_lines_are_not_marked() {
        let diff = unified("二行目\n", "雨は朝から降っていた。\n", 3);
        assert!(diff.lines.iter().all(|l| marked(&l.parts).is_empty()));
    }

    #[test]
    fn very_long_lines_fall_back_to_the_ends() {
        // 頭と尻が長く同じで、なかほどだけが長く違う（表が大きすぎる）。
        let same = "同".repeat(700);
        let old = format!("{same}{}{same}\n", "あ".repeat(500));
        let new = format!("{same}{}{same}\n", "い".repeat(500));
        let diff = unified(&old, &new, 3);
        assert_eq!(marked(&diff.lines[0].parts), ["あ".repeat(500)]);
        assert_eq!(marked(&diff.lines[1].parts), ["い".repeat(500)]);
    }
}
