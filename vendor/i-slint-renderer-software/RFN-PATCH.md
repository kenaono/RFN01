# RFN Edit local patch

Base: crates.io `i-slint-renderer-software` 1.18.1, copied without changes except
`fill_rectangle` in `lib.rs`. Existing upstream copyright and license headers are
retained.

Long Editor Panel text can put selection, caret and text-decoration rectangles
far outside i16. `fill_rectangle` narrows its rectangle to the i16
`PhysicalRect` before the processor clips it, which panics in release builds or
overflows in debug builds. The rectangle is now intersected with the clip while
it is still floating point.

Glyphs needed the same fix up to 1.17.1. Slint 1.18 fixed that path upstream
(#12994: glyph rectangles are i32 and clipped before narrowing), so the glyph
part of this patch was dropped on the upgrade (2026-11-03).

Regression coverage lives in `src/terminal_ui_tests.rs` in the application
(`terminal_panel_long_text_click_and_end_render`). Remove this patch when
upgrading Slint to a version that clips `fill_rectangle` before narrowing.
