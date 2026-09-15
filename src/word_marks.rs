//! 単語チェックモード——**書き手が自分で作る「言語モード」**（要件 7.9）。
//!
//! # 形（書き手の指摘 2026-09-08）
//!
//! **考え方はソースの予約語の色分けと同じ**である。C言語モードが予約語・型・
//! 前処理をそれぞれの色で持つように、**モードが語群を持ち、語群が色を持つ。**
//!
//! ```text
//! モード「作品A」
//!   ├ 語群「人物」#cc3333 ── 田中、佐藤、鈴木…
//!   ├ 語群「地名」#3355cc ── 京都、奈良…
//!   └ 語群「伏線」#886611 ── 鍵、手紙…
//! モード「作品B」
//!   └ 語群「人物」#cc3333 ── （別の作品の別の人たち）
//! ```
//!
//! **モードは文書ごと**（コードエディタの言語モードと同じ粒度）。作品Aの章を
//! 開けば作品Aのモード、というのが素直な形で、**語群を一冊ずつOn/Offするのは
//! 直感的でない**——それは「C言語モードを使うのに予約語グループを個別に
//! 有効化する」ことに当たる。
//!
//! **重複はモードの中でだけ起きる。**別のモードの語群と重なっても、同時に
//! 出ることが無いのだから衝突ではない。
//!
//! # 探し方は`find.rs`と同じ
//!
//! ASCIIの大文字小文字だけを同じ字として畳み、それ以外は畳まない——**書き手が
//! `find`で見つかると思った語が、色でも付く。**校正とは同じ語を何度も探すこと
//! （要件 7.7）で、語群は「毎回打ち込んでいた語を、置いておく場所」である。
//!
//! # 重複は数えて言う。色は付けない
//!
//! 書き手は同じ語を二度登録する——一覧が数百行にもなれば必ず起きる。黙って
//! 先勝ちにすると、書き手から見て「なぜかこの語だけ色が違う」を画面のどこからも
//! 辿れない。だから**取り込んだときに数えて、そう言う**（IMEの辞書と同じ）。
//!
//! - [`Trouble::Repeated`]——**同じ語群の中で二度**。色は1つに決まるので迷いが
//!   無い。1つだけ採り、**言うだけ**にする（一覧を掃除する手がかり）。
//! - [`Trouble::Conflict`]——**同じモードの2つ以上の語群にまたがる**。どちらの
//!   色で出すべきか**編集器には決められない**ので、**色を付けない**。黙って
//!   片方を選ぶのは、選んだことを書き手に隠すことである。
//!
//! # 大きくなるから、木で探す
//!
//! 語ごとに本文を一周すると、語の数×本文の長さになる。数千語では**1打鍵の
//! 2.5ms（技術検証 6.9）に対して桁が合わない。**語を1本の木（トライ）に積んで
//! **本文を一度だけ歩く**——位置ごとの費用は「そこから伸びる語の長さ」で、
//! 一致しない位置ではほとんど1歩で終わる。
//!
//! 木は**モードごとに、表が変わったときにだけ**建てる。組版のたびに建て直しては
//! 意味が消える。
//!
//! **純粋な文字列の演算**であり、窓もペインもファイルシステムも知らない。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// モードをいくつまで持てるか。
pub const MAX_WORD_MODES: usize = 16;

/// 1つのモードが持てる語群の数。
///
/// **色を用意するぶんだけ上限が要る**——描く側は語群ごとに筆を1本持つ。8つは
/// 「人物・地名・伏線・多用しがちな語・禁止語」くらいで、**足りないと言われて
/// から増やす**（要件 15）。
pub const MAX_WORD_GROUPS: usize = 8;

/// 1つのブロックの中で色を付ける数の上限。
///
/// **本文を描くたびに走る**ので、際限は要る。**ただし散文で届かないところに置く**
/// （2026-09-08改訂、書き手の言い直し）。この機能の値打ちは**色が付かなかったこと**に
/// あるので——`リオン`のつもりが`リオソ`だったと気づくため——**上限で黙って止まると、
/// そこから先の本文が「打ち間違えた」ように見える。いちばん質の悪い嘘である。**
///
/// 256にしていたのは「1段落に256か所も色が付けば塗りつぶしだ」という理由だったが、
/// **読める量の歯止めと費用の歯止めを混ぜていた。**読めるかどうかは書き手が決める。
pub const MAX_MARKS_PER_BLOCK: usize = 2048;

/// 1つの語群の語数の上限。
pub const MAX_WORDS_PER_GROUP: usize = 10_000;

/// 画面に出す重複の数。**全部は出さない**——数百並べても読めない。
pub const SHOWN_TROUBLES: usize = 50;

/// **どのモードでもない**、という状態の名前（要件 7.9）。
///
/// コードエディタの`Plain Text`にあたる。**必ず選べる**ので、色分けを止めるのに
/// モードを消す必要が無い。
///
/// **言語で名前が変わる**（国際化②）。どちらの名前も、書き手が付けるモードの名前には使えない。
pub const NO_MODE_NAMES: (&str, &str) = ("なし", "None");

pub fn no_mode() -> &'static str {
    crate::i18n::pick(NO_MODE_NAMES.0, NO_MODE_NAMES.1)
}

/// ひとつの語群——名前と、色と、語（要件 7.9）。
///
/// **色は語群ごと**で、語ごとではない。語ごとに色を選べるようにすると、**色を
/// 決める作業が語の数だけ増える**。分けたいなら語群を分ける、というほうが手数が
/// 少ない。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordGroup {
    /// **変わらない番号**（2026-09-08）。名前は書き手が変えるもので、変えたら
    /// 指していたものが切れる——それは名前の仕事ではない。
    pub id: u32,
    pub name: String,
    /// **`None`は「色を付けない」**（2026-09-08、書き手と決めた除外語群）。
    ///
    /// **語は木に積まれ、最長一致で勝つ。ただし何も塗らない。**`リオン`を色分け
    /// している書き手が`カリオン`をここへ入れると、`カリオン`の中で`リオン`が
    /// 光らなくなる——**照合の仕組みは1行も変えずに、包む語を書き手が言える。**
    pub colour: Option<[f32; 3]>,
    /// **書かれた順のまま**で、並べ替えない。
    pub words: Vec<String>,
}

/// ひとつのモード——名前と、語群（要件 7.9）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WordMode {
    /// **変わらない番号。**文書（タブ）はこれを指す——名前で指していたときは、
    /// 名前を変えた瞬間に文書のモードが切れていた。
    ///
    /// **消した番号は二度と使わない**（表が`next`を覚えている）。使い回すと、
    /// 古いセッションが指していた番号が**別のモード**を指すことになり、
    /// 「切れている」より悪い。
    pub id: u32,
    pub name: String,
    pub groups: Vec<WordGroup>,
}

impl WordMode {
    pub fn words(&self) -> usize {
        self.groups.iter().map(WordGroup::word_count).sum()
    }
}

impl WordGroup {
    /// **覚え書きは語ではない**（2026-09-08）。`10語`と出すのはこの数である。
    pub fn word_count(&self) -> usize {
        self.words.iter().filter(|line| !is_note(line)).count()
    }
}

/// 覚え書きの印。
pub const NOTE_MARK: char = '#';

/// この行は語ではなく覚え書きか（単語チェックモード要件 4.2、2026-09-08）。
///
/// **手で書き足せる形にした以上、見出しが要る**——「主要人物」「脇役」。
/// 書き手の求め：「手作業で追加することを許すなら、コメント追加は必要」。
///
/// **覚え書きは語群の中に、語と同じ並びで残る。**別の場所へ避けると順番が失われ、
/// 「この見出しの下の語」という書き手の並べ方が消える。木には積まないので、
/// 本文の`#`に色が付くことはない。
pub fn is_note(line: &str) -> bool {
    line.trim_start().starts_with(NOTE_MARK)
}

/// 重複の種類（IMEの辞書と同じ考え）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// **同じ語群の中に二度以上。**1つだけ採り、言うだけ。
    Repeated,
    /// **同じモードの2つ以上の語群にまたがる。**色を付けない。
    Conflict,
}

/// 取り込みで見つかった、1つの語についての言い分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordTrouble {
    pub word: String,
    pub kind: Trouble,
    /// どの語群に現れたか（`Conflict`なら2つ以上）。
    pub groups: Vec<usize>,
}

/// 本文の中の、ある一致。
///
/// バイト位置で答える。`find.rs`と同じ理由で安全である：畳むのはASCIIの大小だけ
/// なので、一致した本文は語とちょうど同じ長さで、一致が文字の途中から始まることも
/// ない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordMark {
    pub start: usize,
    pub end: usize,
    /// 何番目の語群か。色はその語群が持っている。
    pub group: usize,
}

/// 木の1つの節。
///
/// 子は**並べた配列**で持ち、二分探索で引く。`HashMap`を節ごとに持つと、数万の節に
/// 対して割り当てが数万回になる——木にした目的が費用なのだから、そこで払っては
/// いけない。
#[derive(Clone, Debug, Default, PartialEq)]
struct Node {
    children: Vec<(u8, u32)>,
    /// ここで終わる語があるなら、その（語群の番号, バイト長）。
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

/// ひとつのモードを、引ける形にしたもの。
#[derive(Clone, Debug, PartialEq)]
pub struct WordMarks {
    pub mode: WordMode,
    /// 取り込みで見つかった重複。**画面が出す**（要件 7.9：参照できること）。
    pub troubles: Vec<WordTrouble>,
    nodes: Vec<Node>,
    any: bool,
    fingerprint: u64,
}

impl Default for WordMarks {
    fn default() -> Self {
        Self::build(WordMode::default())
    }
}

impl WordMarks {
    /// モードから木を建て、そのときに重複を数える。**モードが変わったときにだけ。**
    pub fn build(mode: WordMode) -> Self {
        // どの語が、どの語群に何回あるか。
        let mut seen: HashMap<String, Vec<usize>> = HashMap::new();
        for (at, group) in mode.groups.iter().enumerate() {
            for word in &group.words {
                let word = word.trim();
                if word.is_empty() || is_note(word) {
                    continue;
                }
                seen.entry(word.to_ascii_lowercase()).or_default().push(at);
            }
        }
        let mut troubles: Vec<WordTrouble> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for (at, group) in mode.groups.iter().enumerate() {
            let mut told: Vec<String> = Vec::new();
            for word in &group.words {
                let word = word.trim();
                if word.is_empty() || is_note(word) {
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
                let mut elsewhere = places.clone();
                elsewhere.dedup();
                if elsewhere.len() > 1 {
                    // **語群をまたいだ衝突**：色を付けない。
                    if !refused.contains(&folded) {
                        refused.push(folded.clone());
                        troubles.push(WordTrouble {
                            word: word.to_owned(),
                            kind: Trouble::Conflict,
                            groups: elsewhere,
                        });
                    }
                    told.push(folded);
                } else if places.len() > 1 {
                    // **同じ語群の中で二度以上**：1つ採って、言うだけ。
                    troubles.push(WordTrouble {
                        word: word.to_owned(),
                        kind: Trouble::Repeated,
                        groups: vec![at],
                    });
                    told.push(folded);
                }
            }
        }

        let mut nodes = vec![Node::default()];
        let mut any = false;
        for (at, group) in mode.groups.iter().enumerate() {
            for word in &group.words {
                let word = word.trim();
                if word.is_empty() || is_note(word) || refused.contains(&word.to_ascii_lowercase())
                {
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
                // 同じ語群の中の二度目は、ここで黙って落ちる（上で言ってある）。
                if nodes[node].ends.is_none() {
                    nodes[node].ends = Some((at as u32, word.len() as u32));
                    any = true;
                }
            }
        }
        let fingerprint = fingerprint_of(&mode);
        Self {
            mode,
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

    /// この語群についての言い分だけ。
    pub fn troubles_of(&self, group: usize) -> Vec<&WordTrouble> {
        self.troubles
            .iter()
            .filter(|trouble| trouble.groups.contains(&group))
            .collect()
    }

    /// 何も出すものが無いか。**先に訊く**——モードを持たない書き手に、本文を
    /// 歩く費用を払わせない（普通はこちらである）。
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    /// このモードを一つの数にする。
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
                Some((group, length)) => {
                    let length = length as usize;
                    found.push(WordMark {
                        start: at,
                        end: at + length,
                        group: group as usize,
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
/// 語ではない——**何百語の一覧には見出しが要る**ので、書き手が自分で区切れる
/// ようにしてある。
///
/// **重複はここで落とさない。**落とせば書き手に言えなくなる。
pub fn read_word_file(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        // **`#`の行は捨てずに持ち帰る**（2026-09-08）。覚え書きは語群の中に
        // 残るようになったので、取り込んだ一覧の見出しもそのまま残る——
        // 書き出して直して取り込む道で、並べ方が消えなくなった。
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .take(MAX_WORDS_PER_GROUP)
        .collect()
}

fn fingerprint_of(mode: &WordMode) -> u64 {
    let mut hasher = DefaultHasher::new();
    mode.id.hash(&mut hasher);
    mode.name.hash(&mut hasher);
    for group in &mode.groups {
        group.name.hash(&mut hasher);
        // **色を消したことも見た目の変化である**（2026-09-08）。有無そのものを
        // 混ぜないと、色を落とした語群が同じ数のままになる。
        group.colour.is_some().hash(&mut hasher);
        for channel in group.colour.iter().flatten() {
            channel.to_bits().hash(&mut hasher);
        }
        for word in &group.words {
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

    fn group(name: &str, colour: [f32; 3], words: &[&str]) -> WordGroup {
        WordGroup {
            id: 1,
            name: name.to_owned(),
            colour: Some(colour),
            words: words.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    fn mode(groups: Vec<WordGroup>) -> WordMarks {
        WordMarks::build(WordMode {
            id: 1,
            name: "作品A".to_owned(),
            groups,
        })
    }

    #[test]
    fn finds_every_occurrence_in_order() {
        let marks = mode(vec![group("人物", RED, &["田中"])]);
        let found = marks.marks_in("田中と佐藤、そして田中。", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[1].start, "田中と佐藤、そして".len());
    }

    /// 要件 7.7 と同じ畳み方——**書き手が`find`で見つかると思った語が、色でも付く**。
    #[test]
    fn folds_ascii_case_the_way_find_does() {
        let marks = mode(vec![group("英語", RED, &["windows"])]);

        assert_eq!(marks.marks_in("Windows と windows", 100).len(), 2);
    }

    /// **同じモードの2つの語群にまたがる語には、色を付けない。**どちらの色で
    /// 出すのか編集器には決められない——黙って片方を選ぶのは、選んだことを
    /// 書き手に隠すことである。
    #[test]
    fn a_word_in_two_groups_gets_no_colour_and_is_reported() {
        let marks = mode(vec![
            group("人物", RED, &["田中", "佐藤"]),
            group("敵役", BLUE, &["田中", "山田"]),
        ]);

        assert!(
            marks.marks_in("田中", 100).is_empty(),
            "衝突には色を付けない"
        );
        assert_eq!(marks.marks_in("佐藤", 100).len(), 1);
        assert_eq!(marks.marks_in("山田", 100).len(), 1);

        assert_eq!(marks.troubles.len(), 1);
        assert_eq!(marks.troubles[0].word, "田中");
        assert_eq!(marks.troubles[0].kind, Trouble::Conflict);
        assert_eq!(marks.troubles[0].groups, [0, 1]);
        assert_eq!(marks.conflicts(), 1);
    }

    /// **別のモードとは衝突しない**（書き手の指摘 2026-09-08）。同時に出ることが
    /// 無いのだから、衝突ではない——C言語モードの`int`とC#モードの`int`が
    /// 喧嘩しないのと同じである。
    #[test]
    fn two_modes_do_not_collide_with_each_other() {
        let one = mode(vec![group("人物", RED, &["田中"])]);
        let other = WordMarks::build(WordMode {
            id: 2,
            name: "作品B".to_owned(),
            groups: vec![group("人物", BLUE, &["田中"])],
        });

        assert_eq!(one.conflicts(), 0);
        assert_eq!(other.conflicts(), 0);
        assert_eq!(one.marks_in("田中", 100).len(), 1);
        assert_eq!(other.marks_in("田中", 100).len(), 1);
    }

    /// **同じ語群の中の二度は、言うだけ。**色は1つに決まるので迷いが無い。
    #[test]
    fn a_word_twice_in_one_group_is_kept_and_mentioned() {
        let marks = mode(vec![group("人物", RED, &["田中", "佐藤", "田中"])]);

        assert_eq!(marks.marks_in("田中", 100).len(), 1, "色は付く");
        assert_eq!(marks.troubles.len(), 1);
        assert_eq!(marks.troubles[0].kind, Trouble::Repeated);
        assert_eq!(marks.conflicts(), 0);
    }

    /// 三度あっても言い分は1つ。**一覧に同じ苦情を並べない。**
    #[test]
    fn the_same_trouble_is_told_once() {
        let marks = mode(vec![group("人物", RED, &["猫", "猫", "猫"])]);

        assert_eq!(marks.troubles.len(), 1);
    }

    /// 大小の違いだけでも重複である——探し方がそれを同じ字として畳むのだから、
    /// **登録も同じ字として重なる**。
    #[test]
    fn case_only_differences_are_duplicates_too() {
        let marks = mode(vec![
            group("英語", RED, &["Windows"]),
            group("用語", BLUE, &["windows"]),
        ]);

        assert_eq!(marks.conflicts(), 1);
        assert!(marks.marks_in("Windows", 100).is_empty());
    }

    /// その位置から始まるいちばん長い語が勝つ。
    #[test]
    fn the_longer_word_wins_where_two_start_together() {
        let marks = mode(vec![group("人物", RED, &["田中", "田中さん"])]);
        let found = marks.marks_in("田中さんが来た", 100);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end, "田中さん".len());
    }

    /// 一致したらその先へ飛ぶので、重なった一致は出てこない。
    #[test]
    fn a_match_is_not_looked_inside() {
        let marks = mode(vec![group("英語", RED, &["ABC", "BC"])]);

        assert_eq!(marks.marks_in("ABC", 100).len(), 1);
    }

    /// どの語群についての言い分かを引ける（画面が1つぶんだけ出すため）。
    #[test]
    fn troubles_can_be_asked_for_one_group() {
        let marks = mode(vec![
            group("人物", RED, &["田中", "田中"]),
            group("敵役", BLUE, &["山田"]),
        ]);

        assert_eq!(marks.troubles_of(0).len(), 1);
        assert!(marks.troubles_of(1).is_empty());
    }

    #[test]
    fn the_limit_is_kept() {
        let marks = mode(vec![group("あ", RED, &["あ"])]);

        assert_eq!(marks.marks_in(&"あ".repeat(500), 10).len(), 10);
        // **既定の上限は散文で届かない**（2026-09-08）。上限で黙って止まると、
        // そこから先が「打ち間違えた」ように見える。
        assert!(MAX_MARKS_PER_BLOCK >= 2048);
    }

    /// 文字の途中からは始めない。**畳むのはASCIIだけ**である。
    #[test]
    fn a_match_never_starts_inside_a_character() {
        let marks = mode(vec![group("字", RED, &["亜"])]);
        let found = marks.marks_in("亜亜", 100);

        assert_eq!(found.len(), 2);
        assert_eq!(found[1].start, 3);
    }

    /// 1行1語。**空行は落とす**が、**`#`の行は覚え書きとして持ち帰る**
    /// （2026-09-08、書き手の求め）。**重複はここで落とさない**。
    #[test]
    fn a_word_file_is_one_word_to_a_line() {
        let read = read_word_file("# 主要人物\n田中\n\n  佐藤  \n#脇役\n田中\n");

        assert_eq!(read, ["# 主要人物", "田中", "佐藤", "#脇役", "田中"]);
    }

    /// **覚え書きは語ではない**（2026-09-08）。木に積まず、数にも入れず、
    /// 重複にも数えない——本文の`#`に色が付いてはいけない。
    #[test]
    fn a_note_is_not_a_word() {
        let marks = mode(vec![group("人物", RED, &["# 主要人物", "田中", "# 脇役"])]);

        assert_eq!(marks.mode.groups[0].word_count(), 1, "語は「田中」ひとつ");
        assert!(marks.troubles.is_empty(), "覚え書きは重複ではない");
        assert!(
            marks.marks_in("# 主要人物 と 田中", 100).len() == 1,
            "色が付くのは語だけ"
        );
    }

    /// 印は組版の鍵に混ぜる（要件 7.9）。**モードの名前も数のうち**——名前だけ
    /// 変えても画面の下の表示が変わる。
    #[test]
    fn the_fingerprint_moves_when_anything_the_eye_sees_moves() {
        let one = mode(vec![group("人物", RED, &["猫"])]);
        let other_colour = mode(vec![group("人物", BLUE, &["猫"])]);
        let other_word = mode(vec![group("人物", RED, &["犬"])]);
        let other_name = WordMarks::build(WordMode {
            id: 1,
            name: "作品B".to_owned(),
            groups: vec![group("人物", RED, &["猫"])],
        });

        assert_ne!(one.fingerprint(), other_colour.fingerprint(), "色");
        assert_ne!(one.fingerprint(), other_word.fingerprint(), "語");
        assert_ne!(one.fingerprint(), other_name.fingerprint(), "モードの名前");
        assert_eq!(
            one.fingerprint(),
            mode(vec![group("人物", RED, &["猫"])]).fingerprint()
        );
    }

    /// 語の区切りが数に入っていること。
    #[test]
    fn two_groups_split_differently_are_not_the_same_fingerprint() {
        let one = mode(vec![group("あ", RED, &["ab", "c"])]);
        let other = mode(vec![group("あ", RED, &["a", "bc"])]);

        assert_ne!(one.fingerprint(), other.fingerprint());
    }

    /// モードを持たない書き手に、本文を歩く費用を払わせない。
    #[test]
    fn nothing_to_mark_is_answered_before_the_walk() {
        assert!(WordMarks::default().is_empty());
        assert!(WordMarks::default().marks_in("猫が猫を", 100).is_empty());
    }

    /// **大きい語群でも、本文を一度歩くだけ。**
    #[test]
    fn a_large_group_still_walks_the_text_once() {
        let many: Vec<String> = (0..5_000).map(|at| format!("語{at:04}")).collect();
        let words: Vec<&str> = many.iter().map(String::as_str).collect();
        let marks = mode(vec![group("多い", RED, &words)]);
        let source = "語0000のあとに語4999が来て、語9999は無い。".repeat(50);

        assert_eq!(marks.marks_in(&source, MAX_MARKS_PER_BLOCK).len(), 100);
        assert!(marks.troubles.is_empty());
    }

    #[test]
    fn a_mode_counts_its_words() {
        let held = WordMode {
            id: 1,
            name: "作品A".to_owned(),
            groups: vec![
                group("人物", RED, &["田中", "佐藤"]),
                group("地名", BLUE, &["京都"]),
            ],
        };

        assert_eq!(held.words(), 3);
    }
}

#[cfg(test)]
mod collision_cases {
    use super::*;

    const RED: [f32; 3] = [0.8, 0.2, 0.2];
    const BLUE: [f32; 3] = [0.2, 0.2, 0.8];

    fn built(groups: Vec<WordGroup>) -> WordMarks {
        WordMarks::build(WordMode {
            id: 1,
            name: "調べ".to_owned(),
            groups,
        })
    }

    fn group(name: &str, colour: [f32; 3], words: &[&str]) -> WordGroup {
        WordGroup {
            id: 1,
            name: name.to_owned(),
            colour: Some(colour),
            words: words.iter().map(|held| (*held).to_owned()).collect(),
        }
    }

    fn hits(marks: &WordMarks, source: &str) -> Vec<String> {
        marks
            .marks_in(source, 100)
            .into_iter()
            .map(|mark| source[mark.start..mark.end].to_owned())
            .collect()
    }

    #[test]
    fn case_analysis() {
        // 1. 長い語が登録されていれば勝つ（前・中・後、どこに包まれても）
        let both = built(vec![group("人物", RED, &["リオン", "カリオン"])]);
        assert_eq!(hits(&both, "カリオンとリオン"), ["カリオン", "リオン"]);

        // 2. 長い語が別の語群にいても勝つ（モードの中の話）
        let split = built(vec![
            group("人物", RED, &["リオン"]),
            group("敵役", BLUE, &["カリオン"]),
        ]);
        assert_eq!(hits(&split, "カリオン"), ["カリオン"]);

        // 3. 長い語が登録されていなければ、中で当たる（これが報告された姿）
        let alone = built(vec![group("人物", RED, &["リオン"])]);
        assert_eq!(hits(&alone, "カリオン"), ["リオン"]);

        // 4. 語をまたぐ当たり——包む「語」が存在しない
        assert_eq!(hits(&alone, "メモリオンライン"), ["リオン"]);
        let kanji = built(vec![group("語", RED, &["対応"])]);
        assert_eq!(hits(&kanji, "絶対応答"), ["対応"]);

        // 5. 長い語を登録すれば、またぐ当たりも止まる
        let guarded = built(vec![
            group("人物", RED, &["リオン"]),
            group("除外", BLUE, &["メモリオンライン"]),
        ]);
        assert_eq!(hits(&guarded, "メモリオンライン"), ["メモリオンライン"]);

        // 6. 包む語が二つ重なるとき（カリオン と リオンズ）
        let two = built(vec![group(
            "人物",
            RED,
            &["リオン", "カリオン", "リオンズ"],
        )]);
        assert_eq!(hits(&two, "カリオンズ"), ["カリオン"]);
    }

    /// 除外語群（書き手と決めた 2026-09-08）: **色を持たない語群は、木に積まれて
    /// 最長一致で勝ち、しかし何も塗らない。**
    ///
    /// `marks_in`は印を返し続ける——**塗らないのは描くときの判断**（`draw_tile`）で
    /// ある。ここで落とすと、その位置から短い語がもう一度当たってしまい、止めたい
    /// ものが止まらない。
    #[test]
    fn an_excluded_word_still_takes_its_span() {
        let mut excluded = group("除外", RED, &["カリオン"]);
        excluded.colour = None;
        let marks = built(vec![group("人物", RED, &["リオン"]), excluded]);

        let source = "カリオンとリオン";
        let found = marks.marks_in(source, 100);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].group, 1, "カリオンは除外語群のもの");
        assert_eq!(&source[found[0].start..found[0].end], "カリオン");
        assert_eq!(found[1].group, 0, "外に出たリオンは色が付く");
    }
}
