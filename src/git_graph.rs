//! RFN01-67 PR 2a（書き手と決めた 2026-10-03）: Git Repository のグラフの筋を決める。
//!
//! 画面を知らない。Commit の並び（子が親より先）から、行ごとに「点をどの筋に置くか」と
//! 「どの線を描くか」を決める。線は行の中で閉じている——上の端から点へ入る線（`Into`）、
//! 点から下の端へ出る線（`Out`）、行を素通りする線（`Through`）。行を縦に並べれば線がつながる。
//!
//! - **幹（main）を筋0に置く**（書き手の判断 2026-10-03：「紫の幹は main」）。main より新しい行
//!   では筋0を空けておき、main に無い Commit は今いるブランチのものでも別の筋から分かれて入る。
//!   `// WIP` の行は HEAD を待つ筋に置く（HEAD が幹なら筋0）。
//! - 筋の色は筋を割り当てたときに決め、その筋が続くあいだ変えない。色0は幹だけ。
//! - 筋が`MAX_LANES`本を超えたら、それより右は最後の筋に重ねて描く（`drawn_lane`）。

/// 描く筋の数の上限。これより右の筋は最後の筋に重ねる。
pub const MAX_LANES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// 行を上から下まで素通りする。
    Through { lane: usize, color: usize },
    /// 上の端の`from`の筋から、この行の点へ入る。
    Into { from: usize, color: usize },
    /// この行の点から、下の端の`to`の筋へ出る。
    Out { to: usize, color: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub lane: usize,
    pub color: usize,
    pub lines: Vec<Line>,
}

#[derive(Clone)]
struct Lane {
    /// この筋が次に待っている Commit。
    expect: String,
    color: usize,
    /// HEAD のために空けてあるだけで、まだ線を描かない筋。
    reserved: bool,
}

/// `commits`は`(sha, 親)`を子が先の順で。`trunk`は幹（main）の先端、`head`は HEAD の sha、
/// `wip`は `// WIP` の行を先頭に足すか（足すなら戻り値の先頭がその行）。
pub fn layout(
    commits: &[(&str, &[String])],
    trunk: Option<&str>,
    head: Option<&str>,
    wip: bool,
) -> Vec<Row> {
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut next_color = 1;
    let mut rows = Vec::with_capacity(commits.len() + 1);
    let known = |sha: &&str| commits.iter().any(|(at, _)| at == sha);
    let trunk = trunk.filter(known);
    let head = head.filter(known);
    if let Some(trunk) = trunk {
        lanes.push(Some(Lane {
            expect: trunk.to_owned(),
            color: 0,
            reserved: true,
        }));
    }
    if let (true, Some(head)) = (wip, head) {
        let lane = if Some(head) == trunk {
            0
        } else {
            let at = free(&mut lanes);
            lanes[at] = Some(Lane {
                expect: head.to_owned(),
                color: next_color,
                reserved: false,
            });
            next_color += 1;
            at
        };
        let lane_entry = lanes[lane].as_mut().expect("the lane just set");
        lane_entry.reserved = false;
        let color = lane_entry.color;
        rows.push(Row {
            lane,
            color,
            lines: vec![Line::Out { to: lane, color }],
        });
    }
    for (sha, parents) in commits {
        let mine: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.as_ref().is_some_and(|l| l.expect == *sha))
            .map(|(at, _)| at)
            .collect();
        let lane = match mine.first() {
            Some(&at) => at,
            None => {
                let at = free(&mut lanes);
                lanes[at] = Some(Lane {
                    expect: (*sha).to_owned(),
                    color: next_color,
                    reserved: false,
                });
                next_color += 1;
                at
            }
        };
        let color = lanes[lane].as_ref().map_or(0, |l| l.color);
        let mut lines = Vec::new();
        for (at, held) in lanes.iter().enumerate() {
            let Some(held) = held else {
                continue;
            };
            if held.reserved {
                continue;
            }
            if mine.contains(&at) {
                lines.push(Line::Into {
                    from: at,
                    color: held.color,
                });
            } else if at != lane {
                lines.push(Line::Through {
                    lane: at,
                    color: held.color,
                });
            }
        }
        // 同じ Commit を待っていたほかの筋は、ここで合流して終わる。
        for &at in mine.iter().skip(1) {
            lanes[at] = None;
        }
        match parents.split_first() {
            None => lanes[lane] = None,
            Some((first, rest)) => {
                lanes[lane] = Some(Lane {
                    expect: first.clone(),
                    color,
                    reserved: false,
                });
                lines.push(Line::Out { to: lane, color });
                for parent in rest {
                    let waiting = lanes
                        .iter()
                        .position(|l| l.as_ref().is_some_and(|l| &l.expect == parent));
                    let (to, color) = match waiting {
                        Some(at) => (at, lanes[at].as_ref().map_or(0, |l| l.color)),
                        None => {
                            let at = free(&mut lanes);
                            lanes[at] = Some(Lane {
                                expect: parent.clone(),
                                color: next_color,
                                reserved: false,
                            });
                            next_color += 1;
                            (at, next_color - 1)
                        }
                    };
                    lines.push(Line::Out { to, color });
                }
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(Row { lane, color, lines });
    }
    rows
}

/// 空いている筋（無ければ右に足す）。
fn free(lanes: &mut Vec<Option<Lane>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(at) => at,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

/// 描く位置の筋（`MAX_LANES`本目より右は最後の筋に重ねる）。
pub fn drawn_lane(lane: usize) -> usize {
    lane.min(MAX_LANES - 1)
}

/// 行の中の線を、色ごとに`Path`の`commands`へ。`x(lane)`は筋の横位置、`height`は行の高さ。
pub fn commands(row: &Row, x: impl Fn(usize) -> f32, height: f32) -> Vec<(usize, String)> {
    let mid = height / 2.0;
    let node = x(drawn_lane(row.lane));
    let mut by_color: Vec<(usize, String)> = Vec::new();
    for line in &row.lines {
        let (color, path) = match *line {
            Line::Through { lane, color } => {
                let at = x(drawn_lane(lane));
                (color, format!("M {at} 0 L {at} {height} "))
            }
            Line::Into { from, color } => {
                let at = x(drawn_lane(from));
                let path = if at == node {
                    format!("M {at} 0 L {at} {mid} ")
                } else {
                    let bend = (mid * 0.6).round();
                    format!("M {at} 0 C {at} {bend} {node} {} {node} {mid} ", mid - bend)
                };
                (color, path)
            }
            Line::Out { to, color } => {
                let at = x(drawn_lane(to));
                let path = if at == node {
                    format!("M {node} {mid} L {node} {height} ")
                } else {
                    let bend = (mid * 0.6).round();
                    format!(
                        "M {node} {mid} C {node} {} {at} {} {at} {height} ",
                        mid + bend,
                        height - bend
                    )
                };
                (color, path)
            }
        };
        match by_color.iter_mut().find(|(c, _)| *c == color) {
            Some((_, held)) => held.push_str(&path),
            None => by_color.push((color, path)),
        }
    }
    by_color
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(parents: &[&str]) -> Vec<String> {
        parents.iter().map(|p| (*p).to_owned()).collect()
    }

    /// main: A ← B ← M（M は draft の D を Merge）、draft: A ← D。HEAD は M。
    #[test]
    fn a_merge_draws_a_branch_beside_the_head_lane() {
        let m = owned(&["b", "d"]);
        let b = owned(&["a"]);
        let d = owned(&["a"]);
        let a = owned(&[]);
        let commits: Vec<(&str, &[String])> = vec![("m", &m), ("b", &b), ("d", &d), ("a", &a)];
        let rows = layout(&commits, Some("m"), Some("m"), false);
        assert_eq!(rows.len(), 4);
        // M は筋0、下へ2本（筋0へ B、筋1へ D）。
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].color, 0);
        assert!(rows[0].lines.contains(&Line::Out { to: 0, color: 0 }));
        assert!(rows[0].lines.contains(&Line::Out { to: 1, color: 1 }));
        // B の行では筋1が素通りする。
        assert_eq!(rows[1].lane, 0);
        assert!(rows[1].lines.contains(&Line::Through { lane: 1, color: 1 }));
        // D は筋1。
        assert_eq!(rows[2].lane, 1);
        // A で2本が合流する。
        assert_eq!(rows[3].lane, 0);
        assert!(rows[3].lines.contains(&Line::Into { from: 0, color: 0 }));
        assert!(rows[3].lines.contains(&Line::Into { from: 1, color: 1 }));
        assert!(!rows[3].lines.iter().any(|l| matches!(l, Line::Out { .. })));
    }

    /// main より新しい Commit（origin/main）があっても、main は筋0。筋0は WIP が無ければ
    /// main まで描かない。
    #[test]
    fn the_trunk_keeps_lane_zero_and_wip_joins_it() {
        let o = owned(&["h"]);
        let h = owned(&["a"]);
        let a = owned(&[]);
        let commits: Vec<(&str, &[String])> = vec![("o", &o), ("h", &h), ("a", &a)];
        let rows = layout(&commits, Some("h"), Some("h"), false);
        assert_eq!(rows[0].lane, 1, "origin/main goes beside the head lane");
        assert!(
            !rows[0]
                .lines
                .iter()
                .any(|l| matches!(l, Line::Through { lane: 0, .. })),
            "nothing is drawn above the head without a WIP row"
        );
        assert_eq!(rows[1].lane, 0);
        assert!(rows[1].lines.contains(&Line::Into { from: 1, color: 1 }));
        assert!(!rows[1].lines.contains(&Line::Into { from: 0, color: 0 }));

        let rows = layout(&commits, Some("h"), Some("h"), true);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].lines, vec![Line::Out { to: 0, color: 0 }]);
        assert!(rows[1].lines.contains(&Line::Through { lane: 0, color: 0 }));
        assert!(rows[2].lines.contains(&Line::Into { from: 0, color: 0 }));
    }

    /// 枝が多いときは、`MAX_LANES`本目より右を最後の筋に重ねて描く。
    #[test]
    fn many_branches_are_drawn_on_the_last_lane() {
        let tips: Vec<String> = (0..10).map(|n| format!("t{n}")).collect();
        let root = owned(&["base"]);
        let base = owned(&[]);
        let mut commits: Vec<(&str, &[String])> =
            tips.iter().map(|t| (t.as_str(), root.as_slice())).collect();
        commits.push(("base", &base));
        let rows = layout(&commits, None, None, false);
        assert_eq!(rows[9].lane, 9);
        assert_eq!(drawn_lane(rows[9].lane), MAX_LANES - 1);
        let drawn = commands(&rows[9], |lane| lane as f32 * 10.0 + 5.0, 30.0);
        assert!(drawn.iter().all(|(_, path)| !path.contains("95 ")));
    }

    /// 書き手の確認 2026-10-03：main から作った TestBranch で Commit した（main は動いていない）。
    /// **今いるのが TestBranch でも、幹は main**——TestBranch の Commit は別の筋・別の色で
    /// main の点へ入る。WIP は TestBranch の筋に置く。
    #[test]
    fn a_branch_ahead_of_main_splits_off_the_trunk() {
        let t = owned(&["m"]);
        let m = owned(&["a"]);
        let a = owned(&[]);
        let commits: Vec<(&str, &[String])> = vec![("t", &t), ("m", &m), ("a", &a)];
        let rows = layout(&commits, Some("m"), Some("t"), true);
        assert_eq!(rows.len(), 4);
        // WIP と TestBranch は筋1（紫でない色）。
        assert_eq!((rows[0].lane, rows[0].color), (1, 1));
        assert_eq!((rows[1].lane, rows[1].color), (1, 1));
        assert!(
            !rows[1]
                .lines
                .iter()
                .any(|l| matches!(l, Line::Through { lane: 0, .. })),
            "nothing is drawn on the trunk above main"
        );
        // main の点（筋0・紫）へ、筋1から入る。
        assert_eq!((rows[2].lane, rows[2].color), (0, 0));
        assert!(rows[2].lines.contains(&Line::Into { from: 1, color: 1 }));
        assert!(!rows[2].lines.contains(&Line::Into { from: 0, color: 0 }));
    }

    #[test]
    fn commands_are_grouped_by_colour() {
        let row = Row {
            lane: 0,
            color: 0,
            lines: vec![
                Line::Into { from: 0, color: 0 },
                Line::Out { to: 0, color: 0 },
                Line::Out { to: 1, color: 1 },
            ],
        };
        let drawn = commands(&row, |lane| lane as f32 * 14.0 + 10.0, 30.0);
        assert_eq!(drawn.len(), 2);
        assert_eq!(drawn[0].1, "M 10 0 L 10 15 M 10 15 L 10 30 ");
        assert!(drawn[1].1.starts_with("M 10 15 C 10 24 24 21 24 30"));
    }
}
