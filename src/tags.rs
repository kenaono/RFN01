//! 書き手の求め 2026-09-23: タグ（Obsidian互換）——[Tag機能_仕様と実装計画.md](../Tag機能_仕様と実装計画.md)。
//!
//! **読み方はここに1つだけ置く。**本文の色（`document.rs`）、索引（`workspace_index.rs`）、
//! 検索（`searcher.rs`）、Ctrl+クリックと補完が同じ規則を使う——別々に決めると、
//! 色の付いた語が検索に出ない、ということが起きる。
//!
//! 純粋な計算で、ファイルも窓も知らない。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::document;

/// タグの名前に使える字（`#`の後ろ）：字（日本語を含む）・数字・`_`・`-`・`/`。
pub fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// 名前として成り立つか。**数字だけはタグではない**（`#123`、Obsidianと同じ）。
fn is_tag_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name.chars().all(is_tag_char)
        && name.chars().any(|c| !c.is_numeric() && c != '/')
}

/// `rest`の頭にある本文のタグ——名前と、その後ろ。
///
/// **行頭か空白の後だけ**（`previous`がその前の字）。`#`の直後が空白なら見出しで、
/// 語の途中の`#`（`[[注#見出し]]`、URLの`#`）はタグではない。名前の終わりの`/`は含めない。
pub fn tag_here(rest: &str, previous: Option<char>) -> Option<(&str, &str)> {
    if previous.is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let body = rest.strip_prefix('#')?;
    let run = body.find(|c: char| !is_tag_char(c)).unwrap_or(body.len());
    let name = body[..run].trim_end_matches('/');
    is_tag_name(name).then(|| (name, &body[name.len()..]))
}

/// 文書の中の1つのタグ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagAt {
    /// 本文での位置（バイト）。本文のタグは`#`から、フロントマターは名前の頭から。
    pub at: usize,
    /// `at`からの長さ（バイト）——検索の一覧から開いたときに選ぶ範囲。
    pub len: usize,
    /// `#`を除いた名前、書いてあるとおり。
    pub name: String,
}

/// `text`（`source`の一部）の`source`の中での位置。
fn offset_in(source: &str, text: &str) -> usize {
    text.as_ptr() as usize - source.as_ptr() as usize
}

/// フロントマターの1つの値（`"#小説"`など）をタグとして読む。
fn frontmatter_item<'a>(item: &'a str) -> Option<&'a str> {
    let item = item.trim();
    let item = item
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(item)
        .trim();
    let item = item.strip_prefix('#').unwrap_or(item);
    let item = item.trim_end_matches('/');
    is_tag_name(item).then_some(item)
}

/// フロントマターの`tags:`／`tag:`。`[a, b]`・`a, b`・`a b`・次の行からの`- a`。
fn frontmatter_tags(source: &str, end: usize, found: &mut Vec<TagAt>) {
    let block = &source[..end];
    let mut lines = block.split('\n').skip(1).peekable();
    let push = |item: &str, found: &mut Vec<TagAt>| {
        if let Some(name) = frontmatter_item(item) {
            found.push(TagAt {
                at: offset_in(source, name),
                len: name.len(),
                name: name.to_owned(),
            });
        }
    };
    while let Some(line) = lines.next() {
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(key.trim().to_lowercase().as_str(), "tags" | "tag") {
            continue;
        }
        let value = value.trim_end_matches('\r').trim();
        if value.is_empty() {
            while let Some(next) = lines.peek() {
                let Some(item) = next.trim_start().strip_prefix('-') else {
                    break;
                };
                push(item.trim_end_matches('\r'), found);
                lines.next();
            }
            continue;
        }
        let value = value
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(value);
        for item in value.split([',', ' ', '\t']) {
            push(item, found);
        }
    }
}

/// 1行の中の本文のタグ。インラインコードと`%%`コメントの中は読まない。
fn line_tags(line_start: usize, line: &str, found: &mut Vec<TagAt>) {
    let mut rest = line;
    let mut previous = None;
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            let run = rest.len() - rest.trim_start_matches('`').len();
            let fence = &rest[..run];
            if let Some(close) = rest[run..].find(fence) {
                rest = &rest[run + close + run..];
                previous = Some('`');
                continue;
            }
        }
        if let Some(inner) = rest.strip_prefix("%%")
            && let Some(close) = inner.find("%%")
        {
            rest = &inner[close + 2..];
            previous = Some('%');
            continue;
        }
        if c == '#'
            && let Some((name, after)) = tag_here(rest, previous)
        {
            found.push(TagAt {
                at: line_start + (line.len() - rest.len()),
                len: 1 + name.len(),
                name: name.to_owned(),
            });
            previous = name.chars().next_back();
            rest = after;
            continue;
        }
        previous = Some(c);
        rest = &rest[c.len_utf8()..];
    }
}

/// 文書のタグすべて、出てきた順（フロントマターが先）。
///
/// コードブロック・コメントのブロックなど字のまま出す行（`LineStyle::is_literal`）と、
/// 箇条書きでない字下げの行は読まない——プレビューが記法として読まない行である。
pub fn tags_in(source: &str) -> Vec<TagAt> {
    let mut found = Vec::new();
    let start = document::frontmatter_end(source).unwrap_or(0);
    if start > 0 {
        frontmatter_tags(source, start, &mut found);
    }
    if !source[start..].contains('#') {
        return found;
    }
    let styles = document::line_styles_as(source, document::BulletMarks::all());
    let mut line_start = 0;
    for (index, line) in source.split('\n').enumerate() {
        let here = line_start;
        line_start += line.len() + 1;
        if here < start || !line.contains('#') {
            continue;
        }
        let style = styles.get(index).copied().unwrap_or_default();
        let indented = line.starts_with([' ', '\t']) && style.list_indent == 0;
        if style.is_literal() || indented {
            continue;
        }
        line_tags(here, line, &mut found);
    }
    found
}

/// タグを比べるときの形（大文字小文字を区別しない）。
pub fn key(name: &str) -> String {
    name.to_lowercase()
}

/// 文書のタグの名前、重なりなしで（大文字小文字は同じタグ、最初の書き方を残す）。
pub fn names(source: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tags_in(source)
        .into_iter()
        .filter(|tag| seen.insert(key(&tag.name)))
        .map(|tag| tag.name)
        .collect()
}

/// 検索のタグ（`tag:`の後ろ）を比べる形に：`#`と終わりの`/`を外し、小文字に。
pub fn query_key(query: &str) -> String {
    key(query.trim().trim_start_matches('#').trim_end_matches('/'))
}

/// `name`が検索のタグ`query_key`に当たるか。**子のタグも当たる**（`小説`は`小説/人物`に）。
pub fn matches(name: &str, query_key: &str) -> bool {
    let name = key(name);
    name == query_key
        || name
            .strip_prefix(query_key)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 本文の`byte`の上にあるタグの名前（Ctrl+クリック）。
pub fn tag_at(source: &str, byte: usize) -> Option<String> {
    tags_in(source)
        .into_iter()
        .find(|tag| (tag.at..tag.at + tag.len).contains(&byte))
        .map(|tag| tag.name)
}

/// 検索の欄を、`tag:`の語（比べる形）と残りの語に分ける。
///
/// `tag:`が無ければ欄はそのまま——**今までの検索は1字も変えない**（空白も句の一部）。
pub fn split_query(needle: &str) -> (Vec<String>, String) {
    let is_tag = |word: &str| {
        word.get(..4)
            .is_some_and(|head| head.eq_ignore_ascii_case("tag:"))
    };
    if !needle.split_whitespace().any(is_tag) {
        return (Vec::new(), needle.to_owned());
    }
    let mut tags = Vec::new();
    let mut words = Vec::new();
    for word in needle.split_whitespace() {
        if is_tag(word) {
            let tag = query_key(&word[4..]);
            if !tag.is_empty() {
                tags.push(tag);
            }
        } else {
            words.push(word);
        }
    }
    (tags, words.join(" "))
}

/// Tag Viewの木の1つの節。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagNode {
    /// 最後の段の名前（`小説/人物`なら`人物`）。
    pub name: String,
    /// 全体の名前（最初に見つかった書き方）。検索に使う。
    pub path: String,
    /// このタグか、その子を持つ文書の数。
    pub files: usize,
    pub children: Vec<TagNode>,
}

/// 文書ごとのタグ（名前の並び）から、階層の木を作る。名前順（大文字小文字を区別しない）。
pub fn tree<'a>(documents: impl IntoIterator<Item = &'a [String]>) -> Vec<TagNode> {
    // 比べる形の全体の名前 → （書き方、持つ文書）
    let mut all: BTreeMap<String, (String, HashSet<usize>)> = BTreeMap::new();
    for (index, names) in documents.into_iter().enumerate() {
        for name in names {
            let mut end = 0;
            for segment in name.split('/') {
                end += segment.len();
                if !segment.is_empty() {
                    let path = &name[..end];
                    all.entry(key(path))
                        .or_insert_with(|| (path.to_owned(), HashSet::new()))
                        .1
                        .insert(index);
                }
                end += 1;
            }
        }
    }
    fn build(
        all: &BTreeMap<String, (String, HashSet<usize>)>,
        children: &HashMap<String, Vec<String>>,
        parent: &str,
    ) -> Vec<TagNode> {
        let Some(keys) = children.get(parent) else {
            return Vec::new();
        };
        keys.iter()
            .map(|full| {
                let (path, files) = &all[full];
                TagNode {
                    name: path.rsplit('/').next().unwrap_or(path).to_owned(),
                    path: path.clone(),
                    files: files.len(),
                    children: build(all, children, full),
                }
            })
            .collect()
    }
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    // BTreeMapの順は比べる形の名前順なので、子もその順に並ぶ。
    for full in all.keys() {
        let parent = full.rsplit_once('/').map_or("", |(parent, _)| parent);
        // 親が空の段（`a//b`）を飛ばした名前は、居る親まで上がる。
        let mut parent = parent;
        while !parent.is_empty() && !all.contains_key(parent) {
            parent = parent.rsplit_once('/').map_or("", |(p, _)| p);
        }
        children
            .entry(parent.to_owned())
            .or_default()
            .push(full.clone());
    }
    build(&all, &children, "")
}

/// Tag Viewの1行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagRow {
    pub name: String,
    pub path: String,
    pub depth: usize,
    pub files: usize,
    /// 子がある（▸／▾を出す）。
    pub parent: bool,
    pub open: bool,
}

/// 木を行に並べる。`open`は開いている節（比べる形）——**初めは畳んである**。
/// `filter`があれば、それを名前に含む節とその親だけを、開いた形で出す。
pub fn rows(tree: &[TagNode], open: &HashSet<String>, filter: &str) -> Vec<TagRow> {
    fn wanted(node: &TagNode, filter: &str) -> bool {
        filter.is_empty()
            || key(&node.path).contains(filter)
            || node.children.iter().any(|child| wanted(child, filter))
    }
    fn walk(
        nodes: &[TagNode],
        depth: usize,
        open: &HashSet<String>,
        filter: &str,
        out: &mut Vec<TagRow>,
    ) {
        for node in nodes.iter().filter(|node| wanted(node, filter)) {
            let is_open = !filter.is_empty() || open.contains(&key(&node.path));
            out.push(TagRow {
                name: node.name.clone(),
                path: node.path.clone(),
                depth,
                files: node.files,
                parent: !node.children.is_empty(),
                open: is_open,
            });
            if is_open {
                walk(&node.children, depth + 1, open, filter, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, 0, open, &key(filter.trim()), &mut out);
    out
}

/// 子を持つ節すべて（比べる形）——Expand All。
pub fn all_parents(tree: &[TagNode]) -> HashSet<String> {
    let mut out = HashSet::new();
    fn walk(nodes: &[TagNode], out: &mut HashSet<String>) {
        for node in nodes {
            if !node.children.is_empty() {
                out.insert(key(&node.path));
                walk(&node.children, out);
            }
        }
    }
    walk(tree, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(source: &str) -> Vec<String> {
        tags_in(source).into_iter().map(|tag| tag.name).collect()
    }

    #[test]
    fn a_tag_starts_a_line_or_follows_a_space() {
        assert_eq!(found("#小説 と #メモ。"), ["小説", "メモ"]);
        assert_eq!(found("本文#中\n"), Vec::<String>::new());
        assert_eq!(found("　#全角空白の後"), ["全角空白の後"]);
    }

    #[test]
    fn a_heading_a_number_and_a_full_width_hash_are_not_tags() {
        assert_eq!(found("# 見出し\n## 二つ目 #本当のタグ"), ["本当のタグ"]);
        assert_eq!(found("#123 #2026年"), ["2026年"]);
        assert_eq!(found("＃全角"), Vec::<String>::new());
    }

    #[test]
    fn a_tag_ends_at_punctuation_and_keeps_its_levels() {
        let tags = tags_in("#小説/人物/主人公、それと #a-b_c.");
        assert_eq!(tags[0].name, "小説/人物/主人公");
        assert_eq!(tags[0].at, 0);
        assert_eq!(tags[0].len, "#小説/人物/主人公".len());
        assert_eq!(tags[1].name, "a-b_c");
        assert_eq!(found("#末尾/ 次"), ["末尾"]);
    }

    #[test]
    fn code_links_and_urls_are_not_read() {
        let source =
            "`#コード` #外\n```\n#ブロック\n```\n[[注#見出し]] https://x.jp/#frag %%#注釈%%";
        assert_eq!(found(source), ["外"]);
        assert_eq!(found("    #字下げ\n- #箇条書き"), ["箇条書き"]);
    }

    #[test]
    fn frontmatter_tags_come_in_every_shape() {
        let source = "---\ntitle: 題\ntags: [小説, \"#メモ\"]\n---\n本文 #本文";
        assert_eq!(found(source), ["小説", "メモ", "本文"]);
        let source = "---\ntags:\n  - 小説/人物\n  - '下書き'\naliases: x\n---\n";
        assert_eq!(found(source), ["小説/人物", "下書き"]);
        let source = "---\ntag: a, b c\n---\n";
        assert_eq!(found(source), ["a", "b", "c"]);
        let tags = tags_in("---\ntags: [小説]\n---\n");
        assert_eq!(
            &"---\ntags: [小説]\n---\n"[tags[0].at..tags[0].at + tags[0].len],
            "小説"
        );
        // 閉じていない`---`はフロントマターではない。
        assert_eq!(found("---\ntags: [小説]\n"), Vec::<String>::new());
    }

    #[test]
    fn names_are_unique_whatever_the_case() {
        assert_eq!(names("#Draft #draft #DRAFT/x"), ["Draft", "DRAFT/x"]);
    }

    #[test]
    fn a_query_matches_the_tag_and_its_children() {
        let query = query_key("#小説");
        assert!(matches("小説", &query));
        assert!(matches("小説/人物", &query));
        assert!(!matches("小説家", &query));
        assert!(matches("Draft/One", &query_key("draft")));
    }

    #[test]
    fn a_query_splits_its_tags_off() {
        assert_eq!(
            split_query("tag:#小説 主人公  の"),
            (vec!["小説".to_owned()], "主人公 の".to_owned())
        );
        assert_eq!(
            split_query("TAG:a tag:b/"),
            (vec!["a".into(), "b".into()], String::new())
        );
        // `tag:`が無ければ欄はそのまま。
        assert_eq!(
            split_query(" 二つの  空白 "),
            (Vec::new(), " 二つの  空白 ".to_owned())
        );
    }

    #[test]
    fn the_tree_counts_files_at_every_level() {
        let one = vec!["小説/人物".to_owned(), "メモ".to_owned()];
        let two = vec!["小説".to_owned()];
        let three = vec!["小説/人物/主人公".to_owned()];
        let tree = tree([one.as_slice(), two.as_slice(), three.as_slice()]);
        assert_eq!(tree.len(), 2);
        assert_eq!((tree[0].name.as_str(), tree[0].files), ("メモ", 1));
        let novel = &tree[1];
        assert_eq!((novel.name.as_str(), novel.files), ("小説", 3));
        assert_eq!(novel.children[0].path, "小説/人物");
        assert_eq!(novel.children[0].files, 2);
        assert_eq!(novel.children[0].children[0].name, "主人公");

        let shut = rows(&tree, &HashSet::new(), "");
        assert_eq!(shut.len(), 2);
        assert!(shut[1].parent && !shut[1].open);
        let all = rows(&tree, &all_parents(&tree), "");
        assert_eq!(all.len(), 4);
        assert_eq!(all[3].depth, 2);
        let filtered = rows(&tree, &HashSet::new(), "主人");
        let paths: Vec<_> = filtered.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(paths, ["小説", "小説/人物", "小説/人物/主人公"]);
    }

    #[test]
    fn tag_at_finds_the_tag_under_a_byte() {
        let source = "語 #小説/人物 語";
        assert_eq!(tag_at(source, 4).as_deref(), Some("小説/人物"));
        assert_eq!(tag_at(source, 0), None);
    }
}
