//! Block-split text engine, in either writing direction.
//!
//! The whole document used to be one `IDWriteTextLayout`, rebuilt from scratch
//! for every render, hit test, caret move and selection update. That made every
//! interaction cost O(document), which is what made dragging a selection and
//! scrolling sideways stall on long documents.
//!
//! Now the document is cut into blocks at logical line boundaries (DirectWrite
//! starts a new line at every hard break, so this does not move any line),
//! each block gets its own layout, and only the blocks that intersect the
//! viewport or the caret are touched. Measurements are cached per block text, so
//! an edit re-measures the one block it changed.
//!
//! An engine is built for one [`WritingMode`] and keeps it for its lifetime. The
//! geometry underneath works along the flow and line axes of `text_blocks`, so
//! the mode only has to say how those two map onto the screen — and every place
//! that asks is in `WritingMode` itself.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::c_void,
    hash::{DefaultHasher, Hash, Hasher},
    num::NonZeroUsize,
    ops::Range,
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};

use windows::{
    Win32::{
        Foundation::{E_FAIL, RPC_E_CHANGED_MODE},
        Graphics::{
            Direct2D::{
                Common::{
                    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
                },
                D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT,
                D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Factory,
                ID2D1RenderTarget, ID2D1SolidColorBrush,
            },
            DirectWrite::{
                DWRITE_BREAK_CONDITION, DWRITE_BREAK_CONDITION_NEUTRAL,
                DWRITE_FACTORY_TYPE_ISOLATED, DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT,
                DWRITE_FLOW_DIRECTION_TOP_TO_BOTTOM, DWRITE_FONT_LINE_GAP_USAGE_DEFAULT,
                DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_HIT_TEST_METRICS,
                DWRITE_INLINE_OBJECT_METRICS, DWRITE_LINE_METRICS, DWRITE_LINE_SPACING,
                DWRITE_LINE_SPACING_METHOD_PROPORTIONAL, DWRITE_MEASURING_MODE_NATURAL,
                DWRITE_OVERHANG_METRICS, DWRITE_READING_DIRECTION_LEFT_TO_RIGHT,
                DWRITE_READING_DIRECTION_TOP_TO_BOTTOM, DWRITE_TEXT_RANGE, DWriteCreateFactory,
                IDWriteFactory, IDWriteFontCollection, IDWriteInlineObject,
                IDWriteInlineObject_Impl, IDWriteLocalizedStrings, IDWriteTextFormat,
                IDWriteTextFormat3, IDWriteTextLayout, IDWriteTextLayout1, IDWriteTextRenderer,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap,
                IWICBitmapSource, IWICImagingFactory, WICBitmapCacheOnLoad, WICRect,
            },
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
    },
    core::{BOOL, Error, HSTRING, IUnknown, Interface, Ref, Result, implement, w},
};

use crate::text_blocks::{
    AskedLine, BlockLayoutPlan, BlockMeasure, BlockPlacement, BlockSpan, DEFAULT_INK, Emphasis,
    FlowOrder, LineInfo, LineMarker, LineOrnament, LineRun, LineStyle, LongLine, MAX_HEADING_LEVEL,
    Ornament, PreparedWraps, RecordedWraps, StyleRun, StyledText, TileSpan, Typography,
    block_flow_bound, cells_per_line, line_runs, place_blocks, split_blocks, style_runs,
    wrapping_list_lines,
};

/// Which way the text runs.
///
/// Every direction-dependent decision in this module is made here and nowhere
/// else: how the flow and line axes map onto the screen, which way reading order
/// runs along the flow axis, and how DirectWrite is told to lay the text out.
/// The rest of the engine is written in terms of the two axes and never names a
/// screen axis of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WritingMode {
    /// Lines run down the pane and stack right to left. The flow axis is screen
    /// x, running backwards; the line axis is screen y, the pane's height.
    ///
    /// The default because the vertical pane is what exists so far. A pane that
    /// wants the other mode says so when it builds its engine.
    #[default]
    Vertical,
    /// Lines run across the pane and stack top to bottom. The flow axis is
    /// screen y; the line axis is screen x, the pane's width.
    Horizontal,
}

impl WritingMode {
    /// Vertical writing reads towards smaller screen x, so its blocks are placed
    /// from the far end of the flow axis.
    fn flow_order(self) -> FlowOrder {
        match self {
            WritingMode::Vertical => FlowOrder::Descending,
            WritingMode::Horizontal => FlowOrder::Ascending,
        }
    }

    /// A point given on the two axes, as a screen point.
    ///
    /// Sizes and layout boxes swap the same way, so this is also how a flow
    /// extent and a line extent become a width and a height.
    fn to_screen(self, flow: f32, line: f32) -> (f32, f32) {
        match self {
            WritingMode::Vertical => (flow, line),
            WritingMode::Horizontal => (line, flow),
        }
    }

    /// A screen point, as `(flow, line)`. The inverse of `to_screen`.
    fn to_axes(self, x: f32, y: f32) -> (f32, f32) {
        match self {
            WritingMode::Vertical => (x, y),
            WritingMode::Horizontal => (y, x),
        }
    }

    /// A pixel surface holding `flow` along the flow axis, as `(width, height)`.
    fn to_surface(self, flow: u32, line: u32) -> (u32, u32) {
        match self {
            WritingMode::Vertical => (flow, line),
            WritingMode::Horizontal => (line, flow),
        }
    }

    /// Where a hit-test box sits on the flow axis.
    ///
    /// DirectWrite reports these boxes in screen terms, so which field carries
    /// the flow coordinate depends on the mode.
    fn flow_of(self, metrics: &DWRITE_HIT_TEST_METRICS) -> f32 {
        match self {
            WritingMode::Vertical => metrics.left,
            WritingMode::Horizontal => metrics.top,
        }
    }

    /// Point DirectWrite's own reading and flow directions at this mode.
    fn apply_to(self, format: &IDWriteTextFormat) -> Result<()> {
        let (reading, flow) = match self {
            WritingMode::Vertical => (
                DWRITE_READING_DIRECTION_TOP_TO_BOTTOM,
                DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT,
            ),
            WritingMode::Horizontal => (
                DWRITE_READING_DIRECTION_LEFT_TO_RIGHT,
                DWRITE_FLOW_DIRECTION_TOP_TO_BOTTOM,
            ),
        };
        // SAFETY: The format is alive for the duration of both calls, and the
        // two directions are perpendicular, which DirectWrite requires.
        unsafe {
            format.SetReadingDirection(reading)?;
            format.SetFlowDirection(flow)?;
        }
        Ok(())
    }
}

/// One of the two colours 要件 9 lets the writer set, as Direct2D wants it.
fn colour(rgb: [f32; 3]) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
        a: 1.0,
    }
}

/// The widest tile ever rasterized, and the tile width at the default height.
pub const MAX_TILE_FLOW_SIZE: u32 = 1024;
/// Pixels to aim for in one tile.
///
/// A tile is as tall as the document, which is as tall as the pane. Holding the
/// width fixed would make a full-screen window rasterize three times the pixels
/// per tile; holding the area fixed keeps the cost of redrawing one tile roughly
/// the same whatever the window does.
const TILE_TARGET_PIXELS: u32 = MAX_TILE_FLOW_SIZE * 520;
/// Narrowest tile, so a very tall window does not produce a swarm of slivers.
const MIN_TILE_FLOW_SIZE: u32 = 256;
/// How many block layouts stay resident. A viewport spans one or two blocks, so
/// a handful covers scrolling back and forth without holding the document.
const LAYOUT_CACHE_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitTest {
    pub utf16_position: u32,
    pub is_inside: bool,
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: A guard only exists after this thread successfully called
        // CoInitializeEx, so this balances that call on the same thread.
        unsafe { CoUninitialize() };
    }
}

fn ensure_com_apartment() -> Result<Option<ComApartment>> {
    // Single-threaded, because the window's thread has to be. **This runs
    // before the window exists** — the startup probe lays text out first — and
    // winit calls `OleInitialize` when it creates the window, which fails
    // outright against a multi-threaded apartment. Asking for one here stopped
    // the editor from starting at all.
    //
    // Multi-threaded was tried, on the reasoning that a thread which lays text
    // out never pumps messages and so has no business claiming a single-threaded
    // apartment (7.3). It changed nothing about the tests it was meant to fix,
    // and broke the window. Two threads with genuinely different needs are being
    // served by one decision here; a thread that is not the window's would want
    // the other answer.
    //
    // SAFETY: The reserved pointer is null as required. A changed apartment mode
    // means COM is already usable here and must not be uninitialized by us.
    let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if result.is_ok() {
        Ok(Some(ComApartment))
    } else if result == RPC_E_CHANGED_MODE {
        Ok(None)
    } else {
        Err(result.into())
    }
}

struct RenderTargetCache {
    width: u32,
    height: u32,
    bitmap: IWICBitmap,
    target: ID2D1RenderTarget,
    brush: ID2D1SolidColorBrush,
    /// One per heading level (要件 9), for the levels drawn in another colour
    /// than the body. **Made with the target and told their colour before every
    /// tile**, because a brush belongs to the target that made it while a
    /// colour is a setting that outlives any of them.
    heading_brushes: Vec<ID2D1SolidColorBrush>,
}

/// Per-thread DirectWrite, Direct2D and WIC state.
///
/// These used to be created once per call. They are immutable and cheap to keep,
/// and the COM apartment guard is declared last so it outlives every interface
/// pointer held beside it.
struct Graphics {
    dwrite: IDWriteFactory,
    d2d: ID2D1Factory,
    wic: IWICImagingFactory,
    /// Keyed by size *and* mode: the two modes need different reading and flow
    /// directions set on the format, and this cache is shared by every engine on
    /// the thread.
    /// Keyed by the three things a format itself carries: the body size, the
    /// line spacing and the family (要件 9). Everything else typography asks
    /// for is set per range on the layout, because it varies within a block.
    formats: HashMap<(u32, u32, WritingMode, String), IDWriteTextFormat>,
    target: Option<RenderTargetCache>,
    /// Reused for every `CopyPixels`. A tile is a couple of megabytes, and a
    /// fresh `vec![0; n]` per tile would zero all of it just to overwrite it.
    scratch: Vec<u8>,
    _apartment: Option<ComApartment>,
}

impl Graphics {
    fn new() -> Result<Self> {
        let apartment = ensure_com_apartment()?;
        // SAFETY: COM is initialized on this thread by the guard above, or was
        // already initialized in a compatible mode.
        unsafe {
            Ok(Self {
                // **Isolated, not shared** (技術検証 7.3). A shared factory is
                // one object for the whole process however many threads ask for
                // it, and it is the only thing here that is: the Direct2D
                // factory is single-threaded, the WIC factory is created per
                // thread, and this struct is a `thread_local`. So a fault that
                // appears only when several threads lay text out at once has
                // exactly one place it can live.
                //
                // What it costs is the font cache, which an isolated factory
                // builds per thread rather than once. The editor lays text out
                // on one thread, so today it costs nothing at all.
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)?,
                d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                wic: CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?,
                formats: HashMap::new(),
                target: None,
                scratch: Vec::new(),
                _apartment: apartment,
            })
        }
    }

    fn text_format(
        &mut self,
        typography: &Typography,
        mode: WritingMode,
    ) -> Result<IDWriteTextFormat> {
        let font_size = typography.font_size.max(1.0);
        let line_spacing = typography.line_spacing.max(0.1);
        // 要件 9: the family is part of what makes two formats different, and
        // the writer can change it while the editor is running.
        let family = typography.body_family().to_owned();
        let key = (font_size.to_bits(), line_spacing.to_bits(), mode, family);
        if let Some(format) = self.formats.get(&key) {
            return Ok(format.clone());
        }
        let family = HSTRING::from(key.3.as_str());

        // SAFETY: The factory is alive for the lifetime of this struct, the
        // family name outlives the call, and the locale is a static wide string.
        let format = unsafe {
            self.dwrite.CreateTextFormat(
                &family,
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                w!("ja-JP"),
            )?
        };
        mode.apply_to(&format)?;
        apply_line_spacing(&format, line_spacing)?;
        self.formats.insert(key, format.clone());
        Ok(format)
    }

    /// A WIC bitmap and Direct2D target of the requested size, reused by every
    /// tile of that size. Every tile clears the surface before drawing.
    fn render_target(&mut self, width: u32, height: u32) -> Result<&RenderTargetCache> {
        let matches = self
            .target
            .as_ref()
            .is_some_and(|cache| cache.width == width && cache.height == height);
        if !matches {
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            // SAFETY: The bitmap outlives the render target created from it,
            // both being owned by the cache entry stored below.
            let (bitmap, target, brush, heading_brushes) = unsafe {
                let bitmap = self.wic.CreateBitmap(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnLoad,
                )?;
                let target = self.d2d.CreateWicBitmapRenderTarget(&bitmap, &properties)?;
                // ClearType costs more to rasterize and bakes one particular
                // subpixel order into the bitmap. These pixels are handed to
                // Slint as an image and may be scaled, so the subpixel trick is
                // wrong here anyway; greyscale is both cheaper and more correct.
                target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                // Any colour: the brush is set to the ink of the moment before
                // every tile is drawn, because the ink is a setting now and the
                // render target outlives a change to it.
                let brush = target.CreateSolidColorBrush(&colour(DEFAULT_INK), None)?;
                let mut heading_brushes = Vec::with_capacity(MAX_HEADING_LEVEL);
                for _ in 0..MAX_HEADING_LEVEL {
                    heading_brushes.push(target.CreateSolidColorBrush(&colour(DEFAULT_INK), None)?);
                }
                (bitmap, target, brush, heading_brushes)
            };
            self.target = Some(RenderTargetCache {
                width,
                height,
                bitmap,
                target,
                brush,
                heading_brushes,
            });
        }
        Ok(self.target.as_ref().expect("render target created above"))
    }
}

/// Every font family installed on this machine, by name (要件 9).
///
/// **Sorted and deduplicated**, because it is a list to look down rather than
/// the order a system collection happens to be in. Names are asked for in
/// Japanese first: a Japanese family that also has an English name is listed
/// the way the writer would look for it.
///
/// An empty list is what a machine with no font collection would give, and the
/// panel shows it as no choices rather than as an error — the editor still sets
/// text in whatever DirectWrite falls back to.
pub fn font_families() -> Vec<String> {
    with_graphics(|graphics| {
        // SAFETY: the collection and everything taken out of it live only
        // inside this call. The collection comes back through an out
        // parameter, which is why it arrives as an `Option`.
        let names = unsafe {
            let mut held: Option<IDWriteFontCollection> = None;
            graphics.dwrite.GetSystemFontCollection(&mut held, false)?;
            let Some(collection) = held else {
                return Ok(Vec::new());
            };
            let count = collection.GetFontFamilyCount();
            let mut names = Vec::with_capacity(count as usize);
            for index in 0..count {
                let family = collection.GetFontFamily(index)?;
                if let Some(name) = localized_name(&family.GetFamilyNames()?) {
                    names.push(name);
                }
            }
            names
        };
        Ok(names)
    })
    .map(|mut names| {
        names.sort();
        names.dedup();
        names
    })
    .unwrap_or_default()
}

/// One name out of a family's localized ones.
///
/// Japanese if it is there, English if not, and the first one otherwise —
/// **whatever comes back, it is a name a writer could look for.**
///
/// # Safety
///
/// The strings outlive the call, which is the caller's business.
unsafe fn localized_name(names: &IDWriteLocalizedStrings) -> Option<String> {
    unsafe {
        let mut index = 0u32;
        let mut exists = BOOL(0);
        names
            .FindLocaleName(w!("ja-jp"), &mut index, &mut exists)
            .ok()?;
        if !exists.as_bool() {
            names
                .FindLocaleName(w!("en-us"), &mut index, &mut exists)
                .ok()?;
        }
        if !exists.as_bool() {
            index = 0;
        }
        let length = names.GetStringLength(index).ok()? as usize;
        let mut buffer = vec![0u16; length + 1];
        names.GetString(index, &mut buffer).ok()?;
        buffer.pop();
        String::from_utf16(&buffer).ok()
    }
}

/// Set the line advance as a multiple of the one DirectWrite computed.
///
/// Proportional, not uniform. `SetLineSpacing` with `UNIFORM` replaces every
/// line's advance with one number, which would undo 4.2: a line holding a
/// rotated Latin run genuinely needs more room than a line of plain ideographs,
/// and the block's extent is the sum of those individual advances. Proportional
/// spacing scales each line's own height, so the differences survive and so does
/// the invariant that a block measures to the sum of its lines.
fn apply_line_spacing(format: &IDWriteTextFormat, line_spacing: f32) -> Result<()> {
    if (line_spacing - 1.0).abs() < f32::EPSILON {
        return Ok(());
    }
    let spacing = DWRITE_LINE_SPACING {
        method: DWRITE_LINE_SPACING_METHOD_PROPORTIONAL,
        height: line_spacing,
        baseline: line_spacing,
        leadingBefore: 0.0,
        fontLineGapUsage: DWRITE_FONT_LINE_GAP_USAGE_DEFAULT,
    };
    // SAFETY: The format is alive for this call, and the spacing struct is
    // read before it returns.
    unsafe {
        format
            .cast::<IDWriteTextFormat3>()?
            .SetLineSpacing(&spacing)
    }
}

/// A box of a fixed size standing in for the marker at the head of a line
/// (要件 7.3.2).
///
/// **It draws nothing.** The ink — the bullet, the checkbox, the number — is
/// drawn in the tile pass, where the render target and the brush already are. A
/// COM object holding a render target would have to be rebuilt every time the
/// target is, and the layouts that reference it are cached across exactly that.
///
/// What it is for is the space, and that space is honest: the text after it
/// starts one box in and **the caret agrees** (技術検証 4.12). `leadingSpacing`
/// would have moved the glyphs and left the caret behind (4.11).
#[implement(IDWriteInlineObject)]
struct MarkerBox {
    /// How far the box reaches along the line axis — the indent itself.
    /// DirectWrite reads this as the advance in **either** writing direction,
    /// which is the one thing 4.12 had to be asked rather than assumed.
    along: f32,
    /// Across the line. Kept under what the text on the line already asks for,
    /// so no line grows because a box sits on it.
    across: f32,
    baseline: f32,
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
        Ok(DWRITE_INLINE_OBJECT_METRICS {
            width: self.along,
            height: self.across,
            baseline: self.baseline,
            supportsSideways: true.into(),
        })
    }

    fn GetOverhangMetrics(&self) -> Result<DWRITE_OVERHANG_METRICS> {
        // Nothing is drawn here, so nothing hangs outside the box.
        Ok(DWRITE_OVERHANG_METRICS::default())
    }

    fn GetBreakConditions(
        &self,
        before: *mut DWRITE_BREAK_CONDITION,
        after: *mut DWRITE_BREAK_CONDITION,
    ) -> Result<()> {
        // SAFETY: DirectWrite hands us two pointers to its own storage, and the
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

/// Put a box over every range a marker stands at the head of (要件 7.3.2).
///
/// **Both places that build a layout call this**, right after
/// [`apply_typography`]: the one in `update` that measures the block, and the
/// one in `layout_for` that draws it. A block measured without its boxes and
/// drawn with them would be placed at a size it is not.
///
/// **The same indent for every marker is the whole point** — `-`, `10.` and
/// `- [x]` are four, five and eight characters, and all three set their text at
/// the same place. That used to be the box's width; it is the block's indent
/// now, so the box over a marker is width-less and only hides. See
/// [`LineStyle::indent_steps`].
fn apply_marker_boxes(
    layout: &IDWriteTextLayout,
    typography: &Typography,
    runs: &[StyleRun],
) -> Result<()> {
    if runs.iter().all(|run| run.ornament.is_none()) {
        return Ok(());
    }
    // **A marker's box takes no room.** The step its text begins after is the
    // block's own indent now (要件 7.3.2), and a box that also took one would
    // set the first line of an item a step further in than the lines it wraps
    // on to. What is left is the half of the job only a box can do: an inline
    // object replaces the range it covers, so the marker's glyphs are not
    // drawn.
    //
    // A whole-line box keeps its width. It stands over a line that is nothing
    // but marks — `---`, or a fence — where the room is what the line leaves
    // behind, not an indent for anything after it.
    let box_of = |along: f32| -> IDWriteInlineObject {
        MarkerBox {
            along,
            across: typography.font_size,
            baseline: typography.font_size * 0.8,
        }
        .into()
    };
    let marker = box_of(0.0);
    let whole_line = box_of(typography.indent_step());
    for run in runs {
        let Some(ornament) = run.ornament else {
            continue;
        };
        let object = if ornament.draws_ink() {
            &marker
        } else {
            &whole_line
        };
        let range = DWRITE_TEXT_RANGE {
            startPosition: run.utf16_start,
            length: run.utf16_len,
        };
        // SAFETY: `style_runs` keeps every range inside the block's own text,
        // and both objects outlive the call.
        unsafe { layout.SetInlineObject(object, range)? };
    }
    Ok(())
}

/// The byte offset of a UTF-16 position within `text`.
///
/// A walk, because that is what the question is. Asked about a marker at the
/// head of a line it stops almost at once; asked about where two mark tables
/// stopped agreeing (`reusable_prefix`) it walks that far, once per paragraph
/// whose wrapping could not be reused whole.
fn byte_at_utf16(text: &str, utf16: u32) -> usize {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        if units >= utf16 {
            return byte;
        }
        units += character.len_utf16() as u32;
    }
    text.len()
}

/// What goes in the box standing over one marker (要件 7.3.2).
///
/// A bullet and a checkbox are one glyph for every list in the document, but
/// **`10.` says something `9.` does not**, so an ordered item draws the text the
/// box is standing over. The trailing space is dropped: that space was the gap
/// after the marker, and the gap is now the box.
///
fn marker_ink(ornament: Ornament, block_text: &str, run: &StyleRun) -> String {
    match ornament {
        Ornament::Bullet => "•".to_owned(),
        Ornament::TaskOpen => "☐".to_owned(),
        Ornament::TaskDone => "☑".to_owned(),
        // A box that is there only to hide what it covers. The stroke across a
        // rule and the ground under a fence are the line's, not the box's.
        Ornament::Hidden => String::new(),
        Ornament::Number => {
            let start = byte_at_utf16(block_text, run.utf16_start);
            let end = byte_at_utf16(block_text, run.utf16_start + run.utf16_len);
            block_text[start..end].trim_end().to_owned()
        }
    }
}

/// Draw what stands in each of a block's boxes (要件 7.3.2).
///
/// **Here rather than in the box itself.** Direct2D hands an inline object a
/// renderer, not a render target, so a box that drew its own ink would have to
/// hold one — and the target is rebuilt on every resize while the layouts
/// referencing the box are cached across exactly that (技術検証 4.12).
///
/// Each ornament goes into the rectangle its own range hit-tests to, in the
/// coordinates the block was drawn at. **Nothing here asks which way the line
/// runs**: the rectangle already answers that, and the format is the pane's own.
fn draw_marker_ink(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    format: &IDWriteTextFormat,
    layout: &IDWriteTextLayout,
    runs: &[StyleRun],
    text: &str,
    origin: windows_numerics::Vector2,
    mode: WritingMode,
    indent: f32,
) -> Result<()> {
    // A box is one cluster and hit-tests to one region (技術検証 4.12). The
    // room for a few more costs nothing and keeps a surprise from becoming an
    // insufficient-buffer error in the middle of a draw.
    let mut regions = [DWRITE_HIT_TEST_METRICS::default(); 8];
    for run in runs {
        let Some(ornament) = run.ornament.filter(|kind| kind.draws_ink()) else {
            continue;
        };
        let mut count = 0;
        // SAFETY: `style_runs` keeps every range inside the block's own text,
        // and the buffer is larger than one cluster can need.
        unsafe {
            layout.HitTestTextRange(
                run.utf16_start,
                run.utf16_len,
                origin.X,
                origin.Y,
                Some(&mut regions),
                &mut count,
            )?;
        }
        // **The one thing here that rests on DirectWrite rather than on us**:
        // that a range covering a width-less inline object still hit-tests to a
        // region. It should — the object keeps its text positions and its
        // cluster, and only its advance is nothing — but if it ever does not,
        // this is where every marker in the document would quietly stop being
        // drawn.
        if count == 0 {
            continue;
        }
        let region = regions[0];
        let ink = marker_ink(ornament, text, run);
        let utf16 = ink.encode_utf16().collect::<Vec<u16>>();
        // **The box takes no room now**, so what comes back is a sliver at the
        // head of the item's text rather than a space to draw in. The glyph
        // goes in the gutter the block's own indent opened: one step back along
        // the line axis, and as far along the flow axis as the region reached —
        // which is the height of the line the item starts on, whichever way the
        // text runs.
        let (flow_size, _) = mode.to_axes(region.width, region.height);
        let (back_x, back_y) = mode.to_screen(0.0, -indent);
        let (size_x, size_y) = mode.to_screen(flow_size, indent);
        let left = region.left + back_x;
        let top = region.top + back_y;
        let rect = D2D_RECT_F {
            left,
            top,
            right: left + size_x,
            bottom: top + size_y,
        };
        // SAFETY: The buffer, the format and the brush all outlive the call,
        // and the rectangle is read before it returns.
        unsafe {
            target.DrawText(
                &utf16,
                format,
                &rect,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
    Ok(())
}

/// Whether a run's box has anything drawn in it.
///
/// A free function rather than a closure so the two callers ask the same
/// question, and so the `any` reading it stays one line (`chain_width`).
fn run_draws_ink(run: &StyleRun) -> bool {
    run.ornament.is_some_and(Ornament::draws_ink)
}

/// How much of the body ink each whole-line mark is drawn with.
///
/// **Not colours of their own.** 要件 9 lets the writer set the ink and the
/// paper, and a bar beside a quote, a stroke across a rule and the ground under
/// a code block are marks on the text rather than text: drawing them out of the
/// ink already chosen keeps them in that family whatever the writer picks.
/// These are what put the default ink on the default paper at the design's
/// `rule-strong` and its code ground.
const ORNAMENT_ALPHA: f32 = 0.30;
const CODE_GROUND_ALPHA: f32 = 0.08;

/// Where one block was drawn inside the tile, and how wide the page is.
///
/// Everything that decides a whole-line mark is in flow and line terms, the
/// way the rest of the engine is; this is the one place the two become a
/// screen rectangle.
struct OrnamentPage {
    mode: WritingMode,
    /// The flow coordinate the block layout's own origin was drawn at.
    flow_origin: f32,
    /// The page margin: where an unindented block's text begins on the line
    /// axis, and where the first quote bar stands.
    margin: f32,
    /// What this block's own indent takes, past the margin (要件 7.3.2). The
    /// text begins at `margin + inset`, and the bars stand in between.
    inset: f32,
    /// One step of indenting, which is also the space each bar has to itself.
    indent: f32,
    /// The pane across the line axis, both margins included.
    line_extent: f32,
    font_size: f32,
}

impl OrnamentPage {
    /// The screen rectangle of a box given as a flow range and a line range.
    fn rect(&self, flow: (f32, f32), line: (f32, f32)) -> D2D_RECT_F {
        let (left, top) = self.mode.to_screen(self.flow_origin + flow.0, line.0);
        let (right, bottom) = self.mode.to_screen(self.flow_origin + flow.1, line.1);
        D2D_RECT_F {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// The flow extent of the visual lines one whole-line mark covers, in the block
/// layout's own space.
///
/// **Matched by where each visual line starts, not by counting them.** How
/// many a line wrapped to is a property of the layout; the only thing that says
/// which logical line a visual one came from is where its text begins. `None`
/// for a mark the block measured nothing for.
///
/// An end that reaches as far as the block's own lines do is moved to the
/// block's **placed** edge. `place_blocks` rounds each block's extent on its
/// own, so the sum of a block's lines falls up to half a pixel short of it —
/// and consecutive blocks abut at the placed edge and nowhere else. A mark
/// that stopped where its lines stopped would leave that half pixel of paper
/// showing through the seam, or paint it twice.
fn mark_extent(block: &BlockPlacement, run: &LineRun) -> Option<(f32, f32)> {
    let end = run.utf16_start + run.utf16_len;
    let mut flow_start = f32::INFINITY;
    let mut flow_end = f32::NEG_INFINITY;
    // The block's own reach, to measure the mark's against. **Not its first and
    // last line**: reading order runs the other way along the flow axis in a
    // vertical pane, so line 0 sits at the far end there and a mark holding it
    // was snapped to the wrong edge — which stretched a code block's ground
    // over everything past it. Where a mark reaches is a coordinate, and a
    // coordinate does not care which way the reading goes.
    let mut nearest = f32::INFINITY;
    let mut furthest = f32::NEG_INFINITY;
    for line in block.lines.iter() {
        let line_end = line.flow_start + line.flow_size;
        nearest = nearest.min(line.flow_start);
        furthest = furthest.max(line_end);
        if line.utf16_start < run.utf16_start || line.utf16_start > end {
            continue;
        }
        flow_start = flow_start.min(line.flow_start);
        flow_end = flow_end.max(line_end);
    }
    if flow_start >= flow_end {
        return None;
    }
    if flow_start <= nearest {
        flow_start = block.content_flow_start;
    }
    if flow_end >= furthest {
        flow_end = block.content_flow_start + block.flow_size;
    }
    Some((flow_start, flow_end))
}

/// Draw what stands over each whole logical line of a block (要件 7.3.2).
///
/// **The line's own rectangle, not a range's.** A bar beside a quote runs the
/// height of everything that line wrapped to, a rule crosses the whole page,
/// and a code block's ground reaches over every line between its fences; none
/// of them can be asked of `HitTestTextRange`, which answers in characters.
/// What answers instead is the block's own line table, which the measurement
/// left behind — so this costs no DirectWrite call at all.
///
/// Before the text, so a mark never covers a glyph and the ground stays under
/// one.
fn draw_line_ornaments(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    block: &BlockPlacement,
    runs: &[LineRun],
    page: &OrnamentPage,
) {
    if runs.is_empty() {
        return;
    }
    let bar = (page.font_size * 0.14).round().max(2.0);
    let stroke = (page.font_size * 0.06).round().max(1.0);
    // The design's 4px against its 15px body, kept as the ratio so the ground
    // is rounded the same amount at any size the writer sets.
    let radius = (page.font_size * 0.27).round().max(2.0);
    // What a bar leaves between itself and the text it stands beside.
    let gap = page.indent / 3.0;
    // Where this block's text begins on the line axis, and where the page ends.
    let near = page.margin + page.inset;
    let far = (page.line_extent - page.margin).max(near + stroke);
    for run in runs {
        let Some(flow) = mark_extent(block, run) else {
            continue;
        };
        // A ground is a wash the text sits on; a bar and a stroke are marks
        // beside it. **The brush is the render target's own and the text is
        // drawn with it too**, so whatever is set here goes back below.
        let alpha = match run.ornament {
            LineOrnament::Code => CODE_GROUND_ALPHA,
            _ => ORNAMENT_ALPHA,
        };
        // SAFETY: The brush outlives every call here.
        unsafe { brush.SetOpacity(alpha) };
        match run.ornament {
            // In the gutter the block's own indent opened, one bar per level
            // of quoting. **Each stands at the far end of its own gutter**,
            // one gap clear of the text that level would have begun at — a bar
            // at the near end sits out by the margin with a whole indent of air
            // between it and the words it belongs to.
            LineOrnament::Quote { depth } => {
                for level in 0..depth.max(1) {
                    let text_at = page.margin + (f32::from(level) + 1.0) * page.indent;
                    let at = text_at - gap - bar;
                    let rect = page.rect(flow, (at, at + bar));
                    // SAFETY: The rectangle is read before the call returns,
                    // and the brush and the target both outlive it.
                    unsafe { target.FillRectangle(&rect, brush) };
                }
            }
            // Across the page, halfway along the room the line took.
            LineOrnament::Rule => {
                let half = stroke * 0.5;
                let middle = (flow.0 + flow.1) * 0.5;
                let rect = page.rect((middle - half, middle + half), (near, far));
                // SAFETY: As above.
                unsafe { target.FillRectangle(&rect, brush) };
            }
            // The ground the whole fenced block sits on, from where its text
            // begins to the far margin. **The fences at each end are hidden
            // rather than removed**, so the room they take is the padding.
            LineOrnament::Code => {
                let rect = page.rect(flow, (near, far));
                // **Rounded only where the fences are.** A ground cut in two by
                // a block boundary is square where the halves meet, and where
                // that boundary fell says nothing about the document — a ground
                // that merely ends at one is a whole code block and keeps its
                // corners (see `LineRun::own_ends`).
                if !(run.own_ends.0 && run.own_ends.1) {
                    // SAFETY: As above.
                    unsafe { target.FillRectangle(&rect, brush) };
                } else {
                    let ground = D2D1_ROUNDED_RECT {
                        rect,
                        radiusX: radius,
                        radiusY: radius,
                    };
                    // SAFETY: As above.
                    unsafe { target.FillRoundedRectangle(&ground, brush) };
                }
            }
        }
    }
    // SAFETY: As above. The text after this is drawn at full strength.
    unsafe { brush.SetOpacity(1.0) };
}

/// Set the per-range parts of the spec on one block's layout: the advance
/// between characters, and the size of every heading in the block.
///
/// The ranges come from [`style_runs`] and are relative to the block, so this
/// depends on nothing outside it. Character spacing is applied over the whole
/// block first and then again over each heading, because the spacing is a
/// fraction of the size of the character it follows, and a heading's characters
/// are larger.
fn apply_typography(
    layout: &IDWriteTextLayout,
    typography: &Typography,
    runs: &[StyleRun],
    utf16_len: u32,
) -> Result<()> {
    let spacing = typography.character_spacing;
    let has_spacing = spacing.abs() > f32::EPSILON;
    if !has_spacing && runs.is_empty() {
        return Ok(());
    }
    let layout1 = if has_spacing {
        Some(layout.cast::<IDWriteTextLayout1>()?)
    } else {
        None
    };
    // SAFETY: Every range below lies inside the layout's own text, and the
    // layout outlives the calls.
    unsafe {
        if let Some(layout1) = &layout1 {
            set_character_spacing(layout1, typography.font_size, spacing, 0, utf16_len)?;
        }
        for run in runs {
            // 要件 7.3.2: a box stands over this range and hides its glyphs, so
            // nothing about the font they would have been set in matters. The
            // box itself is set on the layout, not here.
            if run.ornament.is_some() {
                continue;
            }
            let range = DWRITE_TEXT_RANGE {
                startPosition: run.utf16_start,
                length: run.utf16_len,
            };
            let size = typography.font_size * typography.size_scale(run.heading_level);
            // Headings set at body size are the default state of the toolbar, so
            // this is the common case and it should cost nothing.
            if (size - typography.font_size).abs() > f32::EPSILON {
                layout.SetFontSize(size, range)?;
            }
            // 要件 7.3.2: what the markers said about this stretch. Each is set
            // only when it is set, so a document with no emphasis in it asks
            // DirectWrite for nothing it would not have asked anyway.
            //
            // **Overlapping runs are how nesting works.** A bold stretch and
            // the italic one inside it are two ranges, and the later call only
            // changes the attribute it names; the two do not have to be worked
            // out into one flat list of non-overlapping pieces.
            if run.marks.bold {
                layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, range)?;
            }
            if run.marks.italic {
                layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, range)?;
            }
            if run.marks.strike {
                layout.SetStrikethrough(true, range)?;
            }
            // 要件 9: a heading has a family of its own, and a code span has
            // another. **The code one is set last** so that a code span inside
            // a heading is still code — the later call is the one that stands.
            if run.heading_level > 0 {
                let family = HSTRING::from(typography.family_for(run.heading_level));
                layout.SetFontFamilyName(&family, range)?;
            }
            if run.marks.code && !typography.code_font.is_empty() {
                let family = HSTRING::from(typography.code_font.as_str());
                layout.SetFontFamilyName(&family, range)?;
            }
            if let Some(layout1) = &layout1 {
                set_character_spacing(layout1, size, spacing, run.utf16_start, run.utf16_len)?;
            }
        }
    }
    Ok(())
}

/// Split the extra advance evenly either side of the character, so a run stays
/// centred in the space it is given rather than drifting one way.
///
/// # Safety
///
/// The range must lie inside the layout's text.
unsafe fn set_character_spacing(
    layout: &IDWriteTextLayout1,
    size: f32,
    spacing: f32,
    start: u32,
    length: u32,
) -> Result<()> {
    let half = size * spacing * 0.5;
    let range = DWRITE_TEXT_RANGE {
        startPosition: start,
        length,
    };
    // SAFETY: The caller guarantees the range, and the minimum advance of zero
    // leaves DirectWrite's own advance as the floor.
    unsafe { layout.SetCharacterSpacing(half, half, 0.0, range) }
}

thread_local! {
    static GRAPHICS: RefCell<Option<Graphics>> = const { RefCell::new(None) };
}

/// One block to measure, and everything the measuring needs.
///
/// **Owned rather than borrowed**, because it may be measured on another
/// thread while the editor goes on holding the document. A block is measured
/// from its own text and its own styling and nothing else (3.4), which is why
/// this can be a self-contained parcel at all — and why the work divides.
///
/// **Cloned into the queue rather than moved**, so the caller still holds every
/// task after the threads have been asked. A thread that dies takes its answers
/// with it, and the only way to measure those blocks after all is to still have
/// what they were.
#[derive(Clone)]
struct MeasureTask {
    /// Which block of the update this is. The answers come back in whatever
    /// order the threads finish, so each has to say where it belongs.
    index: usize,
    text: String,
    runs: Vec<StyleRun>,
    /// Shared rather than cloned per block: one spec covers a whole update, and
    /// it carries a family name for the body, one for code and one per heading
    /// level.
    typography: Arc<Typography>,
    mode: WritingMode,
    block_box: f32,
    max_flow_size: f32,
    keep_trailing_empty_line: bool,
}

/// Measure one block, and hand back the layout it was measured with.
///
/// **The measurement and the layout are the same object's two answers.** The
/// caret hit test and the tile render both want that exact layout moments from
/// now, so the caller keeps it — except across a thread, where it cannot go
/// (a DirectWrite layout belongs to the thread that made it) and only the
/// measurement comes back.
fn measure_task(
    graphics: &mut Graphics,
    task: &MeasureTask,
) -> Result<(BlockMeasure, IDWriteTextLayout)> {
    let format = graphics.text_format(&task.typography, task.mode)?;
    let utf16 = task.text.encode_utf16().collect::<Vec<u16>>();
    // The layout box is the block's flow bound by its own line box, which way
    // round depending on the mode.
    let (max_width, max_height) = task.mode.to_screen(task.max_flow_size, task.block_box);
    // SAFETY: The UTF-16 buffer stays alive across CreateTextLayout, and the
    // layout owns everything it needs afterwards.
    let layout = unsafe {
        graphics
            .dwrite
            .CreateTextLayout(&utf16, &format, max_width, max_height)?
    };
    apply_typography(&layout, &task.typography, &task.runs, utf16.len() as u32)?;
    apply_marker_boxes(&layout, &task.typography, &task.runs)?;
    let measure = measure_block(
        &layout,
        task.max_flow_size,
        task.keep_trailing_empty_line,
        task.mode,
    )?;
    Ok((measure, layout))
}

/// The measurement in one answer, and nothing for an answer of another kind.
///
/// **A batch is all of one kind**, because only the editor's thread hands work
/// over and it waits for each batch before starting the next; this is the type
/// system being told so.
fn measured_answer(answer: PoolAnswer) -> Option<(usize, BlockMeasure)> {
    match answer {
        PoolAnswer::Measure(index, measure) => Some((index, measure)),
        PoolAnswer::Wrap(..) => None,
    }
}

/// And the wrap positions in one.
fn wrapped_answer(answer: PoolAnswer) -> Option<(usize, Vec<usize>)> {
    match answer {
        PoolAnswer::Wrap(at, found) => Some((at, found)),
        PoolAnswer::Measure(..) => None,
    }
}

/// Where one block's answer goes, for a block the cache had nothing for.
///
/// **Kept beside the tasks rather than in them.** A measuring thread is given
/// what it needs to measure and brings back a measurement and an index; which
/// cache entries that answer belongs in is the editor's business and never
/// leaves this thread.
struct PendingBlock {
    measure_key: u64,
    layout_key: u64,
    keep_trailing_empty_line: bool,
}

/// How many long paragraphs make it worth waking the other threads (要件 2).
///
/// **Two, because a long paragraph is never small.** The cheapest one here is
/// one that only just grew past a block, where a block is 768 cells; below that
/// the split never asks at all.
const PARALLEL_WRAP_MIN: usize = 2;

/// How many blocks make it worth waking the other threads (要件 2).
///
/// **A keystroke measures one block** (6.9), and handing one block to another
/// thread costs more than measuring it. What this is for is the other case: a
/// change of width or of spec throws every measurement away, and the whole
/// document is measured again before the next frame — 470 blocks at 110ms,
/// 657 at 350ms (技術検証 7.4), on every step of a divider drag.
const PARALLEL_MEASURE_MIN: usize = 48;

/// The most threads to measure on.
///
/// Beyond a handful the gain flattens and the cost does not: each thread keeps
/// a DirectWrite factory of its own, isolated from the others (7.3), with its
/// own font cache behind it.
const LAYOUT_THREADS_MAX: usize = 8;

/// How long to wait for a batch before giving up on the threads.
///
/// **Not a performance figure — a way for a bug to stay visible.** A worker
/// that panicked takes its answers with it, and the editor would otherwise
/// wait on them for ever. Long enough that no honest measurement reaches it.
const LAYOUT_WAIT: Duration = Duration::from_secs(5);

/// One long line to find the wrap positions of (要件 2, 技術検証 7.4).
///
/// **The reuse is already decided.** Which earlier wrapping this line can
/// resume from, and how much of it stands, is a comparison of text and marks
/// (6.10) — no graphics, so it is worked out where the cache is and what
/// crosses is only the byte to lay out from.
#[derive(Clone)]
struct WrapTask {
    /// Which question this answers, in the order the split asked them.
    at: usize,
    line: AskedLine,
    page: WrapPage,
    from: usize,
}

/// What one thread was given to do.
enum PoolTask {
    Measure(MeasureTask),
    Wrap(WrapTask),
}

/// And what it answered. **Which question, always** — the answers come back in
/// whatever order the threads finish.
enum PoolAnswer {
    Measure(usize, BlockMeasure),
    Wrap(usize, Vec<usize>),
}

/// The threads that lay text out, and the queues to them.
///
/// **One queue per thread rather than one shared queue**, because a shared
/// `Receiver` has to sit behind a lock, and a thread blocked on that lock while
/// holding it is every other thread's problem. Blocks are bounded in size
/// (`BLOCK_MAX_CELLS`), so dealing them round-robin divides the work evenly
/// enough without any of that.
///
/// **The same threads do both jobs.** They exist for what they keep — a
/// DirectWrite factory and the font cache behind it (7.3) — and that is the
/// same thing whichever question is being asked.
struct LayoutPool {
    queues: Vec<Sender<PoolTask>>,
    done: Receiver<std::result::Result<PoolAnswer, String>>,
}

impl LayoutPool {
    fn start() -> Option<Self> {
        let threads = thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(2)
            .clamp(1, LAYOUT_THREADS_MAX);
        let (done_sender, done) = channel();
        let mut queues = Vec::with_capacity(threads);
        for number in 0..threads {
            let (task_sender, tasks) = channel::<PoolTask>();
            let answers = done_sender.clone();
            let spawned = thread::Builder::new()
                .name(format!("rfnedit-layout-{number}"))
                .spawn(move || layout_worker(&tasks, &answers));
            if spawned.is_ok() {
                queues.push(task_sender);
            }
        }
        (!queues.is_empty()).then_some(Self { queues, done })
    }

    /// Do what these threads can, in whatever order they finish.
    ///
    /// **It may answer fewer questions than it was asked**, and says nothing
    /// about which: a queue refuses when its thread is gone, and a thread that
    /// stopped answering takes its share with it. The caller does whatever is
    /// still unanswered on its own thread, which is the same path it takes when
    /// there are no threads at all — so every way this can fall short ends in
    /// one piece of code.
    fn run(&self, tasks: Vec<PoolTask>) -> Result<Vec<PoolAnswer>> {
        let mut sent = 0usize;
        for (dealt, task) in tasks.into_iter().enumerate() {
            let queue = &self.queues[dealt % self.queues.len()];
            if queue.send(task).is_ok() {
                sent += 1;
            }
        }
        // **Every answer is taken before any failure is reported.** An answer
        // left in the queue would be picked up by the next update and counted
        // against a question it says nothing about.
        let mut done = Vec::with_capacity(sent);
        let mut failure: Option<String> = None;
        for _ in 0..sent {
            match self.done.recv_timeout(LAYOUT_WAIT) {
                Ok(Ok(answer)) => done.push(answer),
                Ok(Err(message)) => failure = failure.or(Some(message)),
                // Every thread is gone, or one of them stopped answering.
                // Either way nothing more is coming.
                Err(_) => break,
            }
        }
        match failure {
            Some(message) => Err(Error::new(E_FAIL, message)),
            None => Ok(done),
        }
    }
}

/// One laying-out thread.
///
/// It keeps its `Graphics` for as long as it lives, which is the whole reason
/// the threads are kept rather than made per update: an isolated DirectWrite
/// factory builds a font cache of its own, and paying for that on every
/// divider drag would cost more than the work it is meant to divide.
fn layout_worker(
    tasks: &Receiver<PoolTask>,
    answers: &Sender<std::result::Result<PoolAnswer, String>>,
) {
    while let Ok(task) = tasks.recv() {
        // The error is carried as text: a Windows error holds COM state of its
        // own and has no business crossing a thread.
        let answered = with_graphics(|graphics| match &task {
            PoolTask::Measure(task) => {
                let (measure, _layout) = measure_task(graphics, task)?;
                Ok(PoolAnswer::Measure(task.index, measure))
            }
            PoolTask::Wrap(task) => {
                let page = &task.page;
                let format = graphics.text_format(&page.typography, page.mode)?;
                let line = task.line.borrowed();
                let found = wrap_offsets(graphics, &format, page, line, task.from);
                Ok(PoolAnswer::Wrap(task.at, found.unwrap_or_default()))
            }
        })
        .map_err(|error| error.to_string());
        if answers.send(answered).is_err() {
            return;
        }
    }
}

/// Hand this work to the laying-out threads, if there are any.
///
/// `None` when there are none, which is not fatal: the caller does the work on
/// its own thread instead, which is what the editor did before this existed.
///
/// **The threads are the process's, not a pane's**, so they are started once
/// and both panes hand work to the same ones. The lock is what lets them live
/// in a `static` at all — a queue is `Send` but not something two threads may
/// hold at once — and it is never contended, because only the editor's thread
/// gets this far.
fn on_layout_threads(tasks: Vec<PoolTask>) -> Option<Result<Vec<PoolAnswer>>> {
    static POOL: OnceLock<Mutex<Option<LayoutPool>>> = OnceLock::new();
    let held = POOL.get_or_init(|| Mutex::new(LayoutPool::start()));
    let pool = held.lock().ok()?;
    Some(pool.as_ref()?.run(tasks))
}

fn with_graphics<T>(body: impl FnOnce(&mut Graphics) -> Result<T>) -> Result<T> {
    GRAPHICS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Graphics::new()?);
        }
        body(slot.as_mut().expect("graphics initialized above"))
    })
}

struct MeasuredBlock {
    text: String,
    keep_trailing_empty_line: bool,
    measure: BlockMeasure,
}

/// What one [`TextEngine::update`] had to re-measure.
///
/// The block count alone is misleading, and misleading in the direction that
/// matters: `blocks == 1` reads like the cheapest possible update, but a block
/// is whatever a paragraph makes it, so that one block may be the entire
/// document. The UTF-16 total is what the cost actually follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateCost {
    pub blocks: usize,
    pub utf16: u32,
    /// UTF-16 units actually laid out to find wrap positions, which is a whole
    /// layout each.
    ///
    /// Separate from `utf16` because it is a separate cost with a separate
    /// cause, and because the one time it was not reported it hid a 205ms floor
    /// under every keystroke in plain sight (6.10). An edit that does not touch
    /// a long paragraph must leave this at zero.
    pub wrapped: u32,
    /// How many of `blocks` were measured on the other threads (要件 2).
    ///
    /// **The one number that says whether the work divided.** A faster
    /// `layout` on its own does not: a cache that happened to hold more looks
    /// exactly the same from outside. Zero is the ordinary case — a keystroke
    /// measures one block and keeps it here.
    pub divided: u32,
    /// How the wrap positions of each long paragraph were arrived at: how many
    /// were asked for, how many came back unchanged, and how many carried on
    /// from a line start the edit did not reach.
    ///
    /// `asked - exact - resumed` is the number laid out from their first
    /// character. `wrapped` says what that cost; these say why it was paid.
    pub wrap_asked: u32,
    pub wrap_exact: u32,
    pub wrap_resumed: u32,
    /// How many of them were found on the other threads (要件 2).
    ///
    /// The same number for the wrap search that `divided` is for the measuring,
    /// and it is a separate one because the two divide differently: **a long
    /// paragraph cannot be cut in half and shared**, so this is bounded by the
    /// longest paragraph in the document however many threads there are.
    pub wrap_divided: u32,
    /// For the paragraph that was laid out from its first character: how many
    /// bytes it still shared with its previous self, and how many line starts
    /// that previous self had.
    ///
    /// Between them these say why the reuse was refused. A small `shared` means
    /// the edit really was near the start of the paragraph and there was
    /// nothing to keep. A large `shared` with few `starts` means the previous
    /// wrapping was not there to be reused.
    pub wrap_shared: usize,
    pub wrap_starts: usize,
}

/// The layout of one document in one writing mode, split into independently
/// laid out blocks.
#[derive(Default)]
pub struct TextEngine {
    mode: WritingMode,
    text: String,
    /// Heading level per logical line of `text`. Blocks cut only at logical line
    /// boundaries, so `block_lines` slices this without ever cutting an entry.
    line_styles: Vec<LineStyle>,
    /// What is marked inside each logical line (要件 7.3.2), sliced by
    /// `block_lines` exactly as `line_styles` is.
    line_spans: Vec<Vec<Emphasis>>,
    /// The marker standing at the head of each logical line (要件 7.3.2). Held
    /// beside the spans for the same reason they are, and compared for the same
    /// reason: **a box changes where a line's text begins**, so an engine whose
    /// markers differ is not describing this document.
    line_markers: Vec<Option<LineMarker>>,
    /// The logical lines each block covers, as an index into `line_styles`.
    /// One entry per block, in reading order.
    block_lines: Vec<Range<usize>>,
    /// The pane's extent along the line axis: its height in vertical writing,
    /// its width in horizontal writing. Every measurement depends on it.
    line_extent: u32,
    typography: Typography,
    margin: f32,
    plan: BlockLayoutPlan,
    /// Measurements keyed by block text. Unchanged blocks survive every edit.
    measures: HashMap<u64, MeasuredBlock>,
    /// Live layouts, most recently used first.
    layouts: Vec<(u64, IDWriteTextLayout)>,
    /// Where each long paragraph wraps, from the last update. **Without this,
    /// every long paragraph was laid out on every keystroke** (6.10).
    wraps: Vec<ParagraphWraps>,
    /// How many list items wrap at this geometry. See [`Self::list_items`].
    wrapping_items: usize,
}

/// Everything about the spec that changes a layout, hashed.
///
/// Only the heading sizes a block actually uses go in, via [`hash_style_runs`];
/// hashing all six here would rebuild every layout in the document when a level
/// nobody used changed size.
fn hash_typography(typography: &Typography, hasher: &mut DefaultHasher) {
    typography.font_size.to_bits().hash(hasher);
    typography.character_spacing.to_bits().hash(hasher);
    typography.line_spacing.to_bits().hash(hasher);
    // 要件 9: **a family changes every measurement**, so unlike the colours it
    // belongs here rather than only in the tile's signature. Two specs that
    // differ by a font are not the same layout and never were.
    typography.body_font.hash(hasher);
    typography.heading_font.hash(hasher);
    typography.code_font.hash(hasher);
}

/// The colours a tile is drawn in (要件 9).
///
/// **Only the tiles are keyed by these**, never the layouts or the measures: a
/// colour is the one setting that changes no geometry, so the layout that was
/// measured is still the right one and only its pixels are stale. This was
/// missed once and the symptom was a setting that appeared to do nothing —
/// every tile was already in the cache under a key that said nothing about
/// colour, so nothing was ever drawn again.
fn hash_colours(typography: &Typography, hasher: &mut DefaultHasher) {
    for channel in typography.ink.iter().chain(typography.paper.iter()) {
        channel.to_bits().hash(hasher);
    }
    for level in &typography.heading_ink {
        for channel in level {
            channel.to_bits().hash(hasher);
        }
    }
}

/// The block-local ranges and the size each is set at.
///
/// The size, not the level: two specs that give a level the same size produce
/// the same pixels and should share the cached layout.
fn hash_style_runs(runs: &[StyleRun], typography: &Typography, hasher: &mut DefaultHasher) {
    for run in runs {
        run.utf16_start.hash(hasher);
        run.utf16_len.hash(hasher);
        let size = typography.font_size * typography.size_scale(run.heading_level);
        size.to_bits().hash(hasher);
        // 要件 7.3.2: two blocks whose text is the same but whose markers said
        // different things are not the same layout, and must not share one.
        run.marks.hash(hasher);
        // And the same for a box standing over the head of a line: it hides the
        // glyphs it covers and moves everything after it.
        run.ornament.hash(hasher);
    }
}

/// The block-local whole-line ornaments.
///
/// **Only the tiles are keyed by these**, for the reason [`hash_colours`] gives:
/// a bar and a stroke change no geometry, so the layout that was measured is
/// still the right one and only its pixels are stale.
fn hash_line_runs(runs: &[LineRun], hasher: &mut DefaultHasher) {
    runs.hash(hasher);
}

/// Identifies a layout object. Two blocks with the same text, set the same way,
/// share one whatever their position in the document.
///
/// The mode is not part of the key: these caches belong to one engine, and an
/// engine keeps the mode it was built with.
/// **The block's own line box, not the pane's extent.** Two blocks of the same
/// text set at different indents are different layouts (要件 7.3.2), and the
/// box is the one number that says so.
fn layout_key(text: &str, runs: &[StyleRun], typography: &Typography, line_box: f32) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hash_typography(typography, &mut hasher);
    hash_style_runs(runs, typography, &mut hasher);
    line_box.to_bits().hash(&mut hasher);
    hasher.finish()
}

/// Identifies a measurement. Same layout, but the last block of the document
/// keeps a trailing empty line the others give up, so it measures differently.
fn measure_key(
    text: &str,
    runs: &[StyleRun],
    typography: &Typography,
    line_box: f32,
    keep_trailing_empty_line: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    layout_key(text, runs, typography, line_box).hash(&mut hasher);
    keep_trailing_empty_line.hash(&mut hasher);
    hasher.finish()
}

/// Rounded, because it is the document's starting edge and every block sits a
/// whole number of pixels from it. See `place_blocks`.
fn margin_for(font_size: f32) -> f32 {
    (font_size * 1.5).max(16.0).round()
}

/// How far one block's text is set in from the page margin (要件 7.3.2).
///
/// **A property of the block, not of the pane.** An indent has to move every
/// visual line a quoted paragraph or a wrapped list item ran to, and the only
/// thing that can is the layout box the block is set in: DirectWrite has no
/// per-paragraph indent, and a box at the head of a line reaches that head and
/// no further (技術検証 7.1). That is why a change of indenting ends a block.
///
/// Every place that turns a line coordinate into a screen one goes through
/// this — drawing, the caret, the hit test, the selection rectangles and the
/// scroll that follows the caret. A place that forgot it would put the caret
/// beside the text it is in.
fn block_inset(span: &BlockSpan, typography: &Typography) -> f32 {
    indent_of(span.indent_steps, typography)
}

/// What a count of indent steps is worth on the line axis.
fn indent_of(steps: u8, typography: &Typography) -> f32 {
    f32::from(steps) * typography.indent_step()
}

/// The pane's line extent, less what one block's own indent takes.
///
/// Only ever used to estimate how far a block reaches along the flow axis,
/// where being on the low side is the safe direction: fewer cells to a line
/// means more lines reserved, and a reservation that is short is what makes
/// DirectWrite drop the end of a block.
fn block_extent(span: &BlockSpan, line_extent: u32, typography: &Typography) -> u32 {
    let inset = block_inset(span, typography) as u32;
    line_extent.saturating_sub(inset).max(1)
}

impl TextEngine {
    /// An engine for one writing mode, which it keeps for its lifetime.
    /// `Default` gives the vertical one.
    pub fn new(mode: WritingMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub fn total_flow_size(&self) -> u32 {
        self.plan.total_flow_size.ceil().max(1.0) as u32
    }

    /// The pane's extent along the line axis: pane height in vertical writing,
    /// pane width in horizontal writing.
    pub fn line_extent(&self) -> u32 {
        self.line_extent.max(1)
    }

    pub fn utf16_len(&self) -> u32 {
        self.plan.utf16_len()
    }

    pub fn block_count(&self) -> usize {
        self.plan.blocks.len()
    }

    /// The largest block in the document, in UTF-16 units.
    ///
    /// A block is the unit of re-measurement, so this is the worst a keystroke
    /// can cost — and it is the one figure that says whether a document holds a
    /// paragraph the split cannot cut. Blocks are bounded in line space
    /// (`BLOCK_MAX_CELLS`) only where there is a logical line boundary to cut
    /// at; one paragraph with no `\n` in it is one block however long it is.
    pub fn largest_block_utf16(&self) -> u32 {
        self.plan
            .blocks
            .iter()
            .map(|block| block.span.utf16_len())
            .max()
            .unwrap_or(0)
    }

    /// How far a tile reaches along the flow axis, keeping the pixels per tile
    /// roughly constant as the window grows or shrinks.
    pub fn tile_flow_size(&self) -> u32 {
        (TILE_TARGET_PIXELS / self.line_extent()).clamp(MIN_TILE_FLOW_SIZE, MAX_TILE_FLOW_SIZE)
    }

    /// True when the engine already describes exactly this text and geometry.
    pub fn matches(
        &self,
        styled: StyledText<'_>,
        line_extent: u32,
        typography: &Typography,
    ) -> bool {
        self.line_extent == line_extent
            && self.typography == *typography
            && self.text == styled.text
            && self.line_styles == styled.lines
            && self.line_spans == styled.spans
            && self.line_markers == styled.markers
    }

    /// Re-split and re-measure the document, reusing every block whose text and
    /// styling did not change. Returns what had to be measured.
    pub fn update(
        &mut self,
        styled: StyledText<'_>,
        line_extent: u32,
        typography: &Typography,
    ) -> Result<UpdateCost> {
        let line_extent = line_extent.max(1);
        let typography = Typography {
            font_size: typography.font_size.max(1.0),
            ..typography.clone()
        };
        if self.matches(styled, line_extent, &typography) {
            return Ok(UpdateCost::default());
        }
        if self.line_extent != line_extent || self.typography != typography {
            // Both feed into every measurement, so nothing cached survives. The
            // wrap positions go too: they are keyed by the geometry, so the old
            // entries would simply never be hit again.
            self.measures.clear();
            self.layouts.clear();
            self.wraps.clear();
        }

        let text = styled.text;
        let mode = self.mode;
        let margin = margin_for(typography.font_size);
        let line_box = (line_extent as f32 - margin * 2.0).max(1.0);
        // The split is charged in line space, so it needs the geometry: the same
        // pane at a different line extent wraps differently and cuts elsewhere.
        let cells = cells_per_line(line_extent, &typography);
        // The same spec, in a form a task can carry. One spec covers the whole
        // update and holds a family name for the body, one for code and one per
        // heading level, so it is shared rather than cloned per task.
        let spec = Arc::new(typography.clone());
        // **Ask first, answer afterwards** (要件 2). A logical line longer than
        // one block is cut at the positions DirectWrite wraps it, and finding
        // those is the one piece of laying out the split itself does. So the
        // split is run once with nothing to answer it, purely to find out which
        // lines it needs — **asking is the only way to know without keeping a
        // second copy of the rule about which lines are too long**, and a second
        // copy is a second opinion about where blocks end.
        //
        // Ordinary documents ask nothing, and then this pass is the split: its
        // blocks are already the right ones, and the second one never runs.
        let mut asking = RecordedWraps::default();
        let spans = split_blocks(styled, cells, &typography, &mut asking);
        let page = WrapPage {
            typography: spec.clone(),
            mode,
            line_extent,
            line_box,
        };
        let answered = self.wrap_answers(&asking.asked, &page, styled, cells, &typography)?;
        let (spans, fresh_wraps, wrap_cost) = match answered {
            Some(done) => done,
            None => (spans, Vec::new(), UpdateCost::default()),
        };
        self.wraps = fresh_wraps;
        // **One slot per block, filled in whatever order the answers arrive.**
        // A block measured on another thread comes back when it comes back, so
        // the order of the document is kept here rather than in the measuring.
        let mut measures: Vec<Option<BlockMeasure>> = vec![None; spans.len()];
        let mut block_lines = Vec::with_capacity(spans.len());
        let mut live_measure_keys = HashSet::with_capacity(spans.len());
        let mut live_layout_keys = HashSet::with_capacity(spans.len());
        let mut fresh_measures = Vec::new();
        let mut fresh_layouts = Vec::new();
        let mut measured = wrap_cost;
        // The blocks the cache had nothing for, and where each answer belongs.
        let mut tasks: Vec<MeasureTask> = Vec::new();
        let mut pending: HashMap<usize, PendingBlock> = HashMap::new();

        // **Deciding what to measure needs no graphics at all.** Which blocks
        // the cache already answers, what ranges each one sets, how wide its box
        // is — all of it is text and arithmetic, and separating it from the
        // measuring is what lets the measuring go somewhere else.
        {
            let last_index = spans.len().saturating_sub(1);
            // Which logical line each block starts on. Counted forwards rather
            // than looked up, so it costs one pass over the text and not one
            // scan per block.
            let mut line_cursor = 0;
            for (index, span) in spans.iter().enumerate() {
                let block_text = &text[span.byte_start..span.byte_end];
                // A block ends just after a newline, so it holds exactly as many
                // logical lines as it has newlines — except the last block of a
                // document that does not end in one, which holds one more.
                let breaks = block_text.matches('\n').count();
                let lines = if block_text.ends_with('\n') {
                    breaks
                } else {
                    breaks + 1
                };
                // Advanced by the newlines, not by the lines covered. A piece
                // cut out of the middle of a long line covers that line without
                // finishing it, so the next piece is still on the same one.
                let advance = breaks;
                let block_levels = styled.lines.get(line_cursor..).unwrap_or(&[]);
                let block_levels = &block_levels[..lines.min(block_levels.len())];
                let block_spans = styled.spans.get(line_cursor..).unwrap_or(&[]);
                let block_spans = &block_spans[..lines.min(block_spans.len())];
                let block_markers = styled.markers.get(line_cursor..).unwrap_or(&[]);
                let block_markers = &block_markers[..lines.min(block_markers.len())];
                let block_styled = StyledText::marked(block_text, block_levels, block_spans);
                // **The boxes have to be here too.** This is the layout the
                // block is measured with, and it is kept as the layout it is
                // drawn with; measuring without them would place every block
                // after a list at a size that is not the one on screen.
                let block_styled = block_styled.with_markers(block_markers);
                block_lines.push(line_cursor..line_cursor + lines);
                line_cursor += advance;

                let runs = style_runs(block_styled);
                let keep_trailing_empty_line = index == last_index;
                // 要件 7.3.2: this block's own box, narrowed by its indent.
                // **The measurement has to be taken in it**, or the block is
                // placed at a size it is not drawn at.
                let block_box = (line_box - block_inset(span, &typography)).max(1.0);
                let key = measure_key(
                    block_text,
                    &runs,
                    &typography,
                    block_box,
                    keep_trailing_empty_line,
                );
                let block_layout = layout_key(block_text, &runs, &typography, block_box);
                live_measure_keys.insert(key);
                live_layout_keys.insert(block_layout);
                if let Some(cached) = self.measures.get(&key)
                    && cached.text == block_text
                    && cached.keep_trailing_empty_line == keep_trailing_empty_line
                {
                    // Cheap now that the line table is shared, and the entry
                    // itself stays put rather than being copied into a new map.
                    measures[index] = Some(cached.measure.clone());
                    continue;
                }

                let extent = block_extent(span, line_extent, &typography);
                let max_flow_size = block_flow_bound(block_styled, extent, &typography);
                measured.blocks += 1;
                measured.utf16 += span.utf16_len();
                pending.insert(
                    index,
                    PendingBlock {
                        measure_key: key,
                        layout_key: block_layout,
                        keep_trailing_empty_line,
                    },
                );
                tasks.push(MeasureTask {
                    index,
                    text: block_text.to_owned(),
                    runs,
                    typography: spec.clone(),
                    mode,
                    block_box,
                    max_flow_size,
                    keep_trailing_empty_line,
                });
            }
        }

        // **On the threads only when there is enough to divide** (要件 2). A
        // keystroke leaves one block to measure, and handing one block over
        // costs more than measuring it; a change of width leaves the whole
        // document, and that is what this is for.
        //
        // The threads bring back measurements and no layouts — a DirectWrite
        // layout belongs to the thread that made it. What that costs is the few
        // blocks on screen, whose layouts `layout_for` builds again when the
        // tiles are drawn; measuring them all again is what it saves.
        let divide = tasks.len() >= PARALLEL_MEASURE_MIN;
        let handed = divide.then(|| {
            let queued = tasks.iter().cloned().map(PoolTask::Measure).collect();
            on_layout_threads(queued)
        });
        if let Some(answered) = handed.flatten() {
            let answered = answered?;
            measured.divided = answered.len() as u32;
            for (index, measure) in answered.into_iter().filter_map(measured_answer) {
                if let Some(slot) = pending.get(&index) {
                    let span = &spans[index];
                    fresh_measures.push((
                        slot.measure_key,
                        MeasuredBlock {
                            text: text[span.byte_start..span.byte_end].to_owned(),
                            keep_trailing_empty_line: slot.keep_trailing_empty_line,
                            measure: measure.clone(),
                        },
                    ));
                }
                measures[index] = Some(measure);
            }
        }

        // **Whatever is left, which is all of it when there are no threads.**
        // A block the threads did not answer for is not a special case here: it
        // is a block that still has no measurement, and this is where a block
        // without one gets measured.
        let left = tasks
            .iter()
            .filter(|task| measures[task.index].is_none())
            .collect::<Vec<&MeasureTask>>();
        if !left.is_empty() {
            with_graphics(|graphics| {
                for task in left {
                    let (measure, layout) = measure_task(graphics, task)?;
                    if let Some(slot) = pending.get(&task.index) {
                        fresh_measures.push((
                            slot.measure_key,
                            MeasuredBlock {
                                text: task.text.clone(),
                                keep_trailing_empty_line: slot.keep_trailing_empty_line,
                                measure: measure.clone(),
                            },
                        ));
                        // Keep the layout that was just built. The caret hit
                        // test and the tile render both want this exact block
                        // moments from now, and building it again is one of the
                        // more expensive things here. **Only here** — a layout
                        // made on another thread belongs to that thread, so a
                        // block measured there is laid out again when it is
                        // drawn (`layout_for`).
                        fresh_layouts.push((slot.layout_key, layout));
                    }
                    measures[task.index] = Some(measure);
                }
                Ok(())
            })?;
        }
        let measures = measures
            .into_iter()
            .map(|measure| measure.expect("every block is measured or cached"))
            .collect::<Vec<BlockMeasure>>();

        self.measures
            .retain(|key, _| live_measure_keys.contains(key));
        self.measures.extend(fresh_measures);
        // Keyed by `layout_key`, so this must be checked against layout keys.
        // Checking it against the measure keys silently emptied the cache on
        // every update, and every caret move then rebuilt its block's layout.
        self.layouts
            .retain(|(key, _)| live_layout_keys.contains(key));
        for (key, layout) in fresh_layouts {
            if !self.layouts.iter().any(|(cached, _)| *cached == key) {
                self.layouts.insert(0, (key, layout));
            }
        }
        self.layouts.truncate(LAYOUT_CACHE_LIMIT);
        self.plan = place_blocks(&spans, &measures, margin, mode.flow_order());
        self.text = text.to_owned();
        self.wrapping_items = wrapping_list_lines(styled, cells, &typography);
        self.line_styles = styled.lines.to_vec();
        self.line_spans = styled.spans.to_vec();
        self.line_markers = styled.markers.to_vec();
        self.block_lines = block_lines;
        self.line_extent = line_extent;
        self.typography = typography;
        self.margin = margin;
        Ok(measured)
    }

    /// Answer every question the recording split asked, and split again
    /// (要件 2, 技術検証 7.4).
    ///
    /// `None` when it asked nothing, which is what an ordinary document does:
    /// the caller keeps the blocks the recording pass already made.
    ///
    /// **Three steps that each need something the others do not.** Deciding
    /// what an earlier update leaves usable is a comparison of text and marks
    /// and has to see the cache; finding where the text wraps is DirectWrite
    /// and can be done anywhere; cutting the blocks is arithmetic. Only the
    /// middle one is worth dividing, and separating them is what lets it be.
    fn wrap_answers(
        &self,
        asked: &[AskedLine],
        page: &WrapPage,
        styled: StyledText<'_>,
        cells: u32,
        typography: &Typography,
    ) -> Result<Option<(Vec<BlockSpan>, Vec<ParagraphWraps>, UpdateCost)>> {
        if asked.is_empty() {
            return Ok(None);
        }
        let mut cost = UpdateCost {
            wrap_asked: asked.len() as u32,
            ..UpdateCost::default()
        };
        let mut answers: Vec<Vec<usize>> = vec![Vec::new(); asked.len()];
        let mut kept: Vec<Vec<usize>> = vec![Vec::new(); asked.len()];
        let mut tasks: Vec<WrapTask> = Vec::new();
        for (at, line) in asked.iter().enumerate() {
            let reuse = wrap_reuse(&self.wraps, line.borrowed());
            if reuse.shared >= cost.wrap_shared {
                cost.wrap_shared = reuse.shared;
                cost.wrap_starts = reuse.starts;
            }
            if reuse.whole {
                cost.wrap_exact += 1;
                answers[at] = reuse.kept;
                continue;
            }
            if reuse.from > 0 {
                cost.wrap_resumed += 1;
            }
            cost.wrapped += line.text[reuse.from..].encode_utf16().count() as u32;
            kept[at] = reuse.kept;
            tasks.push(WrapTask {
                at,
                line: line.clone(),
                page: page.clone(),
                from: reuse.from,
            });
        }

        // **Two paragraphs are already worth dividing.** Unlike a block, a long
        // paragraph is never small: the cheapest one here is the one that only
        // just grew past a block.
        let divide = tasks.len() >= PARALLEL_WRAP_MIN;
        let handed = divide.then(|| {
            let queued = tasks.iter().cloned().map(PoolTask::Wrap).collect();
            on_layout_threads(queued)
        });
        let mut done = vec![false; asked.len()];
        if let Some(found) = handed.flatten() {
            for (at, offsets) in found?.into_iter().filter_map(wrapped_answer) {
                answers[at] = std::mem::take(&mut kept[at]);
                answers[at].extend(offsets);
                done[at] = true;
                cost.wrap_divided += 1;
            }
        }

        // Whatever is left, which is all of it when there are no threads.
        let left = tasks
            .iter()
            .filter(|task| !done[task.at])
            .collect::<Vec<&WrapTask>>();
        if !left.is_empty() {
            with_graphics(|graphics| {
                let format = graphics.text_format(typography, page.mode)?;
                for task in left {
                    let line = task.line.borrowed();
                    let found = wrap_offsets(graphics, &format, page, line, task.from);
                    answers[task.at] = std::mem::take(&mut kept[task.at]);
                    answers[task.at].extend(found.unwrap_or_default());
                }
                Ok(())
            })?;
        }

        // What was asked for this time, which becomes the cache for the next
        // update. Built fresh so a paragraph that no longer exists is dropped
        // without having to be found.
        let current = asked
            .iter()
            .zip(&answers)
            .map(|(line, starts)| ParagraphWraps {
                text: line.text.clone(),
                style: line.style,
                indent_steps: line.indent_steps,
                marks: line.marks.clone(),
                marker: line.marker,
                starts: starts.clone(),
            })
            .collect();
        let mut prepared = PreparedWraps::new(answers);
        let spans = split_blocks(styled, cells, typography, &mut prepared);
        Ok(Some((spans, current, cost)))
    }

    /// How many of the document's logical lines begin with a list marker
    /// (要件 7.3.2).
    ///
    /// **Logged rather than used.** Giving a list item the indent a quote has
    /// means ending a block at every item, and what that costs is a number
    /// about real documents rather than an argument (技術検証 7.1).
    pub fn list_items(&self) -> usize {
        self.line_styles
            .iter()
            .filter(|style| style.kind.is_list())
            .count()
    }

    /// How many of those items take more than one line (要件 7.3.2).
    ///
    /// **The number that settled how the indent is paid for.** An item that
    /// fits on one line has no continuation to align, so "cut only the items
    /// that wrap" looked like the cheap way to give the rest one. Measured on
    /// `testdata/10_箇条書きの計測.md` it was not: 503 of 1749 items wrapped in
    /// a wide pane but 1245 did in a narrow one, which is no saving at all —
    /// and cutting by it would have made block boundaries move with the pane
    /// width. A run of items is one block instead (技術検証 7.1).
    ///
    /// Kept because it says how much of a document the indent is doing work
    /// for. Depends on the geometry, which is why it is worked out where the
    /// split is rather than counted from the styles on the way past.
    pub fn wrapping_items(&self) -> usize {
        self.wrapping_items
    }

    /// How one block's own logical lines are set.
    fn block_levels(&self, block_index: usize) -> &[LineStyle] {
        let Some(lines) = self.block_lines.get(block_index) else {
            return &[];
        };
        let start = lines.start.min(self.line_styles.len());
        let end = lines.end.min(self.line_styles.len());
        &self.line_styles[start..end]
    }

    /// What one block's own logical lines have marked inside them.
    fn block_spans(&self, block_index: usize) -> &[Vec<Emphasis>] {
        let Some(lines) = self.block_lines.get(block_index) else {
            return &[];
        };
        let start = lines.start.min(self.line_spans.len());
        let end = lines.end.min(self.line_spans.len());
        &self.line_spans[start..end]
    }

    /// The markers standing at the head of one block's own logical lines.
    fn block_markers(&self, block_index: usize) -> &[Option<LineMarker>] {
        let Some(lines) = self.block_lines.get(block_index) else {
            return &[];
        };
        let start = lines.start.min(self.line_markers.len());
        let end = lines.end.min(self.line_markers.len());
        &self.line_markers[start..end]
    }

    /// One block's text and its own styling, which together decide everything
    /// about its layout and nothing outside it.
    fn block_styled(&self, block_index: usize) -> StyledText<'_> {
        let Some(block) = self.plan.blocks.get(block_index) else {
            return StyledText::plain("");
        };
        let text = &self.text[block.span.byte_start..block.span.byte_end];
        let levels = self.block_levels(block_index);
        let marked = StyledText::marked(text, levels, self.block_spans(block_index));
        marked.with_markers(self.block_markers(block_index))
    }

    fn layout_for(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
    ) -> Result<IDWriteTextLayout> {
        let (byte_start, byte_end, max_flow_size, inset) = {
            let block = &self.plan.blocks[block_index];
            (
                block.span.byte_start,
                block.span.byte_end,
                block.max_flow_size,
                block_inset(&block.span, &self.typography),
            )
        };
        let runs = style_runs(self.block_styled(block_index));
        let block_text = &self.text[byte_start..byte_end];
        let line_box = (self.line_extent as f32 - self.margin * 2.0 - inset).max(1.0);
        let key = layout_key(block_text, &runs, &self.typography, line_box);
        if let Some(position) = self.layouts.iter().position(|(cached, _)| *cached == key) {
            let entry = self.layouts.remove(position);
            let layout = entry.1.clone();
            self.layouts.insert(0, entry);
            return Ok(layout);
        }

        let format = graphics.text_format(&self.typography, self.mode)?;
        let (max_width, max_height) = self.mode.to_screen(max_flow_size, line_box);
        let utf16 = self.text[byte_start..byte_end]
            .encode_utf16()
            .collect::<Vec<u16>>();
        // SAFETY: The UTF-16 buffer stays alive across CreateTextLayout.
        let layout = unsafe {
            graphics
                .dwrite
                .CreateTextLayout(&utf16, &format, max_width, max_height)?
        };
        // The same spec the measurement was taken under. A layout rebuilt
        // without it would draw and hit test at a different size from the one
        // the block was placed at.
        apply_typography(&layout, &self.typography, &runs, utf16.len() as u32)?;
        apply_marker_boxes(&layout, &self.typography, &runs)?;
        self.layouts.insert(0, (key, layout.clone()));
        self.layouts.truncate(LAYOUT_CACHE_LIMIT);
        Ok(layout)
    }

    /// The tiles the viewport needs, cut out of the blocks it crosses.
    pub fn visible_tiles(
        &self,
        viewport_flow: f32,
        visible_flow: f32,
        prefetch: u32,
    ) -> Vec<TileSpan> {
        self.plan
            .visible_tiles(viewport_flow, visible_flow, self.tile_flow_size(), prefetch)
    }

    /// Render the requested tiles. Each tile shows one block and nothing else.
    ///
    /// `emit` receives the tile, the pixel size of its image, and its BGRA rows.
    /// The image is the tile's flow extent by the pane's line extent, so which of
    /// the two is the width depends on the writing mode.
    pub fn render_tiles(
        &mut self,
        tiles: &[TileSpan],
        preedit_utf16_range: Option<(u32, u32)>,
        mut emit: impl FnMut(TileSpan, u32, u32, &[u8]),
    ) -> Result<()> {
        let mode = self.mode;
        // 要件 9: this sheet's paper. The window paints the page behind the
        // tiles from the same setting, so the two cannot show a seam.
        let paper = colour(self.typography.paper);
        let ink = colour(self.typography.ink);
        let line_extent = self.line_extent();
        let margin = self.margin;
        // One surface for every tile, at the furthest a tile can reach.
        //
        // Tiles are slices of blocks, and blocks are all different sizes, so
        // sizing the surface to the tile would rebuild the WIC bitmap and its
        // render target for almost every tile. That call is the single most
        // expensive thing here. A fixed surface is built once per pane extent;
        // a shorter tile simply leaves the far end of it unread.
        let surface_size = self.tile_flow_size();
        let (surface_width, surface_height) = mode.to_surface(surface_size, line_extent);
        // Read out here: the filter must not hold a borrow of `self` across the
        // loop, because drawing needs `self` mutably to fetch block layouts.
        let block_count = self.plan.blocks.len();

        with_graphics(|graphics| {
            for span in tiles.iter().filter(|span| {
                span.flow_size > 0
                    && span.flow_size <= surface_size
                    && span.block_index < block_count
            }) {
                let tile_start = span.flow_start;
                let tile_size = span.flow_size;

                // Scoped so the cache borrow ends before `layout_for` needs
                // `graphics` mutably again.
                let (target, brush, heading_brushes, bitmap) = {
                    let cache = graphics.render_target(surface_width, surface_height)?;
                    (
                        cache.target.clone(),
                        cache.brush.clone(),
                        cache.heading_brushes.clone(),
                        cache.bitmap.clone(),
                    )
                };

                // SAFETY: The target, brush and bitmap are kept alive by the
                // cache for the whole draw, and BeginDraw/EndDraw are paired.
                unsafe {
                    target.BeginDraw();
                    target.Clear(Some(&paper));
                    // The brushes the target keeps, told what the inks are now.
                    // Cheaper than building them per tile, and the settings may
                    // have moved since the target was made (要件 9).
                    brush.SetColor(&ink);
                    for (level, heading_brush) in heading_brushes.iter().enumerate() {
                        let heading_ink = colour(self.typography.ink_for(level as u8 + 1));
                        heading_brush.SetColor(&heading_ink);
                    }
                }
                let layout = self.layout_for(graphics, span.block_index)?;
                let block = &self.plan.blocks[span.block_index];
                let underline = preedit_utf16_range.and_then(|(start, length)| {
                    block_local_range(
                        block.span.utf16_start,
                        block.span.utf16_end,
                        start,
                        start + length,
                    )
                });
                // 要件 9: a heading is drawn in its own ink. **Set on the
                // layout before every draw rather than once when it is built**
                // — the layout is cached and a colour changes no geometry, so
                // the cached one is still the right layout; and the brush it
                // was given last time belongs to a render target that may since
                // have been rebuilt. Body runs are left alone: they are drawn
                // with the brush handed to `DrawTextLayout`.
                let runs = style_runs(self.block_styled(span.block_index));
                // SAFETY: the layout and the brushes both outlive the draw.
                unsafe {
                    for run in &runs {
                        if run.heading_level == 0 {
                            continue;
                        }
                        let Some(heading_brush) =
                            heading_brushes.get(run.heading_level as usize - 1)
                        else {
                            continue;
                        };
                        layout.SetDrawingEffect(
                            heading_brush,
                            DWRITE_TEXT_RANGE {
                                startPosition: run.utf16_start,
                                length: run.utf16_len,
                            },
                        )?;
                    }
                }
                // The block is drawn at its own offset inside the tile, and the
                // margin plus the block's own indent sit on the line axis. All
                // of it swaps with the mode.
                let block_origin = block.draw_origin() - tile_start as f32;
                let inset = block_inset(&block.span, &self.typography);
                let (origin_x, origin_y) = mode.to_screen(block_origin, margin + inset);
                let origin = windows_numerics::Vector2 {
                    X: origin_x,
                    Y: origin_y,
                };
                // 要件 7.3.2: the marks that belong to whole lines — the bar
                // beside a quote, the stroke across a rule. Drawn from the
                // block's own line table rather than from a hit test, and
                // before the text, so neither ever covers a glyph.
                let page = OrnamentPage {
                    mode,
                    flow_origin: block_origin,
                    margin,
                    inset,
                    indent: self.typography.indent_step(),
                    line_extent: line_extent as f32,
                    font_size: self.typography.font_size,
                };
                let line_marks = line_runs(self.block_styled(span.block_index));
                draw_line_ornaments(&target, &brush, block, &line_marks, &page);
                // SAFETY: The layout outlives the draw call, and the underline
                // is set and cleared on the same layout.
                unsafe {
                    if let Some((start, length)) = underline {
                        layout.SetUnderline(
                            true,
                            DWRITE_TEXT_RANGE {
                                startPosition: start,
                                length,
                            },
                        )?;
                    }
                    target.DrawTextLayout(origin, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                    if let Some((start, length)) = underline {
                        layout.SetUnderline(
                            false,
                            DWRITE_TEXT_RANGE {
                                startPosition: start,
                                length,
                            },
                        )?;
                    }
                }
                // 要件 7.3.2: what stands in each of this block's boxes. After
                // the text, so the ink sits on top of nothing it has to fight.
                if runs.iter().any(run_draws_ink) {
                    let format = graphics.text_format(&self.typography, mode)?;
                    let text = &self.text[block.span.byte_start..block.span.byte_end];
                    let indent = self.typography.indent_step();
                    draw_marker_ink(
                        &target, &brush, &format, &layout, &runs, text, origin, mode, indent,
                    )?;
                }
                // SAFETY: Paired with BeginDraw above.
                unsafe { target.EndDraw(None, None)? };

                let (tile_width, tile_height) = mode.to_surface(tile_size, line_extent);
                let stride = tile_width * 4;
                let needed = stride as usize * tile_height as usize;
                if graphics.scratch.len() < needed {
                    graphics.scratch.resize(needed, 0);
                }
                let bgra = &mut graphics.scratch[..needed];
                // The tile was drawn at the origin of the surface, so only its
                // own pixels are read back.
                let rect = WICRect {
                    X: 0,
                    Y: 0,
                    Width: tile_width as i32,
                    Height: tile_height as i32,
                };
                // SAFETY: The rectangle lies inside the bitmap and the buffer
                // matches the requested stride and height.
                unsafe {
                    let source: IWICBitmapSource = bitmap.cast()?;
                    source.CopyPixels(&rect, stride, bgra)?;
                }
                // Handed over borrowed: the caller converts straight out of this
                // buffer, so the pixels are walked once and copied once.
                emit(*span, tile_width, tile_height, bgra);
            }
            Ok(())
        })
    }

    /// A fingerprint of everything that decides one tile's pixels.
    ///
    /// Deliberately free of coordinates. A tile draws one block at a fixed
    /// offset inside its own slice, so the same block text produces the same
    /// pixels wherever the layout puts the block. That is what lets an edit that
    /// changes the document's width leave every other tile on screen untouched;
    /// the old grid-anchored fingerprint carried the block's offset within the
    /// tile, so one new line invalidated all of them.
    pub fn tile_signature(&self, tile: TileSpan, preedit_utf16_range: Option<(u32, u32)>) -> u64 {
        let mut hasher = DefaultHasher::new();
        tile.sub_index.hash(&mut hasher);
        tile.flow_size.hash(&mut hasher);
        self.line_extent.hash(&mut hasher);
        hash_typography(&self.typography, &mut hasher);
        hash_colours(&self.typography, &mut hasher);
        // Two panes showing the same text at the same size draw different
        // pixels, so a shared tile cache must not confuse them.
        self.mode.hash(&mut hasher);
        if let Some(block) = self.plan.blocks.get(tile.block_index) {
            self.text[block.span.byte_start..block.span.byte_end].hash(&mut hasher);
            // Block-local, like the underline below: the same heading drawn at
            // the same size is the same pixels wherever it sits.
            let runs = style_runs(self.block_styled(tile.block_index));
            hash_style_runs(&runs, &self.typography, &mut hasher);
            // 要件 7.3.2: and the marks that belong to whole lines. **Nothing
            // in the block's own text says a line is one** — three hyphens
            // inside a fence are three hyphens — and neither mark moves a
            // glyph, so without this the tile already in the cache is the one
            // that gets shown. The same trap the colours fell into above.
            let line_marks = line_runs(self.block_styled(tile.block_index));
            hash_line_runs(&line_marks, &mut hasher);
            // The underline is the one thing the block's own text does not say.
            // Hashed block-local, so it stays put when earlier text changes.
            if let Some(local) = preedit_utf16_range.and_then(|(start, length)| {
                block_local_range(
                    block.span.utf16_start,
                    block.span.utf16_end,
                    start,
                    start + length,
                )
            }) {
                local.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Where the caret sits on screen, in the pane's own coordinates.
    pub fn caret_geometry(&mut self, caret_utf16: u32) -> Result<CaretGeometry> {
        if self.plan.is_empty() {
            return Ok(CaretGeometry {
                x: self.margin,
                y: self.margin,
                width: self.typography.font_size,
                height: self.typography.font_size,
            });
        }
        let block_index = self.plan.block_at_utf16(caret_utf16);
        let font_size = self.typography.font_size;
        let margin = self.margin;
        let mode = self.mode;

        with_graphics(|graphics| {
            let layout = self.layout_for(graphics, block_index)?;
            let block = &self.plan.blocks[block_index];
            let local = caret_utf16
                .saturating_sub(block.span.utf16_start)
                .min(block.span.utf16_len());
            let mut point_x = 0.0;
            let mut point_y = 0.0;
            let mut metrics = DWRITE_HIT_TEST_METRICS::default();
            // SAFETY: The layout is alive and the position is clamped into it.
            unsafe {
                layout.HitTestTextPosition(
                    local,
                    false,
                    &mut point_x,
                    &mut point_y,
                    &mut metrics,
                )?;
            }
            // The hit test answers on both axes at once: the flow coordinate
            // needs the block's offset applied, the line coordinate the margin
            // and the block's own indent (要件 7.3.2).
            let (_, line_point) = mode.to_axes(point_x, point_y);
            let inset = block_inset(&block.span, &self.typography);
            let (x, y) = mode.to_screen(
                block.to_global_flow(mode.flow_of(&metrics)),
                margin + inset + line_point,
            );
            Ok(CaretGeometry {
                x,
                y,
                width: metrics.width.max(font_size),
                height: metrics.height.max(font_size),
            })
        })
    }

    /// Selection rectangles for the part of the range that the viewport shows.
    ///
    /// Clipping to `visible_flow` is what keeps a document-wide selection cheap:
    /// only the blocks actually on screen are hit tested.
    pub fn selection_rects(
        &mut self,
        selection_utf16_range: Option<(u32, u32)>,
        visible_flow: (f32, f32),
    ) -> Result<Vec<SelectionRect>> {
        let Some((start, length)) = selection_utf16_range.filter(|(_, length)| *length > 0) else {
            return Ok(Vec::new());
        };
        let utf16_len = self.utf16_len();
        let start = start.min(utf16_len);
        let end = (start + length).min(utf16_len);
        if start >= end {
            return Ok(Vec::new());
        }

        let by_text = self.plan.blocks_in_utf16_range(start, end);
        let by_view = self
            .plan
            .blocks_in_flow_range(visible_flow.0, visible_flow.1);
        let first = by_text.start.max(by_view.start);
        let last = by_text.end.min(by_view.end);
        if first >= last {
            return Ok(Vec::new());
        }
        let margin = self.margin;
        let mode = self.mode;
        let mut rects = Vec::new();

        with_graphics(|graphics| {
            for block_index in first..last {
                let layout = self.layout_for(graphics, block_index)?;
                let block = &self.plan.blocks[block_index];
                let Some((local_start, local_length)) =
                    block_local_range(block.span.utf16_start, block.span.utf16_end, start, end)
                else {
                    continue;
                };
                // Clip to the lines actually on screen. Without this, one
                // oversized logical line would still cost the whole line.
                let Some((visible_start, visible_end)) =
                    block.visible_utf16_range(visible_flow.0, visible_flow.1)
                else {
                    continue;
                };
                let selection_end = local_start + local_length;
                let local_start = local_start.max(visible_start);
                let local_end = selection_end.min(visible_end);
                if local_start >= local_end {
                    continue;
                }
                let local_length = local_end - local_start;
                // Given the same origin the tile is drawn at, DirectWrite
                // reports the regions already in the pane's coordinates.
                let inset = block_inset(&block.span, &self.typography);
                let (origin_x, origin_y) = mode.to_screen(block.draw_origin(), margin + inset);

                // One region per selected UTF-16 unit is the hard upper bound.
                // Supplying it avoids DirectWrite's insufficient-buffer probe.
                let mut metrics = vec![DWRITE_HIT_TEST_METRICS::default(); local_length as usize];
                let mut count = 0;
                // SAFETY: The metrics buffer is large enough for any result the
                // range can produce, and the layout is alive across the call.
                unsafe {
                    layout.HitTestTextRange(
                        local_start,
                        local_length,
                        origin_x,
                        origin_y,
                        Some(&mut metrics),
                        &mut count,
                    )?;
                }
                metrics.truncate(count as usize);
                // A selected empty line has no glyph to cover, so DirectWrite
                // reports a zero-extent region for it. Those would reach Slint
                // as invisible rectangles; drop them at the source instead.
                rects.extend(
                    metrics
                        .into_iter()
                        .filter(|metric| metric.width > 0.0 && metric.height > 0.0)
                        .map(|metric| SelectionRect {
                            left: metric.left,
                            top: metric.top,
                            right: metric.left + metric.width,
                            bottom: metric.top + metric.height,
                        }),
                );
            }
            Ok(())
        })?;

        Ok(rects)
    }

    pub fn hit_test(&mut self, x: f32, y: f32) -> Result<HitTest> {
        if self.plan.is_empty() {
            return Ok(HitTest {
                utf16_position: 0,
                is_inside: false,
            });
        }
        let mode = self.mode;
        let (flow, line) = mode.to_axes(x, y);
        let block_index = self.plan.block_at_flow(flow);
        let margin = self.margin;

        with_graphics(|graphics| {
            let layout = self.layout_for(graphics, block_index)?;
            let block = &self.plan.blocks[block_index];
            let inset = block_inset(&block.span, &self.typography);
            let (layout_x, layout_y) =
                mode.to_screen(block.to_layout_flow(flow), line - margin - inset);
            hit_test_in_block(
                &layout,
                layout_x,
                layout_y,
                block.span.utf16_start,
                block.span.utf16_end,
            )
        })
    }

    /// Move the caret to the neighbouring line, one step along the flow axis.
    ///
    /// `flow_delta` is a screen direction, negative towards the origin: the left
    /// arrow in vertical writing, the up arrow in horizontal writing. Which of
    /// those is the *next* line to read is what the writing mode decides.
    ///
    /// `preferred_line` is the position along the line axis to keep, so that
    /// crossing a short line does not drag the caret back to its start.
    ///
    /// The line table built at measurement time already says where every line
    /// starts and how wide it is, so this is one hit test instead of one
    /// per grapheme in the document.
    pub fn move_caret_by_line(
        &mut self,
        caret_utf16: u32,
        flow_delta: i32,
        preferred_line: Option<f32>,
    ) -> Result<HitTest> {
        let unchanged = HitTest {
            utf16_position: caret_utf16,
            is_inside: false,
        };
        let Some((block_index, line_index)) = self.plan.locate(caret_utf16) else {
            return Ok(unchanged);
        };
        let mode = self.mode;
        let forwards = match mode.flow_order() {
            FlowOrder::Ascending => flow_delta > 0,
            FlowOrder::Descending => flow_delta < 0,
        };
        let Some((next_block, next_line)) = self.plan.step_line(block_index, line_index, forwards)
        else {
            return Ok(unchanged);
        };

        let margin = self.margin;
        let target_line = match preferred_line {
            Some(line) => line,
            None => {
                let caret = self.caret_geometry(caret_utf16)?;
                let (_, line) = mode.to_axes(caret.x, caret.y);
                line
            }
        };
        let Some(center_flow) = self.plan.line_flow_center(next_block, next_line) else {
            return Ok(unchanged);
        };

        with_graphics(|graphics| {
            let layout = self.layout_for(graphics, next_block)?;
            let block = &self.plan.blocks[next_block];
            let inset = block_inset(&block.span, &self.typography);
            let (layout_x, layout_y) = mode.to_screen(
                block.to_layout_flow(center_flow),
                (target_line - margin - inset).max(0.0),
            );
            hit_test_in_block(
                &layout,
                layout_x,
                layout_y,
                block.span.utf16_start,
                block.span.utf16_end,
            )
        })
    }

    /// Move the caret to either end of the line it is on.
    ///
    /// Line boundaries come from the cached line metrics, so this needs no
    /// DirectWrite call at all.
    pub fn move_caret_to_line_edge(&self, caret_utf16: u32, to_end: bool) -> u32 {
        let Some((block_index, line_index)) = self.plan.locate(caret_utf16) else {
            return caret_utf16;
        };
        let edge = if to_end {
            self.plan.line_utf16_text_end(block_index, line_index)
        } else {
            self.plan.line_utf16_start(block_index, line_index)
        };
        edge.unwrap_or(caret_utf16)
    }
}

/// Intersect a global UTF-16 range with a block, returning a block-local range.
fn block_local_range(
    block_start: u32,
    block_end: u32,
    range_start: u32,
    range_end: u32,
) -> Option<(u32, u32)> {
    let start = range_start.max(block_start);
    let end = range_end.min(block_end);
    if start >= end {
        return None;
    }
    Some((start - block_start, end - start))
}

fn hit_test_in_block(
    layout: &IDWriteTextLayout,
    layout_x: f32,
    layout_y: f32,
    block_utf16_start: u32,
    block_utf16_end: u32,
) -> Result<HitTest> {
    let mut trailing = windows::core::BOOL::default();
    let mut inside = windows::core::BOOL::default();
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();
    // SAFETY: The layout is alive for the duration of the call.
    unsafe {
        layout.HitTestPoint(layout_x, layout_y, &mut trailing, &mut inside, &mut metrics)?;
    }
    let local = if trailing.as_bool() {
        metrics.textPosition.saturating_add(metrics.length)
    } else {
        metrics.textPosition
    };
    Ok(HitTest {
        utf16_position: block_utf16_start.saturating_add(local).min(block_utf16_end),
        is_inside: inside.as_bool(),
    })
}

/// Measure one block: its line table and how far it reaches along the flow axis.
///
/// `keep_trailing_empty_line` must be true only for the last block of the
/// document. See the comment on the trailing newline below.
/// Asks DirectWrite where a paragraph wraps, by laying it out.
///
/// This is what cutting inside a logical line costs: the positions to cut at are
/// a property of the layout, so one has to be built to find them, and building
/// it costs what measuring the paragraph costs (6.9). The cut pieces are then
/// measured separately on top of that.
///
/// Only reached for a logical line too long to be one block, so an ordinary
/// document never builds this layout at all.
/// One long paragraph's wrapping, kept so the next update need not find it
/// again from nothing.
struct ParagraphWraps {
    text: String,
    /// How the line was set when these positions were found — **all of what
    /// moves a break**, which is what [`LongLine`] is a list of. Positions
    /// found under anything else are not line starts here.
    ///
    /// The indent is one of them: a paragraph set one step in wraps at a
    /// narrower box and breaks somewhere else. It follows from the style
    /// within one engine, since only a preview indents at all (要件 7.3.1) —
    /// but it is what the layout was actually made at, and a key that leaves
    /// out a thing that moves a break is the kind that goes wrong quietly.
    style: LineStyle,
    indent_steps: u8,
    marks: Vec<Emphasis>,
    marker: Option<LineMarker>,
    /// Byte offsets where each line after the first begins.
    starts: Vec<usize>,
}

impl ParagraphWraps {
    /// Whether these positions were found for a line set exactly as this one
    /// is — the text apart, which each caller compares its own way.
    fn matches(&self, line: LongLine<'_>) -> bool {
        self.style == line.style
            && self.indent_steps == line.indent_steps
            && self.marker == line.marker
    }
}

/// The furthest along the flow axis any one layout is asked to reach.
///
/// DirectWrite's own limit is 262144px. Staying well under it means the editor
/// never arrives there, so what happens at that limit never has to be known.
///
/// This is not a number of characters and cannot be turned into one: how many
/// fit in a line depends on the pane, so the same paragraph reaches a different
/// distance in a different window. It is a guard on the layout, not a rule about
/// documents — the rule about documents is a separate, much smaller number in
/// the editor, and lowering this one to meet it would only make the range
/// between them slower (技術検証 7.4).
const MAX_LAYOUT_FLOW: f32 = 200_000.0;

/// Line starts given back beyond the first changed byte, as a margin.
///
/// Everything before an edit wraps as it did, because a line is decided by the
/// text that precedes it. But a break also depends on the characters just after
/// it: Japanese line breaking will not leave a closing bracket or a full stop at
/// the start of a line, so the last break before an edit can still move when the
/// edit lands right on top of it. Two lines of margin costs about forty
/// characters of re-wrapping and removes the question.
const WRAP_REUSE_MARGIN: usize = 2;

/// Everything a wrap search needs about the page, without the graphics.
///
/// **Owned, so a search can be run on another thread** (要件 2, 技術検証 7.4).
/// The text format is not here: a DirectWrite format belongs to the factory
/// that made it, and each thread has one of its own.
#[derive(Clone)]
struct WrapPage {
    typography: Arc<Typography>,
    mode: WritingMode,
    line_extent: u32,
    line_box: f32,
}

impl WrapPage {
    /// The line extent a paragraph set `steps` in is laid out across.
    ///
    /// **An indented paragraph is asked at the width it will be cut at.** A
    /// wrap position is only a line start for a layout of the same width, and
    /// the pieces are measured in the block's own narrower box (要件 7.3.2).
    fn indented_extent(&self, steps: u8) -> u32 {
        let inset = indent_of(steps, &self.typography);
        self.line_extent.saturating_sub(inset as u32).max(1)
    }

    /// And the box that goes with it, which is what the pieces are measured
    /// in once they are cut.
    fn indented_box(&self, steps: u8) -> f32 {
        let inset = indent_of(steps, &self.typography);
        (self.line_box - inset).max(1.0)
    }
}

/// What an earlier update leaves usable for one long line (6.10).
///
/// **No graphics at all**: which earlier wrapping this line can resume from is
/// decided by comparing text, marks and the way the line is set. That is why
/// the search itself can be run somewhere else — the part that has to see the
/// cache stays where the cache is, and only a byte offset crosses.
struct WrapReuse {
    /// The line starts already known, which the search extends.
    kept: Vec<usize>,
    /// The byte to lay out from. Zero when nothing was reusable.
    from: usize,
    /// The line is unchanged and its whole wrapping stands: nothing to lay out.
    whole: bool,
    /// The widest prefix any earlier paragraph shared and how many line starts
    /// it had, for the log. See [`UpdateCost`].
    shared: usize,
    starts: usize,
}

/// Look one long line up among the paragraphs of the last update.
fn wrap_reuse(previous: &[ParagraphWraps], line: LongLine<'_>) -> WrapReuse {
    let same = previous
        .iter()
        .find(|kept| kept.matches(line) && kept.marks == line.marks && kept.text == line.text);
    if let Some(same) = same {
        return WrapReuse {
            kept: same.starts.clone(),
            from: 0,
            whole: true,
            shared: 0,
            starts: 0,
        };
    }

    // The longest surviving prefix of any paragraph set the same way. With one
    // long paragraph in the document this finds it; with several it finds the
    // edited one, because the others matched in full above.
    let nearest = previous
        .iter()
        .filter(|kept| kept.matches(line))
        .map(|kept| (reusable_prefix(kept, line), kept))
        .max_by_key(|(shared, _)| *shared);
    let (shared, starts) = nearest
        .as_ref()
        .map(|(shared, kept)| (*shared, kept.starts.len()))
        .unwrap_or_default();
    let resume = nearest.and_then(|(shared, kept)| {
        let usable = kept.starts.partition_point(|start| *start <= shared);
        let usable = usable.checked_sub(WRAP_REUSE_MARGIN)?;
        let starts = &kept.starts[..usable];
        Some((*starts.last()?, starts.to_vec()))
    });
    // Laying out from a line start is legitimate for the same reason cutting
    // there is: the lines from there on depend on nothing before it.
    match resume {
        Some((from, kept)) => WrapReuse {
            kept,
            from,
            whole: false,
            shared,
            starts,
        },
        None => WrapReuse {
            kept: Vec::new(),
            from: 0,
            whole: false,
            shared,
            starts,
        },
    }
}

/// Where a paragraph's lines begin, found a window at a time.
fn wrap_offsets(
    graphics: &mut Graphics,
    format: &IDWriteTextFormat,
    page: &WrapPage,
    line: LongLine<'_>,
    from: usize,
) -> Result<Vec<usize>> {
    let mut offsets: Vec<usize> = Vec::new();
    let mut start = from;
    while start < line.text.len() {
        let end = window_end(page, line, start);
        let found = wrap_offsets_in(graphics, format, page, line, start, end)?;
        let whole_rest = end == line.text.len();
        let usable = if whole_rest {
            found.len()
        } else {
            found.len().saturating_sub(WRAP_REUSE_MARGIN)
        };
        if usable == 0 {
            // A window with nothing usable in it cannot be advanced past,
            // so the rest of the line stays in one piece. Slow, and decided.
            break;
        }
        offsets.extend(found[..usable].iter().map(|offset| offset + start));
        if whole_rest {
            break;
        }
        // How far the window actually got. Bounded from below on purpose: a
        // window that comes back with barely more lines than the margin
        // would advance a line at a time, laying the paragraph out once per
        // line. Leaving the rest in one piece is slow; laying it out
        // thousands of times is a hang.
        let advance = found[usable - 1];
        if advance * 4 < end - start {
            break;
        }
        start += advance;
    }
    Ok(offsets)
}

/// The byte offset one window past `start`, on a character boundary.
fn window_end(page: &WrapPage, line: LongLine<'_>, start: usize) -> usize {
    let text = line.text;
    let style = line.style;
    let typography = &page.typography;
    let level = style.heading_level;
    let flow_per_line = typography.font_size * 2.2 * typography.flow_scale(level);
    let lines = (MAX_LAYOUT_FLOW / flow_per_line.max(1.0)).floor().max(1.0);
    let extent = page.indented_extent(line.indent_steps);
    let cells = cells_per_line(extent, typography) as f32;
    let per_line = (cells / typography.size_scale(level)).max(1.0);
    let characters = (lines * per_line) as usize;
    let rest = &text[start..];
    // A character is at least one byte, so a byte length already under the
    // allowance is under it in characters too, and needs no walk.
    if rest.len() <= characters {
        return text.len();
    }
    match rest.char_indices().nth(characters) {
        Some((offset, _)) => start + offset,
        None => text.len(),
    }
}

/// Where the window `from..to` of one long line wraps, relative to `from`.
fn wrap_offsets_in(
    graphics: &mut Graphics,
    format: &IDWriteTextFormat,
    page: &WrapPage,
    line: LongLine<'_>,
    from: usize,
    to: usize,
) -> Result<Vec<usize>> {
    let style = line.style;
    let text = &line.text[from..to];
    // One logical line, so one style covers all of it — and **the same spec
    // the pieces will be measured under**, marks and head box included.
    // Character spacing, heading size, a bold stretch and an indent all move
    // where the text wraps, so a barer layout reports positions the pieces
    // do not actually break at. The box belongs to the head of the line, so
    // only a window that starts there gets one.
    let levels = [style];
    let spans = [line.marks_from(from)];
    let markers = [line.marker.filter(|_| from == 0)];
    let marked = StyledText::marked(text, &levels, &spans);
    let styled = marked.with_markers(&markers);
    let steps = line.indent_steps;
    let typography = &page.typography;
    let bound = block_flow_bound(styled, page.indented_extent(steps), typography);
    let line_box = page.indented_box(steps);
    let (max_width, max_height) = page.mode.to_screen(bound, line_box);
    let utf16 = text.encode_utf16().collect::<Vec<u16>>();
    // SAFETY: The UTF-16 buffer outlives CreateTextLayout, and the format
    // is owned by the caller for the whole call.
    let layout = unsafe {
        graphics
            .dwrite
            .CreateTextLayout(&utf16, format, max_width, max_height)?
    };
    let runs = style_runs(styled);
    apply_typography(&layout, typography, &runs, utf16.len() as u32)?;
    apply_marker_boxes(&layout, typography, &runs)?;
    Ok(wrap_byte_offsets(text, &line_metrics(&layout)?))
}

/// How far a kept paragraph's wrapping still describes `line`, in bytes.
///
/// The text they share, **cut back to before the first marked stretch they do
/// not agree on**. Sharing the text is not enough: a marker that closes changes
/// what came before it — typing the second `**` of a pair makes an earlier
/// stretch bold, and bold text wraps elsewhere. Ordinary typing inside a marked
/// paragraph moves no earlier mark, so this still keeps the prefix in the case
/// that matters (6.10).
fn reusable_prefix(kept: &ParagraphWraps, line: LongLine<'_>) -> usize {
    let shared = common_prefix(&kept.text, line.text);
    let agree = marks_agree_until(&kept.marks, line.marks);
    if agree == u32::MAX {
        return shared;
    }
    shared.min(byte_at_utf16(line.text, agree))
}

/// How far two mark tables say the same thing, in UTF-16 units, or `u32::MAX`
/// when they say it throughout.
///
/// **Sorted before they are walked**: the tables are recorded in the order the
/// markers close rather than in reading order (`document::push_marked`), so the
/// inside of a nested pair comes first.
fn marks_agree_until(before: &[Emphasis], after: &[Emphasis]) -> u32 {
    let in_order = |marks: &[Emphasis]| {
        let mut sorted = marks.to_vec();
        sorted.sort_by_key(|mark| (mark.utf16_start, mark.utf16_len));
        sorted
    };
    let before = in_order(before);
    let after = in_order(after);
    for (index, mark) in before.iter().enumerate() {
        match after.get(index) {
            Some(other) if other == mark => continue,
            Some(other) => return mark.utf16_start.min(other.utf16_start),
            None => return mark.utf16_start,
        }
    }
    match after.get(before.len()) {
        Some(extra) => extra.utf16_start,
        None => u32::MAX,
    }
}

/// How many leading bytes two paragraphs share, ending on a character boundary.
///
/// This is where an edit begins, and everything before it wraps as it did.
fn common_prefix(before: &str, after: &str) -> usize {
    let mut shared = before
        .as_bytes()
        .iter()
        .zip(after.as_bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while shared > 0 && !before.is_char_boundary(shared) {
        shared -= 1;
    }
    shared
}

/// Byte offsets in `text` where each line after the first begins.
///
/// DirectWrite counts in UTF-16 and the split works in bytes, so the two are
/// walked together once. The end of the text is dropped: it closes the last line
/// rather than starting one, and a cut there would make an empty block.
fn wrap_byte_offsets(text: &str, metrics: &[DWRITE_LINE_METRICS]) -> Vec<usize> {
    let mut wanted = Vec::with_capacity(metrics.len());
    let mut utf16 = 0_u32;
    for line in metrics {
        utf16 += line.length;
        wanted.push(utf16);
    }
    wanted.pop();

    let mut offsets = Vec::with_capacity(wanted.len());
    let mut next = 0;
    let mut utf16 = 0_u32;
    for (byte, character) in text.char_indices() {
        // A while loop rather than an if: an empty line would ask for the same
        // offset twice, and only the first of them may become a cut.
        while next < wanted.len() && wanted[next] <= utf16 {
            // Never the start of the text: a cut there makes an empty block.
            if byte > 0 && offsets.last() != Some(&byte) {
                offsets.push(byte);
            }
            next += 1;
        }
        utf16 += character.len_utf16() as u32;
    }
    offsets
}

/// Every line DirectWrite produced for a layout.
///
/// Two calls: one to learn the count, which is expected to report an
/// insufficient buffer, and one to fill it.
fn line_metrics(layout: &IDWriteTextLayout) -> Result<Vec<DWRITE_LINE_METRICS>> {
    let mut line_count = 0_u32;
    // SAFETY: The probe call is expected to fail with the required count; only
    // that count is used.
    unsafe {
        let _ = layout.GetLineMetrics(None, &mut line_count);
    }
    let mut metrics = vec![DWRITE_LINE_METRICS::default(); line_count as usize];
    if line_count > 0 {
        // SAFETY: The buffer holds exactly the count reported above.
        unsafe { layout.GetLineMetrics(Some(&mut metrics), &mut line_count)? };
    }
    metrics.truncate(line_count as usize);
    Ok(metrics)
}

fn measure_block(
    layout: &IDWriteTextLayout,
    max_flow_size: f32,
    keep_trailing_empty_line: bool,
    mode: WritingMode,
) -> Result<BlockMeasure> {
    let mut line_metrics = line_metrics(layout)?;

    // Every block ends just after a newline, and DirectWrite answers a trailing
    // newline with an extra empty line. Within the whole document that position
    // is the first line of the *next* block, so a block that claims it pushes
    // everything after it one line further along the flow axis. Only the final
    // block keeps it, because there the empty line is genuinely the document end.
    if !keep_trailing_empty_line
        && line_metrics.len() > 1
        && line_metrics.last().is_some_and(|line| line.length == 0)
    {
        line_metrics.pop();
    }

    let mut lines = Vec::with_capacity(line_metrics.len());
    let mut utf16_start = 0_u32;

    for line in &line_metrics {
        let mut point_x = 0.0;
        let mut point_y = 0.0;
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: Every line start is a valid position inside this layout.
        unsafe {
            layout.HitTestTextPosition(
                utf16_start,
                false,
                &mut point_x,
                &mut point_y,
                &mut metrics,
            )?;
        }
        lines.push(LineInfo {
            utf16_start,
            utf16_len: line.length,
            newline_len: line.newlineLength,
            flow_start: mode.flow_of(&metrics),
            // The line's own advance along the flow axis. Not `metrics.width`,
            // which is the ink of the one character sitting there. DirectWrite
            // reports this advance as `height` in either writing direction.
            flow_size: line.height,
        });
        utf16_start += line.length;
    }

    // A block's extent is the sum of its lines' advances, and nothing else.
    //
    // Two earlier answers were both wrong, and wrong in the same way: they tried
    // to infer one pitch for the whole block. The ink width DirectWrite reports
    // at a line start is a fraction of a pixel narrower than the cell, so
    // first-start to last-end dropped that fraction once per block. The widest
    // gap between adjacent lines fixed that but is a maximum, and a maximum only
    // grows with the sample, so a forty line block reported less pitch than the
    // same text inside a document of a hundred and sixty. The average gap
    // removed that bias and still missed, by 0.4px per boundary, because there
    // is no single pitch to find: a line holding a rotated Latin run is
    // genuinely wider than one of plain ideographs.
    //
    // The line's own `height` is the advance DirectWrite lays that line out
    // with, so summing them needs no estimate and no neighbour. Blocks abut
    // exactly because the whole document's lines are the blocks' lines.
    let fallback = lines
        .iter()
        .map(|line| line.flow_size)
        .find(|size| *size > 0.0)
        .unwrap_or(0.0);
    for line in &mut lines {
        if line.flow_size <= 0.0 {
            line.flow_size = fallback;
        }
    }

    // The extent comes from the lines alone. The layout's own text metrics cover
    // the trailing empty line too, which is what must not be counted here.
    let (content_flow_start, flow_size) = if lines.is_empty() {
        (0.0, 0.0)
    } else {
        let content_flow_start = lines
            .iter()
            .map(|line| line.flow_start)
            .fold(f32::INFINITY, f32::min);
        (
            content_flow_start,
            lines.iter().map(|line| line.flow_size).sum(),
        )
    };

    Ok(BlockMeasure {
        flow_size,
        content_flow_start,
        max_flow_size,
        lines: lines.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_blocks::{LineKind, Marks, visible_flow_range};

    /// The pane extent along the line axis every test lays text out in.
    const LINE_EXTENT: u32 = 520;

    fn engine_for(text: &str, font_size: f32) -> TextEngine {
        engine_in(WritingMode::Vertical, text, font_size)
    }

    fn engine_in(mode: WritingMode, text: &str, font_size: f32) -> TextEngine {
        engine_set(mode, StyledText::plain(text), &Typography::new(font_size))
    }

    fn engine_set(
        mode: WritingMode,
        styled: StyledText<'_>,
        typography: &Typography,
    ) -> TextEngine {
        let mut engine = TextEngine::new(mode);
        engine
            .update(styled, LINE_EXTENT, typography)
            .expect("DirectWrite block measurement");
        engine
    }

    /// Re-lay out an existing engine with no styling and the default spec.
    fn update_plain(engine: &mut TextEngine, text: &str) -> UpdateCost {
        engine
            .update(StyledText::plain(text), LINE_EXTENT, &plain())
            .expect("DirectWrite block measurement")
    }

    #[test]
    fn measures_a_block_per_logical_line_group() {
        let engine = engine_for("全角：ＡＢＣ１２３\n半角：ABC123\n句読点。（）「」", 24.0);

        assert_eq!(engine.block_count(), 1, "a short document is one block");
        assert!(engine.total_flow_size() > 0);
        assert_eq!(
            engine.utf16_len(),
            "全角：ＡＢＣ１２３\n半角：ABC123\n句読点。（）「」"
                .encode_utf16()
                .count() as u32
        );
    }

    /// The model every block measurement rests on: DirectWrite advances from one
    /// line to the next by the height of the line it is starting, so a block
    /// can size itself from its own line metrics and needs to know nothing about
    /// its neighbours.
    ///
    /// If this ever stops holding, block widths stop adding up to the document's
    /// and every boundary drifts. It is checked directly rather than inferred
    /// from the totals, so a break says what broke.
    #[test]
    fn a_column_advances_by_the_height_of_its_own_line() {
        // A rotated Latin run and plain ideographs, so the columns are not all
        // the same width. That variety is what defeated a single pitch.
        let text = "縦書きの列送りを確かめる段落です。日本語ABC123を含みます。\n\n".repeat(6);
        let engine = engine_for(&text, 22.0);
        let block = &engine.plan.blocks[0];
        assert!(block.lines.len() > 3, "need several lines");

        for pair in block.lines.windows(2) {
            let gap = pair[0].flow_start - pair[1].flow_start;
            assert!(
                (gap - pair[1].flow_size).abs() <= 0.01,
                "columns sit {gap}px apart but the next line's height is {}",
                pair[1].flow_size
            );
        }
    }

    /// The same model, downwards. Horizontal writing stacks lines towards larger
    /// screen y, so a block's lines run the other way and the gap between two
    /// lines is the *later* line's advance rather than the earlier one's.
    #[test]
    fn a_horizontal_line_advances_by_the_height_of_its_own_line() {
        let text = "横書きの行送りを確かめる段落です。日本語ABC123を含みます。\n\n".repeat(6);
        let engine = engine_in(WritingMode::Horizontal, &text, 22.0);
        let block = &engine.plan.blocks[0];
        assert!(block.lines.len() > 3, "need several lines");

        for pair in block.lines.windows(2) {
            assert!(
                pair[1].flow_start > pair[0].flow_start,
                "horizontal lines must stack downwards"
            );
            let gap = pair[1].flow_start - pair[0].flow_start;
            assert!(
                (gap - pair[0].flow_size).abs() <= 0.01,
                "lines sit {gap}px apart but the earlier line's height is {}",
                pair[0].flow_size
            );
        }
    }

    /// Horizontal blocks are placed from the origin end: block 0 at the top,
    /// each one starting where the last finished.
    #[test]
    fn horizontal_blocks_stack_downwards_from_the_first() {
        let text =
            "横書きのブロック配置を確かめる段落です。日本語ABC123を含みます。\n\n".repeat(40);
        let engine = engine_in(WritingMode::Horizontal, &text, 22.0);
        assert!(engine.block_count() > 2, "the sample must span many blocks");

        let margin = margin_for(22.0);
        assert_eq!(engine.plan.blocks[0].flow_start, margin);
        for pair in engine.plan.blocks.windows(2) {
            assert_eq!(
                pair[0].flow_end(),
                pair[1].flow_start,
                "blocks must abut in reading order"
            );
        }
    }

    /// The whole point of the split: laying out blocks separately must land the
    /// columns exactly where one document-wide layout would have put them.
    #[test]
    fn block_widths_sum_to_the_single_layout_width() {
        // Several lengths, so the split lands on different boundaries and the
        // final block comes out a different size each time.
        for repeats in [37, 40, 41, 53] {
            assert_split_matches_one_layout(WritingMode::Vertical, repeats, &plain());
        }
    }

    /// The same invariant with the lines stacking downwards instead. This is the
    /// one that has to hold before anything else in horizontal writing can: if
    /// the blocks do not add up to the single layout, every line below the first
    /// block is drawn at the wrong height.
    #[test]
    fn horizontal_block_heights_sum_to_the_single_layout_height() {
        for repeats in [37, 40, 41, 53] {
            assert_split_matches_one_layout(WritingMode::Horizontal, repeats, &plain());
        }
    }

    /// The same invariant again, with none of the three quantities left at
    /// DirectWrite's own value and with headings scattered through the sample.
    ///
    /// This is the question this round of validation exists to answer. A block
    /// sizes itself from its own line metrics, and every line is now free to be
    /// a different size from its neighbours — if that breaks the sum, per-line
    /// sizing cannot be built on this structure at all. Both writing modes,
    /// because the heading size lands on the line axis and the spacing on the
    /// flow axis, and the two swap between them.
    #[test]
    fn block_extents_sum_to_the_single_layout_under_free_typography() {
        let typography = Typography {
            character_spacing: 0.18,
            line_spacing: 1.35,
            ..Typography::new(22.0)
        }
        .with_heading_ramp(1.8);
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            for repeats in [37, 40, 41, 53] {
                assert_split_matches_one_layout(mode, repeats, &typography);
            }
        }
    }

    fn plain() -> Typography {
        Typography::new(22.0)
    }

    /// **The question the whole approach turns on.**
    ///
    /// A paragraph with no break in it is cut at the positions DirectWrite says
    /// it wraps at. For that to be legitimate, laying out the text *from* such a
    /// position has to produce the same lines it produced inside the paragraph.
    /// It was not obvious that it would: Japanese line breaking has kinsoku
    /// rules that forbid certain characters at the start or the end of a line,
    /// and a rule that looks at what precedes a break could well decide
    /// differently when nothing precedes it.
    ///
    /// If this fails, the paragraph cannot be cut and 6.9's 212ms stands.
    #[test]
    fn a_cut_paragraph_sums_to_the_single_layout() {
        // Long enough to be cut into several pieces, and full of the characters
        // the breaking rules care about: closing brackets and punctuation that
        // may not begin a line, and Latin runs that may not be split.
        let sentence = "日本語ABC123と句読点、括弧（かっこ）「鉤括弧」を含む段落である。";
        let text = format!("{}\n", sentence.repeat(160));

        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let styled = StyledText::plain(&text);
            let engine = engine_set(mode, styled, &plain());
            assert!(
                engine.block_count() > 3,
                "the paragraph must be cut into pieces, not left as {} block(s)",
                engine.block_count()
            );
            assert_blocks_match_one_layout(mode, styled, &plain(), "one long paragraph");
        }
    }

    /// The same paragraph with the three typography quantities moved off their
    /// defaults, because all three change where the text wraps and therefore
    /// where it may be cut.
    #[test]
    fn a_cut_paragraph_holds_under_free_typography() {
        let typography = Typography {
            character_spacing: 0.18,
            line_spacing: 1.35,
            ..Typography::new(22.0)
        };
        let sentence = "縦書きの長い段落で、句読点や（括弧）やABC123が混ざっている。";
        let text = format!("{}\n", sentence.repeat(160));
        let styled = StyledText::plain(&text);

        assert_blocks_match_one_layout(
            WritingMode::Vertical,
            styled,
            &typography,
            "one long paragraph, free typography",
        );
    }

    /// The sample every invariant test lays out: paragraphs with a heading every
    /// fourth logical line, so blocks land on both kinds of line.
    fn sample_document(repeats: usize) -> (String, Vec<LineStyle>) {
        let paragraph = "これは検証用の段落です。句読点、括弧（かっこ）、全角ＡＢＣ、半角ABC123を含みます。\n\n";
        let mut text = String::new();
        let mut levels = Vec::new();
        for index in 0..repeats {
            if index % 4 == 0 {
                text.push_str(&format!("## 第{index}節\n"));
                levels.push(LineStyle::heading(2));
            }
            text.push_str(paragraph);
            levels.push(LineStyle::default());
            levels.push(LineStyle::default());
        }
        (text, levels)
    }

    fn assert_split_matches_one_layout(mode: WritingMode, repeats: usize, typography: &Typography) {
        let (text, levels) = sample_document(repeats);
        let label = format!("{repeats} paragraphs");
        let styled = StyledText::new(&text, &levels);
        assert_blocks_match_one_layout(mode, styled, typography, &label);
    }

    /// Lay the text out both ways and compare: as the blocks the split produces,
    /// and as one layout of the whole thing. The columns must land in the same
    /// places and the extents must add up.
    fn assert_blocks_match_one_layout(
        mode: WritingMode,
        styled: StyledText<'_>,
        typography: &Typography,
        label: &str,
    ) {
        let engine = engine_set(mode, styled, typography);
        assert_engine_matches_one_layout(&engine, styled, typography, label);
    }

    /// The same comparison against an engine that already exists, so an engine
    /// that reached its blocks through a series of edits can be checked as well
    /// as one built in a single pass.
    fn assert_engine_matches_one_layout(
        engine: &TextEngine,
        styled: StyledText<'_>,
        typography: &Typography,
        label: &str,
    ) {
        let text = styled.text;
        let mode = engine.mode;
        assert!(engine.block_count() > 1, "{label}: must span many blocks");

        let font_size = typography.font_size;
        let margin = margin_for(font_size);
        let line_box = (LINE_EXTENT as f32 - margin * 2.0).max(1.0);
        let bound = block_flow_bound(styled, LINE_EXTENT, typography);
        let (max_width, max_height) = mode.to_screen(bound, line_box);
        let utf16 = text.encode_utf16().collect::<Vec<u16>>();
        // The whole document set exactly as its blocks were: same spec, same
        // ranges, only measured in one piece.
        let runs = style_runs(styled);
        let whole = with_graphics(|graphics| {
            let format = graphics.text_format(typography, mode)?;
            // SAFETY: The UTF-16 buffer outlives CreateTextLayout.
            let layout = unsafe {
                graphics
                    .dwrite
                    .CreateTextLayout(&utf16, &format, max_width, max_height)?
            };
            apply_typography(&layout, typography, &runs, utf16.len() as u32)?;
            // The whole document is its own last block, so it keeps the
            // trailing empty line the split blocks give up.
            measure_block(&layout, bound, true, mode)
        })
        .expect("whole document measurement");

        // Column count first: it is the more diagnostic of the two. A block
        // boundary that leaks an extra column shows up here before it shows up
        // as a width delta.
        // The exact measurements, not the placed widths: those are rounded to
        // whole pixels so that tiles cut from a block never move, and the half
        // pixel per block that costs would hide the errors this test is for.
        let split_width: f32 = engine.plan.blocks.iter().map(|b| b.exact_flow_size).sum();
        assert_eq!(
            engine
                .plan
                .blocks
                .iter()
                .map(|b| b.lines.len())
                .sum::<usize>(),
            whole.lines.len(),
            "{label}: splitting must not change the column count"
        );
        // Nothing is estimated any more: both sides are the sum of the same line
        // advances, so the only difference allowed is float summation order.
        let boundaries = (engine.block_count() - 1).max(1) as f32;
        let error = (split_width - whole.flow_size).abs();
        assert!(
            error <= 0.5,
            "{label}: split blocks total {split_width}px \
             but one layout is {}px, over {} blocks — {:.3}px per boundary",
            whole.flow_size,
            engine.block_count(),
            error / boundaries
        );
    }

    /// A heading takes its size from the range it is set over, so the line it
    /// sits on advances further than the body line under it. This is what
    /// `line_cells` is charging for and what `block_flow_bound` is reserving.
    #[test]
    fn a_heading_line_advances_further_than_a_body_line() {
        let text = "見出しの行\n本文の行です\n";
        let levels = [LineStyle::heading(1), LineStyle::default()];
        let styled = StyledText::new(text, &levels);
        let at_one_size = engine_set(WritingMode::Vertical, styled, &plain());
        let big_headings = plain().with_heading_ramp(2.0);
        let with_headings = engine_set(WritingMode::Vertical, styled, &big_headings);

        let flat = &at_one_size.plan.blocks[0].lines;
        let sized = &with_headings.plan.blocks[0].lines;
        assert!(flat.len() >= 2 && sized.len() >= 2, "two lines are needed");
        assert!(
            (flat[0].flow_size - flat[1].flow_size).abs() < 0.5,
            "at one size both lines advance the same: {} and {}",
            flat[0].flow_size,
            flat[1].flow_size
        );
        assert!(
            sized[0].flow_size > sized[1].flow_size * 1.5,
            "the heading line advanced {} against the body line's {}",
            sized[0].flow_size,
            sized[1].flow_size
        );
        // The body line is untouched: only the range that was set changed size.
        assert!(
            (sized[1].flow_size - flat[1].flow_size).abs() < 0.5,
            "setting the heading moved the body line from {} to {}",
            flat[1].flow_size,
            sized[1].flow_size
        );
    }

    /// Character spacing is added to the advance, so the same logical line wraps
    /// into more visual lines. If it did not, `cells_per_line` would be counting
    /// something DirectWrite does not do.
    #[test]
    fn wider_character_spacing_wraps_a_line_sooner() {
        let text = "あ".repeat(120);
        let tight = engine_for(&text, 22.0);
        let loose = engine_set(
            WritingMode::Vertical,
            StyledText::plain(&text),
            &Typography {
                character_spacing: 0.5,
                ..Typography::new(22.0)
            },
        );

        let tight_lines: usize = tight.plan.blocks.iter().map(|b| b.lines.len()).sum();
        let loose_lines: usize = loose.plan.blocks.iter().map(|b| b.lines.len()).sum();
        assert!(
            loose_lines > tight_lines,
            "{tight_lines} lines stayed {loose_lines} at half a size of extra advance"
        );
    }

    /// Proportional line spacing must scale each line's own advance, not replace
    /// them all with one number.
    ///
    /// This is 4.2 restated for the spacing control. A block's extent is the sum
    /// of its lines' individual advances, and
    /// `DWRITE_LINE_SPACING_METHOD_UNIFORM` would replace all of them with one
    /// pitch — which flattens the differences and breaks every block boundary.
    ///
    /// The uneven pair is a heading and a body line. The obvious sample — a line
    /// holding a rotated Latin run against one of plain ideographs — turned out
    /// to measure the *same* advance to within a tenth of a pixel at this size,
    /// so the difference 4.2 found over a whole document is not something two
    /// lines can be relied on to show.
    #[test]
    fn proportional_line_spacing_keeps_the_lines_uneven() {
        let text = "見出しの行\n本文の行です\n";
        let levels = [LineStyle::heading(1), LineStyle::default()];
        let styled = StyledText::new(text, &levels);
        let headings = plain().with_heading_ramp(2.0);
        let airy = Typography {
            line_spacing: 1.5,
            ..headings.clone()
        };
        let tight = engine_set(WritingMode::Vertical, styled, &headings);
        let loose = engine_set(WritingMode::Vertical, styled, &airy);

        let uneven = |engine: &TextEngine| {
            let lines = &engine.plan.blocks[0].lines;
            lines[0].flow_size - lines[1].flow_size
        };
        assert!(
            uneven(&tight) > 1.0,
            "the heading and body lines must differ to start: {}",
            uneven(&tight)
        );
        // Scaled, not levelled. Uniform spacing would bring this to zero.
        assert!(
            uneven(&loose) > uneven(&tight) * 1.4,
            "spacing flattened a {}px difference to {}px",
            uneven(&tight),
            uneven(&loose)
        );
        let stretch = loose.plan.blocks[0].exact_flow_size / tight.plan.blocks[0].exact_flow_size;
        assert!(
            (stretch - 1.5).abs() < 0.05,
            "asking for 1.5 times the advance gave {stretch}"
        );
    }

    /// The bound is what DirectWrite is given as the layout box, and a block
    /// that reaches past it has its last lines clipped away. A heading larger
    /// than the factor the bound assumed is exactly how that happens.
    #[test]
    fn the_flow_bound_holds_for_a_block_of_headings() {
        let typography = Typography {
            line_spacing: 1.4,
            ..Typography::new(22.0)
        }
        .with_heading_ramp(2.4);
        let text = "# 大きな見出しの行です\n".repeat(40);
        let levels = vec![LineStyle::heading(1); 40];
        let engine = engine_set(
            WritingMode::Vertical,
            StyledText::new(&text, &levels),
            &typography,
        );

        for (index, block) in engine.plan.blocks.iter().enumerate() {
            assert!(
                block.exact_flow_size <= block.max_flow_size,
                "block {index} measured {}px inside a {}px bound",
                block.exact_flow_size,
                block.max_flow_size
            );
        }
        let lines: usize = engine.plan.blocks.iter().map(|b| b.lines.len()).sum();
        assert!(lines >= 40, "{lines} lines came back from 40 headings");
    }

    /// Stepping column by column must cross the whole document, in both
    /// directions, without ever standing still.
    ///
    /// A caret that stops advancing partway through looks to the writer like a
    /// key that stopped working. Every boundary it has to get over is here: the
    /// ends of ordinary blocks, and the cuts inside a long paragraph, which are
    /// not at line breaks and so are the ones with no newline to lean on.
    #[test]
    fn stepping_by_column_crosses_every_block_boundary() {
        // Just past two cuts inside the paragraph, which is all this needs: the
        // boundaries are what it is about, not the distance.
        let long = "日本語ABCと句読点、を含む長い段落である。".repeat(60);
        let text = format!("# 見出し\n短い行\n{long}\n終わりの行\n");
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 2, "the sample must span many blocks");
        let utf16_len = engine.utf16_len();

        // Forwards is towards smaller flow coordinates in vertical writing, so
        // it is the negative delta.
        let mut caret = 0;
        let mut steps = 0;
        loop {
            let next = engine
                .move_caret_by_line(caret, -2, None)
                .expect("a column step")
                .utf16_position;
            if next == caret {
                break;
            }
            assert!(
                next > caret,
                "stepping forwards went backwards, from {caret} to {next}"
            );
            caret = next;
            steps += 1;
            assert!(steps < 1_000, "stepping never reached the end");
        }
        assert!(
            caret + 200 >= utf16_len,
            "stepping forwards stalled at {caret} of {utf16_len}"
        );

        let mut back = caret;
        loop {
            let next = engine
                .move_caret_by_line(back, 2, None)
                .expect("a column step")
                .utf16_position;
            if next == back {
                break;
            }
            assert!(next < back, "stepping back went forwards");
            back = next;
        }
        assert!(back < 200, "stepping back stalled at {back} of {utf16_len}");
    }

    #[test]
    fn hit_testing_agrees_with_caret_geometry_across_blocks() {
        let paragraph = "縦書きのヒットテスト検証。日本語ABC123と句読点、を含む段落です。\n\n";
        let text = paragraph.repeat(30);
        let mut engine = engine_for(&text, 22.0);
        let utf16_len = engine.utf16_len();

        for position in (0..utf16_len).step_by(97) {
            let caret = engine.caret_geometry(position).expect("caret geometry");
            let hit = engine
                .hit_test(caret.x + caret.width * 0.5, caret.y + caret.height * 0.5)
                .expect("hit test");
            assert!(
                hit.utf16_position.abs_diff(position) <= 1,
                "hit test at the caret for {position} returned {}",
                hit.utf16_position
            );
        }
    }

    #[test]
    fn renders_only_the_requested_visible_tile() {
        let text = "表示領域の周辺だけを描画する\n".repeat(60);
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 1, "the sample must span many blocks");
        let all = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0);
        assert!(all.len() > 2, "the sample must produce several tiles");
        let tile = all[1];

        let mut seen = Vec::new();
        engine
            .render_tiles(&[tile], None, |span, width, height, bgra| {
                seen.push((span, width, height, bgra.len()));
            })
            .expect("visible tile render");

        assert_eq!(seen.len(), 1);
        let (span, width, height, bytes) = seen[0];
        assert_eq!(span, tile);
        assert_eq!(
            (width, height),
            (tile.flow_size, engine.line_extent()),
            "a vertical tile is its flow extent wide and the pane tall"
        );
        assert_eq!(bytes, width as usize * height as usize * 4);
    }

    /// The horizontal tile is the same slice turned a quarter: as wide as the
    /// pane and as tall as the tile reaches along the flow axis.
    #[test]
    fn renders_a_horizontal_tile_across_the_pane() {
        let text = "横書きのタイルを描画する行です。\n".repeat(60);
        let mut engine = engine_in(WritingMode::Horizontal, &text, 22.0);
        assert!(engine.block_count() > 1, "the sample must span many blocks");
        let all = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0);
        assert!(all.len() > 2, "the sample must produce several tiles");
        let tile = all[1];

        let mut seen = None;
        let mut ink = 0;
        engine
            .render_tiles(&[tile], None, |span, width, height, bgra| {
                seen = Some((span, width, height, bgra.len()));
                ink = bgra
                    .chunks_exact(4)
                    .filter(|pixel| pixel[0] < 180 && pixel[1] < 180 && pixel[2] < 180)
                    .count();
            })
            .expect("horizontal tile render");

        let (span, width, height, bytes) = seen.expect("one tile rendered");
        assert_eq!(span, tile);
        assert_eq!(
            (width, height),
            (engine.line_extent(), tile.flow_size),
            "a horizontal tile is the pane wide and its flow extent tall"
        );
        assert_eq!(bytes, width as usize * height as usize * 4);
        assert!(ink > 100, "expected visible glyph pixels");
    }

    /// A bullet and a checkbox are one glyph apiece for every list in the
    /// document, but an ordered item draws the text its own box stands over:
    /// **`10.` says something `9.` does not** (要件 7.3.2). The trailing space
    /// goes — that space was the gap after the marker, and the gap is the box.
    #[test]
    fn the_ink_for_a_number_is_the_text_its_box_stands_over() {
        let text = "見出し\n10. 番号";
        let run = StyleRun {
            utf16_start: "見出し\n".encode_utf16().count() as u32,
            utf16_len: 4,
            heading_level: 0,
            marks: Marks::default(),
            ornament: Some(Ornament::Number),
        };

        assert_eq!(marker_ink(Ornament::Number, text, &run), "10.");
        assert_eq!(marker_ink(Ornament::Bullet, text, &run), "•");
        assert_eq!(marker_ink(Ornament::TaskOpen, text, &run), "☐");
        assert_eq!(marker_ink(Ornament::TaskDone, text, &run), "☑");
    }

    /// **A marker that closes changes what came before it**, so a paragraph's
    /// wrapping may be carried over only as far as its marks are unchanged
    /// (要件 7.3.2). Typing plain text moves no earlier mark, which is the case
    /// the reuse exists for (6.10).
    #[test]
    fn marks_are_agreed_on_up_to_the_first_that_moved() {
        let bold = |utf16_start, utf16_len| Emphasis {
            utf16_start,
            utf16_len,
            marks: Marks {
                bold: true,
                ..Marks::default()
            },
        };

        // The same table says the same thing throughout.
        assert_eq!(marks_agree_until(&[bold(4, 2)], &[bold(4, 2)]), u32::MAX);
        // One that moved is disagreed on from wherever it now begins.
        assert_eq!(marks_agree_until(&[bold(4, 2)], &[bold(6, 2)]), 4);
        // One that appeared is disagreed on from where it begins.
        assert_eq!(marks_agree_until(&[], &[bold(9, 2)]), 9);
        // An earlier one kept and a later one added: the earlier still agrees,
        // which is what lets an edit late in a paragraph keep its prefix.
        assert_eq!(
            marks_agree_until(&[bold(1, 2)], &[bold(1, 2), bold(9, 2)]),
            9
        );
    }

    /// A block covering nothing, set in by the given number of steps.
    fn indented_span(indent_steps: u8) -> BlockSpan {
        BlockSpan {
            byte_start: 0,
            byte_end: 0,
            utf16_start: 0,
            utf16_end: 0,
            indent_steps,
        }
    }

    /// An indented block is moved in from the margin by one step per level, and
    /// set in a box narrower by the same amount (要件 7.3.2). **Every line of
    /// it**, which is the whole reason the indent belongs to the block rather
    /// than to the head of a line.
    #[test]
    fn an_indented_block_is_set_in_by_one_step_a_level() {
        let typography = plain();
        let step = typography.indent_step();

        assert_eq!(block_inset(&indented_span(0), &typography), 0.0);
        assert_eq!(block_inset(&indented_span(1), &typography), step);
        assert_eq!(block_inset(&indented_span(2), &typography), step * 2.0);
        assert_eq!(block_extent(&indented_span(0), 800, &typography), 800);
        assert_eq!(
            block_extent(&indented_span(1), 800, &typography),
            800 - step as u32
        );
        // A pane narrower than the indent still leaves a box to lay out in.
        assert_eq!(block_extent(&indented_span(4), 10, &typography), 1);
    }

    /// The box over a line that is all marks is there to hide them and nothing
    /// else: what stands in their place is the line's, not the box's
    /// (要件 7.3.2).
    #[test]
    fn a_hidden_line_puts_no_ink_in_its_box() {
        assert!(!Ornament::Hidden.draws_ink());
        assert!(Ornament::Bullet.draws_ink());
        assert!(Ornament::Number.draws_ink());
    }

    /// Measure these on the laying-out threads.
    fn on_threads(tasks: &[MeasureTask]) -> Vec<(usize, BlockMeasure)> {
        let queued = tasks.iter().cloned().map(PoolTask::Measure).collect();
        on_layout_threads(queued)
            .expect("the laying-out threads started")
            .expect("measured on the threads")
            .into_iter()
            .filter_map(measured_answer)
            .collect()
    }

    /// Blocks enough to be dealt round-robin across every queue.
    fn measure_tasks(count: usize) -> Vec<MeasureTask> {
        let typography = Arc::new(plain());
        (0..count)
            .map(|index| {
                let text = format!("ブロック{index}の本文。長さはどれも同じである。\n");
                MeasureTask {
                    index,
                    runs: style_runs(StyledText::plain(&text)),
                    text,
                    typography: typography.clone(),
                    mode: WritingMode::Horizontal,
                    block_box: 400.0,
                    max_flow_size: 4000.0,
                    keep_trailing_empty_line: false,
                }
            })
            .collect()
    }

    /// **The threads have to answer exactly what this thread would** (要件 2).
    /// A block is measured from its own text and its own styling and nothing
    /// else (3.4), so where it was measured cannot show in the answer — and if
    /// it ever did, every coordinate below it in the document would be wrong
    /// while the document itself looked untouched.
    #[test]
    fn measuring_on_the_threads_answers_what_measuring_here_does() {
        let tasks = measure_tasks(60);

        let here = with_graphics(|graphics| {
            let mut done = Vec::with_capacity(tasks.len());
            for task in &tasks {
                done.push((task.index, measure_task(graphics, task)?.0));
            }
            Ok(done)
        })
        .expect("measured on this thread");
        let mut there = on_threads(&tasks);
        there.sort_by_key(|(index, _)| *index);

        assert_eq!(there.len(), here.len(), "every block came back");
        assert_eq!(there, here);
    }

    /// The answers arrive in whatever order the threads finish, so each has to
    /// say which block it is for. **Sorted here only to compare**: the editor
    /// puts each into the slot its index names.
    #[test]
    fn every_block_comes_back_once_and_says_which_it_is() {
        let tasks = measure_tasks(60);

        let mut answered = on_threads(&tasks)
            .into_iter()
            .map(|(index, _)| index)
            .collect::<Vec<usize>>();
        answered.sort_unstable();

        assert_eq!(answered, (0..60).collect::<Vec<usize>>());
    }

    /// The offset is in UTF-16 units, which is what a DirectWrite range is
    /// measured in: a character outside the basic plane is two of them, and a
    /// position inside the pair resolves to the byte after it — the same answer
    /// the preview's own table gives.
    #[test]
    fn a_utf16_offset_counts_a_surrogate_pair_as_two() {
        let text = "𠮷野家";

        assert_eq!(byte_at_utf16(text, 0), 0);
        assert_eq!(byte_at_utf16(text, 1), "𠮷".len());
        assert_eq!(byte_at_utf16(text, 2), "𠮷".len());
        assert_eq!(byte_at_utf16(text, 3), "𠮷野".len());
        assert_eq!(byte_at_utf16(text, 99), text.len());
    }

    /// 要件 7.3.2: an item's text begins one step in — **and so does every line
    /// it wraps to**, which is why the step is the block's indent rather than
    /// the width of a box at the head of the line (技術検証 7.1). The caret
    /// agrees because it is the same box the text is laid out in (4.12); a
    /// `leadingSpacing` would have moved the glyphs and left the caret behind
    /// (4.11).
    ///
    /// The box over the marker keeps only the other half of its old job. It
    /// hides the glyphs and takes no room, so the head of the line and the
    /// item's first character are the same place — a box that still took a step
    /// would set this first line two steps in while its continuations stayed at
    /// one.
    #[test]
    fn an_item_is_set_in_one_step_and_its_marker_takes_no_room() {
        let text = "- 箇条書き";
        let levels = [LineStyle::of_kind(LineKind::Bullet)];
        let bullet = LineMarker {
            utf16_len: 2,
            ornament: Ornament::Bullet,
        };
        let markers = [Some(bullet)];
        let typography = plain();
        let step = typography.indent_step();
        let boxed = StyledText::new(text, &levels).with_markers(&markers);
        // 要件 7.3.1: no markers is a source pane, which is not set in at all.
        let bare = StyledText::new(text, &levels);

        let mut with_box = engine_set(WritingMode::Horizontal, boxed, &typography);
        let mut without = engine_set(WritingMode::Horizontal, bare, &typography);

        let head = with_box.caret_geometry(0).expect("caret geometry");
        let bare_head = without.caret_geometry(0).expect("caret geometry");
        assert!(
            (head.x - bare_head.x - step).abs() < 1.0,
            "expected {step} of indent, got {head:?} against {bare_head:?}"
        );

        let item = with_box.caret_geometry(2).expect("caret geometry");
        assert!(
            (item.x - head.x).abs() < 1.0,
            "the marker's box took room: {item:?} against {head:?}"
        );
    }

    /// One visual line of a block, at a fixed pitch. Only where it starts and
    /// where it sits along the flow axis matter to a whole-line mark.
    fn visual_line(utf16_start: u32, flow_start: f32) -> LineInfo {
        LineInfo {
            utf16_start,
            utf16_len: 10,
            newline_len: 0,
            flow_start,
            flow_size: 20.0,
        }
    }

    /// A block of visual lines at a fixed pitch, placed at the origin. The
    /// placed extent is given on its own, because it is what a mark reaching
    /// the block's edge is snapped to.
    fn placed(starts: &[u32], flow_size: f32) -> BlockPlacement {
        BlockPlacement {
            span: indented_span(0),
            flow_start: 0.0,
            flow_size,
            exact_flow_size: flow_size,
            content_flow_start: 0.0,
            max_flow_size: flow_size,
            lines: starts
                .iter()
                .enumerate()
                .map(|(index, start)| visual_line(*start, index as f32 * 20.0))
                .collect(),
        }
    }

    /// A wrapped logical line is several visual lines, and the mark drawn over
    /// it has to reach across all of them (要件 7.3.2).
    #[test]
    fn a_whole_line_mark_covers_every_visual_line_it_wrapped_to() {
        let block = placed(&[0, 10, 20], 60.0);
        let run = LineRun {
            utf16_start: 0,
            utf16_len: 24,
            ornament: LineOrnament::Rule,
            own_ends: (true, true),
        };

        let flow = mark_extent(&block, &run).expect("a mark");
        assert_eq!(flow, (0.0, 60.0));
    }

    /// And the line after it is not covered, however close it sits. The visual
    /// lines are matched by where they start, which is the only thing that says
    /// which logical line they came from.
    #[test]
    fn a_whole_line_mark_stops_at_the_end_of_its_own_line() {
        let block = placed(&[0, 10, 20], 60.0);
        let run = LineRun {
            utf16_start: 10,
            utf16_len: 5,
            ornament: LineOrnament::Quote { depth: 1 },
            own_ends: (true, true),
        };

        let flow = mark_extent(&block, &run).expect("a mark");
        assert_eq!(flow, (20.0, 40.0));
    }

    /// **A mark that runs to the block's edge is snapped to the placed edge.**
    /// `place_blocks` rounds each block's extent on its own, so the sum of its
    /// lines falls short of it; two blocks carrying one mark would leave that
    /// fraction of a pixel of paper showing through the seam, or paint it
    /// twice. Here the lines sum to 40 and the block was placed at 41.
    #[test]
    fn a_mark_reaching_the_block_edge_is_snapped_to_it() {
        let block = placed(&[0, 10], 41.0);
        let run = LineRun {
            utf16_start: 0,
            utf16_len: 14,
            ornament: LineOrnament::Code,
            own_ends: (true, true),
        };

        let flow = mark_extent(&block, &run).expect("a mark");
        assert_eq!(flow, (0.0, 41.0));
    }

    /// **Which edge a mark reaches is a coordinate, not a line number.**
    /// Reading order runs the other way along the flow axis in a vertical pane,
    /// so the block's first line sits at its far end: a mark holding that line
    /// reaches the far edge, and testing the index instead stretched a code
    /// block's ground over everything past it.
    #[test]
    fn a_mark_is_snapped_by_where_it_reaches_not_by_the_line_it_holds() {
        let mut block = placed(&[0, 10], 41.0);
        // The vertical pane's order: line 0 furthest along, line 1 nearer.
        block.lines = [visual_line(0, 20.0), visual_line(10, 0.0)]
            .into_iter()
            .collect();
        let run = LineRun {
            utf16_start: 0,
            utf16_len: 5,
            ornament: LineOrnament::Code,
            own_ends: (true, true),
        };

        let flow = mark_extent(&block, &run).expect("a mark");
        assert_eq!(flow, (20.0, 41.0));
    }

    /// **A box must not make its line taller.** It takes no width any more —
    /// the indent is the block's (要件 7.3.2) — so the height and the baseline
    /// it reports are all it can still get wrong, and both sit inside what the
    /// text on the line already asks for.
    ///
    /// **Both sides are previews**, differing only in whether the marker ranges
    /// got an inline object. Comparing against a source pane would compare two
    /// different splits as well — that one is not set in, so its lines are one
    /// block where these are two — and `place_blocks` rounds each block on its
    /// own, so the totals would part company by a pixel for a reason that has
    /// nothing to do with boxes.
    #[test]
    fn a_marker_box_does_not_change_the_flow_size() {
        let text = "- 箇条書き\n- もう一行\n本文";
        let levels = [
            LineStyle::of_kind(LineKind::Bullet),
            LineStyle::of_kind(LineKind::Bullet),
            LineStyle::default(),
        ];
        let bullet = LineMarker {
            utf16_len: 2,
            ornament: Ornament::Bullet,
        };
        let markers = [Some(bullet), Some(bullet), None];
        // 要件 7.3.1: a preview whose items are all the caret's line, which is
        // the one place a marker really does go boxless.
        let unboxed = [None; 3];
        let typography = plain();
        let boxed = StyledText::new(text, &levels).with_markers(&markers);
        let bare = StyledText::new(text, &levels).with_markers(&unboxed);

        let with_box = engine_set(WritingMode::Horizontal, boxed, &typography);
        let without = engine_set(WritingMode::Horizontal, bare, &typography);

        assert_eq!(with_box.total_flow_size(), without.total_flow_size());
    }

    /// Hit testing and caret geometry must agree in horizontal writing too, which
    /// is the round trip that says the flow axis was mapped onto screen y
    /// consistently in both directions.
    #[test]
    fn horizontal_hit_testing_agrees_with_caret_geometry() {
        let paragraph = "横書きのヒットテスト検証。日本語ABC123と句読点、を含む段落です。\n\n";
        let text = paragraph.repeat(30);
        let mut engine = engine_in(WritingMode::Horizontal, &text, 22.0);
        let utf16_len = engine.utf16_len();

        for position in (0..utf16_len).step_by(97) {
            let caret = engine.caret_geometry(position).expect("caret geometry");
            let hit = engine
                .hit_test(caret.x + caret.width * 0.5, caret.y + caret.height * 0.5)
                .expect("hit test");
            assert!(
                hit.utf16_position.abs_diff(position) <= 1,
                "hit test at the caret for {position} returned {}",
                hit.utf16_position
            );
        }
    }

    /// Down moves on to the next line in horizontal writing, where left moved on
    /// in vertical writing. The mode is what decides which, so the arrow keys
    /// keep their screen meaning in both panes.
    #[test]
    fn a_downward_move_reaches_the_next_horizontal_line() {
        let text = "一行目です\n二行目です";
        let mut engine = engine_in(WritingMode::Horizontal, text, 24.0);
        let first_line_caret = "一行".encode_utf16().count() as u32;
        let second_line_start = "一行目です\n".encode_utf16().count() as u32;

        let down = engine
            .move_caret_by_line(first_line_caret, 1, None)
            .expect("move down a line");
        assert!(
            down.utf16_position >= second_line_start,
            "the down arrow must reach the second line, got {}",
            down.utf16_position
        );

        let up = engine
            .move_caret_by_line(down.utf16_position, -1, None)
            .expect("move back up a line");
        assert!(
            up.utf16_position < second_line_start,
            "the up arrow must return to the first line, got {}",
            up.utf16_position
        );
    }

    #[test]
    fn draws_glyph_pixels_into_a_tile() {
        let mut engine = engine_for("全角：ＡＢＣ１２３\n半角：ABC123\n句読点。（）「」", 24.0);

        // A short document is one block, and that block is one tile.
        let all = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0);
        assert_eq!(all.len(), 1);
        let mut ink = 0;
        engine
            .render_tiles(&all, None, |_, _, _, bgra| {
                ink = bgra
                    .chunks_exact(4)
                    .filter(|pixel| pixel[0] < 180 && pixel[1] < 180 && pixel[2] < 180)
                    .count();
            })
            .expect("tile render");

        assert!(ink > 100, "expected visible glyph pixels");
    }

    /// The point of the per-tile fingerprint: an edit redraws the tiles that
    /// show it and leaves the rest of the document's tiles alone.
    ///
    /// This is the easy case: one full-width character swapped for another,
    /// which keeps every column and every block width exactly where it was.
    #[test]
    fn an_edit_redraws_its_own_tile_and_no_other() {
        let paragraph = "タイル署名の確認用の段落です。日本語ABC123を含みます。\n\n";
        let text = paragraph.repeat(40);
        let mut engine = engine_for(&text, 22.0);
        assert!(
            engine.block_count() > 2,
            "need several blocks to tell apart"
        );

        // Block 0 holds the first paragraph and sits at the far right.
        let all = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0);
        let edited_tile = *all.first().expect("a tile at the right edge");
        let distant_tile = *all.last().expect("a tile at the left edge");
        assert_ne!(
            edited_tile.block_index, distant_tile.block_index,
            "the tiles must show different blocks"
        );
        let edited_before = engine.tile_signature(edited_tile, None);
        let distant_before = engine.tile_signature(distant_tile, None);
        let width_before = engine.total_flow_size();

        let edited = text.replacen("確認", "検証", 1);
        assert_eq!(
            edited.len(),
            text.len(),
            "the swap must not resize the text"
        );
        update_plain(&mut engine, &edited);

        assert_eq!(
            engine.total_flow_size(),
            width_before,
            "swapping one ideograph for another must not move any column"
        );
        assert_ne!(
            engine.tile_signature(edited_tile, None),
            edited_before,
            "the tile showing the edit must be redrawn"
        );
        assert_eq!(
            engine.tile_signature(distant_tile, None),
            distant_before,
            "a tile whose block did not change must keep its signature"
        );
    }

    /// The hard case, and the reason tiles were moved into blocks.
    ///
    /// Pressing Enter adds a column, so the document gets wider and every block
    /// before the edit slides right. Under a single grid laid over the document
    /// one anchor or the other had to lose: anchored at the right edge the
    /// blocks after the edit stayed put while the grid moved out from under
    /// them, so all ten tiles on screen were redrawn for one keystroke. A tile
    /// cut out of a block carries no coordinate, so every block whose text
    /// survived keeps its pixels — on both sides of the edit.
    #[test]
    fn adding_a_column_keeps_the_tiles_of_every_unchanged_block() {
        // Every line carries its section number, so no two are alike. Block
        // boundaries are chosen from the text of a line, and identical lines all
        // decide the same way: a document of repeated paragraphs offers nothing
        // to key on, falls back on the maximum block size for every boundary,
        // and then an inserted line shifts all of them and no block to the left
        // of the edit survives to be checked at all.
        let text = (0..24)
            .map(|n| {
                format!(
                    "## 第{n}節\n\n改行で列が増える場合を確かめる段落{n}です。日本語ABC123を含みます。\n\n短い行{n}。\n\n"
                )
            })
            .collect::<String>();
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 2, "need several blocks");

        let block_text = |engine: &TextEngine, index: usize| {
            let span = engine.plan.blocks[index].span;
            engine.text[span.byte_start..span.byte_end].to_owned()
        };
        let texts_before = (0..engine.block_count())
            .map(|index| block_text(&engine, index))
            .collect::<Vec<_>>();
        let before = engine
            .visible_tiles(0.0, engine.total_flow_size() as f32, 0)
            .into_iter()
            .map(|tile| (tile, engine.tile_signature(tile, None)))
            .collect::<Vec<_>>();
        let width_before = engine.total_flow_size();

        // Enter at the start of a section, halfway down the document. Breaking a
        // line in the middle instead would not reliably add anything: a line
        // that already wraps over two columns still needs two after the split.
        // An empty line is always exactly one more column.
        let cut = text
            .match_indices("## 第")
            .nth(12)
            .expect("a section in the middle")
            .0;
        let edited = format!("{}\n{}", &text[..cut], &text[cut..]);
        let edited_block = engine
            .plan
            .block_at_utf16(text[..cut].encode_utf16().count() as u32);
        update_plain(&mut engine, &edited);

        assert!(
            engine.total_flow_size() > width_before,
            "an empty line must add a column and widen the document: \
             {width_before}px then {}px",
            engine.total_flow_size()
        );
        let after = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0);

        // Only blocks whose text genuinely survived are claimed here, so that a
        // boundary that did shift shows up as a missing survivor below rather
        // than as a confusing failure here.
        let (mut kept_right, mut kept_left) = (0, 0);
        for (tile, signature) in &before {
            let Some(now) = after.iter().find(|candidate| {
                (candidate.block_index, candidate.sub_index) == (tile.block_index, tile.sub_index)
            }) else {
                continue;
            };
            if now.block_index >= engine.block_count()
                || block_text(&engine, now.block_index) != texts_before[now.block_index]
            {
                continue;
            }
            assert_eq!(
                engine.tile_signature(*now, None),
                *signature,
                "block {} kept its text but lost its tile",
                now.block_index
            );
            if now.block_index < edited_block {
                // Their x is the sum of everything to their left, which now
                // includes one more column.
                assert!(
                    now.flow_start > tile.flow_start,
                    "blocks before the edit follow the right edge"
                );
                kept_right += 1;
            } else if now.block_index > edited_block {
                // Nothing is claimed about their x. A boundary that shifted can
                // resize a block further left and slide this one along with it.
                // Where the pixels go is not what is under test; that they are
                // the same pixels is, and the signature above says so.
                kept_left += 1;
            }
        }
        assert!(
            kept_right > 0,
            "no surviving tile to the right of the edit, where every block moved"
        );
        assert!(
            kept_left > 0,
            "no surviving tile to the left of the edit, where the old right-anchored grid lost them"
        );
    }

    #[test]
    fn selection_rectangles_stay_inside_the_viewport() {
        let text = "選択範囲の描画を確認する段落です。\n\n".repeat(40);
        let mut engine = engine_for(&text, 22.0);
        let content_width = engine.total_flow_size() as f32;
        let visible = visible_flow_range(0.0, 640.0, content_width);

        let rects = engine
            .selection_rects(Some((0, engine.utf16_len())), visible)
            .expect("selection rectangles");

        assert!(!rects.is_empty());
        for rect in &rects {
            assert!(rect.right > rect.left && rect.bottom > rect.top);
        }
        let leftmost = rects.iter().fold(f32::MAX, |low, rect| low.min(rect.left));
        assert!(
            leftmost >= visible.0 - 200.0,
            "a document-wide selection must not produce rectangles far outside the viewport"
        );
    }

    #[test]
    fn horizontal_move_uses_the_neighboring_visual_column() {
        let text = "右列\n左列";
        let mut engine = engine_for(text, 24.0);
        let left_column_caret = "右列\n左".encode_utf16().count() as u32;

        let moved = engine
            .move_caret_by_line(left_column_caret, 1, None)
            .expect("move to the visual right column");
        assert!(
            moved.utf16_position < "右列\n".encode_utf16().count() as u32,
            "right arrow should move from the left column into the right column"
        );

        let back = engine
            .move_caret_by_line(moved.utf16_position, -1, None)
            .expect("move back to the visual left column");
        assert!(
            back.utf16_position >= "右列\n".encode_utf16().count() as u32,
            "left arrow should move from the right column into the left column"
        );
    }

    #[test]
    fn home_and_end_move_to_the_current_vertical_column_edges() {
        let text = "一二三四五";
        let engine = engine_for(text, 24.0);
        let caret = "一二".encode_utf16().count() as u32;

        assert_eq!(engine.move_caret_to_line_edge(caret, false), 0);
        assert_eq!(
            engine.move_caret_to_line_edge(caret, true),
            text.encode_utf16().count() as u32
        );
    }

    #[test]
    fn keeps_preferred_height_when_crossing_an_empty_column() {
        let text = "一二三四五六七八九十\n\n甲乙丙丁戊己庚辛壬癸";
        let mut engine = engine_for(text, 22.0);
        let start = "一二三四五六七八".encode_utf16().count() as u32;
        let empty_column_start = "一二三四五六七八九十\n".encode_utf16().count() as u32;
        let third_column_start = "一二三四五六七八九十\n\n".encode_utf16().count() as u32;
        let preferred_y = engine.caret_geometry(start).expect("caret geometry").y;

        let empty = engine
            .move_caret_by_line(start, -1, Some(preferred_y))
            .expect("move into the empty column");
        assert_eq!(
            empty.utf16_position, empty_column_start,
            "the first left move should enter the empty logical line"
        );

        let third = engine
            .move_caret_by_line(empty.utf16_position, -1, Some(preferred_y))
            .expect("move across the empty column");
        assert!(
            third.utf16_position >= third_column_start,
            "the second left move should reach the third logical line"
        );
        let geometry = engine
            .caret_geometry(third.utf16_position)
            .expect("third column caret geometry");
        assert!(
            (geometry.y - preferred_y).abs() <= 22.0,
            "the empty column must not reset the preferred visual height"
        );
    }

    /// An edit must re-measure the block it touched and nothing else.
    #[test]
    fn an_edit_remeasures_only_the_changed_block() {
        let paragraph = "増分更新の確認用の段落です。日本語ABC123を含みます。\n\n";
        let text = paragraph.repeat(40);
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 2);

        let mut edited = text.clone();
        edited.insert_str(0, "あ");
        let blocks = engine.block_count();
        let measured = update_plain(&mut engine, &edited).blocks;

        assert!(
            measured <= 2,
            "an edit re-measured {measured} of {blocks} blocks; only the block \
             holding the edit (and at most the one its boundary shifted into) should change"
        );
    }

    /// The regression that made cutting slower than not cutting (6.10).
    /// The wrap search built a layout for every long paragraph on every update,
    /// so a keystroke anywhere in the document paid to lay all of them out — a
    /// floor of 205ms on the measurement sample, under an edit of one
    /// character.
    ///
    /// A paragraph whose text has not changed wraps where it wrapped before,
    /// and must not be laid out again to find that out.
    #[test]
    fn an_edit_elsewhere_does_not_lay_a_long_paragraph_out_again() {
        let long = "日本語ABCと句読点、を含む長い段落である。".repeat(120);
        let text = format!("短い行\n{long}\n末尾の行\n");
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 3, "the paragraph must have been cut");

        let edited = text.replace("短い行", "短い行を編集");
        let cost = update_plain(&mut engine, &edited);

        assert_eq!(
            cost.wrapped, 0,
            "an edit outside the paragraph laid it out again"
        );
        assert!(
            cost.utf16 < 1_000,
            "an edit outside the paragraph re-measured {} units",
            cost.utf16
        );
    }

    /// The other half: an edit *inside* the paragraph does lay it out again,
    /// because its wrap positions have genuinely moved.
    #[test]
    fn an_edit_inside_a_long_paragraph_lays_it_out_again() {
        let long = "日本語ABCと句読点、を含む長い段落である。".repeat(120);
        let text = format!("短い行\n{long}\n末尾の行\n");
        let mut engine = engine_for(&text, 22.0);

        let edited = text.replace("日本語ABCと句読点", "日本語ABCと句読点を編集");
        let cost = update_plain(&mut engine, &edited);

        assert!(cost.wrapped > 0, "the changed paragraph must be re-wrapped");
    }

    /// The asymmetry, at the engine: an edit near the end of a long paragraph
    /// only re-wraps from the edit onwards, because everything before it is
    /// decided by text that has not changed (6.9).
    #[test]
    fn an_edit_late_in_a_long_paragraph_only_re_wraps_its_tail() {
        let sentence = "日本語ABCと句読点、を含む長い段落である。";
        let long = sentence.repeat(400);
        let text = format!("{long}\n");
        let mut engine = engine_for(&text, 22.0);

        // In the last twentieth, on a sentence boundary so the edit is a plain
        // insertion rather than a change of the repeated text everywhere.
        let cut = text.char_indices().nth(long.chars().count() * 19 / 20);
        let cut = cut.expect("long enough").0;
        let edited = format!("{}編集{}", &text[..cut], &text[cut..]);
        let cost = update_plain(&mut engine, &edited);

        let whole = text.encode_utf16().count() as u32;
        assert!(
            cost.wrapped * 5 < whole,
            "an edit in the last twentieth re-wrapped {} of {whole} units",
            cost.wrapped
        );
    }

    /// The shape the measurement sample actually has: several long paragraphs,
    /// not one. An edit late in one of them must carry on from that paragraph's
    /// own earlier wrapping, and must leave the others matched whole.
    ///
    /// The single-paragraph case passed while the editor still laid a whole
    /// paragraph out on most keystrokes (6.10), so one paragraph was not enough
    /// to hold the reuse honest.
    #[test]
    fn an_edit_among_several_long_paragraphs_reuses_the_right_one() {
        let sentences = [
            "日本語ABCと句読点、を含む長い段落である。",
            "縦書きの組版では、行が右から左へ積み上がっていく。",
            "折り返しの位置は内容によって決まるものである。",
        ];
        let mut text = String::new();
        let mut ends = Vec::new();
        for (index, sentence) in sentences.iter().enumerate() {
            text.push_str(&sentence.repeat(150 * (index + 1)));
            ends.push(text.len());
            text.push('\n');
        }
        let mut engine = engine_for(&text, 22.0);
        assert!(engine.block_count() > 10, "every paragraph must be cut");

        // Late in the second paragraph, on a sentence boundary. The other two
        // are untouched and the first half of this one is as well.
        let cut = ends[1] - sentences[1].len() * 2;
        let edited = format!("{}編集{}", &text[..cut], &text[cut..]);
        let cost = update_plain(&mut engine, &edited);

        assert_eq!(cost.wrap_asked, 3, "every long paragraph is asked about");
        assert_eq!(cost.wrap_exact, 2, "the untouched paragraphs match whole");
        assert_eq!(cost.wrap_resumed, 1, "the edited paragraph must carry on");
        let whole = text.encode_utf16().count() as u32;
        assert!(
            cost.wrapped * 8 < whole,
            "{} of {whole} units were laid out again",
            cost.wrapped
        );
    }

    /// **A paragraph of any length ends up in bounded blocks.**
    ///
    /// The guarantee asked for is that reaching a maximum must not produce
    /// something unknown — no crash, no clipped text, no layout at
    /// DirectWrite's own limit. Finding the wrap positions a window at a time
    /// is what delivers it: stopping after one window would leave the rest of
    /// the paragraph in a single block, and that block's own layout box would
    /// then be as long as the tail.
    ///
    /// A window is a *distance* along the flow axis, so a large font makes one
    /// only a few hundred characters. That is how this crosses several windows
    /// while laying out about a thousand lines — the first version of this test
    /// used a narrow pane instead, which crosses windows by producing tens of
    /// thousands of lines, and lines are what laying out costs.
    #[test]
    fn a_paragraph_far_past_one_window_is_still_cut_into_bounded_blocks() {
        // 2 characters to a line, and a window of about 900 characters.
        let huge = Typography::new(200.0);
        let text = format!("{}\n", "あ".repeat(3_000));
        let mut engine = TextEngine::new(WritingMode::Vertical);
        engine
            .update(StyledText::plain(&text), 1_000, &huge)
            .expect("a very long paragraph must lay out");

        assert!(
            engine.block_count() >= 4,
            "the paragraph must be cut throughout, not left as {} block(s)",
            engine.block_count()
        );
        let largest = engine.largest_block_utf16();
        let whole = text.encode_utf16().count() as u32;
        assert!(
            largest * 3 < whole,
            "one block still holds {largest} of {whole} units"
        );
        // Covered exactly once, so nothing was dropped at a window boundary.
        let mut cursor = 0;
        for block in &engine.plan.blocks {
            assert_eq!(block.span.utf16_start, cursor, "a gap at a window edge");
            cursor = block.span.utf16_end;
        }
        assert_eq!(cursor, whole);
    }

    /// Reusing the earlier wrap positions must not change where the text ends
    /// up. This is the same invariant as
    /// `a_cut_paragraph_sums_to_the_single_layout`, asked of an engine that took
    /// the shortcut rather than one that laid the paragraph out in full.
    ///
    /// The margin (`WRAP_REUSE_MARGIN`) exists for this test to hold: a break
    /// right before the edit can move, because Japanese line breaking looks at
    /// what follows a break as well as what precedes it.
    #[test]
    fn reusing_earlier_wraps_still_matches_one_layout() {
        let sentence = "日本語ABC123と句読点、括弧（かっこ）「鉤括弧」を含む段落である。";
        let text = format!("{}\n", sentence.repeat(160));
        let mut engine = engine_for(&text, 22.0);

        // Edited at several depths, each starting from the wrapping the one
        // before it left behind, so the reuse compounds.
        for twentieth in [19, 15, 11, 7, 3] {
            let characters = text.chars().count() * twentieth / 20;
            let cut = text.char_indices().nth(characters);
            let cut = cut.expect("long enough").0;
            let edited = format!("{}編集{}", &text[..cut], &text[cut..]);
            update_plain(&mut engine, &edited);

            let label = format!("edited at {twentieth}/20");
            let styled = StyledText::plain(&edited);
            assert_engine_matches_one_layout(&engine, styled, &plain(), &label);
        }
    }

    #[test]
    fn a_repeated_update_measures_nothing() {
        let text = "同じ内容での更新\n\n本文\n";
        let mut engine = engine_for(text, 22.0);

        assert_eq!(update_plain(&mut engine, text), UpdateCost::default());
    }

    #[test]
    fn intersects_a_global_range_with_a_block() {
        assert_eq!(block_local_range(10, 20, 12, 18), Some((2, 6)));
        assert_eq!(block_local_range(10, 20, 0, 15), Some((0, 5)));
        assert_eq!(block_local_range(10, 20, 15, 40), Some((5, 5)));
        assert_eq!(block_local_range(10, 20, 20, 30), None);
        assert_eq!(block_local_range(10, 20, 0, 10), None);
    }
}
