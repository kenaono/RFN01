//! 単語セット——書き手が置いておいた語と、それが本文のどこにあるか（要件 7.9）。
//!
//! **これは検索の言い換えである。**校正とは同じ語を何度も探すことで（要件 7.7）、
//! 単語セットは「毎回打ち込んでいた語を、置いておく場所」にあたる。だから探し方は
//! `find.rs`と同じ規則にしてある——ASCIIの大文字小文字だけを同じ字として畳み、
//! それ以外は畳まない。**書き手が`find`で見つかると思った語が、色でも付く。**
//!
//! # 語は別のファイルに、1行に1語（書き手の指摘 2026-09-08）
//!
//! **単語帳は大きくなる。**人物名だけで何百、表記ゆれを追い始めれば何千になる。
//! 最初は設定ファイルの1行にカンマで並べていたが、**その形は探せないし直せない**
//! ——書き手がそう言った。いまは1セット＝1ファイルで、**1行に1語**である。
//! ファイルはただのテキストなので、この編集器自身で開いて直せる。
//!
//! 設定ファイルが持つのは**色とファイルの場所だけ**で、語そのものは持たない。
//!
//! # 大きくなるから、木で探す
//!
//! 語ごとに本文を一周すると、語の数×本文の長さになる。数千語の単語帳では、
//! **1打鍵の2.5ms（技術検証 6.9）に対して桁が合わない。**だから語を1本の木
//! （トライ）に積んで、**本文を一度だけ歩く**。位置ごとの費用は「そこから伸びる
//! 語の長さ」で、一致しない位置ではほとんど1歩で終わる。
//!
//! 木は**セットを読み直したときにだけ**建てる。組版のたびに建て直しては、木にした
//! 意味が消える（`WordMarks::build`を呼ぶのは`main.rs`の一箇所だけ）。
//!
//! **純粋な文字列の演算**であり、窓もペインもファイルシステムも知らない。
//! `find.rs`と`text_blocks.rs`が取っている取引と同じで、規則をテストで固定できる
//! のが値打ちである。
//!
//! **編集器はどれが正しいかを判定しない**（要件 7.9）。ここにあるのは「書かれた語が
//! どこにあるか」だけで、表記ゆれの判定も、揺れているという指摘も無い。判定を始めた
//! 瞬間に、当たらない指摘を消す作業が書き手に増える。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// 単語セットを何冊まで持てるか。
///
/// **色を用意するぶんだけ上限が要る**——描く側はセットごとに筆を1本持つ。8冊は
/// 「人物・地名・多用しがちな語・伏線・禁止語」くらいで、**足りないと言われてから
/// 増やす**（要件 15）。
pub const MAX_WORD_SETS: usize = 8;

/// 1つのブロックの中で色を付ける数の上限。
///
/// **本文を描くたびに走る**ので、際限は要る。1つの段落の中で256か所も色が付いて
/// いれば、それはもう色分けではなく塗りつぶしである。
pub const MAX_MARKS_PER_BLOCK: usize = 256;

/// 1つのセットの語数の上限。
///
/// 木は語の総バイト数ぶんの節を持つので、ここが記憶の上限でもある。1万語なら
/// 節はおよそ数万——数メガバイトで、文書1つぶんに満たない。
pub const MAX_WORDS_PER_SET: usize = 10_000;

/// 一冊の単語セット（要件 7.9）。
///
/// **色はセットごと**で、語ごとではない。単語帳1＝赤、単語帳2＝青、という書き手の
/// 言い方がそのまま形になっている——語ごとに色を選べるようにすると、**色を決める
/// 作業が語の数だけ増える**。分けたいならセットを分ける、というほうが手数が少ない。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordSet {
    /// 語の入ったファイル。**設定が覚えているのはここまで**で、語そのものは
    /// 覚えない。
    pub path: PathBuf,
    /// このセットの語を出す色。
    pub colour: [f32; 3],
    /// 今は使わない、という状態。**消すのとは別**——校正の段によって使うセットが
    /// 違うので、消さずに畳んでおける必要がある。
    pub muted: bool,
    /// ファイルから読んだ語。**設定ファイルには入らない。**
    pub words: Vec<String>,
}

impl WordSet {
    /// 画面に出す名前。**ファイル名から作る**——書き手に同じ名前を二度書かせない。
    pub fn name(&self) -> String {
        self.path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// 単語セットのファイルを、語の並びに（要件 7.9）。
///
/// **1行に1語。**前後の空白は落とし、空行は語ではない。`#`で始まる行は覚え書きで、
/// 語ではない——**何百語の一覧には見出しが要る**（「主要人物」「脇役」）ので、
/// 書き手が自分で区切れるようにしてある。
///
/// 同じ語が二度あっても構わない。木は二度目を黙って捨てる。
pub fn read_word_file(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .take(MAX_WORDS_PER_SET)
        .collect()
}

/// 本文の中の、ある一致（要件 7.9）。
///
/// バイト位置で答える。`find.rs`と同じ理由で安全である：畳むのはASCIIの大小だけ
/// なので、一致した本文は語とちょうど同じ長さで、一致が文字の途中から始まることも
/// ない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordMark {
    pub start: usize,
    pub end: usize,
    /// 何冊目のセットか。色はそのセットが持っている。
    pub set: usize,
}

/// 木の1つの節。
///
/// 子は**並べた配列**で持ち、二分探索で引く。`HashMap`を節ごとに持つと、数万の節に
/// 対して割り当てが数万回になる——木にした目的が費用なのだから、そこで払っては
/// いけない。
#[derive(Clone, Debug, Default, PartialEq)]
struct Node {
    /// （畳んだバイト, 子の番号）を、バイトの順に。
    children: Vec<(u8, u32)>,
    /// ここで終わる語があるなら、その（セット番号, バイト長）。
    ///
    /// **先に入れたほうが残る**ので、同じ語を2つのセットが持っていたら先の
    /// セットが勝つ——並びは書き手が決めたものである。
    ends: Option<(u32, u32)>,
}

impl Node {
    fn child(&self, byte: u8) -> Option<u32> {
        self.children
            .binary_search_by_key(&byte, |(at, _)| *at)
            .ok()
            .map(|found| self.children[found].1)
    }
}

/// 開いている単語セットのぜんぶと、そこから建てた木。
#[derive(Clone, Debug, PartialEq)]
pub struct WordMarks {
    pub sets: Vec<WordSet>,
    /// 語を積んだ木。**`build`でだけ建つ**——組版のたびに建て直しては意味が無い。
    nodes: Vec<Node>,
    /// 木に語が1つでも入っているか。空の木を歩かせないための、先に訊く答え。
    any: bool,
    fingerprint: u64,
}

impl Default for WordMarks {
    fn default() -> Self {
        Self::build(Vec::new())
    }
}

impl WordMarks {
    /// セットから木を建てる。**読み直したときにだけ呼ぶ。**
    pub fn build(sets: Vec<WordSet>) -> Self {
        let mut nodes = vec![Node::default()];
        let mut any = false;
        for (at, set) in sets.iter().enumerate() {
            if set.muted {
                continue;
            }
            for word in &set.words {
                let word = word.trim();
                if word.is_empty() {
                    continue;
                }
                let mut node = 0usize;
                for byte in word.as_bytes() {
                    let folded = byte.to_ascii_lowercase();
                    node = match nodes[node].child(folded) {
                        Some(next) => next as usize,
                        None => {
                            let next = nodes.len() as u32;
                            nodes.push(Node::default());
                            let children = &mut nodes[node].children;
                            let slot = children
                                .binary_search_by_key(&folded, |(byte, _)| *byte)
                                .unwrap_or_else(|slot| slot);
                            children.insert(slot, (folded, next));
                            next as usize
                        }
                    };
                }
                // **二度目は捨てる。**先に入れたセットが勝つ（要件 7.9）。
                if nodes[node].ends.is_none() {
                    nodes[node].ends = Some((at as u32, word.len() as u32));
                    any = true;
                }
            }
        }
        let fingerprint = fingerprint_of(&sets);
        Self {
            sets,
            nodes,
            any,
            fingerprint,
        }
    }

    /// 何も出すものが無いか。**先に訊く**——単語セットを一冊も持っていない
    /// 書き手に、本文を歩く費用を払わせない（普通はこちらである）。
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    /// このセットの並びを一つの数にする。
    ///
    /// **組版とタイルの鍵に混ぜるためのもの**（要件 7.9）。語や色を変えても本文の
    /// 大きさは1画素も変わらないので、これを混ぜないと**絵置き場にある古い色のままの
    /// 絵がそのまま出る**——技術検証 6.18 が何度も踏んでいる罠である。
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// 本文の中の一致を、位置の順に。**重なりは残さない。**
    ///
    /// 本文を一度だけ歩き、**位置ごとに木をいちばん深くまで下りる**。決め方は2つ。
    ///
    /// 1. **その位置から始まるいちばん長い語が勝つ。**「田中」と「田中さん」の
    ///    両方があるなら、書き手が「田中さん」を入れたのはそこを別のものとして
    ///    見たいからである。
    /// 2. 一致したら**その先へ飛ぶ**ので、重なった一致は出てこない。
    ///
    /// 始めるのは文字の境目だけ。`limit`は1ブロックの上限で、**本文を描くたびに
    /// 走る**ので際限が要る。
    pub fn marks_in(&self, source: &str, limit: usize) -> Vec<WordMark> {
        if self.is_empty() || source.is_empty() || limit == 0 {
            return Vec::new();
        }
        let bytes = source.as_bytes();
        let mut found = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            if !source.is_char_boundary(at) {
                at += 1;
                continue;
            }
            let mut node = 0usize;
            let mut longest: Option<(u32, u32)> = None;
            let mut ahead = at;
            while ahead < bytes.len() {
                let Some(next) = self.nodes[node].child(bytes[ahead].to_ascii_lowercase()) else {
                    break;
                };
                node = next as usize;
                ahead += 1;
                if let Some(ends) = self.nodes[node].ends {
                    longest = Some(ends);
                }
            }
            match longest {
                Some((set, length)) => {
                    let length = length as usize;
                    found.push(WordMark {
                        start: at,
                        end: at + length,
                        set: set as usize,
                    });
                    if found.len() >= limit {
                        break;
                    }
                    at += length;
                }
                None => at += 1,
            }
        }
        found
    }
}

fn fingerprint_of(sets: &[WordSet]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for set in sets {
        set.muted.hash(&mut hasher);
        set.path.hash(&mut hasher);
        for channel in &set.colour {
            channel.to_bits().hash(&mut hasher);
        }
        for word in &set.words {
            word.hash(&mut hasher);
        }
        // 語の区切り。**入れないと、`["ab","c"]`と`["a","bc"]`が同じ数になる。**
        usize::MAX.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [f32; 3] = [1.0, 0.0, 0.0];
    const BLUE: [f32; 3] = [0.0, 0.0, 1.0];

    fn set(colour: [f32; 3], words: &[&str]) -> WordSet {
        WordSet {
            path: PathBuf::from("D:\\原稿\\人物.txt"),
            colour,
            muted: false,
            words: words.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    #[test]
    fn finds_every_occurrence_in_order() {
        let marks = WordMarks::build(vec![set(RED, &["田中"])]);
        let found = marks.marks_in("田中と佐藤、そして田中。", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[1].start, "田中と佐藤、そして".len());
        assert!(found.windows(2).all(|two| two[0].start < two[1].start));
    }

    /// 要件 7.7 と同じ畳み方——**書き手が`find`で見つかると思った語が、色でも付く**。
    #[test]
    fn folds_ascii_case_the_way_find_does() {
        let marks = WordMarks::build(vec![set(RED, &["windows"])]);

        assert_eq!(marks.marks_in("Windows と windows", 100).len(), 2);
    }

    /// その位置から始まるいちばん長い語が勝つ。「田中」と「田中さん」の両方を
    /// 入れたのは、そこを別のものとして見たいからである。
    #[test]
    fn the_longer_word_wins_where_two_start_together() {
        let marks = WordMarks::build(vec![set(RED, &["田中"]), set(BLUE, &["田中さん"])]);
        let found = marks.marks_in("田中さんが来た", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].set, 1, "長いほうのセット");
        assert_eq!(found[0].end, "田中さん".len());
    }

    /// 一致したらその先へ飛ぶので、重なった一致は出てこない。
    #[test]
    fn a_match_is_not_looked_inside() {
        let marks = WordMarks::build(vec![set(RED, &["ABC", "BC"])]);
        let found = marks.marks_in("ABC", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end, 3);
    }

    /// 同じ語を2つのセットが持っていたら、先のセットが勝つ。並びは書き手が
    /// 決めたものである。
    #[test]
    fn the_earlier_set_wins_a_tie() {
        let marks = WordMarks::build(vec![set(RED, &["猫"]), set(BLUE, &["猫"])]);
        let found = marks.marks_in("猫", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].set, 0);
    }

    /// 畳んだセットは出ない。**消すのとは別**——校正の段によって使うセットが違う。
    #[test]
    fn a_muted_set_marks_nothing() {
        let mut only = set(RED, &["猫"]);
        only.muted = true;
        let marks = WordMarks::build(vec![only]);

        assert!(marks.is_empty());
        assert!(marks.marks_in("猫が猫を", 100).is_empty());
    }

    /// 上限は守る。**本文を描くたびに走る**ので、際限なく返してはいけない。
    #[test]
    fn the_limit_is_kept() {
        let marks = WordMarks::build(vec![set(RED, &["あ"])]);
        let found = marks.marks_in(&"あ".repeat(500), 10);

        assert_eq!(found.len(), 10);
        assert_eq!(found[0].start, 0);
    }

    /// 文字の途中からは始めない。**畳むのはASCIIだけ**なので、多バイト文字の
    /// 途中のバイトが語の頭と同じでも当たらない。
    #[test]
    fn a_match_never_starts_inside_a_character() {
        let marks = WordMarks::build(vec![set(RED, &["亜"])]);
        let found = marks.marks_in("亜亜", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[1].start, 3);
    }

    /// 1行1語。**空行と`#`の行は語ではない**——何百語の一覧には見出しが要る。
    #[test]
    fn a_word_file_is_one_word_to_a_line() {
        let read = read_word_file("# 主要人物\n田中\n\n  佐藤  \n#脇役\n鈴木\n");

        assert_eq!(read, ["田中", "佐藤", "鈴木"]);
    }

    /// 印は組版の鍵に混ぜる（要件 7.9）。**語や色を変えても本文の大きさは変わらない**
    /// ので、混ぜていないと古い色のままの絵が出る（6.18の罠）。
    #[test]
    fn the_fingerprint_moves_when_anything_the_eye_sees_moves() {
        let one = WordMarks::build(vec![set(RED, &["猫"])]);
        let other_colour = WordMarks::build(vec![set(BLUE, &["猫"])]);
        let other_word = WordMarks::build(vec![set(RED, &["犬"])]);
        let mut hushed = set(RED, &["猫"]);
        hushed.muted = true;
        let hushed = WordMarks::build(vec![hushed]);

        assert_ne!(one.fingerprint(), other_colour.fingerprint(), "色");
        assert_ne!(one.fingerprint(), other_word.fingerprint(), "語");
        assert_ne!(one.fingerprint(), hushed.fingerprint(), "畳んだ");
        assert_eq!(
            one.fingerprint(),
            WordMarks::build(vec![set(RED, &["猫"])]).fingerprint(),
            "同じものは同じ"
        );
    }

    /// 語の区切りが数に入っていること。**入れないと`["ab","c"]`と`["a","bc"]`が
    /// 同じ数になる**——別の色分けなのに描き直らない。
    #[test]
    fn two_sets_split_differently_are_not_the_same_fingerprint() {
        let one = WordMarks::build(vec![set(RED, &["ab", "c"])]);
        let other = WordMarks::build(vec![set(RED, &["a", "bc"])]);

        assert_ne!(one.fingerprint(), other.fingerprint());
    }

    /// 一冊も持っていない書き手に、本文を歩く費用を払わせない。
    #[test]
    fn nothing_to_mark_is_answered_before_the_walk() {
        assert!(WordMarks::default().is_empty());
        assert!(WordMarks::default().marks_in("猫が猫を", 100).is_empty());
    }

    /// **大きい単語帳でも、本文を一度歩くだけ**（書き手の指摘 2026-09-08）。
    ///
    /// 語ごとに本文を一周する形では、5,000語×本文の長さになる。木なら位置ごとに
    /// 数歩で、**語を増やしても本文を歩く回数は増えない**。ここで固定しているのは
    /// 時間ではなく形——大きいセットでも当たるものが当たること。
    #[test]
    fn a_large_set_still_walks_the_text_once() {
        let many: Vec<String> = (0..5_000).map(|at| format!("語{at:04}")).collect();
        let words: Vec<&str> = many.iter().map(String::as_str).collect();
        let marks = WordMarks::build(vec![set(RED, &words)]);
        let source = "語0000のあとに語4999が来て、語9999は無い。".repeat(50);
        let found = marks.marks_in(&source, MAX_MARKS_PER_BLOCK);

        assert_eq!(found.len(), 100, "1周につき2つ、50周");
        assert!(found.iter().all(|mark| mark.set == 0));
    }
}
