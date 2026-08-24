//! How the editing area is divided between panes (要件 6.4).
//!
//! **Pure arithmetic, and deliberately so.** Nothing here knows about Windows,
//! about Slint or about what a pane draws: a tree of splits, an area of pixels,
//! and where each pane lands. That makes every rule about splitting, unsplitting
//! and dragging a boundary testable without a window, which is the same bargain
//! `text_blocks.rs` takes (ペイン分割設計 4).
//!
//! Slint cannot draw a tree of unknown depth — a component may not instantiate
//! itself — so the tree is flattened here into a list of rectangles and the
//! window places them. That is why this module ends at `place`.

/// A rectangle in the editing area's own pixels, with the origin at its
/// top-left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Which way a split cuts.
///
/// `SideBySide` puts its children left and right, so the boundary between them
/// is a vertical line that moves horizontally. Naming the arrangement rather
/// than the boundary keeps 要件 6.4's "右方向または下方向へ分割" readable: right
/// is `SideBySide`, down is `Stacked`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    SideBySide,
    Stacked,
}

/// How wide the grab area of a boundary is.
///
/// Wider than the line it draws, so there is something to take hold of. The
/// panes on either side lose this between them, which is why it is taken out of
/// the area before the ratio is applied rather than after.
pub const DIVIDER: f32 = 9.0;
/// No pane is ever divided below this, in pixels.
///
/// A ratio alone cannot say this: half of a narrow window is narrower than half
/// of a wide one, and a pane thinner than a line of text is not a pane. The
/// bound is applied where the ratio is used, so a window that grows gives the
/// ratio back rather than keeping whatever the clamp made of it.
pub const MIN_PANE: f32 = 120.0;

/// The editing area, divided.
///
/// A leaf names a pane by its number; everything else is a boundary with two
/// children. **The tree is the whole of the arrangement** — which panes exist,
/// how they sit, and where each boundary is (要件 8.5 restores exactly this).
#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Pane(usize),
    Divided {
        split: Split,
        /// The fraction of the area the first child gets, before the minimum
        /// pane size has its say.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

/// Where one boundary is, and which way it moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Boundary {
    pub rect: Rect,
    pub split: Split,
}

impl Layout {
    /// One pane, filling everything.
    pub fn single(pane: usize) -> Self {
        Layout::Pane(pane)
    }

    /// Two panes, evenly divided.
    pub fn divided(split: Split, first: usize, second: usize) -> Self {
        Layout::Divided {
            split,
            ratio: 0.5,
            first: Box::new(Layout::Pane(first)),
            second: Box::new(Layout::Pane(second)),
        }
    }

    /// Every pane in the tree, in the order they are drawn.
    pub fn panes(&self) -> Vec<usize> {
        let mut found = Vec::new();
        self.collect_panes(&mut found);
        found
    }

    fn collect_panes(&self, found: &mut Vec<usize>) {
        match self {
            Layout::Pane(pane) => found.push(*pane),
            Layout::Divided { first, second, .. } => {
                first.collect_panes(found);
                second.collect_panes(found);
            }
        }
    }

    /// Divide the pane `at`, putting `new_pane` on the far side of the boundary
    /// (要件 6.4: 右方向または下方向).
    ///
    /// Only the named pane is divided, however deep it sits — that is what
    /// "左右に分割した後で左側だけを上下に分割できる" asks for.
    pub fn divide(&mut self, at: usize, split: Split, new_pane: usize) -> bool {
        match self {
            Layout::Pane(pane) if *pane == at => {
                *self = Layout::Divided {
                    split,
                    ratio: 0.5,
                    first: Box::new(Layout::Pane(at)),
                    second: Box::new(Layout::Pane(new_pane)),
                };
                true
            }
            Layout::Pane(_) => false,
            Layout::Divided { first, second, .. } => {
                first.divide(at, split, new_pane) || second.divide(at, split, new_pane)
            }
        }
    }

    /// Take a pane out, and put its boundary's other child in their place
    /// (要件 6.4: あるペインのタブをすべて閉じるとその分割を解除する).
    ///
    /// The last pane cannot go: an editing area with no pane has nothing to show
    /// and nowhere to type.
    pub fn remove(&mut self, pane: usize) -> bool {
        match self {
            Layout::Pane(_) => false,
            Layout::Divided { first, second, .. } => {
                if **first == Layout::Pane(pane) {
                    *self = (**second).clone();
                    return true;
                }
                if **second == Layout::Pane(pane) {
                    *self = (**first).clone();
                    return true;
                }
                first.remove(pane) || second.remove(pane)
            }
        }
    }

    /// Put two panes in each other's places (要件 6.4: 編集ペインの入れ替え).
    ///
    /// The arrangement does not change — only which pane is where.
    pub fn swap(&mut self, one: usize, other: usize) {
        match self {
            Layout::Pane(pane) => {
                if *pane == one {
                    *pane = other;
                } else if *pane == other {
                    *pane = one;
                }
            }
            Layout::Divided { first, second, .. } => {
                first.swap(one, other);
                second.swap(one, other);
            }
        }
    }

    /// The tree as one line of text, for the session to keep (要件 8.5).
    ///
    /// Prefix order and one token per word: `S` and its axis and ratio, then its
    /// two children; `P` and a pane's number. No brackets, because prefix order
    /// needs none — the shape is in the reading.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut String) {
        match self {
            Layout::Pane(pane) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&format!("P {pane}"));
            }
            Layout::Divided {
                split,
                ratio,
                first,
                second,
            } => {
                if !out.is_empty() {
                    out.push(' ');
                }
                let axis = match split {
                    Split::SideBySide => "h",
                    Split::Stacked => "v",
                };
                out.push_str(&format!("S {axis} {ratio:.4}"));
                first.encode_into(out);
                second.encode_into(out);
            }
        }
    }

    /// Read back what [`encode`](Layout::encode) wrote.
    ///
    /// `None` for anything that does not read as a whole tree — a session file
    /// from a later build, or one that was cut short. **The arrangement is worth
    /// having but never worth failing to start over**, so the caller opens with
    /// one pane instead.
    pub fn decode(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let tree = Layout::decode_from(&mut words)?;
        words.next().is_none().then_some(tree)
    }

    fn decode_from<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<Self> {
        match words.next()? {
            "P" => Some(Layout::Pane(words.next()?.parse().ok()?)),
            "S" => {
                let split = match words.next()? {
                    "h" => Split::SideBySide,
                    "v" => Split::Stacked,
                    _ => return None,
                };
                let ratio: f32 = words.next()?.parse().ok()?;
                Some(Layout::Divided {
                    split,
                    ratio: ratio.clamp(0.0, 1.0),
                    first: Box::new(Layout::decode_from(words)?),
                    second: Box::new(Layout::decode_from(words)?),
                })
            }
            _ => None,
        }
    }

    /// Where everything goes, given the area to fill.
    ///
    /// The panes come back in drawing order and the boundaries in the order a
    /// drag names them ([`move_boundary`](Layout::move_boundary) walks the same
    /// one).
    pub fn place(&self, area: Rect) -> (Vec<(usize, Rect)>, Vec<Boundary>) {
        let mut panes = Vec::new();
        let mut boundaries = Vec::new();
        self.place_into(area, &mut panes, &mut boundaries);
        (panes, boundaries)
    }

    fn place_into(
        &self,
        area: Rect,
        panes: &mut Vec<(usize, Rect)>,
        boundaries: &mut Vec<Boundary>,
    ) {
        match self {
            Layout::Pane(pane) => panes.push((*pane, area)),
            Layout::Divided {
                split,
                ratio,
                first,
                second,
            } => {
                let (one, boundary, two) = cut(area, *split, *ratio);
                boundaries.push(Boundary {
                    rect: boundary,
                    split: *split,
                });
                first.place_into(one, panes, boundaries);
                second.place_into(two, panes, boundaries);
            }
        }
    }

    /// Move one boundary by a fraction of the area it divides.
    ///
    /// `index` is the boundary's position in what [`place`](Layout::place)
    /// returned, which is why a drag can name one without the window knowing
    /// anything about the tree.
    pub fn move_boundary(&mut self, index: usize, by: f32) -> bool {
        let mut seen = 0;
        self.move_boundary_into(index, by, &mut seen)
    }

    fn move_boundary_into(&mut self, index: usize, by: f32, seen: &mut usize) -> bool {
        let Layout::Divided {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        if *seen == index {
            *ratio = (*ratio + by).clamp(0.05, 0.95);
            return true;
        }
        *seen += 1;
        first.move_boundary_into(index, by, seen) || second.move_boundary_into(index, by, seen)
    }
}

/// Divide one area in two, with the boundary between them.
///
/// The boundary's width comes off the top, so the two panes and the gap between
/// them add up to exactly what there was. Each side keeps [`MIN_PANE`] where
/// there is room for it: a ratio near the edge would otherwise leave a pane too
/// thin to put a line of text in.
fn cut(area: Rect, split: Split, ratio: f32) -> (Rect, Rect, Rect) {
    let along = match split {
        Split::SideBySide => area.width,
        Split::Stacked => area.height,
    };
    let usable = (along - DIVIDER).max(0.0);
    let wanted = usable * ratio.clamp(0.0, 1.0);
    // Only where both sides can have it: in an area too small for two minimums
    // the halves are simply equal, which is the least surprising thing a window
    // dragged very small can do.
    let first = if usable >= MIN_PANE * 2.0 {
        wanted.clamp(MIN_PANE, usable - MIN_PANE)
    } else {
        usable / 2.0
    };
    let second = usable - first;
    match split {
        Split::SideBySide => (
            Rect::new(area.x, area.y, first, area.height),
            Rect::new(area.x + first, area.y, DIVIDER, area.height),
            Rect::new(area.x + first + DIVIDER, area.y, second, area.height),
        ),
        Split::Stacked => (
            Rect::new(area.x, area.y, area.width, first),
            Rect::new(area.x, area.y + first, area.width, DIVIDER),
            Rect::new(area.x, area.y + first + DIVIDER, area.width, second),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0.0, 0.0, 1009.0, 600.0)
    }

    /// One pane fills everything, and there is nothing to drag.
    #[test]
    fn a_single_pane_takes_the_whole_area() {
        let layout = Layout::single(0);
        let (panes, boundaries) = layout.place(area());

        assert_eq!(panes, vec![(0, area())]);
        assert!(boundaries.is_empty());
    }

    /// The two panes and the boundary add up to exactly the area: a gap or an
    /// overlap of one pixel is a line of text drawn twice or not at all.
    #[test]
    fn a_division_spends_every_pixel_it_was_given() {
        let mut layout = Layout::single(0);
        assert!(layout.divide(0, Split::SideBySide, 1));
        let (panes, boundaries) = layout.place(area());

        let left = panes[0].1;
        let right = panes[1].1;
        let boundary = boundaries[0].rect;
        assert_eq!(left.width + boundary.width + right.width, area().width);
        assert_eq!(boundary.x, left.width);
        assert_eq!(right.x, left.width + DIVIDER);
        // Across the split nothing is divided at all.
        assert_eq!(left.height, area().height);
        assert_eq!(right.height, area().height);
    }

    /// Stacking divides the other way, and nothing else changes.
    #[test]
    fn stacking_divides_the_other_axis() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::Stacked, 1);
        let (panes, boundaries) = layout.place(area());

        let top = panes[0].1;
        let bottom = panes[1].1;
        assert_eq!(top.width, area().width);
        assert_eq!(bottom.y, top.height + DIVIDER);
        assert_eq!(
            top.height + boundaries[0].rect.height + bottom.height,
            600.0
        );
    }

    /// 要件 6.4: 左右に分割した後で左側だけを上下に分割できる。
    #[test]
    fn only_the_named_pane_is_divided_however_deep_it_sits() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        assert!(layout.divide(0, Split::Stacked, 2));
        let (panes, boundaries) = layout.place(area());

        assert_eq!(layout.panes(), vec![0, 2, 1]);
        assert_eq!(boundaries.len(), 2);
        // The pane that was not named keeps the whole of its side.
        let right = panes.iter().find(|(pane, _)| *pane == 1).unwrap().1;
        assert_eq!(right.height, area().height);
        // The two that share the left side split its height between them.
        let top = panes.iter().find(|(pane, _)| *pane == 0).unwrap().1;
        let bottom = panes.iter().find(|(pane, _)| *pane == 2).unwrap().1;
        assert_eq!(top.width, bottom.width);
        assert_eq!(top.height + DIVIDER + bottom.height, area().height);
    }

    /// Dividing a pane that is not there changes nothing.
    #[test]
    fn dividing_a_pane_that_is_not_there_does_nothing() {
        let mut layout = Layout::single(0);
        assert!(!layout.divide(7, Split::SideBySide, 1));
        assert_eq!(layout, Layout::single(0));
    }

    /// 要件 6.4: あるペインのタブをすべて閉じるとその分割を解除する。
    #[test]
    fn removing_a_pane_gives_its_place_to_the_other_side() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        layout.divide(1, Split::Stacked, 2);

        assert!(layout.remove(2));
        assert_eq!(
            layout.panes(),
            vec![0, 1],
            "the stack collapses into pane 1"
        );
        assert!(layout.remove(1));
        assert_eq!(layout, Layout::single(0), "and the whole area is pane 0's");
        assert!(!layout.remove(0), "the last pane cannot go");
    }

    /// 要件 6.4: 編集ペインの入れ替え。The arrangement stays; the panes move.
    #[test]
    fn swapping_moves_the_panes_and_not_the_boundaries() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        let before = layout.place(area()).1;

        layout.swap(0, 1);

        assert_eq!(layout.panes(), vec![1, 0]);
        assert_eq!(layout.place(area()).1, before);
    }

    /// A boundary is named by where it came back in, so a drag needs to know
    /// nothing about the tree.
    #[test]
    fn a_boundary_is_moved_by_the_position_it_was_reported_in() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        layout.divide(0, Split::Stacked, 2);

        // The second boundary is the stack inside the left side.
        assert!(layout.move_boundary(1, 0.25));
        let (panes, _) = layout.place(area());
        let top = panes.iter().find(|(pane, _)| *pane == 0).unwrap().1;
        let bottom = panes.iter().find(|(pane, _)| *pane == 2).unwrap().1;
        assert!(top.height > bottom.height, "{top:?} {bottom:?}");
        // The one that was not named has not moved.
        let right = panes.iter().find(|(pane, _)| *pane == 1).unwrap().1;
        assert_eq!(right.height, area().height);
        assert!(!layout.move_boundary(9, 0.1), "there is no ninth boundary");
    }

    /// A boundary dragged past the end stops rather than turning a pane inside
    /// out.
    #[test]
    fn a_boundary_cannot_be_dragged_off_the_edge() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);

        layout.move_boundary(0, -9.0);
        let (panes, _) = layout.place(area());
        assert!(panes[0].1.width >= MIN_PANE, "{:?}", panes[0].1);
        assert!(panes[1].1.width >= MIN_PANE, "{:?}", panes[1].1);
    }

    /// The arrangement survives a restart (要件 8.5), boundaries and all.
    #[test]
    fn a_tree_reads_back_as_the_tree_it_was() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        layout.divide(1, Split::Stacked, 2);
        layout.move_boundary(0, 0.2);

        let written = layout.encode();
        assert_eq!(Layout::decode(&written), Some(layout.clone()));
        assert_eq!(Layout::decode(&written).unwrap().encode(), written);
    }

    /// Anything that does not read as a whole tree is refused rather than
    /// guessed at: the caller opens with one pane, which is always right.
    #[test]
    fn a_session_that_does_not_read_as_a_tree_is_refused() {
        assert_eq!(Layout::decode("P 0"), Some(Layout::single(0)));
        assert_eq!(Layout::decode(""), None);
        assert_eq!(Layout::decode("S h 0.5 P 0"), None, "a child is missing");
        assert_eq!(Layout::decode("P 0 P 1"), None, "two trees, not one");
        assert_eq!(Layout::decode("S x 0.5 P 0 P 1"), None, "no such axis");
        assert_eq!(Layout::decode("P nine"), None);
    }

    /// An area too small for two whole panes is halved rather than left with a
    /// pane of no width at all.
    #[test]
    fn an_area_too_small_for_two_panes_is_simply_halved() {
        let mut layout = Layout::single(0);
        layout.divide(0, Split::SideBySide, 1);
        layout.move_boundary(0, 0.4);

        let narrow = Rect::new(0.0, 0.0, 100.0, 400.0);
        let (panes, _) = layout.place(narrow);
        assert_eq!(panes[0].1.width, panes[1].1.width);
        assert_eq!(panes[0].1.width + DIVIDER + panes[1].1.width, 100.0);
    }
}
