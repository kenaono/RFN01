use windows::{
    Win32::Graphics::DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
        DWRITE_HIT_TEST_METRICS, DWRITE_READING_DIRECTION_TOP_TO_BOTTOM, DWRITE_TEXT_METRICS,
        DWriteCreateFactory, IDWriteFactory, IDWriteTextLayout,
    },
    core::{Result, w},
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

    // SAFETY: DirectWrite objects are created and used on this thread. The UTF-16
    // buffer remains alive for the complete CreateTextLayout call.
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
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

        let (first_caret_x, first_caret_y) = hit_test_position(&layout, 0)?;
        let second_position = u32::from(!utf16.is_empty());
        let (second_caret_x, second_caret_y) = hit_test_position(&layout, second_position)?;

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

unsafe fn hit_test_position(layout: &IDWriteTextLayout, position: u32) -> Result<(f32, f32)> {
    let mut x = 0.0;
    let mut y = 0.0;
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();

    // SAFETY: All output pointers refer to initialized stack storage and remain
    // valid for the duration of the call.
    unsafe {
        layout.HitTestTextPosition(position, false, &mut x, &mut y, &mut metrics)?;
    }
    Ok((x, y))
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
}
