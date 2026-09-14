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

/// 探し方——**帯の3つの切り替え**（E1）。
///
/// **1つの型に集めてあるのは、規則が1つでなければならないからである。**
/// 検索・件数・全置換・一件置換が別々に畳み方を決めていたときに起きたことが
/// 追加要件の0番に書いてある（`alpha`で見つけた`Alpha`が置換されずに飛んだ）。
/// ここを通す限り、4つは同じ答えを出す。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// 大文字と小文字を別の字として扱うか。**既定は畳む**（要件 7.7）。
    pub match_case: bool,
    /// 語の全体だけを一致とみなすか。
    ///
    /// **英数字の語のための切り替えである。**日本語には語の切れ目が無いので、
    /// `猫`は`黒猫`の中でも一致し続ける——字種の変わり目を語の切れ目とみなす
    /// 案は採らない。`猫が`の`猫`と`黒猫`の`猫`は書き手にとって同じ語であり、
    /// 片方だけを落とす規則は**失敗の向きが悪い**（見えない見落としになる）。
    /// 単語チェックモード要件 8.1.3 が文字種の規則を見送ったのと同じ理由。
    pub whole_word: bool,
    /// 語を正規表現として読むか（E1、書き手の求め 2026-09-10）。
    ///
    /// **入りと切りだけ。**ワイルドカードを別の切り替えにはしない——`*`と`?`は
    /// 正規表現の`.*`と`.`で書けるし、**同じことを2つの切り替えで言うと、
    /// どちらが効いているのかを画面に出す仕事が増える。**
    ///
    /// 入れると「一致の長さは針の長さ」が成り立たなくなる。それを前提にした
    /// 関数を並べないために[`Search`]がある。
    pub regex: bool,
    /// 探す範囲（バイト）。`None`は文書ぜんぶ。
    ///
    /// **範囲は書き手が選んだものそのもの**で、一致へ移っても動かない
    /// （E1：「検索結果へ移動しても最初の対象範囲を維持し、範囲外の本文を
    /// 変えない」）。置換で長さが変われば、変わったぶんだけ終わりがずれる
    /// ——それを持っているのは呼び出し側である。
    pub within: Option<(usize, usize)>,
}

impl Rules {
    /// 探してよいバイトの範囲。**文書の外は指せない。**
    fn scope(self, source: &str) -> (usize, usize) {
        let (start, end) = self.within.unwrap_or((0, source.len()));
        let start = start.min(source.len());
        (start, end.clamp(start, source.len()))
    }

    /// 2つの並びが同じ字か。**畳んでも長さが変わらない**のがASCIIの折り畳みの
    /// 性質で、それがバイト位置をそのまま返せる理由である。
    fn same(self, left: &[u8], right: &[u8]) -> bool {
        if self.match_case {
            left == right
        } else {
            left.eq_ignore_ascii_case(right)
        }
    }

    /// 語の切れ目の条件を満たすか。
    ///
    /// **針の端が語をつくる字のときだけ問う**（正規表現の`\b`と同じ）。
    /// `(a`のように記号で始まる語を探したとき、単語単位にしたせいで一つも
    /// 見つからない、が起きないようにするためである。
    fn word_edges(self, haystack: &str, needle: &str, at: usize, len: usize) -> bool {
        if !self.whole_word {
            return true;
        }
        let opens = needle.chars().next().is_some_and(is_word_letter);
        let closes = needle.chars().next_back().is_some_and(is_word_letter);
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + len..].chars().next();
        (!opens || before.is_none_or(|letter| !is_word_letter(letter)))
            && (!closes || after.is_none_or(|letter| !is_word_letter(letter)))
    }
}

/// 語をつくる字か（[`Rules::whole_word`]）。
///
/// **英数字と`_`だけ**、全角のそれも含める。漢字も仮名も入れない——入れれば
/// `黒猫`の中の`猫`が単語単位で見つからなくなり、日本語の本文で単語単位が
/// 「ほとんど何も見つからない切り替え」になる。
fn is_word_letter(letter: char) -> bool {
    letter.is_ascii_alphanumeric()
        || letter == '_'
        || matches!(letter, '０'..='９' | 'Ａ'..='Ｚ' | 'ａ'..='ｚ')
}

/// 1回の検索——**語と探し方を1つに束ねたもの**（E1）。
///
/// **なぜ型にするか。**正規表現を入れると「一致の長さは針の長さ」が成り立たなく
/// なる。長さを前提にした関数を4つ並べておくと、次に規則が増えたとき**どれか1つが
/// 取り残される**——追加要件の0番で直したのがまさにそれだった（`alpha`で見つけた
/// `Alpha`が置換されなかった）。ここを通る限り、検索・件数・色・置換は同じ一巡を
/// 見ている。
///
/// **組み立てで誤りが分かる。**`(`だけの正規表現は「まだ書きかけ」であって、
/// 黙って0件にするのは嘘である——[`Search::new`]が理由を返し、帯がそれを言う。
pub struct Search {
    needle: String,
    rules: Rules,
    /// 正規表現のときだけ。素の語では[`found_at`]が歩く。
    pattern: Option<regex::Regex>,
}

impl Search {
    /// 探し方を束ねる。正規表現が正しくなければ、その理由を返す。
    ///
    /// **大小の別は組み立てに混ぜる**（`(?i)`）——正規表現の畳み方はUnicodeの
    /// それで、素の語のASCIIだけの畳み方より広い。**それでも仮名は畳まれない**
    /// （`ア`と`あ`は大小の関係にない）ので、要件 7.7 が心配していたことは起きない。
    pub fn new(needle: &str, rules: Rules) -> Result<Self, String> {
        let pattern = if rules.regex && !needle.is_empty() {
            let built = regex::RegexBuilder::new(needle)
                .case_insensitive(!rules.match_case)
                // **`^`と`$`は行の端**（編集器の作法）。`.`は改行に当たらない
                // ままなので、行をまたぐ一致は`\n`や`[\s\S]`と書いた人にだけ
                // 起きる——E1が「複数行検索は次の段階」と言ったのは、書き手が
                // 頼んでいないのに一致が行を越えることのほうである。
                .multi_line(true)
                .build();
            match built {
                Ok(pattern) => Some(pattern),
                // **書き手に見せるのは1行。**どこがどう悪いかは正規表現の
                // 作法の話で、帯に置くには長い——大事なのは「いま探せていない」
                // ことが画面に出ていることである。
                Err(_) => return Err("正規表現が正しくありません".to_owned()),
            }
        } else {
            None
        };
        Ok(Self {
            needle: needle.to_owned(),
            rules,
            pattern,
        })
    }

    /// この本文の一致を順に見せる。**すべてがここを通る。**
    ///
    /// `visit`が`false`を返したらそこで止める——次の1つだけが要る呼び手
    /// （[`Self::next`]）が、文書ぜんぶを歩かずに済むように。
    ///
    /// **重なる一致は返さない**（`str::matches`と同じ）。長さ0の一致
    /// （`a*`が空に当たるような）も返さない：**何も無いところは一致ではない**し、
    /// 返せば同じ場所で止まり続ける。
    fn walk(&self, source: &str, mut visit: impl FnMut(usize, usize) -> bool) {
        if self.needle.is_empty() {
            return;
        }
        let (low, high) = self.rules.scope(source);
        match &self.pattern {
            Some(pattern) => {
                // **範囲の中だけを見せる。**`^`と`$`はその切れ端の端に当たる
                // ——範囲内検索は「そこだけが本文である」という約束なので、
                // それでよい。
                let Some(hay) = source.get(low..high) else {
                    return;
                };
                for found in pattern.find_iter(hay) {
                    if found.is_empty() {
                        continue;
                    }
                    let start = low + found.start();
                    let end = low + found.end();
                    if !self
                        .rules
                        .word_edges(source, found.as_str(), start, end - start)
                    {
                        continue;
                    }
                    if !visit(start, end) {
                        return;
                    }
                }
            }
            None => {
                let mut from = low;
                while let Some(at) = found_at(source, &self.needle, from, self.rules) {
                    let end = at + self.needle.len();
                    if !visit(at, end) {
                        return;
                    }
                    from = end;
                }
            }
        }
    }

    /// 次（または前）の一致。**文書は輪**で、範囲内検索ではその範囲が輪である。
    ///
    /// 前向きは`from`以降に始まる最初の一致、後ろ向きは`from`までに終わる最後の
    /// 一致。どちらも見つからなければ輪を回って端から返す。
    pub fn next(&self, source: &str, from: usize, forwards: bool) -> Option<(usize, usize)> {
        let mut first = None;
        let mut answer = None;
        if forwards {
            self.walk(source, |start, end| {
                if first.is_none() {
                    first = Some((start, end));
                }
                if start >= from {
                    answer = Some((start, end));
                    return false;
                }
                true
            });
            answer.or(first)
        } else {
            let mut last = None;
            self.walk(source, |start, end| {
                last = Some((start, end));
                if end <= from {
                    answer = Some((start, end));
                }
                true
            });
            answer.or(last)
        }
    }

    /// 何件あって、いま何件目か（E1）。
    ///
    /// **一巡で両方を答える。**「12件ある」と「その3件目にいる」は同じ一巡から
    /// 出るもので、別々に数えれば同じ本文を2度歩くことになる。
    ///
    /// `at`は立っている一致の**始まり**。それが数の中の一致でなければ`None`
    /// ——**立っていないことは0件目ではない。**[`Self::next`]も同じ一巡を
    /// 歩くので、**着いた先は必ず数の中にある。**
    pub fn tally(&self, source: &str, at: Option<usize>) -> (usize, Option<usize>) {
        let mut total = 0;
        let mut which = None;
        self.walk(source, |start, _| {
            total += 1;
            if at == Some(start) {
                which = Some(total);
            }
            true
        });
        (total, which)
    }

    /// 色を付けるための一致の並び（E1の③）。
    ///
    /// `limit`は歯止め——**見せられる数の決まりではなく、1打鍵の費用の**。
    pub fn spans(&self, source: &str, limit: usize) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        if limit == 0 {
            return found;
        }
        self.walk(source, |start, end| {
            found.push((start, end));
            found.len() < limit
        });
        found
    }

    /// この範囲が、この探し方の一致そのものか（要件 7.7）。
    ///
    /// **一件置換が訊く唯一の問い。**「いま選ばれているのは検索が見つけたものか」
    /// ——ここが検索より厳しいと、見つかったのに置換されない一致ができる。
    pub fn covers(&self, source: &str, start: usize, end: usize) -> bool {
        let mut found = false;
        self.walk(source, |at, to| {
            if (at, to) == (start, end) {
                found = true;
                return false;
            }
            // 並びは前から後ろなので、通り過ぎたら無い。
            at < start
        });
        found
    }

    /// 一致をすべて置き換えて、その数（要件 7.7）。
    ///
    /// **書いたものは読み直さない**ので、`a`を`aa`にしても走らない。
    /// **入れる字はそのまま**——正規表現でも`$1`のような後方参照は解さない。
    /// 解すると、`$`を打った書き手が置換のたびに驚くことになる（要件 15：
    /// 要るとなったら足す）。
    ///
    /// 範囲内検索では**範囲の外を1バイトも触らない**。
    pub fn replace_all(&self, source: &str, replacement: &str) -> (String, usize) {
        let mut out = String::with_capacity(source.len());
        let mut from = 0;
        let mut replaced = 0;
        self.walk(source, |start, end| {
            out.push_str(&source[from..start]);
            out.push_str(replacement);
            from = end;
            replaced += 1;
            true
        });
        out.push_str(&source[from..]);
        (out, replaced)
    }
}

/// 覚えておく語の数（E1の④）。
///
/// **10は「↑を押して探すより打ち直したほうが早い」の手前**である。下書きの履歴
/// （要件 12.4）が同じ10で、同じ理由で選ばれている——短い列だから読める。
pub const REMEMBERED_TERMS: usize = 10;

/// 探した語の並び——**打ち直さないための短い列**（E1の④）。
///
/// **↑と↓だけで歩く**（書き手の選択 2026-09-10）。帯に釦を足せば、半分の窓で
/// 最初にはみ出すのはこの帯である（`Match case`と書ける幅がもう無い、②）。
/// 一行の欄で↑↓は何も起こさない鍵なので、割り当てても書き手から取り上げるものが
/// 無い。
///
/// **打ちかけの字は返ってくる。**↑で遡った書き手が↓で戻ると、遡る前に打っていた
/// 字がそこにある——履歴を覗くことと、打った字を捨てることは別である。
///
/// **欄を変えたのが誰かで見分ける。**最後にこの列が置いた字と欄の字が違えば、
/// 書き手の手が入ったとみなして遡りを畳む。打鍵のたびに知らせてもらわずに済み、
/// 範囲の取り直し（`search_selection`、E1の②）と同じ見分け方である。
#[derive(Clone, Debug, Default)]
pub struct Terms {
    /// 新しいものから。同じ語は1つだけ。
    kept: Vec<String>,
    /// どこまで遡ったか。`None`は「遡っていない＝欄にいる」。
    at: Option<usize>,
    /// 遡り始める前に欄にあった字。
    typed: String,
    /// この列が最後に欄へ置いた字。
    placed: Option<String>,
}

impl Terms {
    /// 前の run が残した並びから始める（E1の④、セッション）。
    ///
    /// **読むほうで整える。**セッションは手で直せるファイルで、空の語や同じ語が
    /// 並んでいることがある——それを持ったまま歩くと、↑が同じ語を2回出す。
    pub fn restored(kept: Vec<String>) -> Self {
        let mut terms = Self::default();
        // 古いほうから入れ直すと、`remember`の規則がそのまま並び直しになる。
        for term in kept.into_iter().rev() {
            terms.remember(&term);
        }
        terms
    }

    /// 覚えている語、新しいものから。
    pub fn kept(&self) -> &[String] {
        &self.kept
    }

    /// 探した語を頭へ（E1の④）。
    ///
    /// **同じ語は1つ**（下書きの履歴と同じ規則、要件 12.4）。10件しか無いので、
    /// 繰り返しは書き手が要る語を端から押し出す。
    ///
    /// **見つかったかどうかは問わない。**見つからなかった語こそ打ち直したくない
    /// ものである——次の一手は、たいていその語を少し変えてもう一度探すことだから。
    pub fn remember(&mut self, term: &str) {
        if term.is_empty() {
            return;
        }
        self.kept.retain(|kept| kept != term);
        self.kept.insert(0, term.to_owned());
        self.kept.truncate(REMEMBERED_TERMS);
        self.settle();
    }

    /// 遡りを畳む。次の↑は、いちばん新しい語から始まる。
    pub fn settle(&mut self) {
        self.at = None;
        self.typed.clear();
        self.placed = None;
    }

    /// ↑（`back`）と↓で、欄に入れる字（E1の④）。
    ///
    /// `None`は**動かない**——履歴の端と、遡っていないところでの↓である。
    /// 端で止まるのは、押し続けた書き手の欄が空になって「消えた」と見えるより
    /// よい：列の終わりは、列が無くなることではない。
    pub fn step(&mut self, back: bool, field: &str) -> Option<String> {
        if self.placed.as_deref() != Some(field) {
            // 書き手が打っている。遡りはここで畳み、この字が戻る先になる。
            self.at = None;
            self.typed = field.to_owned();
        }
        let next = match (self.at, back) {
            (None, true) => 0,
            // 遡っていないところでの↓。**打った字より新しいものは無い。**
            (None, false) => return None,
            (Some(at), true) => at + 1,
            (Some(0), false) => {
                self.at = None;
                self.placed = Some(self.typed.clone());
                return Some(self.typed.clone());
            }
            (Some(at), false) => at - 1,
        };
        let term = self.kept.get(next)?.clone();
        self.at = Some(next);
        self.placed = Some(term.clone());
        Some(term)
    }
}

/// Where the needle is, at or after `from`, ignoring the case of ASCII letters.
///
/// **The scan is over bytes and the answer is a byte position**, which is only
/// safe because the folding leaves lengths alone: every byte the needle matches
/// is either the same byte or the same letter in the other case, so the matched
/// text is exactly as long as the needle.
fn found_at(haystack: &str, needle: &str, from: usize, rules: Rules) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    if pin.is_empty() || pin.len() > hay.len() {
        return None;
    }
    let (low, high) = rules.scope(haystack);
    if pin.len() > high - low {
        return None;
    }
    let last = high - pin.len();
    let mut at = from.max(low);
    while at <= last {
        // The boundary is asked about first because it is the cheap half, and
        // because it is the one that would matter if this ever folded anything
        // outside ASCII. As it stands a match cannot begin inside a character:
        // a byte in the middle of one is `0x80..=0xBF`, and no needle begins
        // with one of those.
        if haystack.is_char_boundary(at)
            && rules.same(&hay[at..at + pin.len()], pin)
            && rules.word_edges(haystack, &haystack[at..at + pin.len()], at, pin.len())
        {
            return Some(at);
        }
        at += 1;
    }
    None
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
    /// Full logical line; the UI elides it and shows the full text on hover.
    pub preview: String,
}

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
        while let Some(at) = found_at(line, needle, from, Rules::default()) {
            hits.push(Hit {
                line: number + 1,
                at: line_start + at,
                preview: line.trim_end_matches('\r').to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 既定の探し方——ASCIIの大小を畳み、語の途中でも当たり、文書ぜんぶ。
    const PLAIN: Rules = Rules {
        match_case: false,
        whole_word: false,
        regex: false,
        within: None,
    };

    /// 試験のための組み立て。**正しくない探し方はここで落ちる**ので、
    /// 誤りそのものを見る試験だけが`Search::new`を直に呼ぶ。
    fn seek(needle: &str, rules: Rules) -> Search {
        Search::new(needle, rules).expect("読める探し方")
    }

    const SOURCE: &str = "春の海 ひねもすのたり のたりかな";

    #[test]
    fn finds_the_next_match_from_where_the_caret_is() {
        let at = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(
            seek("のたり", PLAIN).next(SOURCE, 0, true),
            Some((at, at + 9))
        );
        assert_eq!(
            seek("のたり", PLAIN).next(SOURCE, at + 1, true),
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
            seek("のたり", PLAIN).next(SOURCE, second + 1, true),
            Some((first, first + 9)),
            "past the last match, back to the first",
        );
        assert_eq!(
            seek("のたり", PLAIN).next(SOURCE, 0, false),
            Some((second, second + 9)),
            "backwards from the start, round to the last",
        );
    }

    #[test]
    fn searching_backwards_finds_the_match_before_the_caret() {
        let first = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(
            seek("のたり", PLAIN).next(SOURCE, second, false),
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
            seek("のたり", PLAIN).next(SOURCE, inside, true),
            Some((second, second + 9)),
        );
        assert_eq!(
            seek("のたり", PLAIN).next(SOURCE, inside, false),
            Some((second, second + 9)),
            "backwards from inside it, round to the last",
        );
    }

    #[test]
    fn upper_and_lower_case_are_the_same_letter() {
        let source = "Windows 11 と windows の WINDOWS";
        let first = seek("windows", PLAIN)
            .next(source, 0, true)
            .expect("the first one");
        assert_eq!(first, (0, 7));
        let second = seek("WINDOWS", PLAIN)
            .next(source, first.1, true)
            .expect("the second one");
        assert_eq!(&source[second.0..second.1], "windows");
        assert_eq!(seek("windows", PLAIN).tally(source, None).0, 3);
    }

    #[test]
    fn only_ascii_letters_are_folded() {
        // Full-width Ａ and half-width a are different characters, and a
        // Japanese document is full of pairs a search must not treat as equal.
        assert_eq!(seek("a", PLAIN).next("Ａ", 0, true), None);
        assert_eq!(seek("あ", PLAIN).next("ア", 0, true), None);
        // And a needle that is not ASCII at all still finds itself.
        assert_eq!(
            seek("あけぼの", PLAIN).next("春はあけぼの", 0, true),
            Some((6, 18))
        );
    }

    #[test]
    fn a_replacement_is_written_as_it_was_typed() {
        // Both matches go, and neither takes its own case with it.
        let (replaced, count) = seek("CAT", PLAIN).replace_all("Cat and cat", "dog");
        assert_eq!(replaced, "dog and dog");
        assert_eq!(count, 2);
    }

    #[test]
    fn a_search_backwards_folds_case_as_well() {
        let source = "cat と CAT";
        let found = seek("Cat", PLAIN)
            .next(source, source.len(), false)
            .expect("the last one");
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
        while let Some((start, end)) = seek("alpha", PLAIN).next(source, from, true) {
            assert!(
                seek("alpha", PLAIN).covers(source, start, end),
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
        assert_eq!(seek("alpha", PLAIN).tally(source, None).0, 3);
        assert_eq!(seek("alpha", PLAIN).replace_all(source, "beta").1, 3);
    }

    /// Anything that is not exactly one match is not one: a longer span, a
    /// shorter one, a start inside a character, and the empty needle a search
    /// box has before it is typed into.
    #[test]
    fn a_span_that_is_not_the_needle_is_not_a_match() {
        let source = "あかalphaあか";

        assert!(
            !seek("alpha", PLAIN).covers(source, 6, 12),
            "one byte too long"
        );
        assert!(!seek("alpha", PLAIN).covers(source, 6, 10), "too short");
        assert!(
            !seek("alpha", PLAIN).covers(source, 5, 10),
            "starts inside a character"
        );
        assert!(!seek("", PLAIN).covers(source, 6, 6), "nothing matches");
        assert!(seek("ALPHA", PLAIN).covers(source, 6, 11));
    }

    /// **件数と現在位置は同じ一巡から出る**（E1）。帯が`3 / 12`と言えるのは、
    /// 数えながら「いま立っているのはどれか」を見ているからである。
    #[test]
    fn the_count_says_which_match_it_is_standing_on() {
        let first = SOURCE.find("のたり").expect("is there");
        let second = SOURCE.rfind("のたり").expect("is there twice");

        assert_eq!(
            seek("のたり", PLAIN).tally(SOURCE, Some(first)),
            (2, Some(1))
        );
        assert_eq!(
            seek("のたり", PLAIN).tally(SOURCE, Some(second)),
            (2, Some(2))
        );
        // 一致の頭でない場所に立っているのは、0件目ではなく「どれでもない」。
        assert_eq!(seek("のたり", PLAIN).tally(SOURCE, Some(0)), (2, None));
        assert_eq!(seek("のたり", PLAIN).tally(SOURCE, None), (2, None));
        assert_eq!(seek("冬の海", PLAIN).tally(SOURCE, Some(0)), (0, None));
    }

    /// 数え方は[`Search::replace_all`]の置き換え方と同じ——重なる一致は二度
    /// 数えない。**ここが食い違うと、「2件」と言った直後の全置換が3件を報せる。**
    ///
    /// **見つかるものは必ず数の中にある**（2026-09-10、正規表現のために一巡へ
    /// 揃えたときに手に入った）。以前は`next`だけが別の道を歩いていて、
    /// `あああ`の2文字目から`ああ`を探すと**数に無い一致**へ着けた——帯はそこで
    /// 件数だけを言うしかなかった。いまは`next`も同じ網目を歩く。
    #[test]
    fn what_is_counted_is_what_a_replace_all_would_replace() {
        let source = "あああ あああ";

        let (total, _) = seek("ああ", PLAIN).tally(source, None);

        assert_eq!(total, 2);
        assert_eq!(seek("ああ", PLAIN).replace_all(source, "い").1, total);
        // 網目の外（2文字目から始まる`ああ`）へは着かない。次の一致は
        // **数の中の2件目**である。
        let next = seek("ああ", PLAIN).next(source, 3, true).expect("is there");
        assert_eq!(next.0, 10);
        assert_eq!(
            seek("ああ", PLAIN).tally(source, Some(next.0)),
            (2, Some(2))
        );
    }

    /// E1: **大文字と小文字を別の字として扱う切り替え。**既定は畳むほうで、
    /// それは要件 7.7 が決めている（`windows`を探す書き手は文の頭のそれも
    /// 指している）。
    #[test]
    fn matching_case_finds_only_what_was_typed() {
        let source = "Cat と cat と CAT";
        let strict = Rules {
            match_case: true,
            ..PLAIN
        };

        let found = seek("cat", strict)
            .next(source, 0, true)
            .expect("the lower one");
        assert_eq!(&source[found.0..found.1], "cat");
        assert_eq!(seek("cat", strict).tally(source, None).0, 1);
        assert_eq!(seek("cat", PLAIN).tally(source, None).0, 3);
        // 置換も同じ規則で動く——**4つが同じ答えを出す**のがこの型の値打ち。
        assert_eq!(
            seek("cat", strict).replace_all(source, "犬").0,
            "Cat と 犬 と CAT"
        );
    }

    /// E1: **単語単位は英数字の語のための切り替えである。**
    ///
    /// 日本語には語の切れ目が無いので、`猫`は`黒猫`の中でも一致し続ける
    /// ——字種の変わり目で切る案を採らない理由が[`Rules::whole_word`]にある。
    #[test]
    fn whole_word_only_binds_where_a_word_letter_touches_it() {
        let whole = Rules {
            whole_word: true,
            ..PLAIN
        };

        let source = "cat cats concat cat_1";
        assert_eq!(seek("cat", PLAIN).tally(source, None).0, 4);
        assert_eq!(
            seek("cat", whole).tally(source, None).0,
            1,
            "頭の1つだけが語"
        );
        // `_`は語をつくる字なので、`cat_1`の`cat`は語の一部である。
        assert_eq!(seek("cat_1", whole).tally(source, None).0, 1);

        // 日本語は変わらない——ここが変わると「ほとんど何も見つからない
        // 切り替え」になる。
        let japanese = "猫と黒猫と猫又";
        assert_eq!(seek("猫", whole).tally(japanese, None).0, 3);

        // 針の端が語をつくる字でなければ、その側は問わない（`\b`と同じ）。
        assert_eq!(seek("-", whole).tally("a-b と -", None).0, 2);
    }

    /// E1: **選んだ範囲の外は、探しも置き換えもしない。**
    ///
    /// 輪も範囲の輪である——端まで行った検索が文書の頭へ戻れば、選んでいない
    /// 本文を探し始めることになる。
    #[test]
    fn a_scope_is_the_whole_document_the_search_has() {
        let source = "まえがき 猫 ほんぶん 猫 猫 あとがき 猫";
        // 「ほんぶん」から始まる2つの猫だけを囲む。
        let start = source.find("ほんぶん").expect("is there");
        let end = source.rfind("あとがき").expect("is there");
        let inside = Rules {
            within: Some((start, end)),
            ..PLAIN
        };

        assert_eq!(seek("猫", PLAIN).tally(source, None).0, 4);
        assert_eq!(seek("猫", inside).tally(source, None).0, 2);

        // 範囲の最後の一致から次を探すと、範囲の頭へ戻る（文書の頭ではない）。
        let last = source[..end].rfind('猫').expect("is there");
        let wrapped = seek("猫", inside)
            .next(source, last + 3, true)
            .expect("wraps");
        assert!(wrapped.0 >= start && wrapped.0 < end);
        // 後ろ向きも同じ輪の中。
        let back = seek("猫", inside)
            .next(source, start, false)
            .expect("wraps back");
        assert_eq!(back.0, last);

        // 範囲の外の猫は、一致でもなければ置き換えもされない。
        let outside = source.find('猫').expect("is there");
        assert!(!seek("猫", inside).covers(source, outside, outside + 3));
        let (replaced, count) = seek("猫", inside).replace_all(source, "犬");
        assert_eq!(count, 2);
        assert_eq!(replaced, "まえがき 猫 ほんぶん 犬 犬 あとがき 猫");
    }

    /// E1: **画面に色を付けるための一致の並び。**数え方は[`tally`]と同じで、
    /// 上限は打鍵の費用の歯止めである。
    #[test]
    fn every_match_can_be_shown_at_once() {
        let source = "猫と犬と猫と猫";

        let all = seek("猫", PLAIN).spans(source, 100);

        assert_eq!(all.len(), seek("猫", PLAIN).tally(source, None).0);
        for (start, end) in &all {
            assert_eq!(&source[*start..*end], "猫");
            assert!(seek("猫", PLAIN).covers(source, *start, *end));
        }
        // 上限で止まる。**止まったことは色の付かなさとして出る**ので、
        // 散文で届かないところに置いてある（`MAX_SHOWN_MATCHES`）。
        assert_eq!(seek("猫", PLAIN).spans(source, 2).len(), 2);
        assert!(seek("猫", PLAIN).spans(source, 0).is_empty());
        assert!(seek("", PLAIN).spans(source, 100).is_empty());
        // 探し方をそのまま受ける——範囲の外は色が付かない。
        let inside = Rules {
            within: Some((0, 3)),
            ..PLAIN
        };
        assert_eq!(seek("猫", inside).spans(source, 100), vec![(0, 3)]);
    }

    /// E1（書き手の求め 2026-09-10）: **正規表現。**入りと切りだけで、
    /// ワイルドカードもここに含まれる（`*`は`.*`、`?`は`.`）。
    #[test]
    fn a_pattern_finds_what_a_plain_needle_cannot() {
        let source = "猫が3匹、犬が12匹";
        let rules = Rules {
            regex: true,
            ..PLAIN
        };

        // 長さの違う一致。**数・色・置換が同じ一巡から出る**ので食い違わない。
        let digits = seek("[0-9]+", rules);
        assert_eq!(digits.spans(source, 10), vec![(6, 7), (19, 21)]);
        assert_eq!(digits.tally(source, Some(19)), (2, Some(2)));
        assert_eq!(digits.replace_all(source, "N").0, "猫がN匹、犬がN匹");
        // 見つけたものは、そのまま置換できる（要件 7.7 の畳み方は1つ）。
        assert!(digits.covers(source, 19, 21));
        assert!(!digits.covers(source, 19, 20), "半端な範囲は一致ではない");

        // 選び（`|`）も日本語のまま書ける。
        assert_eq!(seek("猫|犬", rules).tally(source, None).0, 2);
    }

    /// E1: `^`と`$`は**行の端**に当たる（編集器の作法）。`.`は改行に当たらない
    /// ので、行をまたぐ一致はそう書いた人にだけ起きる。
    #[test]
    fn a_pattern_reads_the_lines_as_lines() {
        let source = "第一章\n中身\n第二章\n";
        let rules = Rules {
            regex: true,
            ..PLAIN
        };

        assert_eq!(seek("^第", rules).tally(source, None).0, 2);
        assert_eq!(seek("章$", rules).tally(source, None).0, 2);
        assert_eq!(
            seek("章.中", rules).tally(source, None).0,
            0,
            "`.`は改行に当たらない"
        );
        assert_eq!(seek("章\n中", rules).tally(source, None).0, 1);
    }

    /// E1: **書きかけの正規表現に「0件」と答えない。**`(`は誤りであって、
    /// 見つからないのとは違う——黙って0件にすると、書き手は自分の本文のほうを
    /// 疑う（単語チェックモード要件 2.1.1 と同じ根）。
    #[test]
    fn a_pattern_that_does_not_read_says_so() {
        let rules = Rules {
            regex: true,
            ..PLAIN
        };

        assert!(Search::new("(", rules).is_err());
        assert!(Search::new("[a-", rules).is_err());
        // 切りのときは、同じ字がただの語である。
        assert!(Search::new("(", PLAIN).is_ok());
        assert_eq!(seek("(", PLAIN).tally("(a) (b)", None).0, 2);
    }

    /// E1: **何も無いところは一致ではない。**`a*`は空にも当たるが、それを
    /// 返せば同じ場所で止まり続け、色は文書ぜんぶに付く。
    #[test]
    fn an_empty_match_is_not_a_match() {
        let rules = Rules {
            regex: true,
            ..PLAIN
        };
        let source = "aa bb aa";

        assert_eq!(seek("a*", rules).spans(source, 10), vec![(0, 2), (6, 8)]);
        assert_eq!(seek("x*", rules).tally(source, None), (0, None));
    }

    /// E1: 探し方の3つは正規表現にも効く。**語の切れ目の決め方は1つ**で、
    /// 正規表現の`\b`ではなく[`Rules::whole_word`]のそれを使う——2つあれば、
    /// 切り替えを入れた書き手が別の答えを受け取る。
    #[test]
    fn the_other_rules_still_hold_over_a_pattern() {
        let source = "cat cats CAT";
        let strict = Rules {
            regex: true,
            match_case: true,
            ..PLAIN
        };
        let whole = Rules {
            regex: true,
            whole_word: true,
            ..PLAIN
        };

        assert_eq!(seek("c.t", strict).tally(source, None).0, 2, "CATは別の字");
        assert_eq!(
            seek("c.t", whole).tally(source, None).0,
            2,
            "catsは語の一部"
        );
    }

    /// E1: 置換に入れる字は**そのまま**——正規表現でも`$1`は解さない。
    #[test]
    fn a_replacement_is_written_as_it_was_typed_even_under_a_pattern() {
        let rules = Rules {
            regex: true,
            ..PLAIN
        };

        let (text, count) = seek("[0-9]+", rules).replace_all("章1と章22", "$1");

        assert_eq!(text, "章$1と章$1");
        assert_eq!(count, 2);
    }

    #[test]
    fn a_needle_that_is_not_there_is_not_found() {
        assert_eq!(seek("冬の海", PLAIN).next(SOURCE, 0, true), None);
        assert_eq!(
            seek("", PLAIN).next(SOURCE, 0, true),
            None,
            "nothing matches"
        );
        assert_eq!(seek("あ", PLAIN).next("", 0, true), None);
        assert_eq!(seek("のたり", PLAIN).tally(SOURCE, None).0, 2);
        assert_eq!(seek("", PLAIN).tally(SOURCE, None).0, 0);
    }

    /// A match can only start at a character boundary, so a byte position
    /// handed back is always one a slice can begin at.
    #[test]
    fn a_match_never_starts_inside_a_character() {
        let source = "あいうえお";
        let (start, end) = seek("うえ", PLAIN).next(source, 0, true).expect("is there");

        assert!(source.is_char_boundary(start));
        assert!(source.is_char_boundary(end));
        assert_eq!(&source[start..end], "うえ");
    }

    /// **What goes in is never searched again**, so a replacement containing
    /// the needle finishes rather than running away.
    #[test]
    fn replacing_every_match_does_not_search_what_it_wrote() {
        let (text, replaced) = seek("あ", PLAIN).replace_all("ああ", "ああ");

        assert_eq!(text, "ああああ");
        assert_eq!(replaced, 2);
        assert_eq!(
            seek("", PLAIN).replace_all("ああ", "い"),
            ("ああ".to_owned(), 0)
        );
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
                seek("章", PLAIN).next(source, hit.at, true),
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

    #[test]
    fn a_long_result_preserves_the_full_line_for_hover() {
        let source = format!("    {}終端", "あ".repeat(200));
        let hits = hits_in(&source, "あ", 50);
        assert_eq!(hits[0].preview, source);
    }

    /// 履歴は新しいものが上で、同じ語は1つ（E1の④）。
    #[test]
    fn the_same_term_twice_is_one_entry() {
        let mut terms = Terms::default();

        terms.remember("白猫");
        terms.remember("黒猫");
        terms.remember("白猫");

        assert_eq!(terms.kept(), ["白猫", "黒猫"]);
    }

    /// 空の語は覚えない——`Clear`のあとの欄は、探した語ではない。
    #[test]
    fn nothing_is_not_a_term() {
        let mut terms = Terms::default();

        terms.remember("");

        assert!(terms.kept().is_empty());
    }

    /// 上限を越えたぶんは、古いほうから落ちる。
    #[test]
    fn only_the_last_ten_are_kept() {
        let mut terms = Terms::default();

        for number in 0..REMEMBERED_TERMS + 3 {
            terms.remember(&format!("語{number}"));
        }

        assert_eq!(terms.kept().len(), REMEMBERED_TERMS);
        assert_eq!(terms.kept()[0], format!("語{}", REMEMBERED_TERMS + 2));
        assert_eq!(terms.kept()[REMEMBERED_TERMS - 1], "語3");
    }

    /// ↑で遡り、↓で戻る。**打ちかけの字は返ってくる**（E1の④）。
    #[test]
    fn stepping_back_and_forward_returns_what_was_typed() {
        let mut terms = Terms::default();
        terms.remember("黒猫");
        terms.remember("白猫");

        assert_eq!(terms.step(true, "しろ"), Some("白猫".to_owned()));
        assert_eq!(terms.step(true, "白猫"), Some("黒猫".to_owned()));
        assert_eq!(terms.step(false, "黒猫"), Some("白猫".to_owned()));
        assert_eq!(terms.step(false, "白猫"), Some("しろ".to_owned()));
        // 打ちかけの字より新しいものは無い。
        assert_eq!(terms.step(false, "しろ"), None);
    }

    /// 列の端では止まる。**欄が空になることはない。**
    #[test]
    fn the_oldest_term_is_where_stepping_back_stops() {
        let mut terms = Terms::default();
        terms.remember("白猫");

        assert_eq!(terms.step(true, ""), Some("白猫".to_owned()));
        assert_eq!(terms.step(true, "白猫"), None);
    }

    /// **欄を打ち直したら、遡りは畳まれる。**次の↑はいちばん新しい語から。
    #[test]
    fn typing_in_the_field_folds_the_walk_away() {
        let mut terms = Terms::default();
        terms.remember("黒猫");
        terms.remember("白猫");

        assert_eq!(terms.step(true, ""), Some("白猫".to_owned()));
        // 書き手が打った——置いた字と違う。
        assert_eq!(terms.step(true, "三毛"), Some("白猫".to_owned()));
        assert_eq!(terms.step(false, "白猫"), Some("三毛".to_owned()));
    }

    /// 探した語を覚えると、遡りは畳まれる（E1の④）。
    #[test]
    fn remembering_a_term_starts_the_walk_over() {
        let mut terms = Terms::default();
        terms.remember("黒猫");
        terms.remember("白猫");
        terms.step(true, "");

        terms.remember("三毛");

        assert_eq!(terms.step(true, "三毛"), Some("三毛".to_owned()));
    }

    /// セッションから読んだ並びも、同じ規則で整う（E1の④）。
    #[test]
    fn a_restored_list_drops_what_it_cannot_hold() {
        let kept = vec![
            "白猫".to_owned(),
            "".to_owned(),
            "黒猫".to_owned(),
            "白猫".to_owned(),
        ];

        let terms = Terms::restored(kept);

        assert_eq!(terms.kept(), ["白猫", "黒猫"]);
    }
}
