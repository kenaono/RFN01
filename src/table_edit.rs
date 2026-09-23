//! 表を置く・行と列を足し引きする・列の幅を書く（RFN01-49）。
//!
//! **原稿の書き換えだけを持つ**——どれも`(範囲, 置く字, 編集後の選択)`を返し、呼ぶ側が
//! 編集の道（`apply_span_edit`）へ渡す。表の読み方（`|`の位置・区切り行の揃え）は
//! [`crate::text_blocks`]の1か所にあり、ここはそれを呼ぶ。
//!
//! **列の幅は区切り行の`-`の数**で、`-`1つが行の長さの1%（書き手の合意 2026-09-23）。
//! すべての列で数が同じ表は「幅の指定なし」で、中身で幅が決まる。

use std::ops::Range;

use crate::document::line_span;
use crate::text_blocks::{LineKind, LineStyle, table_cells};

/// 書き換え：置き換える範囲、置く字、書き換えたあとに選ばれている範囲。
pub type Edit = (Range<usize>, String, (usize, usize));

/// 1列の幅の下限（`---`）。他のアプリが区切り行として読める最小の形でもある。
pub const MIN_WIDTH: u32 = 3;

/// 行の長さいっぱい（`-`の数の合計）。
pub const FULL_WIDTH: u32 = 100;

/// 新しいセルの中身。**`|`の間を2字空け、キャレットは1字目の後ろ**に立てる
/// ——打った字が`| 字 |`の形に収まる。
const EMPTY_CELL: &str = "  ";

/// 右クリックの「Table ▸」の操作（RFN01-49）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableEdit {
    RowBefore,
    RowAfter,
    ColumnBefore,
    ColumnAfter,
    DeleteRow,
    DeleteColumn,
    ResetWidths,
}

impl TableEdit {
    /// 並び（右クリックの行の順）。番号は右クリックメニューとの境目でだけ使う。
    pub const ALL: [TableEdit; 7] = [
        TableEdit::RowBefore,
        TableEdit::RowAfter,
        TableEdit::ColumnBefore,
        TableEdit::ColumnAfter,
        TableEdit::DeleteRow,
        TableEdit::DeleteColumn,
        TableEdit::ResetWidths,
    ];

    pub fn number(self) -> i32 {
        Self::ALL.iter().position(|what| *what == self).unwrap_or(0) as i32
    }

    pub fn from_number(number: i32) -> Option<Self> {
        Self::ALL.get(usize::try_from(number).ok()?).copied()
    }

    /// メニューの文言（日本語・英語）。**画面の上下左右で言う**（書き手の求め 2026-09-23：
    /// 前／後は分かりにくい）。縦書きでは行が右から左へ、列が上から下へ並ぶので、
    /// 前の行は右、前の列は上になる。
    pub fn title(self, vertical: bool) -> (&'static str, &'static str) {
        match (self, vertical) {
            (TableEdit::RowBefore, false) => ("上に行を追加", "Insert Row Above"),
            (TableEdit::RowAfter, false) => ("下に行を追加", "Insert Row Below"),
            (TableEdit::ColumnBefore, false) => ("左に列を追加", "Insert Column Left"),
            (TableEdit::ColumnAfter, false) => ("右に列を追加", "Insert Column Right"),
            (TableEdit::RowBefore, true) => ("右に行を追加", "Insert Row Right"),
            (TableEdit::RowAfter, true) => ("左に行を追加", "Insert Row Left"),
            (TableEdit::ColumnBefore, true) => ("上に列を追加", "Insert Column Above"),
            (TableEdit::ColumnAfter, true) => ("下に列を追加", "Insert Column Below"),
            (TableEdit::DeleteRow, _) => ("行を削除", "Delete Row"),
            (TableEdit::DeleteColumn, _) => ("列を削除", "Delete Column"),
            (TableEdit::ResetWidths, _) => ("列幅を戻す", "Reset Column Widths"),
        }
    }
}

/// 区切り行の1列：両端の`:`と、`-`の数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuleCell {
    left: bool,
    right: bool,
    dashes: u32,
}

/// 区切り行の列。区切り行でなければ`None`。**読み方は`table_alignments`と同じ**
/// ——行を閉じる`|`が残す空のセルは列ではない。
fn rule_cells(line: &str) -> Option<Vec<RuleCell>> {
    if !line.starts_with('|') {
        return None;
    }
    let cells = table_cells(line);
    if cells.len() < 2 {
        return None;
    }
    let mut found = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        let text = line[cell.byte_start..cell.byte_end].trim();
        if text.is_empty() && index + 1 == cells.len() {
            break;
        }
        let dashes = text.trim_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|letter| letter == '-') {
            return None;
        }
        found.push(RuleCell {
            left: text.starts_with(':'),
            right: text.ends_with(':'),
            dashes: dashes.len() as u32,
        });
    }
    (!found.is_empty()).then_some(found)
}

/// 区切り行が言う列の幅（行の長さに対する%）。**すべての列で`-`の数が同じなら
/// 指定なし**（`None`）——`| --- | --- |`は中身で幅が決まる表である。1列の表も
/// 指定なしになる（比べる相手が無い）。
pub fn table_widths(line: &str) -> Option<Vec<u32>> {
    let cells = rule_cells(line)?;
    let first = cells.first()?.dashes;
    if cells.iter().all(|cell| cell.dashes == first) {
        return None;
    }
    Some(cells.iter().map(|cell| cell.dashes).collect())
}

/// 区切り行を書く。`| :---: | --- |`の形。
fn rule_text(cells: &[RuleCell]) -> String {
    let mut text = String::from("|");
    for cell in cells {
        text.push(' ');
        if cell.left {
            text.push(':');
        }
        text.push_str(&"-".repeat(cell.dashes.max(1) as usize));
        if cell.right {
            text.push(':');
        }
        text.push_str(" |");
    }
    text
}

/// 空の行（`|  |  |`）。
fn empty_row(columns: usize) -> String {
    let mut text = String::from("|");
    for _ in 0..columns {
        text.push_str(EMPTY_CELL);
        text.push('|');
    }
    text
}

/// [`empty_row`]の`column`番目のセルに立つキャレットの位置（行の頭から）。
fn empty_row_caret(column: usize) -> usize {
    1 + column * (EMPTY_CELL.len() + 1) + 1
}

/// 表を置けるか（RFN01-49）：字を選んでおらず、キャレットの行が引用・箇条書き・
/// コード・表・フロントマターの中にないこと。
pub fn can_place_table(source: &str, styles: &[LineStyle], from: usize, to: usize) -> bool {
    if from != to || from > source.len() {
        return false;
    }
    let index = source[..line_span(source, from).0].matches('\n').count();
    styles.get(index).is_none_or(|style| {
        style.quote_depth == 0
            && style.list_indent == 0
            && !style.kind.is_code()
            && !style.kind.is_table()
            && !style.kind.is_list()
            && style.kind != LineKind::FrontMatter
    })
}

/// 行の中身の終わり（改行の手前）。
fn content_end(source: &str, start: usize) -> usize {
    source[start..]
        .find('\n')
        .map_or(source.len(), |at| start + at)
}

/// その位置の行に字があるか。文書の外は空として扱う。
fn line_has_text(source: &str, start: usize) -> bool {
    start < source.len() && !source[start..content_end(source, start)].trim().is_empty()
}

/// `rows`行（見出し行を含む）×`columns`列の表を置く（RFN01-49 ①）。
///
/// キャレットの行が空ならその行に、字があれば次の行に置く。**前後の段落とつながらない
/// よう空行を挟む**——表の後ろに字の行が続くと、他のアプリはそれを表の行として読む。
/// キャレットは見出し行の最初のセルに立つ。
pub fn insert_table(
    source: &str,
    styles: &[LineStyle],
    from: usize,
    to: usize,
    rows: usize,
    columns: usize,
) -> Option<Edit> {
    if rows == 0 || columns == 0 || !can_place_table(source, styles, from, to) {
        return None;
    }
    let rule = rule_text(&vec![
        RuleCell {
            left: false,
            right: false,
            dashes: MIN_WIDTH,
        };
        columns
    ]);
    let row = empty_row(columns);
    let mut lines = vec![row.clone(), rule];
    lines.extend(std::iter::repeat_n(row, rows - 1));
    let table = lines.join("\n");

    let (start, _) = line_span(source, from);
    let end = content_end(source, start);
    let next = (end < source.len()).then_some(end + 1);
    let next_has_text = next.is_some_and(|next| line_has_text(source, next));
    let (region, lead) = if source[start..end].trim().is_empty() {
        let previous_has_text = start > 0 && line_has_text(source, line_span(source, start - 1).0);
        (start..end, if previous_has_text { "\n" } else { "" })
    } else {
        (end..end, "\n\n")
    };
    let tail = if next_has_text { "\n" } else { "" };
    let caret = region.start + lead.len() + empty_row_caret(0);
    let text = format!("{lead}{table}{tail}");
    Some((region, text, (caret, caret)))
}

/// キャレットのある表：各行の頭と中身の終わり（改行を除く）と、キャレットの行が
/// 何行目か。0が見出し行、1が区切り行。
struct Table {
    lines: Vec<(usize, usize)>,
    here: usize,
}

impl Table {
    fn at(source: &str, styles: &[LineStyle], byte: usize) -> Option<Table> {
        let byte = byte.min(source.len());
        let (caret_line, _) = line_span(source, byte);
        let index = source[..caret_line].matches('\n').count();
        let is_table = |index: usize| styles.get(index).is_some_and(|style| style.kind.is_table());
        if !is_table(index) {
            return None;
        }
        let mut first = index;
        while first > 0 && is_table(first - 1) {
            first -= 1;
        }
        // 行の頭を数え直す。表の頭の行まで戻り、そこから表の行を並べる。
        let mut start = caret_line;
        for _ in first..index {
            start = line_span(source, start - 1).0;
        }
        let mut lines = Vec::new();
        let mut line = first;
        loop {
            let end = content_end(source, start);
            lines.push((start, end));
            line += 1;
            if end >= source.len() || !is_table(line) {
                break;
            }
            start = end + 1;
        }
        let table = Table {
            lines,
            here: index - first,
        };
        // 見出し行と区切り行がそろって、はじめて表である。
        (table.lines.len() >= 2).then_some(table)
    }

    fn text<'a>(&self, source: &'a str, line: usize) -> &'a str {
        let (start, end) = self.lines[line];
        &source[start..end]
    }

    fn rule<'a>(&self, source: &'a str) -> Option<Vec<RuleCell>> {
        rule_cells(self.text(source, 1))
    }

    fn span(&self) -> Range<usize> {
        self.lines[0].0..self.lines[self.lines.len() - 1].1
    }
}

/// キャレットが何列目にいるか：行の頭からキャレットまでにある`|`の数から1を引く。
/// **`|`の後ろの余白は次のセル**（`table_tab`と同じ読み方）。
fn column_at(line: &str, offset: usize, columns: usize) -> usize {
    let bars = table_cells(line)
        .iter()
        .filter(|cell| cell.byte_start <= offset)
        .count();
    bars.saturating_sub(1).min(columns.saturating_sub(1))
}

/// 行の`column`番目のセルに立てるキャレットの位置（行の頭から）。セルに字があれば
/// その頭、空ならセルの1字目の後ろ。
fn cell_caret(line: &str, column: usize) -> usize {
    let cells = table_cells(line);
    let Some(cell) = cells.get(column) else {
        return line.len();
    };
    let text = &line[cell.byte_start..cell.byte_end];
    let lead = text.len() - text.trim_start().len();
    if lead == text.len() {
        cell.byte_start + text.len().min(1)
    } else {
        cell.byte_start + lead
    }
}

/// 右クリックの「Table ▸」の行がそれぞれ押せるか。キャレットが表の中になければ`None`
/// （そのときは「Table ▸」自体を出さない）。
pub fn table_edit_state(source: &str, styles: &[LineStyle], byte: usize) -> Option<[bool; 7]> {
    let table = Table::at(source, styles, byte)?;
    let columns = table.rule(source)?.len();
    let body = table.here >= 2;
    let specified = table_widths(table.text(source, 1)).is_some();
    Some(TableEdit::ALL.map(|what| match what {
        TableEdit::RowBefore | TableEdit::DeleteRow => body,
        TableEdit::RowAfter | TableEdit::ColumnBefore | TableEdit::ColumnAfter => true,
        TableEdit::DeleteColumn => columns >= 2,
        TableEdit::ResetWidths => specified,
    }))
}

/// 右クリックの「Table ▸」の操作を、キャレットのある表へ（RFN01-49 ②）。
///
/// **見出し行の前には足さない・見出し行は消さない**（表が表でなくなる）。
/// 区切り行にいるときは見出し行にいるものとして扱う。
pub fn table_edit(
    source: &str,
    styles: &[LineStyle],
    byte: usize,
    what: TableEdit,
) -> Option<Edit> {
    let table = Table::at(source, styles, byte)?;
    let rule = table.rule(source)?;
    let columns = rule.len();
    let (here_start, _) = table.lines[table.here];
    let column = column_at(
        table.text(source, table.here),
        byte.saturating_sub(here_start),
        columns,
    );
    match what {
        TableEdit::RowBefore | TableEdit::RowAfter => {
            let after = what == TableEdit::RowAfter;
            if !after && table.here < 2 {
                return None;
            }
            let row = empty_row(columns);
            let caret_in_row = empty_row_caret(column);
            if after {
                let (_, end) = table.lines[table.here.max(1)];
                let caret = end + 1 + caret_in_row;
                Some((end..end, format!("\n{row}"), (caret, caret)))
            } else {
                let caret = here_start + caret_in_row;
                Some((here_start..here_start, format!("{row}\n"), (caret, caret)))
            }
        }
        TableEdit::DeleteRow => {
            if table.here < 2 {
                return None;
            }
            let (start, end) = table.lines[table.here];
            // 行と、その後ろの改行を消す。文書の最後の行なら、前の改行を消す。
            let region = if end < source.len() {
                start..end + 1
            } else {
                start - 1..end
            };
            // 行き先：次の表の行の同じ列。無ければ前の行の同じ列。
            let caret = if table.here + 1 < table.lines.len() {
                start + cell_caret(table.text(source, table.here + 1), column)
            } else {
                // 前が区切り行なら、見出し行へ。
                let target = if table.here == 2 { 0 } else { table.here - 1 };
                table.lines[target].0 + cell_caret(table.text(source, target), column)
            };
            Some((region, String::new(), (caret, caret)))
        }
        TableEdit::ColumnBefore | TableEdit::ColumnAfter => {
            let at = column + usize::from(what == TableEdit::ColumnAfter);
            let widths = inserted_widths(&rule, at);
            let mut new_rule = rule.clone();
            new_rule.insert(
                at,
                RuleCell {
                    left: false,
                    right: false,
                    dashes: MIN_WIDTH,
                },
            );
            for (cell, width) in new_rule.iter_mut().zip(widths) {
                cell.dashes = width;
            }
            rebuild(source, &table, &rule_text(&new_rule), |line| {
                let cells = table_cells(line);
                let (point, piece) = match cells.get(at) {
                    Some(cell) => (cell.byte_start - 1, format!("|{EMPTY_CELL}")),
                    None if cells.len() == at => (line.len(), format!("{EMPTY_CELL}|")),
                    None => return (line.to_owned(), None),
                };
                let mut text = line.to_owned();
                text.insert_str(point, &piece);
                (text, Some(point + 2))
            })
        }
        TableEdit::DeleteColumn => {
            if columns < 2 {
                return None;
            }
            let widths = removed_widths(&rule, column);
            let mut new_rule = rule.clone();
            new_rule.remove(column);
            for (cell, width) in new_rule.iter_mut().zip(widths) {
                cell.dashes = width;
            }
            let stay = column.min(columns - 2);
            rebuild(source, &table, &rule_text(&new_rule), |line| {
                let cells = table_cells(line);
                let Some(cell) = cells.get(column) else {
                    return (line.to_owned(), None);
                };
                let from = cell.byte_start - 1;
                let to = cells
                    .get(column + 1)
                    .map_or(line.len(), |next| next.byte_start - 1);
                let mut text = line.to_owned();
                text.replace_range(from..to, "");
                let caret = cell_caret(&text, stay);
                (text, Some(caret))
            })
        }
        TableEdit::ResetWidths => {
            table_widths(table.text(source, 1))?;
            let reset = rule
                .iter()
                .map(|cell| RuleCell {
                    dashes: MIN_WIDTH,
                    ..*cell
                })
                .collect::<Vec<_>>();
            rule_edit(source, &table, &rule_text(&reset), byte)
        }
    }
}

/// 表の全部の行を書き直す。区切り行は`rule`に、ほかの行は`row`で書き換える。
/// `row`は書き換えた行と、キャレットの行ならそこでの位置を返す。
fn rebuild(
    source: &str,
    table: &Table,
    rule: &str,
    row: impl Fn(&str) -> (String, Option<usize>),
) -> Option<Edit> {
    let span = table.span();
    let mut text = String::new();
    let mut caret = None;
    for line in 0..table.lines.len() {
        if line > 0 {
            text.push('\n');
        }
        if line == 1 {
            text.push_str(rule);
            continue;
        }
        let (written, at) = row(table.text(source, line));
        if line == table.here {
            caret = at.map(|at| span.start + text.len() + at);
        }
        text.push_str(&written);
    }
    // 区切り行にいたら、見出し行の同じ場所へ。
    let caret = caret.unwrap_or(span.start + 2).min(span.start + text.len());
    Some((span, text, (caret, caret)))
}

/// 区切り行だけを書き換える。キャレットは同じ字の上に残す（書き換えで伸び縮みしたぶん動く）。
fn rule_edit(source: &str, table: &Table, rule: &str, byte: usize) -> Option<Edit> {
    let (start, end) = table.lines[1];
    let region = start..end;
    if &source[region.clone()] == rule {
        return None;
    }
    let caret = if byte >= end {
        byte - region.len() + rule.len()
    } else {
        byte.min(start + rule.len())
    };
    Some((region, rule.to_owned(), (caret, caret)))
}

/// 列を足したあとの`-`の数（`at`に新しい列）。
///
/// **幅の指定なしの表はそのまま指定なし**（みな同じ数）。指定のある表では、新しい列が
/// 平均の幅を取り、ほかの列は同じ比率で細くなる——表の長さは変わらない。
fn inserted_widths(rule: &[RuleCell], at: usize) -> Vec<u32> {
    let dashes = rule.iter().map(|cell| cell.dashes).collect::<Vec<_>>();
    let specified = dashes.iter().any(|count| *count != dashes[0]);
    if !specified {
        return vec![dashes[0]; dashes.len() + 1];
    }
    let total: u32 = dashes.iter().sum();
    let new = total as f32 / dashes.len() as f32;
    let scale = (total as f32 - new) / total as f32;
    let mut widths = dashes
        .iter()
        .map(|count| *count as f32 * scale)
        .collect::<Vec<_>>();
    widths.insert(at, new);
    settle_widths(&widths, total)
}

/// 列を消したあとの`-`の数。指定のある表では、残りの列が比率を保って広がり、表の長さを保つ。
fn removed_widths(rule: &[RuleCell], at: usize) -> Vec<u32> {
    let mut dashes = rule.iter().map(|cell| cell.dashes).collect::<Vec<_>>();
    let specified = dashes.iter().any(|count| *count != dashes[0]);
    let total: u32 = dashes.iter().sum();
    dashes.remove(at);
    if !specified {
        return dashes;
    }
    let left: u32 = dashes.iter().sum();
    let scale = total as f32 / left.max(1) as f32;
    let widths = dashes
        .iter()
        .map(|count| *count as f32 * scale)
        .collect::<Vec<_>>();
    settle_widths(&widths, total)
}

/// 幅（%、小数）を`-`の数にする：下限3、合計は`total`を目安に100まで。**すべて同じ数に
/// なったら最後の列を1つずらす**——同じ数は「指定なし」と読まれ、指定したはずの幅が
/// 中身の幅に戻ってしまう。
fn settle_widths(widths: &[f32], total: u32) -> Vec<u32> {
    let mut counts = widths
        .iter()
        .map(|width| (width.round() as u32).max(MIN_WIDTH))
        .collect::<Vec<_>>();
    let limit = total.clamp(MIN_WIDTH * counts.len() as u32, FULL_WIDTH.max(total));
    while counts.iter().sum::<u32>() > limit {
        let Some(widest) = counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > MIN_WIDTH)
            .max_by_key(|(_, count)| **count)
            .map(|(at, _)| at)
        else {
            break;
        };
        counts[widest] -= 1;
    }
    keep_specified(&mut counts);
    counts
}

/// すべて同じ数なら最後の列を1つずらす（[`settle_widths`]）。1列なら何もできない。
fn keep_specified(counts: &mut [u32]) {
    let Some(first) = counts.first().copied() else {
        return;
    };
    if counts.len() < 2 || counts.iter().any(|count| *count != first) {
        return;
    }
    let last = counts.len() - 1;
    let sum: u32 = counts.iter().sum();
    if sum < FULL_WIDTH {
        counts[last] += 1;
    } else if counts[last] > MIN_WIDTH {
        counts[last] -= 1;
    }
}

/// 境目を引いたあとの列の幅（%）（RFN01-49 ③）。
///
/// - `stated`：区切り行が言う幅（指定なしなら`None`）。
/// - `visible`：いま見えている各列の字の幅、`reach`：表の長さ、`line_box`：行の長さ
///   （どれも同じ単位）。
/// - `boundary`：引いた境目。`1..columns`は列の間（その前後の2列が変わる）、`columns`は
///   表の外側の縁（最後の列が変わり、表の長さが変わる）。
/// - `delta`：境目を動かした長さ（行の軸、読む向きへ正）。
///
/// **つかんだ時点で表の大きさを変えない**：指定なしの表は、いま見えている幅を
/// 行の長さに対する割合にしてから動かす（書き手の案 2026-09-23）。
pub fn dragged_widths(
    stated: Option<&[u32]>,
    visible: &[f32],
    reach: f32,
    line_box: f32,
    boundary: usize,
    delta: f32,
) -> Option<Vec<u32>> {
    let columns = visible.len();
    if columns < 2 || boundary == 0 || boundary > columns || line_box <= 0.0 {
        return None;
    }
    let mut widths: Vec<f32> = match stated {
        Some(stated) if stated.len() == columns && stated.iter().sum::<u32>() <= FULL_WIDTH => {
            stated.iter().map(|count| *count as f32).collect()
        }
        _ => {
            let whole = (reach / line_box * FULL_WIDTH as f32).min(FULL_WIDTH as f32);
            let sum: f32 = visible.iter().sum();
            visible
                .iter()
                .map(|width| {
                    if sum > 0.0 {
                        width / sum * whole
                    } else {
                        whole / columns as f32
                    }
                })
                .collect()
        }
    };
    let step = delta / line_box * FULL_WIDTH as f32;
    let min = MIN_WIDTH as f32;
    if boundary < columns {
        let pair = widths[boundary - 1] + widths[boundary];
        let before = (widths[boundary - 1] + step).clamp(min, (pair - min).max(min));
        widths[boundary - 1] = before;
        widths[boundary] = pair - before;
        let mut counts = widths
            .iter()
            .map(|width| (width.round() as u32).max(MIN_WIDTH))
            .collect::<Vec<_>>();
        // 引いた2列の合計は保つ——丸めで表の長さが動かないように。
        let pair = (pair.round() as u32).max(MIN_WIDTH * 2);
        counts[boundary - 1] = counts[boundary - 1].min(pair - MIN_WIDTH);
        counts[boundary] = pair - counts[boundary - 1];
        let total: u32 = counts.iter().sum();
        if total > FULL_WIDTH {
            return Some(settle_widths(
                &counts.iter().map(|count| *count as f32).collect::<Vec<_>>(),
                FULL_WIDTH,
            ));
        }
        keep_specified(&mut counts);
        Some(counts)
    } else {
        let mut counts = widths
            .iter()
            .map(|width| (width.round() as u32).max(MIN_WIDTH))
            .collect::<Vec<_>>();
        let others: u32 = counts[..columns - 1].iter().sum();
        let room = FULL_WIDTH.saturating_sub(others).max(MIN_WIDTH);
        let last = (widths[columns - 1] + step).round().clamp(min, room as f32);
        counts[columns - 1] = last as u32;
        keep_specified(&mut counts);
        Some(counts)
    }
}

/// 表の区切り行を、この幅（%）で書き直す（RFN01-49 ③）。`table_line`は表の頭の行のどこか。
/// 揃えの`:`は保つ。
pub fn widths_edit(
    source: &str,
    styles: &[LineStyle],
    table_line: usize,
    widths: &[u32],
    caret: usize,
) -> Option<Edit> {
    let table = Table::at(source, styles, table_line)?;
    let rule = table.rule(source)?;
    if rule.len() != widths.len() {
        return None;
    }
    let cells = rule
        .iter()
        .zip(widths)
        .map(|(cell, width)| RuleCell {
            dashes: (*width).max(MIN_WIDTH),
            ..*cell
        })
        .collect::<Vec<_>>();
    rule_edit(source, &table, &rule_text(&cells), caret)
}

/// 表の区切り行が言う幅。指定なし、または表の中でなければ`None`。
pub fn stated_widths(source: &str, styles: &[LineStyle], byte: usize) -> Option<Vec<u32>> {
    let table = Table::at(source, styles, byte)?;
    table_widths(table.text(source, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::line_styles;

    fn apply(source: &str, edit: Edit) -> (String, usize) {
        let (region, text, (caret, _)) = edit;
        let mut next = source.to_owned();
        next.replace_range(region, &text);
        (next, caret)
    }

    fn insert(source: &str, at: usize, rows: usize, columns: usize) -> Option<(String, usize)> {
        let styles = line_styles(source);
        insert_table(source, &styles, at, at, rows, columns).map(|edit| apply(source, edit))
    }

    fn edit(source: &str, at: usize, what: TableEdit) -> Option<(String, usize)> {
        let styles = line_styles(source);
        table_edit(source, &styles, at, what).map(|edit| apply(source, edit))
    }

    #[test]
    fn a_table_goes_on_an_empty_line() {
        let (text, caret) = insert("", 0, 3, 2).unwrap();
        assert_eq!(text, "|  |  |\n| --- | --- |\n|  |  |\n|  |  |");
        assert_eq!(caret, 2);
        let styles = line_styles(&text);
        assert_eq!(styles[0].kind, LineKind::TableRow);
        assert_eq!(styles[1].kind, LineKind::TableRule);
        assert_eq!(styles[3].kind, LineKind::TableRow);
    }

    #[test]
    fn a_table_after_prose_is_kept_apart_from_it() {
        let source = "前の段落\n次の段落";
        let (text, caret) = insert(source, 3, 1, 1).unwrap();
        assert_eq!(text, "前の段落\n\n|  |\n| --- |\n\n次の段落");
        assert_eq!(&text[caret - 2..caret], "| ");
    }

    #[test]
    fn a_table_on_an_empty_line_between_paragraphs_is_kept_apart() {
        // 「上」は3バイトなので、空の行は4バイト目から。
        let source = "上\n\n下";
        let (text, _) = insert(source, 4, 1, 2).unwrap();
        assert_eq!(text, "上\n\n|  |  |\n| --- | --- |\n\n下");
        let source = "上\n\n\n下";
        let (text, _) = insert(source, 4, 1, 2).unwrap();
        assert_eq!(text, "上\n\n|  |  |\n| --- | --- |\n\n下");
    }

    #[test]
    fn a_table_is_not_placed_where_it_would_not_be_one() {
        assert!(insert("> 引用", 3, 2, 2).is_none());
        assert!(insert("- 項目", 3, 2, 2).is_none());
        assert!(insert("```\nコード\n```", 5, 2, 2).is_none());
        assert!(insert("| a | b |\n| --- | --- |", 3, 2, 2).is_none());
        let styles = line_styles("本文");
        assert!(insert_table("本文", &styles, 0, 3, 2, 2).is_none());
        assert!(insert("本文", 0, 0, 2).is_none());
    }

    const TABLE: &str = "| 名前 | 役割 |\n| --- | :---: |\n| 主人公 | 語り手 |\n| 犬 | 相棒 |";

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap()
    }

    #[test]
    fn rows_are_added_before_and_after() {
        let (text, caret) = edit(TABLE, at(TABLE, "犬"), TableEdit::RowBefore).unwrap();
        assert_eq!(
            text,
            "| 名前 | 役割 |\n| --- | :---: |\n| 主人公 | 語り手 |\n|  |  |\n| 犬 | 相棒 |"
        );
        assert_eq!(caret, at(&text, "|  |  |\n| 犬") + 2);

        let (text, caret) = edit(TABLE, at(TABLE, "語り手"), TableEdit::RowAfter).unwrap();
        assert!(text.contains("| 主人公 | 語り手 |\n|  |  |\n| 犬"));
        assert_eq!(caret, at(&text, "|  |  |\n| 犬") + 5);

        // 見出し行の後ろは、区切り行の後ろ。
        let (text, _) = edit(TABLE, at(TABLE, "名前"), TableEdit::RowAfter).unwrap();
        assert!(text.starts_with("| 名前 | 役割 |\n| --- | :---: |\n|  |  |\n| 主人公"));
        assert!(edit(TABLE, at(TABLE, "名前"), TableEdit::RowBefore).is_none());
        assert!(edit(TABLE, at(TABLE, ":---:"), TableEdit::RowBefore).is_none());
    }

    #[test]
    fn a_body_row_is_deleted_and_the_header_is_not() {
        let (text, caret) = edit(TABLE, at(TABLE, "主人公"), TableEdit::DeleteRow).unwrap();
        assert_eq!(text, "| 名前 | 役割 |\n| --- | :---: |\n| 犬 | 相棒 |");
        assert_eq!(caret, at(&text, "犬"));
        let (text, caret) = edit(TABLE, at(TABLE, "相棒"), TableEdit::DeleteRow).unwrap();
        assert_eq!(
            text,
            "| 名前 | 役割 |\n| --- | :---: |\n| 主人公 | 語り手 |"
        );
        assert_eq!(caret, at(&text, "語り手"));
        assert!(edit(TABLE, at(TABLE, "名前"), TableEdit::DeleteRow).is_none());
        // 本文の行が無くなっても、見出し行と区切り行で表のまま。
        let two = "| a | b |\n| --- | --- |\n| c | d |\n";
        let (text, caret) = edit(two, at(two, "c"), TableEdit::DeleteRow).unwrap();
        assert_eq!(text, "| a | b |\n| --- | --- |\n");
        assert_eq!(caret, at(&text, "a"));
    }

    #[test]
    fn columns_are_added_before_and_after() {
        let (text, caret) = edit(TABLE, at(TABLE, "語り手"), TableEdit::ColumnBefore).unwrap();
        assert_eq!(
            text,
            "| 名前 |  | 役割 |\n| --- | --- | :---: |\n| 主人公 |  | 語り手 |\n| 犬 |  | 相棒 |"
        );
        assert_eq!(caret, at(&text, "|  | 語り手") + 2);

        let (text, _) = edit(TABLE, at(TABLE, "語り手"), TableEdit::ColumnAfter).unwrap();
        assert_eq!(
            text,
            "| 名前 | 役割 |  |\n| --- | :---: | --- |\n| 主人公 | 語り手 |  |\n| 犬 | 相棒 |  |"
        );
        let styles = line_styles(&text);
        assert!(styles.iter().all(|style| style.kind.is_table()));
    }

    #[test]
    fn a_column_is_deleted_but_not_the_last_one() {
        let (text, caret) = edit(TABLE, at(TABLE, "名前"), TableEdit::DeleteColumn).unwrap();
        assert_eq!(text, "| 役割 |\n| :---: |\n| 語り手 |\n| 相棒 |");
        assert_eq!(caret, at(&text, "役割"));
        let (text, _) = edit(TABLE, at(TABLE, "相棒"), TableEdit::DeleteColumn).unwrap();
        assert_eq!(text, "| 名前 |\n| --- |\n| 主人公 |\n| 犬 |");
        assert!(edit(&text, 2, TableEdit::DeleteColumn).is_none());
    }

    #[test]
    fn the_state_says_what_can_be_done() {
        let styles = line_styles(TABLE);
        let header = table_edit_state(TABLE, &styles, 2).unwrap();
        assert_eq!(header, [false, true, true, true, false, true, false]);
        let body = table_edit_state(TABLE, &styles, at(TABLE, "犬")).unwrap();
        assert_eq!(body, [true, true, true, true, true, true, false]);
        assert!(table_edit_state("本文", &line_styles("本文"), 0).is_none());
    }

    #[test]
    fn widths_are_read_only_when_they_differ() {
        assert_eq!(table_widths("| --- | --- |"), None);
        assert_eq!(table_widths("|-----|-----|"), None);
        assert_eq!(table_widths("| --- |"), None);
        assert_eq!(
            table_widths("| ---------- | :--------------------: |"),
            Some(vec![10, 20])
        );
        assert_eq!(table_widths("| 見出し | x |"), None);
    }

    const WIDE: &str = "| a | b | c |\n| ---------- | :--------------------: | ------------------------------ |\n| 1 | 2 | 3 |";

    #[test]
    fn a_column_added_to_a_table_with_widths_takes_the_average() {
        let (text, _) = edit(WIDE, at(WIDE, "2"), TableEdit::ColumnAfter).unwrap();
        let rule = text.lines().nth(1).unwrap();
        let widths = table_widths(rule).unwrap();
        // 新しい列は平均（60÷3＝20）、ほかは40÷60の比で細る：10→7、20→13、30→20。
        assert_eq!(widths, vec![7, 13, 20, 20]);
        assert!(rule.contains(":-"), "the alignment stays: {rule}");
    }

    #[test]
    fn a_column_removed_from_a_table_with_widths_gives_its_room_away() {
        let (text, _) = edit(WIDE, at(WIDE, "1"), TableEdit::DeleteColumn).unwrap();
        let widths = table_widths(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(widths.iter().sum::<u32>(), 60);
        assert_eq!(widths, vec![24, 36]);
    }

    #[test]
    fn widths_are_reset_to_even_dashes() {
        let (text, caret) = edit(WIDE, at(WIDE, "3"), TableEdit::ResetWidths).unwrap();
        assert_eq!(text, "| a | b | c |\n| --- | :---: | --- |\n| 1 | 2 | 3 |");
        assert_eq!(caret, at(&text, "3"));
        assert!(edit(TABLE, 2, TableEdit::ResetWidths).is_none());
    }

    #[test]
    fn a_drag_starts_from_what_is_seen() {
        // 行の長さ1000、表の長さ300、字の幅は100と200。つかんだだけでは動かない。
        let widths = dragged_widths(None, &[100.0, 200.0], 300.0, 1000.0, 1, 0.0).unwrap();
        assert_eq!(widths, vec![10, 20]);
        // 境目を行の5%ぶん進める：前の列が広がり、後ろの列が同じだけ狭まる。
        // 15と15は「指定なし」と読まれるので、最後の列を1つずらす。
        let widths = dragged_widths(None, &[100.0, 200.0], 300.0, 1000.0, 1, 50.0).unwrap();
        assert_eq!(widths, vec![15, 16]);
        // 縁を引くと最後の列だけが変わる。
        let widths = dragged_widths(None, &[100.0, 200.0], 300.0, 1000.0, 2, 100.0).unwrap();
        assert_eq!(widths, vec![10, 30]);
    }

    #[test]
    fn a_drag_keeps_the_limits() {
        let stated = [10, 20];
        let widths = dragged_widths(Some(&stated), &[1.0, 1.0], 1.0, 1000.0, 1, -500.0).unwrap();
        assert_eq!(widths, vec![3, 27]);
        let widths = dragged_widths(Some(&stated), &[1.0, 1.0], 1.0, 1000.0, 2, 5000.0).unwrap();
        assert_eq!(widths, vec![10, 90]);
        let widths = dragged_widths(Some(&stated), &[1.0, 1.0], 1.0, 1000.0, 2, -5000.0).unwrap();
        assert_eq!(widths, vec![10, 3]);
        assert!(dragged_widths(None, &[100.0], 100.0, 1000.0, 1, 10.0).is_none());
    }

    #[test]
    fn widths_are_written_into_the_rule() {
        let styles = line_styles(TABLE);
        let caret = at(TABLE, "犬");
        let edit = widths_edit(TABLE, &styles, 0, &[12, 30], caret).unwrap();
        let (text, moved) = apply(TABLE, edit);
        assert_eq!(
            text.lines().nth(1).unwrap(),
            format!("| {} | :{}: |", "-".repeat(12), "-".repeat(30))
        );
        assert_eq!(moved, at(&text, "犬"));
        assert_eq!(
            stated_widths(&text, &line_styles(&text), 0),
            Some(vec![12, 30])
        );
    }
}
