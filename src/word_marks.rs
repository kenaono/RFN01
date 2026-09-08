//! 単語帳——書き手が置いておいた語と、それが本文のどこにあるか（要件 7.9）。
//!
//! **これは検索の言い換えである。**校正とは同じ語を何度も探すことで（要件 7.7）、
//! 単語帳は「毎回打ち込んでいた語を、置いておく場所」にあたる。だから探し方は
//! `find.rs`と同じ規則にしてある——ASCIIの大文字小文字だけを同じ字として畳み、
//! それ以外は畳まない。**書き手が`find`で見つかると思った語が、色でも付く。**
//!
//! **純粋な文字列の演算**であり、窓もペインも知らない。`find.rs`と`text_blocks.rs`が
//! 取っている取引と同じで、規則をテストで固定できるのが値打ちである。
//!
//! **編集器はどれが正しいかを判定しない**（要件 7.9）。ここにあるのは「書かれた語が
//! どこにあるか」だけで、表記ゆれの判定も、揺れているという指摘も無い。判定を始めた
//! 瞬間に、当たらない指摘を消す作業が書き手に増える。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 単語帳を何冊まで持てるか（要件 15 の宿題への、いまの答え）。
///
/// **色を用意するぶんだけ上限が要る**——描く側は帳ごとに筆を1本持つ。8冊は
/// 「固有名詞・多用しがちな語・伏線・人物3人ぶん」くらいで、**足りないと言われてから
/// 増やす**。実測してから決める、と要件15に書いてある。
pub const MAX_WORD_LISTS: usize = 8;

/// 1つのブロックの中で色を付ける数の上限。
///
/// **本文を描くたびに走る**ので、際限は要る。1つの段落の中で256か所も色が付いて
/// いれば、それはもう色分けではなく塗りつぶしである。
pub const MAX_MARKS_PER_BLOCK: usize = 256;

/// 一冊の単語帳（要件 7.9）。
///
/// **色は帳ごと**で、語ごとではない。単語帳1＝赤、単語帳2＝青、という書き手の言い方が
/// そのまま形になっている——語ごとに色を選べるようにすると、**色を決める作業が語の数だけ
/// 増える**。分けたいなら帳を分ける、というほうが手数が少ない。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordList {
    /// 書き手がこの帳に付けた名前。画面と設定ファイルにだけ出る。
    pub name: String,
    /// この帳の語を出す色。
    pub colour: [f32; 3],
    /// 帳に入っている語。**順番は書き手が入れた順**で、並べ替えない。
    pub words: Vec<String>,
    /// この帳を今は使わない、という状態。**消すのとは別**——校正の段によって
    /// 使う帳が違うので、消さずに畳んでおける必要がある。
    pub muted: bool,
}

/// 本文の中の、ある一致（要件 7.9）。
///
/// バイト位置で答える。`find.rs`と同じ理由で安全である：畳むのはASCIIの大小だけなので、
/// 一致した本文は語とちょうど同じ長さで、一致が文字の途中から始まることもない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordMark {
    pub start: usize,
    pub end: usize,
    /// 何冊目の帳か。色はその帳が持っている。
    pub list: usize,
}

/// 開いている単語帳のぜんぶ。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordMarks {
    pub lists: Vec<WordList>,
}

impl WordMarks {
    /// 何も出すものが無いか。**先に訊く**——単語帳を一冊も持っていない書き手に、
    /// 本文を走査する費用を払わせない（普通はこちらである）。
    pub fn is_empty(&self) -> bool {
        self.lists
            .iter()
            .all(|list| list.muted || list.words.iter().all(|word| word.trim().is_empty()))
    }

    /// この単語帳の並びを一つの数にする。
    ///
    /// **組版とタイルの鍵に混ぜるためのもの**（要件 7.9）。語や色を変えても本文の
    /// 大きさは1画素も変わらないので、これを混ぜないと**絵置き場にある古い色のままの
    /// 絵がそのまま出る**——技術検証 6.18 が何度も踏んでいる罠で、直前に端末の見た目で
    /// 踏んだのと同じものである。
    ///
    /// **畳んだ帳も数に入る。**畳めば色が消えるので、それは見た目が変わったということ。
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for list in &self.lists {
            list.muted.hash(&mut hasher);
            for channel in &list.colour {
                channel.to_bits().hash(&mut hasher);
            }
            for word in &list.words {
                word.hash(&mut hasher);
            }
            // 語の区切り。**入れないと、`["ab","c"]`と`["a","bc"]`が同じ数になる。**
            usize::MAX.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// 本文の中の一致を、位置の順に。**重なりは残さない。**
    ///
    /// 重なったときの決め方は2つ、この順に訊く。
    ///
    /// 1. **長いほうが勝つ。**「田中」と「田中さん」の両方が帳にあるなら、
    ///    書き手が「田中さん」を入れたのは、そこを別のものとして見たいからである。
    /// 2. 同じ長さなら**先の帳が勝つ**。並びは書き手が決めたもので、上にあるほうが
    ///    その人にとって強い。
    ///
    /// 勝った一致の中からは、次を探し直さない——`limit`はその数の上限で、
    /// **本文を描くたびに走る**ので、上限は費用から決まる（要件 15）。
    pub fn marks_in(&self, source: &str, limit: usize) -> Vec<WordMark> {
        if self.is_empty() || source.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut found: Vec<WordMark> = Vec::new();
        for (at, list) in self.lists.iter().enumerate() {
            if list.muted {
                continue;
            }
            for word in &list.words {
                let word = word.trim();
                if word.is_empty() {
                    continue;
                }
                let mut from = 0;
                while let Some(start) = found_at(source, word, from) {
                    found.push(WordMark {
                        start,
                        end: start + word.len(),
                        list: at,
                    });
                    // **一致の先へ進む**（1バイトではなく）。同じ語が自分自身の中で
                    // 重なることは無い。
                    from = start + word.len();
                    if found.len() >= limit.saturating_mul(4) {
                        break;
                    }
                }
            }
        }
        // 長い順・先の帳の順に見て、まだ空いている場所だけを取る。
        found.sort_by(|one, other| {
            let length = (other.end - other.start).cmp(&(one.end - one.start));
            length
                .then(one.list.cmp(&other.list))
                .then(one.start.cmp(&other.start))
        });
        let mut kept: Vec<WordMark> = Vec::new();
        for mark in found {
            if kept
                .iter()
                .any(|held| mark.start < held.end && held.start < mark.end)
            {
                continue;
            }
            kept.push(mark);
            if kept.len() >= limit {
                break;
            }
        }
        kept.sort_by_key(|mark| mark.start);
        kept
    }
}

/// `find.rs`と同じ探し方。**同じ規則であることが要件 7.9 の要点**なので、
/// 畳み方を変えるならあちらと一緒に変える。
fn found_at(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    if pin.is_empty() || pin.len() > hay.len() || from > hay.len() - pin.len() {
        return None;
    }
    let last = hay.len() - pin.len();
    let mut at = from;
    while at <= last {
        if haystack.is_char_boundary(at) && hay[at..at + pin.len()].eq_ignore_ascii_case(pin) {
            return Some(at);
        }
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(colour: [f32; 3], words: &[&str]) -> WordList {
        WordList {
            name: "帳".to_owned(),
            colour,
            words: words.iter().map(|word| (*word).to_owned()).collect(),
            muted: false,
        }
    }

    const RED: [f32; 3] = [1.0, 0.0, 0.0];
    const BLUE: [f32; 3] = [0.0, 0.0, 1.0];

    #[test]
    fn finds_every_occurrence_in_order() {
        let marks = WordMarks {
            lists: vec![book(RED, &["田中"])],
        };
        let found = marks.marks_in("田中と佐藤、そして田中。", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[1].start, "田中と佐藤、そして".len());
        assert!(found.windows(2).all(|two| two[0].start < two[1].start));
    }

    /// 要件 7.7 と同じ畳み方——**書き手が`find`で見つかると思った語が、色でも付く**。
    #[test]
    fn folds_ascii_case_the_way_find_does() {
        let marks = WordMarks {
            lists: vec![book(RED, &["windows"])],
        };

        assert_eq!(marks.marks_in("Windows と windows", 100).len(), 2);
    }

    /// 重なったときは長いほうが勝つ。「田中」と「田中さん」の両方を入れたのは、
    /// そこを別のものとして見たいからである。
    #[test]
    fn the_longer_word_wins_where_two_overlap() {
        let marks = WordMarks {
            lists: vec![book(RED, &["田中"]), book(BLUE, &["田中さん"])],
        };
        let found = marks.marks_in("田中さんが来た", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].list, 1, "長いほうの帳");
        assert_eq!(found[0].end, "田中さん".len());
    }

    /// 同じ長さで重なったら、先の帳が勝つ。並びは書き手が決めたものである。
    #[test]
    fn the_earlier_list_wins_a_tie() {
        let marks = WordMarks {
            lists: vec![book(RED, &["猫"]), book(BLUE, &["猫"])],
        };
        let found = marks.marks_in("猫", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].list, 0);
    }

    /// 畳んだ帳は出ない。**消すのとは別**——校正の段によって使う帳が違う。
    #[test]
    fn a_muted_list_marks_nothing() {
        let mut list = book(RED, &["猫"]);
        list.muted = true;
        let marks = WordMarks { lists: vec![list] };

        assert!(marks.is_empty());
        assert!(marks.marks_in("猫が猫を", 100).is_empty());
    }

    /// 空の語は語ではない。**書きかけの行が本文を全部塗らない**ため。
    #[test]
    fn blank_words_mark_nothing() {
        let marks = WordMarks {
            lists: vec![book(RED, &["", "   ", "猫"])],
        };
        let found = marks.marks_in("猫", 100);

        assert_eq!(found.len(), 1);
    }

    /// 上限は守る。**本文を描くたびに走る**ので、際限なく返してはいけない。
    #[test]
    fn the_limit_is_kept() {
        let marks = WordMarks {
            lists: vec![book(RED, &["あ"])],
        };
        let found = marks.marks_in(&"あ".repeat(500), 10);

        assert_eq!(found.len(), 10);
        assert_eq!(found[0].start, 0);
    }

    /// 印は組版の鍵に混ぜる（要件 7.9）。**語や色を変えても本文の大きさは変わらない**
    /// ので、混ぜていないと古い色のままの絵が出る（6.18の罠）。
    #[test]
    fn the_fingerprint_moves_when_anything_the_eye_sees_moves() {
        let one = WordMarks {
            lists: vec![book(RED, &["猫"])],
        };
        let other_colour = WordMarks {
            lists: vec![book(BLUE, &["猫"])],
        };
        let other_word = WordMarks {
            lists: vec![book(RED, &["犬"])],
        };
        let mut muted = one.clone();
        muted.lists[0].muted = true;

        assert_ne!(one.fingerprint(), other_colour.fingerprint(), "色");
        assert_ne!(one.fingerprint(), other_word.fingerprint(), "語");
        assert_ne!(one.fingerprint(), muted.fingerprint(), "畳んだ");
        assert_eq!(
            one.fingerprint(),
            one.clone().fingerprint(),
            "同じものは同じ"
        );
    }

    /// 語の区切りが数に入っていること。**入れないと`["ab","c"]`と`["a","bc"]`が
    /// 同じ数になる**——別の色分けなのに描き直らない。
    #[test]
    fn two_lists_split_differently_are_not_the_same_fingerprint() {
        let one = WordMarks {
            lists: vec![book(RED, &["ab", "c"])],
        };
        let other = WordMarks {
            lists: vec![book(RED, &["a", "bc"])],
        };

        assert_ne!(one.fingerprint(), other.fingerprint());
    }

    /// 一冊も持っていない書き手に、本文を走査する費用を払わせない。
    #[test]
    fn nothing_to_mark_is_answered_before_the_scan() {
        assert!(WordMarks::default().is_empty());
        assert!(WordMarks::default().marks_in("猫が猫を", 100).is_empty());
    }
}
