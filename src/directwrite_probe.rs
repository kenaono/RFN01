use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::{
    Win32::Graphics::DirectWrite::{
        DWRITE_BREAK_CONDITION, DWRITE_BREAK_CONDITION_NEUTRAL, DWRITE_FACTORY_TYPE_ISOLATED,
        DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_HIT_TEST_METRICS, DWRITE_INLINE_OBJECT_METRICS,
        DWRITE_OVERHANG_METRICS, DWRITE_READING_DIRECTION_TOP_TO_BOTTOM, DWRITE_TEXT_METRICS,
        DWRITE_TEXT_RANGE, DWriteCreateFactory, IDWriteFactory, IDWriteInlineObject,
        IDWriteInlineObject_Impl, IDWriteTextLayout, IDWriteTextRenderer,
    },
    core::{BOOL, IUnknown, Ref, Result, implement, w},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectWriteProbeReport {
    pub layout_width: f32,
    pub layout_height: f32,
    pub first_caret_x: f32,
    pub first_caret_y: f32,
    pub second_caret_x: f32,
    pub second_caret_y: f32,
}

pub fn probe_vertical_layout(text: &str) -> Result<DirectWriteProbeReport> {
    let utf16: Vec<u16> = text.encode_utf16().collect();

    // Isolated for the reason the renderer's is (技術検証 7.3): a shared
    // factory is one object for the whole process, and this file makes two more
    // of them — one here and one in its own tests, which run beside every other
    // graphics test.

    // SAFETY: DirectWrite objects are created and used on this thread. The UTF-16
    // buffer remains alive for the complete CreateTextLayout call.
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)?;
        let format = factory.CreateTextFormat(
            w!("Yu Mincho"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            24.0,
            w!("ja-JP"),
        )?;
        format.SetReadingDirection(DWRITE_READING_DIRECTION_TOP_TO_BOTTOM)?;
        format.SetFlowDirection(DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT)?;

        let layout = factory.CreateTextLayout(&utf16, &format, 360.0, 480.0)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;

        let (first_caret_x, first_caret_y) = hit_test_position(&layout, 0, false)?;
        let second_position = u32::from(!utf16.is_empty());
        let (second_caret_x, second_caret_y) = hit_test_position(&layout, second_position, false)?;

        Ok(DirectWriteProbeReport {
            layout_width: metrics.width,
            layout_height: metrics.height,
            first_caret_x,
            first_caret_y,
            second_caret_x,
            second_caret_y,
        })
    }
}

unsafe fn hit_test_position(
    layout: &IDWriteTextLayout,
    position: u32,
    trailing: bool,
) -> Result<(f32, f32)> {
    let mut x = 0.0;
    let mut y = 0.0;
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();

    // SAFETY: All output pointers refer to initialized stack storage and remain
    // valid for the duration of the call.
    unsafe {
        layout.HitTestTextPosition(position, trailing, &mut x, &mut y, &mut metrics)?;
    }
    Ok((x, y))
}

/// A box of a fixed size that stands in for the marker at the head of a line
/// (要件 7.3.2).
///
/// **It draws nothing.** The ornaments — the bullet, the checkbox, the quote
/// bar — are drawn in the tile pass, where the render target and the brush
/// already are; a COM object that held a render target would have to be rebuilt
/// every time the target is, and the layouts that reference it are cached
/// across exactly that. What this is for is the **space**: one width for every
/// marker, so markers of different lengths (`-`, `10.`, `- [x]`) set their text
/// at the same indent, and so that hit testing knows the indent is there.
///
/// That last point is the whole reason to try an inline object at all.
/// `leadingSpacing` moves the glyphs and leaves the caret's leading edge behind
/// (4.11), which would put the caret somewhere the text is not.
#[implement(IDWriteInlineObject)]
struct MarkerBox {
    /// How far the box reaches along the line axis — the indent itself.
    along: f32,
    /// Across the line, which only has to be enough not to disturb the line's
    /// own height.
    across: f32,
    baseline: f32,
    /// Whether the object says it can be laid sideways.
    sideways: bool,
    /// How many times DirectWrite asked for the metrics. Shared rather than
    /// owned so the probe can read it after the object has been handed over.
    asked: Arc<AtomicU32>,
}

impl IDWriteInlineObject_Impl for MarkerBox_Impl {
    fn Draw(
        &self,
        _context: *const c_void,
        _renderer: Ref<IDWriteTextRenderer>,
        _origin_x: f32,
        _origin_y: f32,
        _sideways: BOOL,
        _right_to_left: BOOL,
        _effect: Ref<IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn GetMetrics(&self) -> Result<DWRITE_INLINE_OBJECT_METRICS> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        Ok(DWRITE_INLINE_OBJECT_METRICS {
            width: self.along,
            height: self.across,
            baseline: self.baseline,
            supportsSideways: self.sideways.into(),
        })
    }

    fn GetOverhangMetrics(&self) -> Result<DWRITE_OVERHANG_METRICS> {
        // Nothing is drawn, so nothing hangs outside the box.
        Ok(DWRITE_OVERHANG_METRICS::default())
    }

    fn GetBreakConditions(
        &self,
        before: *mut DWRITE_BREAK_CONDITION,
        after: *mut DWRITE_BREAK_CONDITION,
    ) -> Result<()> {
        // SAFETY: DirectWrite hands us two pointers to its own storage, and
        // writes are guarded against the null it is allowed to pass.
        unsafe {
            if !before.is_null() {
                *before = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
            if !after.is_null() {
                *after = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
        }
        Ok(())
    }
}

/// Where a line's text and its caret land once a fixed-width box stands in for
/// the marker at its head (要件 7.3.2).
///
/// Every distance is along the **line axis** — across the page when the text is
/// horizontal, down the column when it is vertical — so the two writing
/// directions can be compared with the same numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InlineObjectProbeReport {
    /// Where the caret sits at the head of the line.
    pub head: f32,
    /// Where it sits when asked for the far side of the box's last unit.
    pub box_far_side: f32,
    /// Where the first character past the box sits. **This is the indent** if
    /// the box is a real box in the run.
    pub first_text: f32,
    /// And the one after it, so one advance of real text is visible too.
    pub second_text: f32,
    /// How many rectangles the box's own range hit-tests to.
    pub box_rects: usize,
    /// How far the first of those rectangles reaches along the line axis.
    pub box_rect_extent: f32,
    /// How many times DirectWrite asked the object for its metrics. Zero would
    /// mean the box was never consulted, whatever the other numbers say.
    pub metrics_asked: u32,
}

/// Put a fixed-width box over the first `marker_utf16` units of `text` and
/// report where everything landed.
///
/// The question this answers is whether an inline object is a **real box in the
/// run**: does the text after it start one box-width in, does the caret agree,
/// and does hit testing see it. Nothing here draws, so no render target is
/// needed and the whole thing runs from a test.
pub fn probe_inline_object(
    text: &str,
    marker_utf16: u32,
    box_along: f32,
    vertical: bool,
) -> Result<InlineObjectProbeReport> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let asked = Arc::new(AtomicU32::new(0));
    let object: IDWriteInlineObject = MarkerBox {
        along: box_along,
        across: 24.0,
        baseline: 19.0,
        sideways: true,
        asked: Arc::clone(&asked),
    }
    .into();

    // SAFETY: Every DirectWrite object is created and used on this thread, and
    // the UTF-16 buffer outlives the CreateTextLayout call.
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)?;
        let format = factory.CreateTextFormat(
            w!("Yu Mincho"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            24.0,
            w!("ja-JP"),
        )?;
        if vertical {
            format.SetReadingDirection(DWRITE_READING_DIRECTION_TOP_TO_BOTTOM)?;
            format.SetFlowDirection(DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT)?;
        }

        let layout = factory.CreateTextLayout(&utf16, &format, 480.0, 480.0)?;
        layout.SetInlineObject(
            &object,
            DWRITE_TEXT_RANGE {
                startPosition: 0,
                length: marker_utf16,
            },
        )?;
        // Asking for the metrics is what makes DirectWrite lay the line out,
        // and therefore what makes it consult the box.
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;

        let head = hit_test_position(&layout, 0, false)?;
        let far_side = hit_test_position(&layout, marker_utf16.saturating_sub(1), true)?;
        let first_text = hit_test_position(&layout, marker_utf16, false)?;
        let second_text = hit_test_position(&layout, marker_utf16 + 1, false)?;

        // One region per unit is the hard upper bound, which avoids
        // DirectWrite's insufficient-buffer probe.
        let mut regions = vec![DWRITE_HIT_TEST_METRICS::default(); marker_utf16.max(1) as usize];
        let mut count = 0;
        layout.HitTestTextRange(0, marker_utf16, 0.0, 0.0, Some(&mut regions), &mut count)?;
        regions.truncate(count as usize);
        let extent = match regions.first() {
            Some(region) if vertical => region.height,
            Some(region) => region.width,
            None => 0.0,
        };

        let along = |point: (f32, f32)| if vertical { point.1 } else { point.0 };
        Ok(InlineObjectProbeReport {
            head: along(head),
            box_far_side: along(far_side),
            first_text: along(first_text),
            second_text: along(second_text),
            box_rects: regions.len(),
            box_rect_extent: extent,
            metrics_asked: asked.load(Ordering::Relaxed),
        })
    }
}

/// What a box **in the middle of a line** actually advances the text by
/// (要件 7.3.2 の表).
///
/// The box at the head of a line advances by its `width` in both writing
/// directions (4.12). A table's boxes are not at the head — they stand where
/// the bars are, between two cells — and down a column those were seen to
/// advance by something else (7.7). This asks the question directly: one box,
/// one thing under it, one answer.
///
/// The layout is made far longer than the text so that **nothing wraps**: a
/// wrapped line would report the head of the next line as the advance, and that
/// number would look like a rule when it is an artefact.
#[cfg(test)]
pub fn probe_box_advance(
    before: &str,
    covered: &str,
    after: &str,
    along: f32,
    across: f32,
    font_size: f32,
    vertical: bool,
    sideways: bool,
) -> Result<f32> {
    let text = format!("{before}{covered}{after}");
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let start = before.encode_utf16().count() as u32;
    let length = covered.encode_utf16().count() as u32;
    let asked = Arc::new(AtomicU32::new(0));
    let object: IDWriteInlineObject = MarkerBox {
        along,
        across,
        baseline: across * 0.8,
        sideways,
        asked: Arc::clone(&asked),
    }
    .into();

    // SAFETY: Every DirectWrite object is created and used on this thread, and
    // the UTF-16 buffer outlives the CreateTextLayout call.
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)?;
        let format = factory.CreateTextFormat(
            w!("Yu Mincho"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            font_size,
            w!("ja-JP"),
        )?;
        if vertical {
            format.SetReadingDirection(DWRITE_READING_DIRECTION_TOP_TO_BOTTOM)?;
            format.SetFlowDirection(DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT)?;
        }

        let layout = factory.CreateTextLayout(&utf16, &format, 8000.0, 8000.0)?;
        layout.SetInlineObject(
            &object,
            DWRITE_TEXT_RANGE {
                startPosition: start,
                length,
            },
        )?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;

        let head = hit_test_position(&layout, start, false)?;
        let past = hit_test_position(&layout, start + length, false)?;
        let along_axis = |point: (f32, f32)| if vertical { point.1 } else { point.0 };
        Ok(along_axis(past) - along_axis(head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directwrite_places_the_next_character_below_the_first() {
        let report = probe_vertical_layout("日本語").expect("DirectWrite vertical layout");

        assert!(report.layout_width > 0.0);
        assert!(report.layout_height > 0.0);
        assert!(report.second_caret_y > report.first_caret_y);
    }

    /// How wide the stand-in box is made in these tests. Two body characters at
    /// 24pt, which is about what a list marker should reserve.
    const BOX: f32 = 48.0;

    /// 要件 7.3.2 turns on this: a list marker has to become an indent that the
    /// **caret** agrees with. `leadingSpacing` moves the glyphs and leaves the
    /// caret's leading edge behind (4.11), so this asks whether an inline
    /// object is a real box in the run instead of assuming it.
    #[test]
    fn an_inline_object_indents_the_text_and_the_caret_together() {
        let report = probe_inline_object("- 箇条書き", 2, BOX, false).expect("inline object");

        assert!(report.metrics_asked > 0, "{report:?}");
        assert!(report.head.abs() < 0.5, "{report:?}");
        assert!((report.first_text - BOX).abs() < 0.5, "{report:?}");
        assert!(report.second_text > report.first_text, "{report:?}");
        // A position inside the box may resolve to either edge, but never past
        // it: the box is one cluster, not two places to put a caret.
        assert!(report.box_far_side >= -0.5, "{report:?}");
        assert!(report.box_far_side <= BOX + 0.5, "{report:?}");
    }

    /// The same box down a column, which is the pane this editor exists for.
    /// A box that only worked across the page would be no use here.
    #[test]
    fn an_inline_object_reserves_space_down_a_column_too() {
        let report = probe_inline_object("- 箇条書き", 2, BOX, true).expect("inline object");

        assert!(report.metrics_asked > 0, "{report:?}");
        assert!(report.head.abs() < 0.5, "{report:?}");
        // **How much** it reserves is the box's own width, the same as across
        // the page — but only because the box says it cannot lie sideways
        // (4.14). The two tests below are where that is pinned; this one is
        // still worth having as the plain statement that a marker's indent is
        // its box down a column too.
        assert!((report.first_text - BOX).abs() < 0.5, "{report:?}");
        assert!(report.second_text > report.first_text, "{report:?}");
    }

    /// Hit testing has to see the box as one region, or a click at the head of
    /// a list line lands somewhere the text is not — and the selection drawn
    /// over a marker would be the wrong shape.
    #[test]
    fn hit_testing_reports_the_box_as_one_region_of_its_own_width() {
        let report = probe_inline_object("- 箇条書き", 2, BOX, false).expect("inline object");

        assert_eq!(report.box_rects, 1, "{report:?}");
        assert!((report.box_rect_extent - BOX).abs() < 0.5, "{report:?}");
    }

    /// 要件 7.3.2 の表: **a box says it cannot be laid sideways, and that is
    /// what makes its width mean the same thing down a column as across a
    /// page** (4.14).
    ///
    /// A sideways-capable box laid over an upright character advances by its
    /// `across` instead of its `along` — the object is stood upright with the
    /// text around it, and upright it is its height that runs along the line.
    /// **A table's boxes land on upright characters** whenever a cell's tail is
    /// cut, so a table built on `along` came out short by exactly the
    /// difference. `supportsSideways: false` is not a workaround for that: the
    /// box draws nothing, so it genuinely has no sideways form to offer, and
    /// saying so leaves one meaning of `width` for both directions.
    #[test]
    fn a_box_that_cannot_lie_sideways_advances_by_its_width_either_way() {
        // Two boxes of the same width over the same span, one of which begins
        // on an upright character. Nothing else differs.
        for covered in ["| ", "央 | "] {
            for sideways in [false, true] {
                let advance =
                    probe_box_advance("あい", covered, "うえ", 43.0, 22.0, 22.0, true, sideways)
                        .expect("inline object");
                let across_the_page =
                    probe_box_advance("あい", covered, "うえ", 43.0, 22.0, 22.0, false, sideways)
                        .expect("inline object");
                // Across the page the box is honoured whatever it says.
                assert!(
                    (across_the_page - 43.0).abs() < 0.5,
                    "{covered:?} sideways {sideways}: {across_the_page}"
                );
                let upright_run = covered.starts_with('央') && sideways;
                let expected = if upright_run { 22.0 } else { 43.0 };
                assert!(
                    (advance - expected).abs() < 0.5,
                    "{covered:?} sideways {sideways}: {advance}, wanted {expected}"
                );
            }
        }
    }

    /// And the value it takes instead is the box's `across`, not a rounding of
    /// its width to the em: with `across` at 44 an upright run advances 44.
    /// **The number identifies which of the two the layout read**, which is the
    /// whole of why this is written down (4.14).
    #[test]
    fn a_sideways_box_on_an_upright_run_advances_by_its_across() {
        for across in [0.0_f32, 44.0] {
            let advance =
                probe_box_advance("あい", "央 | ", "うえ", 43.0, across, 22.0, true, true)
                    .expect("inline object");
            assert!((advance - across).abs() < 0.5, "across {across}: {advance}");
        }
    }

    /// A marker of a different length must reserve the same indent — that is
    /// the whole point of a box rather than a wider space.
    #[test]
    fn a_longer_marker_reserves_the_same_indent() {
        let short = probe_inline_object("- 箇条書き", 2, BOX, false).expect("inline object");
        let long = probe_inline_object("10. 箇条書き", 4, BOX, false).expect("inline object");

        assert!((short.first_text - long.first_text).abs() < 0.5, "{long:?}");
    }
}
