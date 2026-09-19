# RFN Edit local patch

Base: crates.io `i-slint-renderer-software` 1.17.1, copied without changes except
the Parley text renderer in `lib.rs`. Existing upstream copyright and license
headers are retained.

Long Editor Panel text can have glyph positions or scroll offsets outside i16.
Previously these were narrowed independently, before clipping, causing a cast
panic in release builds or arithmetic overflow in debug builds. Glyph placement
now combines offsets in floating point and excludes offscreen glyphs before
narrowing to framebuffer coordinates. Text selection/caret rectangles are also
clipped before integer conversion.

Regression coverage lives in `src/terminal_ui_tests.rs` in the application.
Remove/rebase this patch when upgrading Slint to a version that fixes this path.
