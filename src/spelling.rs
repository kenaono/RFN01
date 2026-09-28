//! 英語の綴りの確認（RFN01-63、書き手と合意 2026-09-28）。
//!
//! **判定はここの1か所だけ。**どこが英単語で、どこを調べないかを、文書全体を数える
//! [`scan`]と、タイルに印を描く[`SpellMarks::marks_in`]が同じ[`words`]から聞く。
//! 別々に書くと、ステータスバーの数と画面の波線が食い違う。
//!
//! **ここはWindowsを知らない。**語が誤りかどうかは呼ぶ側が答える（`spelling_ui`が
//! Windowsのスペルチェックに聞く）。ここにあるのは本文の読み方と、答えの持ち方である。
//!
//! 要件 7.9 は「編集器はどれが正しいかを判定しない」と言う。**英語の綴りは正解が一つの
//! たぐいで、表記ゆれの判定とは別**——と書き手と整理して入れた。だから日本語は調べず、
//! 書き手がRunから頼んだ文書だけを調べる。

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use crate::text_blocks::{LineKind, LineStyle};

/// 1行の中の、調べる英単語の1つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// 行の中のバイト範囲。
    pub range: Range<usize>,
    /// すぐ前の語と同じ（`the the`）。**綴りが正しくても誤り**として印を付ける。
    pub repeated: bool,
}

/// 英単語の字——ASCIIの英字と、ラテン文字の拡張（`café`・`naïve`）。
///
/// `×`と`÷`はこの範囲にあるが字ではない。
fn is_letter(c: char) -> bool {
    c.is_ascii_alphabetic() || (('\u{00C0}'..='\u{024F}').contains(&c) && c != '×' && c != '÷')
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '’'
}

/// 語に接していたら、その語は文章ではない——ファイル名（`draft.md`）、識別子
/// （`snake_case`・`v2`）、パス、メールアドレス。
fn joins_a_name(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '_' | '/' | '\\' | '@' | '=' | '#' | '$' | '%' | '&')
}

/// 行の中の、調べないところ（バイト範囲）。
///
/// **書き手が書いた文ではないもの**：コード、リンク（見出しの字も含む。整形表示では
/// リンクとして出るので、ソースと数を揃える）、URL、ルビの読み、青空文庫の注記、
/// `<`で始まるタグ。
fn skipped(line: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < line.len() {
        let rest = &line[at..];
        let close = |open: usize, close: &str| {
            rest[open..]
                .find(close)
                .map(|end| at..at + open + end + close.len())
        };
        let found = if rest.starts_with('`') {
            let ticks = rest.bytes().take_while(|b| *b == b'`').count();
            close(ticks, &"`".repeat(ticks))
        } else if rest.starts_with("[[") {
            close(2, "]]")
        } else if rest.starts_with('[') || rest.starts_with("![") {
            let open = if rest.starts_with('!') { 2 } else { 1 };
            rest[open..].find("](").and_then(|middle| {
                let target = open + middle + 2;
                rest[target..]
                    .find(')')
                    .map(|end| at..at + target + end + 1)
            })
        } else if rest.starts_with('《') {
            close('《'.len_utf8(), "》")
        } else if rest.starts_with("［＃") {
            close("［＃".len(), "］")
        } else if rest.starts_with('<')
            && rest[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
        {
            close(1, ">")
        } else if starts_url(rest) {
            let end = rest
                .char_indices()
                .find(|(_, c)| c.is_whitespace() || !c.is_ascii() || matches!(c, ')' | '>' | '"'))
                .map_or(rest.len(), |(end, _)| end);
            Some(at..at + end)
        } else {
            None
        };
        match found {
            Some(range) if range.end > at => {
                at = range.end;
                out.push(range);
            }
            _ => {
                // 次の字へ。字の途中へ入らないよう、字の長さだけ進む。
                at += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    out
}

fn starts_url(rest: &str) -> bool {
    let lower = |prefix: &str| {
        rest.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    lower("http://") || lower("https://") || lower("www.") || lower("mailto:") || lower("file:")
}

/// 1行の中の英単語（RFN01-63）。**調べる範囲の決まりはここだけにある。**
///
/// 語は英字の並びで、字と字のあいだの`'`（`don't`）は語の中に入れる。`-`では切る
/// （`e-mail`は`e`と`mail`）。[`skipped`]の範囲と、名前の一部に見える語は返さない。
pub fn words(line: &str) -> Vec<Word> {
    let skips = skipped(line);
    let mut out: Vec<Word> = Vec::new();
    // 繰り返しを見るための、すぐ前の語。**間に空白しか無いとき**だけ比べる。
    let mut previous: Option<(Range<usize>, bool)> = None;
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut index = 0;
    while index < chars.len() {
        let (start, c) = chars[index];
        if !is_letter(c) {
            if !c.is_whitespace() {
                previous = None;
            }
            index += 1;
            continue;
        }
        let mut end_index = index + 1;
        while end_index < chars.len() {
            let c = chars[end_index].1;
            let next_is_letter = chars.get(end_index + 1).is_some_and(|(_, n)| is_letter(*n));
            if is_letter(c) || (is_apostrophe(c) && next_is_letter) {
                end_index += 1;
            } else {
                break;
            }
        }
        let end = chars.get(end_index).map_or(line.len(), |(at, _)| *at);
        let before = index.checked_sub(1).map(|i| chars[i].1);
        let after = chars.get(end_index).map(|(_, c)| *c);
        // `draft.md`の両側は名前。`e.g.`・`U.S.`は通す——略語の点は1字ずつに付く。
        let dotted = end_index - index > 1
            && ((before == Some('.') && index >= 2 && is_letter(chars[index - 2].1))
                || (after == Some('.')
                    && chars.get(end_index + 1).is_some_and(|(_, n)| is_letter(*n))));
        let in_name = before.is_some_and(joins_a_name) || after.is_some_and(joins_a_name) || dotted;
        let in_skip = skips
            .iter()
            .any(|skip| skip.start < end && start < skip.end);
        index = end_index;
        if in_name || in_skip {
            previous = None;
            continue;
        }
        let repeated = previous.as_ref().is_some_and(|(held, _)| {
            line[held.clone()].eq_ignore_ascii_case(&line[start..end])
                && line[held.end..start].chars().all(|c| c == ' ' || c == '\t')
        });
        previous = Some((start..end, repeated));
        out.push(Word {
            range: start..end,
            repeated,
        });
    }
    out
}

/// 文書全体を数えた結果（RFN01-63）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// 誤りの語（綴り）。**位置ではなく語で持つ**——描くときにタイルの本文から
    /// 探し直すので、打っているあいだに位置がずれない。
    pub wrong: HashSet<String>,
    /// 誤りの数（繰り返しを含む）。ステータスバーに出す。
    pub count: usize,
}

/// 文書全体を数える（RFN01-63）。
///
/// `styles`はソースの行ごとの形（`document::line_styles`）で、**コードと表の行は
/// 調べない**。`is_wrong`は語ごとに1度しか呼ばない。`ignored`の語は数えない。
pub fn scan(
    source: &str,
    styles: &[LineStyle],
    ignored: &HashSet<String>,
    mut is_wrong: impl FnMut(&str) -> bool,
) -> Scan {
    let mut asked: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    let mut out = Scan::default();
    for word in readable_words(source, styles) {
        let text = &source[word.range.clone()];
        if ignored.contains(text) {
            continue;
        }
        if word.repeated {
            out.count += 1;
            continue;
        }
        let wrong = *asked.entry(text).or_insert_with(|| is_wrong(text));
        if wrong {
            out.count += 1;
            out.wrong.insert(text.to_owned());
        }
    }
    out
}

/// 文書全体の、調べる英単語（範囲はソースのバイト）。**[`scan`]と[`marked`]が
/// 同じここを通る**——数と、移る先が食い違わない。
///
/// 表はセルごとに描かれて印を付ける道が無いので、コードと同じく読まない。
fn readable_words<'a>(source: &'a str, styles: &'a [LineStyle]) -> impl Iterator<Item = Word> + 'a {
    let mut line_start = 0;
    source
        .split('\n')
        .enumerate()
        .flat_map(move |(index, line)| {
            let start = line_start;
            line_start += line.len() + 1;
            let skipped = styles.get(index).is_some_and(|style| {
                style.kind.is_code()
                    || matches!(style.kind, LineKind::TableRow | LineKind::TableRule)
            });
            let found = if skipped { Vec::new() } else { words(line) };
            found.into_iter().map(move |word| Word {
                range: start + word.range.start..start + word.range.end,
                repeated: word.repeated,
            })
        })
}

/// 印の付いている語の、ソースの範囲（文書の順）。誤りから誤りへ移るときに使う。
pub fn marked(source: &str, styles: &[LineStyle], marks: &SpellMarks) -> Vec<Range<usize>> {
    readable_words(source, styles)
        .filter(|word| {
            let text = &source[word.range.clone()];
            !marks.ignored.contains(text) && (word.repeated || marks.wrong.contains(text))
        })
        .map(|word| word.range)
        .collect()
}

/// `marked`の中で、`from`の次（`back`なら前）の範囲。端を越えたら反対の端へ戻り、
/// そのとき2つめが真になる。
///
/// `from`は選んでいる語の頭か、何も選んでいなければキャレット。`caret`が真なら
/// **キャレットの所から始まる語も「次」**——文書の頭でF8を押せば、頭の語へ移る。
pub fn step_from(
    found: &[Range<usize>],
    from: usize,
    caret: bool,
    back: bool,
) -> Option<(Range<usize>, bool)> {
    let next = if back {
        found.iter().rev().find(|range| range.start < from)
    } else {
        found
            .iter()
            .find(|range| range.start > from || (caret && range.start == from))
    };
    match next {
        Some(range) => Some((range.clone(), false)),
        None => {
            let wrapped = if back { found.last() } else { found.first() };
            wrapped.map(|range| (range.clone(), true))
        }
    }
}

/// タイルへ渡す、誤りの語（RFN01-63）。
///
/// **`Typography`の外に置く**（単語チェックモード要件 8.4と同じ）：印は幾何を1画素も
/// 動かさないので、変わってもタイルだけが古くなる。指紋をタイルの署名に混ぜる。
#[derive(Debug, Default)]
pub struct SpellMarks {
    wrong: HashSet<String>,
    ignored: HashSet<String>,
    fingerprint: u64,
}

impl SpellMarks {
    pub fn new(wrong: HashSet<String>, ignored: HashSet<String>) -> Self {
        let mut sorted: Vec<&String> = wrong.iter().chain(ignored.iter()).collect();
        sorted.sort();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        wrong.len().hash(&mut hasher);
        for word in sorted {
            word.hash(&mut hasher);
        }
        Self {
            wrong,
            ignored,
            fingerprint: hasher.finish(),
        }
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// 印を付ける範囲（`text`の中のバイト範囲）。
    ///
    /// `text`はタイルのブロックの本文で、改行で行に分かれている。`covered`は
    /// 調べない範囲（コード・リンク・箱）で、それと重なる語には付けない。
    pub fn marks_in(&self, text: &str, covered: &[Range<usize>]) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut line_start = 0;
        for line in text.split('\n') {
            for word in words(line) {
                let range = line_start + word.range.start..line_start + word.range.end;
                let spelled = &line[word.range.clone()];
                if self.ignored.contains(spelled) {
                    continue;
                }
                if !(word.repeated || self.wrong.contains(spelled)) {
                    continue;
                }
                if covered
                    .iter()
                    .any(|skip| skip.start < range.end && range.start < skip.end)
                {
                    continue;
                }
                out.push(range);
            }
            line_start += line.len() + 1;
        }
        out
    }
}

/// 綴りを確認している文書の状態（RFN01-63、`OpenDocument::spelling`）。
///
/// **確認を終えるまでだけ持つ。**再起動では戻さず、Ignoreした語も残さない。
#[derive(Debug, Default)]
pub struct DocumentSpelling {
    /// タイルへ渡すもの。数え直すたびに作り直す。
    pub marks: std::sync::Arc<SpellMarks>,
    /// 書き手が「Ignore」した語。
    pub ignored: HashSet<String>,
    /// 誤りの数。
    pub count: usize,
}

/// 位置`at`（行の中のバイト）にある、印の付く語（右クリック）。
///
/// 返すのは語の範囲と、繰り返しかどうか。印が付かない語なら`None`。
pub fn word_at(line: &str, at: usize, marks: &SpellMarks) -> Option<(Range<usize>, bool)> {
    words(line)
        .into_iter()
        .find(|word| word.range.start <= at && at <= word.range.end)
        .filter(|word| {
            let spelled = &line[word.range.clone()];
            !marks.ignored.contains(spelled) && (word.repeated || marks.wrong.contains(spelled))
        })
        .map(|word| (word.range, word.repeated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelled(line: &str) -> Vec<&str> {
        words(line)
            .into_iter()
            .map(|word| &line[word.range])
            .collect()
    }

    #[test]
    fn english_words_come_out_of_japanese_prose() {
        assert_eq!(
            spelled("彼はcomputerを使ってprogramingをした。"),
            ["computer", "programing"]
        );
        assert_eq!(
            spelled("I don't know, café."),
            ["I", "don't", "know", "café"]
        );
        assert_eq!(spelled("e-mail"), ["e", "mail"]);
        assert!(spelled("日本語だけの文。").is_empty());
    }

    #[test]
    fn what_is_not_prose_is_not_read() {
        assert_eq!(spelled("see `codde` here"), ["see", "here"]);
        assert_eq!(spelled("``a `b` c`` done"), ["done"]);
        assert_eq!(
            spelled("go to [the page](https://exmaple.com) now"),
            ["go", "to", "now"]
        );
        assert_eq!(spelled("open [[Drafft note]] now"), ["open", "now"]);
        assert_eq!(spelled("URL https://exmaple.com/pathh end"), ["URL", "end"]);
        assert_eq!(
            spelled("｜漢字《kanji》 and ［＃「word」に傍点］ ok"),
            ["and", "ok"]
        );
        assert_eq!(spelled("a <span class=x>tag</span> b"), ["a", "tag", "b"]);
    }

    #[test]
    fn names_are_not_words() {
        assert_eq!(
            spelled("draft.md snake_case v2 me@mail path/to x=y end."),
            ["end"]
        );
        // `e.g.`の点は名前ではない。
        assert_eq!(spelled("e.g. this"), ["e", "g", "this"]);
    }

    #[test]
    fn a_word_said_twice_is_marked_the_second_time() {
        let found = words("over the the lazy The the dog");
        let repeated: Vec<bool> = found.iter().map(|word| word.repeated).collect();
        assert_eq!(repeated, [false, false, true, false, false, true, false]);
        // 句読点を挟めば繰り返しではない。
        assert!(words("that, that").iter().all(|word| !word.repeated));
    }

    #[test]
    fn a_scan_counts_each_mark_and_asks_once_per_word() {
        let source = "teh cat teh dog\n```\nteh\n```\nthe the end\n\n| teh | a |\n| --- | --- |\n| teh | b |";
        let styles = crate::document::line_styles(source);
        let mut asked = Vec::new();
        let found = scan(source, &styles, &HashSet::new(), |word| {
            asked.push(word.to_owned());
            word == "teh"
        });
        assert_eq!(
            found.count, 3,
            "teh twice, the the once; the code line is not read"
        );
        assert_eq!(found.wrong, HashSet::from(["teh".to_owned()]));
        assert_eq!(asked.iter().filter(|word| *word == "teh").count(), 1);

        let ignored = HashSet::from(["teh".to_owned()]);
        let found = scan(source, &styles, &ignored, |word| word == "teh");
        assert_eq!(found.count, 1);
    }

    #[test]
    fn marks_follow_the_words_not_the_places() {
        let marks = SpellMarks::new(HashSet::from(["teh".to_owned()]), HashSet::new());
        let text = "a teh b\nthe the teh";
        let found: Vec<&str> = marks
            .marks_in(text, &[])
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["teh", "the", "teh"]);
        // 覆われた語には付けない（コードの行・リンク・ルビの箱）。
        let covered = vec![Range { start: 2, end: 5 }];
        assert_eq!(
            marks.marks_in("a teh b", &covered),
            Vec::<Range<usize>>::new()
        );
        // 無視した語には付けない。
        let ignoring = SpellMarks::new(
            HashSet::from(["teh".to_owned()]),
            HashSet::from(["teh".to_owned()]),
        );
        assert!(ignoring.marks_in("teh", &[]).is_empty());
        assert_ne!(marks.fingerprint(), ignoring.fingerprint());
    }

    #[test]
    fn stepping_goes_from_mark_to_mark_and_wraps() {
        let source = "teh a\n```\nteh\n```\nb teh the the";
        let styles = crate::document::line_styles(source);
        let marks = SpellMarks::new(HashSet::from(["teh".to_owned()]), HashSet::new());
        let found = marked(source, &styles, &marks);
        let spelled: Vec<&str> = found.iter().map(|range| &source[range.clone()]).collect();
        assert_eq!(spelled, ["teh", "teh", "the"], "the code line is skipped");
        let first = found[0].clone();
        let second = found[1].clone();
        let last = found[2].clone();
        // 頭の語を選んでいれば次へ、キャレットが頭にあるだけならその語へ。
        assert_eq!(
            step_from(&found, 0, false, false),
            Some((second.clone(), false))
        );
        assert_eq!(
            step_from(&found, 0, true, false),
            Some((first.clone(), false))
        );
        assert_eq!(
            step_from(&found, second.start, false, false),
            Some((last.clone(), false))
        );
        assert_eq!(
            step_from(&found, last.start, false, false),
            Some((first.clone(), true))
        );
        assert_eq!(
            step_from(&found, second.start, false, true),
            Some((first.clone(), false))
        );
        assert_eq!(step_from(&found, 0, true, true), Some((last, true)));
        assert_eq!(step_from(&[], 0, true, false), None);
    }

    #[test]
    fn the_word_under_the_pointer() {
        let marks = SpellMarks::new(HashSet::from(["teh".to_owned()]), HashSet::new());
        assert_eq!(word_at("say teh now", 5, &marks), Some((4..7, false)));
        assert_eq!(word_at("say teh now", 1, &marks), None);
        assert_eq!(word_at("the the", 5, &marks), Some((4..7, true)));
    }
}
