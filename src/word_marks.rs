//! 単語セット——書き手が取り込んだ語と、それが本文のどこにあるか（要件 7.9）。
//!
//! **これは検索の言い換えである。**校正とは同じ語を何度も探すことで（要件 7.7）、
//! 単語セットは「毎回打ち込んでいた語を、置いておく場所」にあたる。だから探し方は
//! `find.rs`と同じ規則にしてある——ASCIIの大文字小文字だけを同じ字として畳み、
//! それ以外は畳まない。**書き手が`find`で見つかると思った語が、色でも付く。**
//!
//! # 取り込む。参照する。重複は言う（書き手の指摘 2026-09-08）
//!
//! **IMEの辞書と同じ形にしてある。**ファイルを指しっぱなしにするのではなく、
//! **取り込んで編集器の表にする**。表は編集器が持ち、画面から見える。
//!
//! そうする理由は重複である。書き手は同じ語を二度登録する——一覧が数百行にも
//! なれば必ず起きる——のに、**指しっぱなしの形では黙って先勝ちにするしかない**。
//! それは書き手から見て「なぜかこの語だけ色が違う」であり、原因を画面のどこからも
//! 辿れない。表にしてあれば、**取り込んだときに数えて、そう言える**。
//!
//! 重複には2つある。**同じ意味ではないので、扱いも違う。**
//!
//! - [`Trouble::Repeated`]——**同じセットの中で二度**。色は1つに決まるので迷いが
//!   無い。1つだけ採り、**言うだけ**にする（一覧を掃除する手がかりとして）。
//! - [`Trouble::Conflict`]——**2つ以上のセットにまたがる**。どちらの色で出すべきか
//!   **編集器には決められない**ので、**色を付けない**。黙って片方を選ぶのは、
//!   選んだことを書き手に隠すことである。
//!
//! # 大きくなるから、木で探す
//!
//! 語ごとに本文を一周すると、語の数×本文の長さになる。数千語の単語帳では、
//! **1打鍵の2.5ms（技術検証 6.9）に対して桁が合わない。**だから語を1本の木
//! （トライ）に積んで、**本文を一度だけ歩く**。位置ごとの費用は「そこから伸びる
//! 語の長さ」で、一致しない位置ではほとんど1歩で終わる。
//!
//! 木は**表が変わったときにだけ**建てる（`WordMarks::build`）。組版のたびに
//! 建て直しては、木にした意味が消える。
//!
//! **純粋な文字列の演算**であり、窓もペインもファイルシステムも知らない。
//! `find.rs`と`text_blocks.rs`が取っている取引と同じで、規則をテストで固定できる
//! のが値打ちである。
//!
//! **編集器はどれが正しいかを判定しない**（要件 7.9）。ここにあるのは「書かれた語が
//! どこにあるか」と「同じ語が二度あること」だけで、表記ゆれの判定は無い。

use std::collections::HashMap;
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

/// 画面に出す重複の数。**全部は出さない**——数百並べても読めない。
pub const SHOWN_TROUBLES: usize = 50;

/// 取り込んだ一冊（要件 7.9）。
///
/// **色はセットごと**で、語ごとではない。単語帳1＝赤、単語帳2＝青、という書き手の
/// 言い方がそのまま形になっている——語ごとに色を選べるようにすると、**色を決める
/// 作業が語の数だけ増える**。分けたいならセットを分ける、というほうが手数が少ない。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordSet {
    /// 書き手が付けた名前。**取り込んだファイルの名前が初期値**になる。
    pub name: String,
    /// このセットの語を出す色。
    pub colour: [f32; 3],
    /// 今は使わない、という状態。**消すのとは別**——校正の段によって使うセットが
    /// 違うので、消さずに畳んでおける必要がある。
    pub muted: bool,
    /// **どこから取り込んだか。**もう一度取り込むときの既定の場所で、
    /// それ以外の意味は無い——**語はもうこの表の中にある**。
    pub source: PathBuf,
    /// 取り込んだ語。**書かれた順のまま**で、並べ替えない。
    pub words: Vec<String>,
}

/// 重複の種類（書き手の指摘 2026-09-08、IMEの辞書と同じ考え）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// **同じセットの中に二度以上。**色は1つに決まるので迷いが無い。1つだけ採り、
    /// 言うだけにする——一覧を掃除する手がかり。
    Repeated,
    /// **2つ以上のセットにまたがる。**どちらの色で出すのか編集器には決められない
    /// ので、**色を付けない**。黙って片方を選ぶのは、選んだことを隠すことである。
    Conflict,
}

/// 取り込みで見つかった、1つの語についての言い分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordTrouble {
    pub word: String,
    pub kind: Trouble,
    /// どのセットに現れたか（`Conflict`なら2つ以上）。名前ではなく番号で持つ
    /// ——名前は書き手が変えられる。
    pub sets: Vec<usize>,
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

/// 取り込んである表のぜんぶと、そこから建てた木。
#[derive(Clone, Debug, PartialEq)]
pub struct WordMarks {
    pub sets: Vec<WordSet>,
    /// 取り込みで見つかった重複。**画面が出す**（要件 7.9：参照できること）。
    pub troubles: Vec<WordTrouble>,
    nodes: Vec<Node>,
    any: bool,
    fingerprint: u64,
}

impl Default for WordMarks {
    fn default() -> Self {
        Self::build(Vec::new())
    }
}

impl WordMarks {
    /// 表から木を建て、そのときに重複を数える。**表が変わったときにだけ呼ぶ。**
    ///
    /// 畳んだセットは数にも木にも入らない。**畳めば色が出ないので、他のセットと
    /// 衝突しようが無い**——畳んだセットが原因で別のセットの色が消えるのは、
    /// 画面から辿れない振る舞いである。
    pub fn build(sets: Vec<WordSet>) -> Self {
        // どの語が、どのセットに何回あるか。**畳んだセットは見ない。**
        let mut seen: HashMap<String, Vec<usize>> = HashMap::new();
        for (at, set) in sets.iter().enumerate() {
            if set.muted {
                continue;
            }
            for word in &set.words {
                let word = word.trim();
                if word.is_empty() {
                    continue;
                }
                seen.entry(word.to_ascii_lowercase()).or_default().push(at);
            }
        }
        let mut troubles: Vec<WordTrouble> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for (at, set) in sets.iter().enumerate() {
            if set.muted {
                continue;
            }
            let mut told: Vec<String> = Vec::new();
            for word in &set.words {
                let word = word.trim();
                if word.is_empty() {
                    continue;
                }
                let folded = word.to_ascii_lowercase();
                let Some(places) = seen.get(&folded) else {
                    continue;
                };
                // **言うのは一度だけ。**同じ語が3回あっても言い分は1つである。
                if told.contains(&folded) {
                    continue;
                }
                let elsewhere: Vec<usize> = {
                    let mut all = places.clone();
                    all.dedup();
                    all
                };
                if elsewhere.len() > 1 {
                    // **セットをまたいだ衝突**：色を付けない。
                    if !refused.contains(&folded) {
                        refused.push(folded.clone());
                        troubles.push(WordTrouble {
                            word: word.to_owned(),
                            kind: Trouble::Conflict,
                            sets: elsewhere,
                        });
                    }
                    told.push(folded);
                } else if places.len() > 1 {
                    // **同じセットの中で二度以上**：1つ採って、言うだけ。
                    troubles.push(WordTrouble {
                        word: word.to_owned(),
                        kind: Trouble::Repeated,
                        sets: vec![at],
                    });
                    told.push(folded);
                }
            }
        }

        let mut nodes = vec![Node::default()];
        let mut any = false;
        for (at, set) in sets.iter().enumerate() {
            if set.muted {
                continue;
            }
            for word in &set.words {
                let word = word.trim();
                if word.is_empty() || refused.contains(&word.to_ascii_lowercase()) {
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
                // 同じセットの中の二度目は、ここで黙って落ちる（上で言ってある）。
                if nodes[node].ends.is_none() {
                    nodes[node].ends = Some((at as u32, word.len() as u32));
                    any = true;
                }
            }
        }
        let fingerprint = fingerprint_of(&sets);
        Self {
            sets,
            troubles,
            nodes,
            any,
            fingerprint,
        }
    }

    /// 色を付けない語がいくつあるか（`Conflict`の数）。
    pub fn conflicts(&self) -> usize {
        self.troubles
            .iter()
            .filter(|trouble| trouble.kind == Trouble::Conflict)
            .count()
    }

    /// このセットについての言い分だけ。
    pub fn troubles_of(&self, set: usize) -> Vec<&WordTrouble> {
        self.troubles
            .iter()
            .filter(|trouble| trouble.sets.contains(&set))
            .collect()
    }

    /// 何も出すものが無いか。**先に訊く**——一冊も取り込んでいない書き手に、
    /// 本文を歩く費用を払わせない（普通はこちらである）。
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    /// この表を一つの数にする。
    ///
    /// **組版とタイルの鍵に混ぜるためのもの**（要件 7.9）。語や色を変えても本文の
    /// 大きさは1画素も変わらないので、これを混ぜないと**絵置き場にある古い色のままの
    /// 絵がそのまま出る**——技術検証 6.18 が何度も踏んでいる罠である。
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// 本文の中の一致を、位置の順に。**重なりは残さない。**
    ///
    /// 本文を一度だけ歩き、**位置ごとに木をいちばん深くまで下りる**。その位置から
    /// 始まるいちばん長い語が勝ち、一致したらその先へ飛ぶ。始めるのは文字の境目だけ。
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

/// 取り込んだファイルの中身を、語の並びに（要件 7.9）。
///
/// **1行に1語。**前後の空白は落とし、空行は語ではない。`#`で始まる行は覚え書きで、
/// 語ではない——**何百語の一覧には見出しが要る**（「主要人物」「脇役」）ので、
/// 書き手が自分で区切れるようにしてある。
///
/// **重複はここで落とさない。**落とせば書き手に言えなくなる——数えるのは
/// [`WordMarks::build`]で、言うのは画面である。
pub fn read_word_file(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .take(MAX_WORDS_PER_SET)
        .collect()
}

fn fingerprint_of(sets: &[WordSet]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for set in sets {
        set.muted.hash(&mut hasher);
        set.name.hash(&mut hasher);
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

    fn set(name: &str, colour: [f32; 3], words: &[&str]) -> WordSet {
        WordSet {
            name: name.to_owned(),
            colour,
            muted: false,
            source: PathBuf::from(format!("D:\\原稿\\{name}.txt")),
            words: words.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    #[test]
    fn finds_every_occurrence_in_order() {
        let marks = WordMarks::build(vec![set("人物", RED, &["田中"])]);
        let found = marks.marks_in("田中と佐藤、そして田中。", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[1].start, "田中と佐藤、そして".len());
    }

    /// 要件 7.7 と同じ畳み方——**書き手が`find`で見つかると思った語が、色でも付く**。
    #[test]
    fn folds_ascii_case_the_way_find_does() {
        let marks = WordMarks::build(vec![set("英語", RED, &["windows"])]);

        assert_eq!(marks.marks_in("Windows と windows", 100).len(), 2);
    }

    /// **2つのセットにまたがる語には、色を付けない**（書き手の指摘 2026-09-08）。
    ///
    /// どちらの色で出すのか編集器には決められない。黙って片方を選ぶのは、
    /// 選んだことを書き手に隠すことである。
    #[test]
    fn a_word_in_two_sets_gets_no_colour_and_is_reported() {
        let marks = WordMarks::build(vec![
            set("人物", RED, &["田中", "佐藤"]),
            set("敵役", BLUE, &["田中", "山田"]),
        ]);

        assert!(
            marks.marks_in("田中", 100).is_empty(),
            "衝突には色を付けない"
        );
        assert_eq!(
            marks.marks_in("佐藤", 100).len(),
            1,
            "衝突していない語は付く"
        );
        assert_eq!(marks.marks_in("山田", 100).len(), 1);

        let told = &marks.troubles;
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].word, "田中");
        assert_eq!(told[0].kind, Trouble::Conflict);
        assert_eq!(told[0].sets, [0, 1], "どちらにあるかを言う");
        assert_eq!(marks.conflicts(), 1);
    }

    /// **同じセットの中の二度は、言うだけ。**色は1つに決まるので迷いが無い。
    #[test]
    fn a_word_twice_in_one_set_is_kept_and_mentioned() {
        let marks = WordMarks::build(vec![set("人物", RED, &["田中", "佐藤", "田中"])]);

        assert_eq!(marks.marks_in("田中", 100).len(), 1, "色は付く");
        assert_eq!(marks.troubles.len(), 1);
        assert_eq!(marks.troubles[0].kind, Trouble::Repeated);
        assert_eq!(marks.troubles[0].word, "田中");
        assert_eq!(marks.conflicts(), 0, "これは衝突ではない");
    }

    /// 三度あっても言い分は1つ。**一覧に同じ苦情を並べない。**
    #[test]
    fn the_same_trouble_is_told_once() {
        let marks = WordMarks::build(vec![set("人物", RED, &["猫", "猫", "猫"])]);

        assert_eq!(marks.troubles.len(), 1);
    }

    /// 大小の違いだけでも重複である——探し方がそれを同じ字として畳むのだから、
    /// **登録も同じ字として重なる**。
    #[test]
    fn case_only_differences_are_duplicates_too() {
        let marks = WordMarks::build(vec![
            set("英語", RED, &["Windows"]),
            set("用語", BLUE, &["windows"]),
        ]);

        assert_eq!(marks.conflicts(), 1);
        assert!(marks.marks_in("Windows", 100).is_empty());
    }

    /// **畳んだセットは衝突しない。**色が出ないのだから、他のセットの色を
    /// 消す理由が無い——それは画面から辿れない振る舞いである。
    #[test]
    fn a_muted_set_does_not_take_a_colour_away() {
        let mut hushed = set("敵役", BLUE, &["田中"]);
        hushed.muted = true;
        let marks = WordMarks::build(vec![set("人物", RED, &["田中"]), hushed]);

        assert_eq!(marks.conflicts(), 0);
        assert_eq!(marks.marks_in("田中", 100).len(), 1);
        assert_eq!(marks.marks_in("田中", 100)[0].set, 0);
    }

    /// その位置から始まるいちばん長い語が勝つ。
    #[test]
    fn the_longer_word_wins_where_two_start_together() {
        let marks = WordMarks::build(vec![set("人物", RED, &["田中", "田中さん"])]);
        let found = marks.marks_in("田中さんが来た", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end, "田中さん".len());
    }

    /// 一致したらその先へ飛ぶので、重なった一致は出てこない。
    #[test]
    fn a_match_is_not_looked_inside() {
        let marks = WordMarks::build(vec![set("英語", RED, &["ABC", "BC"])]);
        let found = marks.marks_in("ABC", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end, 3);
    }

    /// どのセットについての言い分かを引ける（画面が1セットぶんだけ出すため）。
    #[test]
    fn troubles_can_be_asked_for_one_set() {
        let marks = WordMarks::build(vec![
            set("人物", RED, &["田中", "田中"]),
            set("敵役", BLUE, &["山田"]),
        ]);

        assert_eq!(marks.troubles_of(0).len(), 1);
        assert!(marks.troubles_of(1).is_empty());
    }

    #[test]
    fn the_limit_is_kept() {
        let marks = WordMarks::build(vec![set("あ", RED, &["あ"])]);
        let found = marks.marks_in(&"あ".repeat(500), 10);

        assert_eq!(found.len(), 10);
    }

    /// 文字の途中からは始めない。**畳むのはASCIIだけ**なので、多バイト文字の
    /// 途中のバイトが語の頭と同じでも当たらない。
    #[test]
    fn a_match_never_starts_inside_a_character() {
        let marks = WordMarks::build(vec![set("字", RED, &["亜"])]);
        let found = marks.marks_in("亜亜", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[1].start, 3);
    }

    /// 1行1語。**空行と`#`の行は語ではない**——何百語の一覧には見出しが要る。
    /// **重複はここで落とさない**：落とせば書き手に言えなくなる。
    #[test]
    fn a_word_file_is_one_word_to_a_line() {
        let read = read_word_file("# 主要人物\n田中\n\n  佐藤  \n#脇役\n田中\n");

        assert_eq!(read, ["田中", "佐藤", "田中"]);
    }

    /// 印は組版の鍵に混ぜる（要件 7.9）。
    #[test]
    fn the_fingerprint_moves_when_anything_the_eye_sees_moves() {
        let one = WordMarks::build(vec![set("人物", RED, &["猫"])]);
        let other_colour = WordMarks::build(vec![set("人物", BLUE, &["猫"])]);
        let other_word = WordMarks::build(vec![set("人物", RED, &["犬"])]);
        let mut hushed = set("人物", RED, &["猫"]);
        hushed.muted = true;
        let hushed = WordMarks::build(vec![hushed]);

        assert_ne!(one.fingerprint(), other_colour.fingerprint(), "色");
        assert_ne!(one.fingerprint(), other_word.fingerprint(), "語");
        assert_ne!(one.fingerprint(), hushed.fingerprint(), "畳んだ");
        assert_eq!(
            one.fingerprint(),
            WordMarks::build(vec![set("人物", RED, &["猫"])]).fingerprint()
        );
    }

    /// 語の区切りが数に入っていること。
    #[test]
    fn two_sets_split_differently_are_not_the_same_fingerprint() {
        let one = WordMarks::build(vec![set("あ", RED, &["ab", "c"])]);
        let other = WordMarks::build(vec![set("あ", RED, &["a", "bc"])]);

        assert_ne!(one.fingerprint(), other.fingerprint());
    }

    #[test]
    fn nothing_to_mark_is_answered_before_the_walk() {
        assert!(WordMarks::default().is_empty());
        assert!(WordMarks::default().marks_in("猫が猫を", 100).is_empty());
    }

    /// **大きい単語帳でも、本文を一度歩くだけ**（書き手の指摘 2026-09-08）。
    #[test]
    fn a_large_set_still_walks_the_text_once() {
        let many: Vec<String> = (0..5_000).map(|at| format!("語{at:04}")).collect();
        let words: Vec<&str> = many.iter().map(String::as_str).collect();
        let marks = WordMarks::build(vec![set("多い", RED, &words)]);
        let source = "語0000のあとに語4999が来て、語9999は無い。".repeat(50);
        let found = marks.marks_in(&source, MAX_MARKS_PER_BLOCK);

        assert_eq!(found.len(), 100, "1周につき2つ、50周");
        assert!(marks.troubles.is_empty(), "重複は無い");
    }
}
