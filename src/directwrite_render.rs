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
                DWRITE_READING_DIRECTION_TOP_TO_BOTTOM, DWRITE_TEXT_ALIGNMENT_CENTER,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS,
                DWRITE_TEXT_RANGE, DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection,
                IDWriteInlineObject, IDWriteInlineObject_Impl, IDWriteLocalizedStrings,
                IDWriteTextFormat, IDWriteTextFormat3, IDWriteTextLayout, IDWriteTextLayout1,
                IDWriteTextRenderer,
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

use crate::terminal::{Attrs as CellAttrs, Color as CellColor, Line as CellLine, character_width};
use crate::text_blocks::{
    Align, AskedLine, BlockLayoutPlan, BlockMeasure, BlockPlacement, BlockSpan, CrossSlices,
    DEFAULT_INK, Emphasis, FlowOrder, GridCell, LineInfo, LineKind, LineMarker, LineOrnament,
    LineRun, LineStyle, LongLine, MAX_HEADING_LEVEL, Marks, Ornament, PreparedWraps, RecordedWraps,
    StyleRun, StyledText, TableGrid, TileSpan, Typography, block_flow_bound, cells_per_line,
    line_runs, place_blocks, split_blocks, style_runs, table_alignments, table_cells, tables,
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
/// How far a tile may reach **across** the flow (要件 9).
///
/// Wide enough that every page that fits a pane is one slice — so a wrapped
/// document is cut exactly as it was before a line could be longer than its
/// pane — and small enough that one tile stays about two megabytes whatever the
/// writer sets the line length to.
const MAX_TILE_CROSS: u32 = 2048;
/// The box a line is laid out in when it is not wrapped (要件 9).
///
/// **A number rather than an absence**: DirectWrite lays text out into a box,
/// and "no wrapping" is a box no line reaches the end of. A million pixels is
/// forty thousand characters of 24px body text on one line — past that the line
/// wraps, and a document with a line that long has other troubles.
const FREE_LINE_BOX: f32 = 1_000_000.0;

/// How long a line may be (要件 9).
///
/// **The engine is told which of the two it is, not handed a very large
/// number.** The difference is not the size: a line that is wrapped puts the
/// page's width in, and a line that is not takes the page's width out — the
/// longest line the document holds is then what the reader scrolls across.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineFit {
    /// Wrapped inside this many pixels across the flow, margins included.
    Extent(u32),
    /// Not wrapped. Every logical line is one line, however long.
    Free,
}

impl Default for LineFit {
    /// An engine nothing has been laid out in yet. The first `update` replaces
    /// it, and a zero-wide line is not one anybody can write on — which is what
    /// an engine holding no document is.
    fn default() -> Self {
        Self::Extent(0)
    }
}

impl LineFit {
    /// The box one block is laid out in, its own indent taken off.
    fn line_box(self, margin: f32, inset: f32) -> f32 {
        match self {
            Self::Extent(extent) => (extent as f32 - margin * 2.0 - inset).max(1.0),
            Self::Free => FREE_LINE_BOX,
        }
    }

    /// The width to charge the split with (`cells_per_line`). A free line is
    /// charged at the box it is given, which is to say **one line per logical
    /// line** — which is what not wrapping means.
    fn charged_extent(self) -> u32 {
        match self {
            Self::Extent(extent) => extent,
            Self::Free => FREE_LINE_BOX as u32,
        }
    }
}
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
    /// And the one a comment inside code is drawn in (要件 7.3.2).
    comment_brush: ID2D1SolidColorBrush,
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
    /// The terminal's formats (追加要件 Terminal), keyed by size, family and
    /// weight. **Kept apart from the document's**: a terminal's format has no
    /// writing mode to speak of and no line spacing — a cell grid decides its
    /// own line advance — so it shares nothing with the ones above but the
    /// factory that made them.
    cell_formats: HashMap<(u32, String, bool), IDWriteTextFormat>,
    /// The terminal's cell size, and what it was measured for (追加要件
    /// Terminal). **Every wheel notch and every chunk of output asks for it**,
    /// and building a layout to answer is the same work each time.
    cell_size: Option<((u32, String), cells::CellSize)>,
    target: Option<RenderTargetCache>,
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
                cell_formats: HashMap::new(),
                cell_size: None,
                target: None,
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
            let (bitmap, target, brush, heading_brushes, comment_brush) = unsafe {
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
                let comment_brush = target.CreateSolidColorBrush(&colour(DEFAULT_INK), None)?;
                (bitmap, target, brush, heading_brushes, comment_brush)
            };
            self.target = Some(RenderTargetCache {
                width,
                height,
                bitmap,
                target,
                brush,
                heading_brushes,
                comment_brush,
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
    /// which is the one thing 4.12 had to be asked rather than assumed. That
    /// holds only because the box says it cannot lie sideways; see
    /// `GetMetrics` below.
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
            // **Saying no here is what gives `width` one meaning** (技術検証
            // 4.14). Down a column, a sideways-capable box that lands on an
            // upright character is stood upright with it, and upright it is
            // the box's `height` that runs along the line — so the same box
            // advanced 43 across the page and 22 down a column. A table's
            // boxes land on upright characters whenever a cell's tail is cut,
            // which is why only shrunken tables came out wrong.
            //
            // It is not a trick: this box draws nothing, so it has no sideways
            // form to offer, and the honest answer is the one that makes the
            // two directions agree.
            supportsSideways: false.into(),
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
    // A whole-line box keeps its width (`keeps_room`). It stands over a line
    // that is nothing but marks — `---`, or a fence — where the room is what
    // the line leaves behind, not an indent for anything after it.
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
        // 要件 7.3.2: **a table's boxes are the ones built per run.** Every
        // marker begins its text at the same step and can share one object; no
        // two cells of a table can, because what the box holds is what is left
        // of the column before it — and the box over the delimiter row is as
        // wide as the whole table (技術検証 7.7).
        let object = if ornament.keeps_room() {
            &whole_line
        } else {
            &marker
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

/// Which of the document's logical lines each block covers.
///
/// A block ends just after a newline, so it holds exactly as many logical lines
/// as it has newlines — except a last block that does not end in one, which
/// holds one more. **The cursor advances by the breaks and not by the lines
/// covered**: a piece cut out of the middle of a long line covers that line
/// without finishing it, so the next piece is still on the same one.
///
/// Counted forwards once for the whole document rather than looked up per
/// block, which costs one pass over the text and not one scan per block.
fn block_line_ranges(text: &str, spans: &[BlockSpan]) -> Vec<Range<usize>> {
    let mut ranges = Vec::with_capacity(spans.len());
    let mut cursor = 0;
    for span in spans {
        let block_text = &text[span.byte_start..span.byte_end];
        let breaks = block_text.matches('\n').count();
        let lines = if block_text.ends_with('\n') {
            breaks
        } else {
            breaks + 1
        };
        ranges.push(cursor..cursor + lines);
        cursor += breaks;
    }
    ranges
}

/// One block's text with its own slice of the document's per-line attributes.
///
/// **The boxes come too.** This is the styling the block is measured with and
/// the styling it is drawn with; a block measured without its boxes would be
/// placed at a size it is not shown at.
fn block_styling<'a>(
    styled: StyledText<'a>,
    span: &BlockSpan,
    lines: &Range<usize>,
) -> StyledText<'a> {
    let text = &styled.text[span.byte_start..span.byte_end];
    let count = lines.end - lines.start;
    let levels = styled.lines.get(lines.start..).unwrap_or(&[]);
    let levels = &levels[..count.min(levels.len())];
    let spans = styled.spans.get(lines.start..).unwrap_or(&[]);
    let spans = &spans[..count.min(spans.len())];
    let markers = styled.markers.get(lines.start..).unwrap_or(&[]);
    let markers = &markers[..count.min(markers.len())];
    // 要件 7.3.1: and which of this block's lines is the one shown as its own
    // source, counted from this block's first line like everything else here.
    let source_line = styled
        .source_line
        .and_then(|line| line.checked_sub(lines.start))
        .filter(|line| *line < count);
    StyledText::marked(text, levels, spans)
        .with_markers(markers)
        .with_source_line(source_line)
}

/// How much paper a table's cell keeps above and below its text, as a fraction
/// of the font size (要件 7.3.2).
///
/// **Half an em on each side**, which is what makes a boxed cell read as a cell
/// rather than as text with a line through it.
const TABLE_CELL_PAD: f32 = 0.5;

/// How wide the gap between two columns of a table is, as a fraction of the
/// font size (要件 7.3.2).
///
/// **One character of the body face.** It is the gap a writer already leaves by
/// hand when writing ` | `, and it reads as a boundary between two columns
/// without a rule drawn between them.
const TABLE_GUTTER: f32 = 1.0;

/// The box a cell is measured in: wide enough that nothing wraps inside it.
///
/// A cell is measured on a line of its own, and what is wanted is the width its
/// text asks for — so the box must never be the thing that decides it.
const CELL_BOX: f32 = 1.0e6;

/// How many UTF-16 units `text` is.
fn utf16_units(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// One cell's text laid out on a line of its own, in a box `room` across
/// (要件 7.3.2).
///
/// **The spec a cell is measured with is the spec it is drawn with.** A cell
/// measured plain and drawn bold is a column that does not line up — the same
/// rule that made `WrapPoints::line_starts` take a whole `LongLine` (6.10).
fn cell_layout(
    graphics: &Graphics,
    format: &IDWriteTextFormat,
    typography: &Typography,
    mode: WritingMode,
    text: &str,
    runs: &[StyleRun],
    room: f32,
    flow_room: f32,
) -> Result<IDWriteTextLayout> {
    let utf16 = text.encode_utf16().collect::<Vec<u16>>();
    // The box is given on the two axes and handed over as a width and a height,
    // which way round depending on the mode.
    //
    // **The flow bound matters down a column.** A vertical layout sets its
    // first line against the far edge of its box, so a cell measured in a box
    // wide enough for anything is drawn a box's width away from where it
    // belongs. Asking for room enough and no more puts the text at the near
    // edge, which is where the cell is.
    let (max_width, max_height) = mode.to_screen(flow_room, room);
    // SAFETY: The UTF-16 buffer stays alive across CreateTextLayout, and the
    // layout owns everything it needs afterwards.
    let layout = unsafe {
        graphics
            .dwrite
            .CreateTextLayout(&utf16, format, max_width, max_height)?
    };
    apply_typography(&layout, typography, runs, utf16.len() as u32)?;
    Ok(layout)
}

/// How far one cell's text reaches along the line axis, given room enough for
/// all of it.
///
/// **DirectWrite answers in screen terms, so which of the two is the answer
/// depends on the mode** — a cell's text runs across the screen in a horizontal
/// pane and down it in a vertical one. Reading the width in both left every
/// cell measured as the thickness of its own line, which is the same for all of
/// them: the columns then had nothing to correct by, and each row's second
/// column began wherever its first cell happened to end.
fn cell_along(
    graphics: &Graphics,
    format: &IDWriteTextFormat,
    typography: &Typography,
    mode: WritingMode,
    text: &str,
    runs: &[StyleRun],
) -> Result<f32> {
    if text.is_empty() {
        return Ok(0.0);
    }
    let layout = cell_layout(
        graphics, format, typography, mode, text, runs, CELL_BOX, CELL_BOX,
    )?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: The layout is alive and the metrics are written into our own
    // storage.
    unsafe { layout.GetMetrics(&mut metrics)? };
    let (_, along) = mode.to_axes(metrics.width, metrics.height);
    Ok(along)
}

/// How far one cell's text reaches along the **flow** axis when it is set in a
/// column `room` across — which is to say how tall the cell is (要件 7.3.2).
///
/// **This is where a long cell wraps.** The column is the box; DirectWrite
/// breaks the text inside it and answers with the height that took.
fn cell_flow(
    graphics: &Graphics,
    format: &IDWriteTextFormat,
    typography: &Typography,
    mode: WritingMode,
    text: &str,
    runs: &[StyleRun],
    room: f32,
) -> Result<f32> {
    if text.is_empty() {
        return Ok(0.0);
    }
    let layout = cell_layout(
        graphics, format, typography, mode, text, runs, room, CELL_BOX,
    )?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: The layout is alive and the metrics are written into our own
    // storage.
    unsafe { layout.GetMetrics(&mut metrics)? };
    let (flow, _) = mode.to_axes(metrics.width, metrics.height);
    Ok(flow)
}

/// The narrowest a column is allowed to become, as a fraction of the font size.
///
/// **Two characters.** Shrinking a table in proportion can otherwise take a
/// column below the width of one word, and a column narrower than its text
/// wraps every character onto a line of its own — a table taller than the page
/// rather than one wider than it. Past this the table is left wider than the
/// pane, which is the lesser of the two.
const TABLE_MIN_COLUMN: f32 = 2.0;

/// One line of a table's block, and what it shows.
struct RowPlan {
    utf16_start: u32,
    utf16_len: u32,
    newline_len: u32,
    /// Cells, or the one cell a row shown as its own source becomes.
    cells: Vec<CellPlan>,
    /// The header row is set bold; the delimiter row shows nothing at all.
    header: bool,
    /// How tall the row came to, padding included.
    flow_size: f32,
}

struct CellPlan {
    utf16_start: u32,
    utf16_len: u32,
    text: String,
    marks: Vec<StyleRun>,
    column: usize,
    /// How far the text reaches with room enough for all of it.
    natural: f32,
    /// And how tall it is once set in its column.
    flow_size: f32,
}

/// Set a table out as a grid of cells, and measure what it comes to
/// (要件 7.3.2).
///
/// **This is the whole of what makes a table's block different from every
/// other.** Elsewhere a block is one layout and the measurement is that
/// layout's line metrics; here each cell is set in a box of its own, so that a
/// cell with more text than its column has room for **wraps inside the column**
/// rather than losing its tail. What comes back is where every box is, and what
/// the block as a whole came to.
fn measure_table(
    graphics: &mut Graphics,
    styled: StyledText<'_>,
    typography: &Typography,
    mode: WritingMode,
    line_box: f32,
    keep_trailing_empty_line: bool,
) -> Result<Option<(TableGrid, BlockMeasure)>> {
    let found = tables(styled);
    // **One table to a block** (`split_blocks`), so the first is the only one.
    let Some(table) = found.first() else {
        return Ok(None);
    };
    let format = graphics.text_format(typography, mode)?;
    let gutter = typography.font_size * TABLE_GUTTER;
    let pad = gutter * 0.5;
    let room = typography.font_size * TABLE_CELL_PAD;
    let aligns = table
        .rule
        .and_then(|rule| table_alignments(&styled.text[rule.byte_start..rule.byte_end]))
        .unwrap_or_default();

    // Every line of the block, in order, and the cells it shows. **The rows a
    // table has are the lines its block has**: a table shares its block with
    // nothing.
    let mut plans: Vec<RowPlan> = Vec::new();
    let mut byte = 0_usize;
    let mut utf16 = 0_u32;
    let mut body_rows = 0_usize;
    for (index, line) in styled.text.split('\n').enumerate() {
        let has_break = byte + line.len() < styled.text.len();
        let utf16_len = utf16_units(line);
        let kind = styled.style_at(index).kind;
        let source = styled.source_line == Some(index);
        let mut cells = Vec::new();
        let mut header = false;
        if source {
            // 要件 7.3.1: the row the caret is on shows its own source, bars
            // and all — one cell holding the whole line.
            cells.push(CellPlan {
                utf16_start: utf16,
                utf16_len,
                text: line.to_owned(),
                marks: Vec::new(),
                column: 0,
                natural: 0.0,
                flow_size: 0.0,
            });
        } else if matches!(kind, LineKind::TableRow) {
            header = body_rows == 0;

            for cell in table_cells(line) {
                let text = &line[cell.byte_start..cell.byte_end];
                let lead = text.len() - text.trim_start().len();
                let text_start = cell.byte_start + lead;
                let text_end = text_start + text.trim().len();
                let start = utf16_units(&line[..text_start]);
                let end = utf16_units(&line[..text_end]);
                let mut marks = cell_marks(styled, index, start, end);
                if header && end > start {
                    marks.insert(0, bold_run(0, end - start));
                }
                let text = line[text_start..text_end].to_owned();
                // **The bar that closes a row leaves an empty cell behind**,
                // and a cell with nothing in it is nothing on the page. It is
                // still a column, so it still counts here — a row with more
                // cells than another decides how many there are.
                let natural = cell_along(graphics, &format, typography, mode, &text, &marks)?;
                cells.push(CellPlan {
                    utf16_start: utf16 + start,
                    utf16_len: end - start,
                    text,
                    marks,
                    column: cells.len(),
                    natural,
                    flow_size: 0.0,
                });
            }
        }
        // **The header is the first row of the table**, whether or not the
        // caret happens to be on it. Counted here rather than in the branch
        // above, which the caret's own row does not take.
        if matches!(kind, LineKind::TableRow) {
            body_rows += 1;
        }
        plans.push(RowPlan {
            utf16_start: utf16,
            utf16_len,
            newline_len: u32::from(has_break),
            cells,
            header,
            flow_size: 0.0,
        });
        byte += line.len() + 1;
        utf16 += utf16_len + u32::from(has_break);
    }

    // A column is as wide as its widest cell, and the table gives in proportion
    // when it does not fit — but no column below what one word needs.
    // **A row may hold more cells than the delimiter row named**, and the bar
    // that closes a row leaves an empty one at the end of every row: the
    // columns are however many the widest row has.
    let columns = plans.iter().map(|row| row.cells.len()).max().unwrap_or(0);
    let mut widths = (0..columns)
        .map(|column| {
            plans
                .iter()
                .flat_map(|row| row.cells.iter())
                .filter(|cell| cell.column == column)
                .fold(0.0_f32, |widest, cell| widest.max(cell.natural))
        })
        .collect::<Vec<f32>>();
    let natural: f32 = widths.iter().sum();
    let (_, reach) = column_heads(&widths, pad, gutter);
    if reach > line_box && natural > 0.0 {
        let room = (line_box - (reach - natural)).max(0.0);
        let floor = typography.font_size * TABLE_MIN_COLUMN;
        let scale = (room / natural).clamp(0.0, 1.0);
        for width in &mut widths {
            *width = (*width * scale).max(width.min(floor));
        }
        // **The floor may have put the table back over the edge**, and a table
        // wider than its pane is worse than a narrow column: it is the one
        // thing the shrinking exists to prevent. So what the floor took is
        // taken back in proportion from everybody.
        let held: f32 = widths.iter().sum();
        if held > room && held > 0.0 {
            let again = room / held;
            for width in &mut widths {
                *width *= again;
            }
        }
    }
    let (heads, reach) = column_heads(&widths, pad, gutter);

    // How tall each cell is once it is set in its column, and so how tall each
    // row is.
    for plan in &mut plans {
        let mut tallest = 0.0_f32;
        // **The one cell a source row becomes is as wide as the table**, and
        // every other cell is as wide as its column.
        let whole = plan.cells.len() == 1 && widths.len() != 1;
        for cell in &mut plan.cells {
            let width = if whole {
                reach
            } else {
                widths.get(cell.column).copied().unwrap_or(reach)
            };
            cell.flow_size = cell_flow(
                graphics,
                &format,
                typography,
                mode,
                &cell.text,
                &cell.marks,
                width,
            )?;
            tallest = tallest.max(cell.flow_size);
        }
        plan.flow_size = if plan.cells.is_empty() {
            0.0
        } else {
            tallest + room * 2.0
        };
    }

    // And where all of it sits. **The rows run the way the reading does**: down
    // the page across a page, right to left down a column (`FlowOrder`). Laid
    // out one way for both, a vertical table came out with its header at the
    // left and its rules in the gaps between the rows rather than on them.
    let total: f32 = plans.iter().map(|plan| plan.flow_size).sum();
    let ascending = matches!(mode.flow_order(), FlowOrder::Ascending);
    let mut cells = Vec::new();
    let mut rules: Vec<f32> = Vec::new();
    let mut lines = Vec::new();
    let mut cursor = 0.0_f32;
    for plan in &plans {
        let flow = if ascending {
            cursor
        } else {
            total - cursor - plan.flow_size
        };
        cursor += plan.flow_size;
        if !plan.cells.is_empty() {
            // Both edges of the row. Which of them a reader would call the
            // rule "above" it depends on the direction; the set does not.
            rules.push(flow);
            rules.push(flow + plan.flow_size);
        }
        let single = plan.cells.len() == 1 && widths.len() != 1;
        for cell in &plan.cells {
            if cell.utf16_len == 0 {
                continue;
            }
            let line_start = if single {
                0.0
            } else {
                heads.get(cell.column).copied().unwrap_or(0.0)
            };
            let line_size = if single {
                reach
            } else {
                widths.get(cell.column).copied().unwrap_or(reach)
            };
            cells.push(GridCell {
                utf16_start: cell.utf16_start,
                utf16_len: cell.utf16_len,
                row: lines.len(),
                column: cell.column,
                flow_start: flow + room,
                flow_size: cell.flow_size,
                line_start,
                line_size,
                align: aligns.get(cell.column).copied().unwrap_or_default(),
                header: plan.header,
                marks: cell.marks.clone(),
            });
        }
        lines.push(LineInfo {
            utf16_start: plan.utf16_start,
            utf16_len: plan.utf16_len + plan.newline_len,
            newline_len: plan.newline_len,
            flow_start: flow,
            flow_size: plan.flow_size,
        });
    }
    rules.sort_by(|left, right| left.partial_cmp(right).expect("no rule is NaN"));
    rules.dedup_by(|left, right| (*left - *right).abs() < 0.5);
    if !keep_trailing_empty_line && lines.last().is_some_and(|line| line.flow_size == 0.0) {
        lines.pop();
    }

    let column_rules = std::iter::once(0.0)
        .chain(
            (1..widths.len())
                .filter(|column| widths[*column] > 0.0)
                .map(|column| heads[column] - gutter * 0.5),
        )
        .chain([reach])
        .collect::<Vec<f32>>();

    let grid = TableGrid {
        cells,
        rules,
        columns: column_rules,
        reach,
    };
    let measure = BlockMeasure {
        flow_size: total,
        // 要件 7.3.2: a table's own answer to the same question — how far its
        // widest row reaches across the flow (`reach`).
        line_reach: reach,
        content_flow_start: 0.0,
        max_flow_size: total.max(1.0),
        lines: Arc::from(lines),
        grid: None,
    };
    Ok(Some((grid, measure)))
}

/// Where each column of a table begins across the line axis, and how far the
/// whole of it reaches (要件 7.3.2).
///
/// **The gap after a column is the gutter, except at the table's edge, where it
/// is the page's own pad.** The bar that closes a row leaves an empty cell
/// behind, and the box over that bar is the table's far-side padding rather
/// than one more gutter — a gutter there makes every row wider than the table
/// drawn under it, and the row then wraps to a second line. **It wrapped in a
/// vertical pane and not in a horizontal one**, because the overflow falls at
/// the end of the row and there is nothing after it to carry down.
///
/// **One walk, read by everything**: whether the table has to be narrowed, how
/// wide the rule under its header is, and where each box carries its cell. A
/// second opinion anywhere here is a table drawn at one width and set at
/// another.
fn column_heads(widths: &[f32], pad: f32, gutter: f32) -> (Vec<f32>, f32) {
    // The last column anything reached. Past it there is nothing to separate,
    // so nothing is added and every further column begins at the same edge.
    let last = widths.iter().rposition(|width| *width > 0.0);
    let mut heads = Vec::with_capacity(widths.len());
    let mut head = pad;
    for (column, width) in widths.iter().enumerate() {
        heads.push(head);
        head += width
            + match last {
                Some(last) if column < last => gutter,
                Some(last) if column == last => pad,
                _ => 0.0,
            };
    }
    (heads, head)
}

/// A stretch set bold and marked nothing else (要件 7.3.2).
fn bold_run(utf16_start: u32, utf16_len: u32) -> StyleRun {
    StyleRun {
        utf16_start,
        utf16_len,
        heading_level: 0,
        marks: Marks {
            bold: true,
            ..Marks::default()
        },
        ornament: None,
    }
}

/// What one cell's own stretch of its line has marked, moved to be relative to
/// the cell (要件 7.3.2).
fn cell_marks(styled: StyledText<'_>, line: usize, start: u32, end: u32) -> Vec<StyleRun> {
    styled
        .marks_at(line)
        .iter()
        .filter_map(|emphasis| {
            let from = emphasis.utf16_start.max(start);
            let to = (emphasis.utf16_start + emphasis.utf16_len).min(end);
            (from < to).then_some(StyleRun {
                utf16_start: from - start,
                utf16_len: to - from,
                heading_level: 0,
                marks: emphasis.marks,
                ornament: None,
            })
        })
        .collect()
}

/// The boxes that carry a table's cells to the heads of their columns
/// (要件 7.3.2).
///
/// **The one thing about a block that its own text and some arithmetic cannot
/// decide.** A column is as wide as the widest cell anywhere in it, and how
/// wide a cell is only DirectWrite knows. What comes back is ordinary
/// [`StyleRun`]s, so the widths travel with every other run — into the cache
/// key, and onto the measuring threads — rather than beside them (技術検証 7.7).
///
/// Each box covers **the padding around one bar**: the spaces left after the
/// previous cell, the bar itself, and the spaces before this one. Its width is
/// what is left of the previous column plus the gutter, so **a box depends on
/// the column before it and on nothing else** — not on how far along the row it
/// sits — and every cell's text begins exactly at its column's head whatever
/// padding the writer typed.
/// Everything drawn over one block: what stands over a range of characters and
/// what stands over whole lines (要件 7.3.2).
struct BlockMarks {
    runs: Vec<StyleRun>,
    lines: Vec<LineRun>,
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
        // rule and the ground under a fence are the line's, not the box's, and
        // what an indent stands for is the block's. A table's bar stands for
        // the gap between two columns, which is room and not ink.
        //
        // **A table's rule has no word either** — it is a shape, drawn across
        // the box rather than set in it, and `draw_marker_ink` takes it before
        // it ever asks what goes inside.
        Ornament::Hidden | Ornament::Indent => String::new(),
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

/// How thick a drawn rule is, at the size the writer set (要件 7.3.2).
///
/// **One answer for both.** The stroke across a `---` line and the rule under a
/// table's header are the same mark drawn the same way; only how far each
/// reaches differs, and a second opinion about the weight would show as two
/// kinds of rule on one page.
fn rule_stroke(font_size: f32) -> f32 {
    (font_size * 0.06).round().max(1.0)
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
    /// Where this tile begins across the line axis (要件 9). Everything here is
    /// in the page's coordinates, and the tile holds one slice of it — so the
    /// slice's own near edge is taken off once, here, rather than by each mark.
    line_origin: f32,
    font_size: f32,
}

impl OrnamentPage {
    /// The screen rectangle of a box given as a flow range and a line range.
    fn rect(&self, flow: (f32, f32), line: (f32, f32)) -> D2D_RECT_F {
        let near = line.0 - self.line_origin;
        let far = line.1 - self.line_origin;
        let (left, top) = self.mode.to_screen(self.flow_origin + flow.0, near);
        let (right, bottom) = self.mode.to_screen(self.flow_origin + flow.1, far);
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
/// With `to_block_edge`, an end that reaches as far as the block's own lines do
/// is moved to the block's **placed** edge. `place_blocks` rounds each block's
/// extent on its own, so the sum of a block's lines falls up to half a pixel
/// short of it — and consecutive blocks abut at the placed edge and nowhere
/// else. A mark that stopped where its lines stopped would leave that half
/// pixel of paper showing through the seam, or paint it twice.
///
/// **A table's rules ask for it off.** They stand on the rows' own edges: what
/// they are drawing is where one row stops and the next begins, and the block's
/// placed edge is a fact about the block, not about the table in it. Rounded
/// out to it, the row at the table's edge came out narrower than the rest by
/// however much the rounding was — plainly so down a column.
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
/// One cell's own layout, set the way it is drawn (要件 7.3.2).
///
/// **The same layout for the ink and for every question about a position.** A
/// cell hit tested in a layout set differently from the one it is drawn in is a
/// caret standing where the text is not.
fn cell_layout_for(
    graphics: &mut Graphics,
    typography: &Typography,
    mode: WritingMode,
    block_text: &str,
    cell: &GridCell,
) -> Result<IDWriteTextLayout> {
    let format = graphics.text_format(typography, mode)?;
    let start = byte_at_utf16(block_text, cell.utf16_start);
    let end = byte_at_utf16(block_text, cell.utf16_start + cell.utf16_len);
    let layout = cell_layout(
        graphics,
        &format,
        typography,
        mode,
        &block_text[start..end],
        &cell.marks,
        cell.line_size,
        cell.flow_size.max(1.0),
    )?;
    let alignment = match cell.align {
        Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
        Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
        Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
    };
    // SAFETY: The layout outlives the call.
    unsafe { layout.SetTextAlignment(alignment)? };
    Ok(layout)
}

/// Draw a table: its rules, and every cell in a box of its own (要件 7.3.2).
///
/// **Nothing here asks which way the page runs.** Every distance the grid holds
/// is on the flow axis or the line axis, and `OrnamentPage` turns a pair of
/// those into a rectangle — the same tool the bar beside a quote is drawn with.
///
/// **Free of the engine**, like everything else a tile is drawn with: the page,
/// the grid and the block's own text are all a table needs.
fn draw_grid(
    graphics: &mut Graphics,
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    grid: &TableGrid,
    block_text: &str,
    typography: &Typography,
    page: &OrnamentPage,
) -> Result<()> {
    let mode = page.mode;
    let margin = page.margin;
    let block_origin = page.flow_origin;
    let stroke = rule_stroke(page.font_size);
    let half = stroke * 0.5;
    let near = grid.rules.first().copied().unwrap_or(0.0);
    let far = grid.rules.last().copied().unwrap_or(0.0);
    // SAFETY: The brush and the target both outlive every call here, and
    // each rectangle is read before its call returns.
    //
    // `OrnamentPage::rect` takes the line axis absolutely, the way every
    // other mark on the page gives it: from the page's edge, not from where
    // the block's text begins.
    unsafe {
        brush.SetOpacity(ORNAMENT_ALPHA);
        // **The two rules at the edges are drawn just inside them.** A
        // stroke centred on the block's own edge is half outside it, and
        // the tile a block is drawn into ends exactly there — so half of
        // the table's own frame would be cut away.
        let inside = |at: f32, first: f32, last: f32| {
            if at <= first {
                (at, at + stroke)
            } else if at >= last {
                (at - stroke, at)
            } else {
                (at - half, at + half)
            }
        };
        for at in &grid.rules {
            let (from, to) = inside(*at, near, far);
            let rect = page.rect((from, to), (margin, margin + grid.reach));
            target.FillRectangle(&rect, brush);
        }
        for at in &grid.columns {
            let (from, to) = inside(*at, 0.0, grid.reach);
            let rect = page.rect((near, far), (margin + from, margin + to));
            target.FillRectangle(&rect, brush);
        }
        brush.SetOpacity(1.0);
    }

    for cell in &grid.cells {
        if cell.utf16_len == 0 {
            continue;
        }
        let layout = cell_layout_for(graphics, typography, mode, block_text, cell)?;
        let (x, y) = mode.to_screen(
            block_origin + cell.flow_start,
            margin + cell.line_start - page.line_origin,
        );
        let origin = windows_numerics::Vector2 { X: x, Y: y };
        // SAFETY: The layout and the brush both outlive the draw.
        unsafe {
            target.DrawTextLayout(origin, &layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }
    Ok(())
}

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
    let stroke = rule_stroke(page.font_size);
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
            // 要件 7.3.2: a link is underlined and nothing else. **Not a colour
            // of its own** — 要件 9 gives the writer the ink, the paper and the
            // headings, and a colour nobody can set is a colour that will not
            // suit somebody's paper. An underline is legible on any of them.
            if run.marks.link {
                layout.SetUnderline(true, range)?;
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

/// The layout one block is set in, built from the block and nothing else.
///
/// **Three callers and one layout.** A block is measured in it (`measure_task`),
/// drawn in it (`draw_tile`) and hit tested in it (`layout_for`), and a layout
/// built differently in any of the three is a caret standing where the text is
/// not. It takes the block's own text and spec rather than the engine, so that
/// the measuring threads can call it too (要件 2, 技術検証 7.4).
fn build_block_layout(
    graphics: &mut Graphics,
    typography: &Typography,
    mode: WritingMode,
    text: &str,
    runs: &[StyleRun],
    max_flow_size: f32,
    line_box: f32,
) -> Result<IDWriteTextLayout> {
    let format = graphics.text_format(typography, mode)?;
    let utf16 = text.encode_utf16().collect::<Vec<u16>>();
    // The layout box is the block's flow bound by its own line box, which way
    // round depending on the mode.
    let (max_width, max_height) = mode.to_screen(max_flow_size, line_box);
    // SAFETY: The UTF-16 buffer stays alive across CreateTextLayout, and the
    // layout owns everything it needs afterwards.
    let layout = unsafe {
        graphics
            .dwrite
            .CreateTextLayout(&utf16, &format, max_width, max_height)?
    };
    apply_typography(&layout, typography, runs, utf16.len() as u32)?;
    apply_marker_boxes(&layout, typography, runs)?;
    Ok(layout)
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
    let layout = build_block_layout(
        graphics,
        &task.typography,
        task.mode,
        &task.text,
        &task.runs,
        task.max_flow_size,
        task.block_box,
    )?;
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

/// One tile to draw, and everything it takes to draw it (要件 2).
///
/// **Owned, like a [`MeasureTask`].** A tile shows one block, and a block is
/// drawn from its own text, its own marks and the spec — nothing else (3.4).
/// Saying so in a type is what keeps the drawing free of the engine: `draw_tile`
/// cannot reach for anything this does not carry.
///
/// **It was made to cross a thread and it does not** (技術検証 7.8): drawing is
/// the one part of laying text out that measured slower divided than whole. The
/// parcel stayed because the division it was cut for is the same division that
/// makes the drawing readable.
struct TileTask {
    span: TileSpan,
    /// Where the block sits, and what its lines measured to.
    block: BlockPlacement,
    /// **The block's own text, not the document's.** Every offset in the marks
    /// and in the grid is counted from here.
    text: String,
    runs: Vec<StyleRun>,
    lines: Vec<LineRun>,
    typography: Arc<Typography>,
    mode: WritingMode,
    margin: f32,
    line_extent: u32,
    /// The box the block is set in on the line axis, its own indent taken off
    /// (要件 7.3.2). The layout has to be rebuilt in it or the block is drawn at
    /// a size it was not placed at.
    line_box: f32,
    /// The surface every tile of this pane is drawn on, along the flow and
    /// across it.
    surface_size: u32,
    surface_cross: u32,
    /// Where the IME's underline falls inside this block, if it falls in it.
    underline: Option<(u32, u32)>,
}

impl TileTask {
    /// The size of this tile's image: its own extent along the flow by the
    /// pane's extent across it, whichever way round the mode puts them.
    fn pixel_size(&self) -> (u32, u32) {
        self.mode
            .to_surface(self.span.flow_size, self.span.cross_size)
    }
}

/// Where drawn tiles go, and — the point of the trait — **whose memory they are
/// written into**.
///
/// A tile is a couple of megabytes, and the caller has somewhere to put it that
/// the engine cannot know about: an image the window will hold. Handing the
/// pixels over afterwards would mean writing them twice and allocating for
/// every tile, and **allocating two megabytes costs more than drawing them
/// does** (技術検証 7.8: 0.72ms against 0.31ms). So the engine asks for the
/// buffer first and draws straight into it.
pub trait TileSink {
    /// Room for one tile: `width * height * 4` bytes, filled with **BGRA** —
    /// the order the bitmap is in, not the order Slint wants. Turning it round
    /// belongs to whoever holds the buffer, where it can be done in place.
    fn buffer(&mut self, span: TileSpan, width: u32, height: u32) -> &mut [u8];

    /// The pixels are in it.
    fn filled(&mut self, span: TileSpan);
}

/// Draw one tile into the buffer the sink gave for it.
///
/// `cached` is the block's own layout when the caller has one. The engine keeps
/// the layout its measurement produced (`layout_for`); with `None` the task is
/// built into a layout here, and **that is not free** — DirectWrite lays a
/// layout out when it is first drawn, so a tile given a fresh one pays for the
/// whole block rather than for its own slice (技術検証 7.8).
fn draw_tile(
    graphics: &mut Graphics,
    task: &TileTask,
    cached: Option<IDWriteTextLayout>,
    into: &mut [u8],
) -> Result<()> {
    let mode = task.mode;
    let typography = &task.typography;
    // 要件 9: this sheet's paper. The window paints the page behind the tiles
    // from the same setting, so the two cannot show a seam.
    let paper = colour(typography.paper);
    let ink = colour(typography.ink);
    let line_extent = task.line_extent;
    let margin = task.margin;
    let (surface_width, surface_height) = mode.to_surface(task.surface_size, task.surface_cross);
    let (target, brush, heading_brushes, comment_brush, bitmap) = {
        let cache = graphics.render_target(surface_width, surface_height)?;
        (
            cache.target.clone(),
            cache.brush.clone(),
            cache.heading_brushes.clone(),
            cache.comment_brush.clone(),
            cache.bitmap.clone(),
        )
    };

    // SAFETY: The target, brush and bitmap are kept alive by the cache for the
    // whole draw, and BeginDraw/EndDraw are paired.
    unsafe {
        target.BeginDraw();
        target.Clear(Some(&paper));
        // The brushes the target keeps, told what the inks are now. Cheaper
        // than building them per tile, and the settings may have moved since
        // the target was made (要件 9).
        brush.SetColor(&ink);
        for (level, heading_brush) in heading_brushes.iter().enumerate() {
            let heading_ink = colour(typography.ink_for(level as u8 + 1));
            heading_brush.SetColor(&heading_ink);
        }
        comment_brush.SetColor(&colour(typography.comment_ink()));
    }

    // The block is drawn at its own offset inside the tile, and the margin plus
    // the block's own indent sit on the line axis. All of it swaps with the mode.
    let block_origin = task.block.draw_origin() - task.span.flow_start as f32;
    // 要件 7.3.2: **a table is drawn cell by cell.** It is not one layout, so
    // none of what follows applies to it: no block-wide text, no boxes, no
    // whole-line marks. Its rules and its cells are all there is
    // (`measure_table`).
    if let Some(grid) = &task.block.grid {
        let page = OrnamentPage {
            mode,
            flow_origin: block_origin,
            margin,
            inset: 0.0,
            indent: typography.indent_step(),
            line_extent: line_extent as f32,
            line_origin: task.span.cross_start as f32,
            font_size: typography.font_size,
        };
        draw_grid(
            graphics, &target, &brush, grid, &task.text, typography, &page,
        )?;
    } else {
        let layout = match cached {
            Some(layout) => layout,
            None => build_block_layout(
                graphics,
                typography,
                mode,
                &task.text,
                &task.runs,
                task.block.max_flow_size,
                task.line_box,
            )?,
        };
        // 要件 9: a heading is drawn in its own ink. **Set on the layout before
        // every draw rather than once when it is built** — the layout is cached
        // and a colour changes no geometry, so the cached one is still the right
        // layout; and the brush it was given last time belongs to a render
        // target that may since have been rebuilt. Body runs are left alone:
        // they are drawn with the brush handed to `DrawTextLayout`.
        //
        // SAFETY: the layout and the brushes both outlive the draw.
        unsafe {
            for run in &task.runs {
                let range = DWRITE_TEXT_RANGE {
                    startPosition: run.utf16_start,
                    length: run.utf16_len,
                };
                // 要件 7.3.2: a comment inside code is drawn in its own ink,
                // which is the writer's own faded towards their own paper
                // (`Typography::comment_ink`).
                if run.marks.comment {
                    layout.SetDrawingEffect(&comment_brush, range)?;
                    continue;
                }
                if run.heading_level == 0 {
                    continue;
                }
                let Some(heading_brush) = heading_brushes.get(run.heading_level as usize - 1)
                else {
                    continue;
                };
                layout.SetDrawingEffect(heading_brush, range)?;
            }
        }
        let inset = block_inset(&task.block.span, typography);
        // 要件 9: this tile holds one slice of the page across the flow, so the
        // block is drawn that far back — the same shift `block_origin` is along
        // the flow, and the only two the tile makes.
        let cross_origin = task.span.cross_start as f32;
        let (origin_x, origin_y) = mode.to_screen(block_origin, margin + inset - cross_origin);
        let origin = windows_numerics::Vector2 {
            X: origin_x,
            Y: origin_y,
        };
        // 要件 7.3.2: the marks that belong to whole lines — the bar beside a
        // quote, the stroke across a rule. Drawn from the block's own line table
        // rather than from a hit test, and before the text, so neither ever
        // covers a glyph.
        let page = OrnamentPage {
            mode,
            flow_origin: block_origin,
            margin,
            inset,
            indent: typography.indent_step(),
            line_extent: line_extent as f32,
            line_origin: cross_origin,
            font_size: typography.font_size,
        };
        draw_line_ornaments(&target, &brush, &task.block, &task.lines, &page);
        // SAFETY: The layout outlives the draw call, and the underline is set
        // and cleared on the same layout.
        unsafe {
            if let Some((start, length)) = task.underline {
                layout.SetUnderline(
                    true,
                    DWRITE_TEXT_RANGE {
                        startPosition: start,
                        length,
                    },
                )?;
            }
            target.DrawTextLayout(origin, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            if let Some((start, length)) = task.underline {
                layout.SetUnderline(
                    false,
                    DWRITE_TEXT_RANGE {
                        startPosition: start,
                        length,
                    },
                )?;
            }
        }
        // 要件 7.3.2: what stands in each of this block's boxes. After the text,
        // so the ink sits on top of nothing it has to fight.
        if task.runs.iter().any(run_draws_ink) {
            let format = graphics.text_format(typography, mode)?;
            draw_marker_ink(
                &target,
                &brush,
                &format,
                &layout,
                &task.runs,
                &task.text,
                origin,
                mode,
                typography.indent_step(),
            )?;
        }
    }
    // SAFETY: Paired with BeginDraw above.
    unsafe { target.EndDraw(None, None)? };

    let (tile_width, tile_height) = task.pixel_size();
    let stride = tile_width * 4;
    let needed = stride as usize * tile_height as usize;
    let Some(pixels) = into.get_mut(..needed) else {
        return Err(Error::new(
            E_FAIL,
            "the tile buffer is smaller than the tile",
        ));
    };
    // The tile was drawn at the origin of the surface, so only its own pixels
    // are read back.
    let rect = WICRect {
        X: 0,
        Y: 0,
        Width: tile_width as i32,
        Height: tile_height as i32,
    };
    // SAFETY: The rectangle lies inside the bitmap and the buffer matches the
    // requested stride and height.
    unsafe {
        let source: IWICBitmapSource = bitmap.cast()?;
        source.CopyPixels(&rect, stride, pixels)?;
    }
    Ok(())
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
/// (`BLOCK_MAX_CELLS`) wherever there is a boundary to cut at, so dealing them
/// round-robin divides the work evenly enough without any of that. A paragraph
/// with no break in it and a table (要件 7.3.2) are the two that can be larger,
/// and neither can be cut without changing what is on the page.
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
    /// The logical line shown as its own source, if one is (要件 7.3.1). Held
    /// beside the markers **and compared beside them**: moving the caret from
    /// one row of a table to the next changes not one character and not one
    /// mark, and yet the boxes over both rows have to change.
    source_line: Option<usize>,
    /// The logical lines each block covers, as an index into `line_styles`.
    /// One entry per block, in reading order.
    block_lines: Vec<Range<usize>>,
    /// How long a line may be, as the caller asked for it (要件 9). Every
    /// measurement depends on it.
    fit: LineFit,
    /// How wide the page turned out to be, across the flow. **The same as the
    /// extent when the line is wrapped**, and the longest line the document
    /// holds (margins and indents included) when it is not — the reader scrolls
    /// across that, so it is what everything drawn in pixels is measured by.
    page_extent: u32,
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
/// Everything about a table that changes its picture (要件 7.3.2).
///
/// **Read, not measured.** The grid is the measurement's own answer, kept in
/// the plan; a signature that measured cells again would cost a DirectWrite
/// call per tile per frame to learn what is already known.
fn hash_grid(grid: &TableGrid, hasher: &mut DefaultHasher) {
    grid.reach.to_bits().hash(hasher);
    for at in grid.rules.iter().chain(&grid.columns) {
        at.to_bits().hash(hasher);
    }
    for cell in &grid.cells {
        cell.utf16_start.hash(hasher);
        cell.utf16_len.hash(hasher);
        cell.flow_start.to_bits().hash(hasher);
        cell.flow_size.to_bits().hash(hasher);
        cell.line_start.to_bits().hash(hasher);
        cell.line_size.to_bits().hash(hasher);
        cell.align.hash(hasher);
        cell.marks.hash(hasher);
    }
}

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
///
/// **Takes the layout key rather than making one** (2026-09-06). Every block
/// wants both keys, and hashing its text is the whole cost of either — asking
/// for them separately hashed the document twice on every keystroke.
fn measure_key(
    layout: u64,
    keep_trailing_empty_line: bool,
    table_source_line: Option<usize>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    layout.hash(&mut hasher);
    keep_trailing_empty_line.hash(&mut hasher);
    // 要件 7.3.1 と 7.3.2: **which row of a table the caret is on**, and only
    // for a table. Everywhere else the active line has already changed the
    // runs — nothing is put over it, so its boxes are gone — but a table's
    // boxes are not made from the runs at all: they are the widths
    // `measure_table` works out, and a row reads the same either way (a bar is
    // a bar). Without this the caret could walk into a row and the grid the
    // cache hands back would still be the one that covers its bars.
    table_source_line.hash(&mut hasher);
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
        self.page_extent.max(1)
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

    /// How far a tile reaches across the flow (要件 9).
    ///
    /// **The whole page while the page fits a pane**, which is every wrapped
    /// document and exactly what a tile was before this existed. A page wider
    /// than that is cut, and then only the slices the pane is showing are drawn.
    pub fn tile_cross_size(&self) -> u32 {
        self.line_extent().min(MAX_TILE_CROSS)
    }

    /// How far a tile reaches along the flow axis, keeping the pixels per tile
    /// roughly constant as the window grows or shrinks.
    pub fn tile_flow_size(&self) -> u32 {
        (TILE_TARGET_PIXELS / self.tile_cross_size()).clamp(MIN_TILE_FLOW_SIZE, MAX_TILE_FLOW_SIZE)
    }

    /// True when the engine already describes exactly this text and geometry.
    pub fn matches(&self, styled: StyledText<'_>, fit: LineFit, typography: &Typography) -> bool {
        self.fit == fit
            && self.typography == *typography
            && self.text == styled.text
            && self.line_styles == styled.lines
            && self.line_spans == styled.spans
            && self.line_markers == styled.markers
            && self.source_line == styled.source_line
    }

    /// Re-split and re-measure the document, reusing every block whose text and
    /// styling did not change. Returns what had to be measured.
    pub fn update(
        &mut self,
        styled: StyledText<'_>,
        fit: LineFit,
        typography: &Typography,
    ) -> Result<UpdateCost> {
        let fit = match fit {
            LineFit::Extent(extent) => LineFit::Extent(extent.max(1)),
            LineFit::Free => LineFit::Free,
        };
        let typography = Typography {
            font_size: typography.font_size.max(1.0),
            ..typography.clone()
        };
        if self.matches(styled, fit, &typography) {
            return Ok(UpdateCost::default());
        }
        if self.fit != fit || self.typography != typography {
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
        let line_box = fit.line_box(margin, 0.0);
        // The split is charged in line space, so it needs the geometry: the same
        // pane at a different line extent wraps differently and cuts elsewhere.
        let cells = cells_per_line(fit.charged_extent(), &typography);
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
            line_extent: fit.charged_extent(),
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
        let mut live_measure_keys = HashSet::with_capacity(spans.len());
        let mut live_layout_keys = HashSet::with_capacity(spans.len());
        let mut fresh_measures = Vec::new();
        let mut fresh_layouts = Vec::new();
        let mut measured = wrap_cost;
        // The blocks the cache had nothing for, and where each answer belongs.
        let mut tasks: Vec<MeasureTask> = Vec::new();
        let mut pending: HashMap<usize, PendingBlock> = HashMap::new();

        let block_lines = block_line_ranges(text, &spans);
        // 要件 7.3.2: this block's own box, narrowed by its indent. **The
        // measurement has to be taken in it**, or the block is placed at a size
        // it is not drawn at — and a table has to be brought inside the same
        // one.
        let block_boxes = spans
            .iter()
            .map(|span| fit.line_box(margin, block_inset(span, &typography)))
            .collect::<Vec<f32>>();
        // 要件 7.3.2: **the one thing here that text and arithmetic cannot
        // decide.** A table's column is as wide as the widest cell anywhere in
        // it, and how wide a cell is only DirectWrite knows — so the tables are
        // measured first, in one pass, and what comes back is ordinary style
        // runs. A document with no table in it asks for nothing and never wakes
        // the graphics at all (技術検証 7.7).
        // The tables the cache has nothing for, gathered in the pass below and
        // measured together afterwards — **one `with_graphics` for all of
        // them**, which is what the pre-pass this replaced was for.
        let last_index = spans.len().saturating_sub(1);
        let mut table_tasks: Vec<usize> = Vec::new();

        // **Deciding what to measure needs no graphics at all.** Which blocks
        // the cache already answers, what ranges each one sets, how wide its box
        // is — all of it is text and arithmetic, and separating it from the
        // measuring is what lets the measuring go somewhere else.
        {
            for (index, span) in spans.iter().enumerate() {
                let block_text = &text[span.byte_start..span.byte_end];
                let block_styled = block_styling(styled, span, &block_lines[index]);

                let runs = style_runs(block_styled);
                let keep_trailing_empty_line = index == last_index;
                let block_box = block_boxes[index];
                // 要件 7.3.2: **a table is measured like every other block, and
                // that is the point** (2026-09-06). It used to be measured
                // ahead of this loop, outside the cache, so **every table in
                // the document was re-measured on every keystroke** — a cell
                // laid out per cell per table per key, wherever the writer was
                // typing. Measured: 1.13ms per 25-row table, so a plan with 16
                // of them cost 19.3ms of a keystroke against 1.3ms with none.
                // A table's columns are a function of its own block's text,
                // the spec, the mode and the box — the same four the key
                // already carries — plus the row the caret is on, which is why
                // `measure_key` takes it.
                // **The same test `tables` makes**, both halves of it: a
                // source pane sets a table as text, bars and all (要件 7.3.1),
                // so there is no grid there to measure.
                let table = block_styled.is_preview()
                    && block_styled.lines.iter().any(|line| line.kind.is_table());
                let block_layout = layout_key(block_text, &runs, &typography, block_box);
                let key = measure_key(
                    block_layout,
                    keep_trailing_empty_line,
                    table.then(|| block_styled.source_line).flatten(),
                );
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
                // **A table is not one layout**, so there is no `MeasureTask`
                // it could be and nothing to hand a thread: its cells are laid
                // out one at a time, and only DirectWrite on this thread can
                // say how wide a cell is (技術検証 7.7).
                if table {
                    table_tasks.push(index);
                    continue;
                }
                let extent = block_extent(span, fit.charged_extent(), &typography);
                let max_flow_size = block_flow_bound(block_styled, extent, &typography);
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

        // 要件 7.3.2: **the tables that changed, and only those.** One
        // `with_graphics` for all of them, and a document whose tables are
        // where they were never wakes the graphics at all — which is what a
        // keystroke somewhere else in the document is.
        if !table_tasks.is_empty() {
            with_graphics(|graphics| {
                for index in &table_tasks {
                    let index = *index;
                    let span = &spans[index];
                    let block_styled = block_styling(styled, span, &block_lines[index]);
                    let (grid, mut measure) = measure_table(
                        graphics,
                        block_styled,
                        &typography,
                        mode,
                        block_boxes[index],
                        index == last_index,
                    )?
                    .expect("a block whose lines are a table holds one (`tables`)");
                    measure.grid = Some(Arc::new(grid));
                    if let Some(slot) = pending.get(&index) {
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
                Ok(())
            })?;
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
        self.source_line = styled.source_line;
        self.block_lines = block_lines;
        self.fit = fit;
        // 要件 9: how wide the page came out. A wrapped line makes it the extent
        // it was given; a free one makes it the longest line the document holds,
        // each block's own indent counted with it and the margins around the
        // whole. **Taken from the measurements rather than from the layouts**,
        // so a block that came out of the cache counts the same as one just
        // measured.
        self.page_extent = match fit {
            LineFit::Extent(extent) => extent,
            LineFit::Free => {
                let reach = spans
                    .iter()
                    .zip(&measures)
                    .map(|(span, measure)| measure.line_reach + block_inset(span, &typography))
                    .fold(0.0_f32, f32::max);
                (reach + margin * 2.0).ceil().max(1.0) as u32
            }
        };
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
        // 要件 7.3.1: block-local, like every other index here.
        let lines = self.block_lines.get(block_index).cloned().unwrap_or(0..0);
        let source_line = self
            .source_line
            .and_then(|line| line.checked_sub(lines.start))
            .filter(|line| *line < lines.end - lines.start);
        marked
            .with_markers(self.block_markers(block_index))
            .with_source_line(source_line)
    }

    /// The box one block's text is set in, across the line axis (要件 7.3.2).
    ///
    /// The page less both margins less this block's own indent — the same
    /// figure the update measured it at, which is what keeps the layout built
    /// here the layout the block was placed by.
    fn block_line_box(&self, span: &BlockSpan) -> f32 {
        let inset = block_inset(span, &self.typography);
        self.fit.line_box(self.margin, inset)
    }

    /// Every range one block sets, and the marks that stand over its whole
    /// lines (要件 7.3.2).
    ///
    /// **One place, because three had to agree.** The layout is built with
    /// these ranges, what stands in each box is drawn from them, and the tile
    /// signature is taken over them; two of the three once read `style_runs`
    /// alone and a table's rule was drawn by nobody (技術検証 7.7). A table
    /// asks for nothing here now — it is not one layout at all.
    fn block_marks(&self, block_index: usize) -> BlockMarks {
        let styled = self.block_styled(block_index);
        BlockMarks {
            runs: style_runs(styled),
            lines: line_runs(styled),
        }
    }

    /// The rectangles a selection covers inside one table (要件 7.3.2).
    ///
    /// **Cell by cell**, because that is how a table is set. What falls between
    /// two cells — a bar, the padding around it, a delimiter row — shows
    /// nothing, which is right: none of it is on the page.
    fn grid_selection_rects(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
        grid: &TableGrid,
        range: (u32, u32),
        visible_flow: (f32, f32),
        rects: &mut Vec<SelectionRect>,
    ) -> Result<()> {
        let mode = self.mode;
        let margin = self.margin;
        let (span, draw_origin) = {
            let block = &self.plan.blocks[block_index];
            (block.span, block.draw_origin())
        };
        for cell in &grid.cells {
            let start = span.utf16_start + cell.utf16_start;
            let end = start + cell.utf16_len;
            let Some((local_start, local_length)) = block_local_range(start, end, range.0, range.1)
            else {
                continue;
            };
            if local_length == 0 {
                continue;
            }
            let flow = draw_origin + cell.flow_start;
            if flow > visible_flow.1 || flow + cell.flow_size < visible_flow.0 {
                continue;
            }
            let layout = self.cell_layout_of(graphics, &span, cell)?;
            let (origin_x, origin_y) = mode.to_screen(flow, margin + cell.line_start);
            let mut metrics = vec![DWRITE_HIT_TEST_METRICS::default(); local_length as usize];
            let mut count = 0;
            // SAFETY: The buffer holds one region per unit, which is the most
            // a range can produce, and the layout is alive across the call.
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
            for region in metrics {
                if region.width <= 0.0 || region.height <= 0.0 {
                    continue;
                }
                rects.push(SelectionRect {
                    left: region.left,
                    top: region.top,
                    right: region.left + region.width,
                    bottom: region.top + region.height,
                });
            }
        }
        Ok(())
    }

    /// One cell of this engine's text, in the layout it is drawn in
    /// ([`cell_layout_for`]).
    fn cell_layout_of(
        &self,
        graphics: &mut Graphics,
        block: &BlockSpan,
        cell: &GridCell,
    ) -> Result<IDWriteTextLayout> {
        let text = &self.text[block.byte_start..block.byte_end];
        cell_layout_for(graphics, &self.typography, self.mode, text, cell)
    }

    /// The layout a position falls in, where inside it the position is, and
    /// where that layout sits within the block (要件 7.3.2).
    ///
    /// **A table is the one block with more than one layout.** Everywhere else
    /// this is the block's own layout at the block's own origin, which is what
    /// every caller took for granted before a cell could wrap.
    fn layout_at(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
        local: u32,
    ) -> Result<(IDWriteTextLayout, u32, f32, f32)> {
        let Some(block) = self.plan.blocks.get(block_index) else {
            let layout = self.layout_for(graphics, block_index)?;
            return Ok((layout, local, 0.0, 0.0));
        };
        let (grid, span) = (block.grid.clone(), block.span);
        let Some(grid) = grid else {
            let layout = self.layout_for(graphics, block_index)?;
            return Ok((layout, local, 0.0, 0.0));
        };
        let Some(index) = grid.cell_at(local) else {
            let layout = self.layout_for(graphics, block_index)?;
            return Ok((layout, local, 0.0, 0.0));
        };
        let cell = &grid.cells[index];
        let inside = local.saturating_sub(cell.utf16_start).min(cell.utf16_len);
        let layout = self.cell_layout_of(graphics, &span, cell)?;
        Ok((layout, inside, cell.flow_start, cell.line_start))
    }

    fn layout_for(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
    ) -> Result<IDWriteTextLayout> {
        let (byte_start, byte_end, max_flow_size, line_box) = {
            let block = &self.plan.blocks[block_index];
            (
                block.span.byte_start,
                block.span.byte_end,
                block.max_flow_size,
                self.block_line_box(&block.span),
            )
        };
        // 要件 7.3.2: the table's boxes are measured again here rather than
        // kept. **The same measurement either way** — the cells and the spec
        // are what they were — so the key below is the key the update put on
        // this block's measurement.
        let runs = self.block_marks(block_index).runs;
        let block_text = &self.text[byte_start..byte_end];
        let key = layout_key(block_text, &runs, &self.typography, line_box);
        if let Some(position) = self.layouts.iter().position(|(cached, _)| *cached == key) {
            let entry = self.layouts.remove(position);
            let layout = entry.1.clone();
            self.layouts.insert(0, entry);
            return Ok(layout);
        }

        // The same spec the measurement was taken under. A layout rebuilt
        // without it would draw and hit test at a different size from the one
        // the block was placed at.
        let layout = build_block_layout(
            graphics,
            &self.typography,
            self.mode,
            &self.text[byte_start..byte_end],
            &runs,
            max_flow_size,
            line_box,
        )?;
        self.layouts.insert(0, (key, layout.clone()));
        self.layouts.truncate(LAYOUT_CACHE_LIMIT);
        Ok(layout)
    }

    /// The tiles the viewport needs, cut out of the blocks it crosses.
    ///
    /// **Both axes**, because the page may be wider than the pane (要件 9): the
    /// flow pair says how far down the document the pane is looking, the other
    /// how far across the page.
    pub fn visible_tiles(
        &self,
        viewport_flow: f32,
        visible_flow: f32,
        prefetch: u32,
        viewport_across: f32,
        visible_across: f32,
    ) -> Vec<TileSpan> {
        self.plan.visible_tiles(
            viewport_flow,
            visible_flow,
            self.tile_flow_size(),
            prefetch,
            CrossSlices {
                extent: self.line_extent(),
                tile_size: self.tile_cross_size(),
                viewport: viewport_across,
                visible: visible_across,
            },
        )
    }

    /// The parcels the requested tiles are drawn from.
    ///
    /// **Nothing here touches graphics.** It is the same division `update`
    /// makes between deciding what to measure and measuring it (技術検証 7.4):
    /// what a tile shows is decided from the document, and drawing it needs
    /// nothing but the answer.
    fn tile_tasks(
        &self,
        tiles: &[TileSpan],
        preedit_utf16_range: Option<(u32, u32)>,
    ) -> Vec<TileTask> {
        // One surface for every tile, at the furthest a tile can reach.
        //
        // Tiles are slices of blocks, and blocks are all different sizes, so
        // sizing the surface to the tile would rebuild the WIC bitmap and its
        // render target for almost every tile. That call is the single most
        // expensive thing in a draw. A fixed surface is built once per pane
        // extent; a shorter tile simply leaves the far end of it unread.
        let surface_size = self.tile_flow_size();
        let surface_cross = self.tile_cross_size();
        let block_count = self.plan.blocks.len();
        // One spec for the whole batch, shared rather than cloned per tile: it
        // carries a family name for the body, one for code and one per heading
        // level.
        let spec = Arc::new(self.typography.clone());
        tiles
            .iter()
            .filter(|span| {
                span.flow_size > 0
                    && span.flow_size <= surface_size
                    && span.block_index < block_count
            })
            .map(|span| {
                let block = self.plan.blocks[span.block_index].clone();
                // 要件 7.3.2: a table asks for none of these — it is not one
                // layout, so no whole-block text is set and no box stands over
                // any of it.
                let BlockMarks { runs, lines } = match block.grid {
                    Some(_) => BlockMarks {
                        runs: Vec::new(),
                        lines: Vec::new(),
                    },
                    None => self.block_marks(span.block_index),
                };
                let underline = preedit_utf16_range.and_then(|(start, length)| {
                    block_local_range(
                        block.span.utf16_start,
                        block.span.utf16_end,
                        start,
                        start + length,
                    )
                });
                TileTask {
                    span: *span,
                    text: self.text[block.span.byte_start..block.span.byte_end].to_owned(),
                    line_box: self.block_line_box(&block.span),
                    block,
                    runs,
                    lines,
                    typography: spec.clone(),
                    mode: self.mode,
                    margin: self.margin,
                    line_extent: self.line_extent(),
                    surface_size,
                    surface_cross,
                    underline,
                }
            })
            .collect()
    }

    /// Render the requested tiles. Each tile shows one block and nothing else.
    ///
    /// The pixels go into the buffers `sink` hands over, in BGRA. The image is
    /// the tile's flow extent by the pane's line extent, so which of the two is
    /// the width depends on the writing mode.
    ///
    /// **On this thread, and it has to be** (要件 2, 技術検証 7.8). Drawing is
    /// the one part of laying text out that does not divide: measured, eight
    /// threads rasterizing tiles take seven times as long as one.
    pub fn render_tiles(
        &mut self,
        tiles: &[TileSpan],
        preedit_utf16_range: Option<(u32, u32)>,
        sink: &mut impl TileSink,
    ) -> Result<()> {
        let tasks = self.tile_tasks(tiles, preedit_utf16_range);
        if tasks.is_empty() {
            return Ok(());
        }
        with_graphics(|graphics| {
            for task in &tasks {
                // The layout the measurement produced, when this thread still
                // has it. **Worth reaching for**: a block's tiles share one
                // layout, and DirectWrite does not lay a layout out until it is
                // drawn — so a tile handed a fresh one pays for the block again
                // (21 tiles over 10 blocks: 6.5ms with the cache, 18.2ms
                // without).
                let cached = match task.block.grid {
                    Some(_) => None,
                    None => self.layout_for(graphics, task.span.block_index).ok(),
                };
                let (width, height) = task.pixel_size();
                // **Written once, where they are wanted.** The buffer is the
                // caller's, so nothing here allocates and nothing copies the
                // tile again afterwards.
                draw_tile(
                    graphics,
                    task,
                    cached,
                    sink.buffer(task.span, width, height),
                )?;
                sink.filled(task.span);
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
        // 要件 9: which slice across the page this is. Two slices of one block
        // hold different words, so they are different tiles.
        tile.cross_start.hash(&mut hasher);
        tile.cross_size.hash(&mut hasher);
        self.line_extent().hash(&mut hasher);
        hash_typography(&self.typography, &mut hasher);
        hash_colours(&self.typography, &mut hasher);
        // Two panes showing the same text at the same size draw different
        // pixels, so a shared tile cache must not confuse them.
        self.mode.hash(&mut hasher);
        if let Some(block) = self.plan.blocks.get(tile.block_index) {
            self.text[block.span.byte_start..block.span.byte_end].hash(&mut hasher);
            // Block-local, like the underline below: the same heading drawn at
            // the same size is the same pixels wherever it sits.
            // **Without the table's boxes, and that is not an omission**
            // (要件 7.3.2). A column's width is a function of this block's own
            // text, the spec and the mode, and all three are already hashed
            // above — so a signature that carries them says nothing the rest of
            // it does not. Measuring cells here would cost a DirectWrite call
            // per tile per frame to learn what is already known (技術検証 7.7).
            let runs = style_runs(self.block_styled(tile.block_index));
            hash_style_runs(&runs, &self.typography, &mut hasher);
            // 要件 7.3.2: and the marks that belong to whole lines. **Nothing
            // in the block's own text says a line is one** — three hyphens
            // inside a fence are three hyphens — and neither mark moves a
            // glyph, so without this the tile already in the cache is the one
            // that gets shown. The same trap the colours fell into above.
            //
            // **The rules between a table's columns are left out here too**,
            // for the reason a table's grid is: neither is a mark on a line.
            let line_marks = line_runs(self.block_styled(tile.block_index));
            hash_line_runs(&line_marks, &mut hasher);
            // 要件 7.3.2: **and the table, if this block is one.** Every
            // distance in it changes the picture, and the grid is already laid
            // out — this reads what the measurement left behind rather than
            // measuring a cell per tile per frame. **Left out, a table narrowed
            // by a resize that did not change its height would keep the tile it
            // had**: the same trap the colours fell into (6.22b).
            if let Some(grid) = &block.grid {
                hash_grid(grid, &mut hasher);
            }
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
            let span = self.plan.blocks[block_index].span;
            let local = caret_utf16
                .saturating_sub(span.utf16_start)
                .min(span.utf16_len());
            // 要件 7.3.2: **which layout, and where it sits.** For a table this
            // is the cell that holds the position, set in its own box; for
            // everything else the block's own layout at the block's own origin.
            let (layout, inside, cell_flow, cell_line) =
                self.layout_at(graphics, block_index, local)?;
            let block = &self.plan.blocks[block_index];
            let mut point_x = 0.0;
            let mut point_y = 0.0;
            let mut metrics = DWRITE_HIT_TEST_METRICS::default();
            // SAFETY: The layout is alive and the position is clamped into it.
            unsafe {
                layout.HitTestTextPosition(
                    inside,
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
                block.to_global_flow(mode.flow_of(&metrics) + cell_flow),
                margin + inset + cell_line + line_point,
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
                // 要件 7.3.2: **a table is selected cell by cell.** Each has
                // its own layout, so each answers for its own part of the
                // range; the bars between them hold no ink and no rectangle.
                if let Some(grid) = self.plan.blocks[block_index].grid.clone() {
                    self.grid_selection_rects(
                        graphics,
                        block_index,
                        &grid,
                        (start, end),
                        visible_flow,
                        &mut rects,
                    )?;
                    continue;
                }
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
            // 要件 7.3.2: **in a table, the click picks the cell first.** The
            // block holds no single layout to ask, and the cell a point is in
            // is the one whose box it lands in — or, in a gutter or a rule, the
            // nearest one.
            if let Some(grid) = self.plan.blocks[block_index].grid.clone() {
                return self.grid_hit_test(graphics, block_index, &grid, flow, line);
            }
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

    /// Where a point in a table lands (要件 7.3.2).
    ///
    /// **The nearest cell, then that cell's own hit test.** Distance is taken
    /// on both axes at once and squared only against itself, so a point in a
    /// gutter goes to the cell beside it rather than to one a row away.
    fn grid_hit_test(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
        grid: &TableGrid,
        flow: f32,
        line: f32,
    ) -> Result<HitTest> {
        let mode = self.mode;
        let margin = self.margin;
        let (span, draw_origin) = {
            let block = &self.plan.blocks[block_index];
            (block.span, block.draw_origin())
        };
        let local_flow = flow - draw_origin;
        let local_line = line - margin;
        let mut nearest: Option<(f32, &GridCell)> = None;
        for cell in &grid.cells {
            let away = |value: f32, start: f32, size: f32| {
                (start - value).max(value - (start + size)).max(0.0)
            };
            let flow_away = away(local_flow, cell.flow_start, cell.flow_size);
            let line_away = away(local_line, cell.line_start, cell.line_size);
            let distance = flow_away * flow_away + line_away * line_away;
            if nearest.is_none_or(|(best, _)| distance < best) {
                nearest = Some((distance, cell));
            }
        }
        let Some((_, cell)) = nearest else {
            return Ok(HitTest {
                utf16_position: span.utf16_start,
                is_inside: false,
            });
        };
        let layout = self.cell_layout_of(graphics, &span, cell)?;
        let (layout_x, layout_y) =
            mode.to_screen(local_flow - cell.flow_start, local_line - cell.line_start);
        let start = span.utf16_start + cell.utf16_start;
        hit_test_in_block(&layout, layout_x, layout_y, start, start + cell.utf16_len)
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
            // 要件 7.3.2: a table's line is a row of it, and the row is a set
            // of cells — so the step lands where a click at the same place
            // would.
            if let Some(grid) = self.plan.blocks[next_block].grid.clone() {
                return self.grid_hit_test(graphics, next_block, &grid, center_flow, target_line);
            }
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

    /// The runs a rectangular selection covers, one per **layout line**
    /// (要件 7.1).
    ///
    /// **The lines are the ones on the screen, not the ones in the file.** A
    /// Markdown paragraph is one file line however far it wraps, so a rectangle
    /// built out of file lines cannot be drawn across prose at all: the line
    /// below the one the writer is on is the blank line that ends the
    /// paragraph, and the one below that is a heading. What they are pointing
    /// at is what they can see, and this walks that.
    ///
    /// `line_lo` and `line_hi` are coordinates on the **line** axis — an x
    /// where the text runs down the page and a y where it runs across — the
    /// same measurement `move_caret_by_line` holds on to when the caret steps
    /// between lines. Which of the two ends of the rectangle they came from
    /// does not matter; they are sorted here.
    ///
    /// One run per line between the two positions, empty runs included: a line
    /// with nothing under the columns is still one of the lines.
    pub fn rectangle_runs(
        &mut self,
        from_utf16: u32,
        to_utf16: u32,
        line_lo: f32,
        line_hi: f32,
    ) -> Result<Vec<(u32, u32)>> {
        let (start, end) = (from_utf16.min(to_utf16), from_utf16.max(to_utf16));
        let (line_lo, line_hi) = (line_lo.min(line_hi), line_lo.max(line_hi));
        let (Some(first), Some(last)) = (self.plan.locate(start), self.plan.locate(end)) else {
            return Ok(Vec::new());
        };
        let margin = self.margin;
        let mode = self.mode;
        // **The walk is bounded by the lines that exist.** A plan that cannot
        // reach `last` — nothing here should produce one, but the loop is the
        // only place that would spin — stops at the count instead.
        let bound = self.plan.line_count();

        with_graphics(|graphics| {
            let mut runs = Vec::new();
            let mut at = first;
            for _ in 0..bound {
                let Some(center_flow) = self.plan.line_flow_center(at.0, at.1) else {
                    break;
                };
                let mut ends = [start, start];
                for (slot, line) in ends.iter_mut().zip([line_lo, line_hi]) {
                    let hit = if let Some(grid) = self.plan.blocks[at.0].grid.clone() {
                        // 要件 7.3.2: in a table the point picks a cell first,
                        // the same way a click does.
                        self.grid_hit_test(graphics, at.0, &grid, center_flow, line)?
                    } else {
                        let layout = self.layout_for(graphics, at.0)?;
                        let block = &self.plan.blocks[at.0];
                        let inset = block_inset(&block.span, &self.typography);
                        let (x, y) = mode.to_screen(
                            block.to_layout_flow(center_flow),
                            (line - margin - inset).max(0.0),
                        );
                        hit_test_in_block(
                            &layout,
                            x,
                            y,
                            block.span.utf16_start,
                            block.span.utf16_end,
                        )?
                    };
                    *slot = hit.utf16_position;
                }
                runs.push((ends[0].min(ends[1]), ends[0].max(ends[1])));
                if at == last {
                    break;
                }
                let Some(next) = self.plan.step_line(at.0, at.1, true) else {
                    break;
                };
                at = next;
            }
            Ok(runs)
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

    // 要件 9: how far the longest line reached across the flow. **The layout's
    // own metrics**, which are in screen terms like every other box DirectWrite
    // reports — so which of the two is the line axis is the mode's business.
    let mut text_metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: the layout is alive for the call and the struct is plain data.
    unsafe { layout.GetMetrics(&mut text_metrics)? };
    let (_, line_reach) = mode.to_axes(text_metrics.width, text_metrics.height);

    Ok(BlockMeasure {
        flow_size,
        line_reach,
        content_flow_start,
        max_flow_size,
        lines: lines.into(),
        grid: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_blocks::{LineKind, Marks, visible_flow_range};

    /// The pane extent along the line axis every test lays text out in.
    const LINE_EXTENT: u32 = 520;

    /// Every tile a render produced, kept whole.
    ///
    /// **The pixels are the test's own buffers.** The editor draws into the
    /// images the window will hold ([`TileSink`]); a test has nowhere to put
    /// them but a `Vec`, and wants to look at all of them together anyway.
    #[derive(Default)]
    struct DrawnTiles {
        tiles: Vec<(TileSpan, u32, u32, Vec<u8>)>,
    }

    impl TileSink for DrawnTiles {
        fn buffer(&mut self, span: TileSpan, width: u32, height: u32) -> &mut [u8] {
            let room = width as usize * height as usize * 4;
            self.tiles.push((span, width, height, vec![0; room]));
            &mut self.tiles.last_mut().expect("just pushed").3
        }

        fn filled(&mut self, _span: TileSpan) {}
    }

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
            .update(styled, LineFit::Extent(LINE_EXTENT), typography)
            .expect("DirectWrite block measurement");
        engine
    }

    /// Re-lay out an existing engine with no styling and the default spec.
    fn update_plain(engine: &mut TextEngine, text: &str) -> UpdateCost {
        engine
            .update(
                StyledText::plain(text),
                LineFit::Extent(LINE_EXTENT),
                &plain(),
            )
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

    /// A preview pane's styling for `source`, the way the editor hands it over.
    fn preview_of(source: &str) -> (crate::document::PreviewDocument, Vec<LineStyle>) {
        (
            crate::document::PreviewDocument::from_source(source),
            crate::document::line_styles(source),
        )
    }

    /// Where the caret sits along the line axis at the first character of
    /// `word`, which every test below uses to ask where a column begins.
    ///
    /// **Through `to_axes` rather than reading `x`**, because the line axis is
    /// the screen's x in one mode and its y in the other — which is the very
    /// thing these tests are here to hold.
    fn line_axis_at(engine: &mut TextEngine, mode: WritingMode, text: &str, word: &str) -> f32 {
        let byte = text.find(word).expect("the word is in the preview");
        let caret = engine
            .caret_geometry(utf16_units(&text[..byte]))
            .expect("DirectWrite hit test");
        mode.to_axes(caret.x, caret.y).1
    }

    /// The other axis: which line the position sits on. A row of a table has
    /// to be **one line**, so the cells of a row all answer the same here.
    fn cross_axis_at(engine: &mut TextEngine, mode: WritingMode, text: &str, word: &str) -> f32 {
        let byte = text.find(word).expect("the word is in the preview");
        let caret = engine
            .caret_geometry(utf16_units(&text[..byte]))
            .expect("DirectWrite hit test");
        mode.to_axes(caret.x, caret.y).0
    }

    /// The one table a document under test holds, as it was set out.
    fn grid_of(engine: &TextEngine) -> Arc<TableGrid> {
        engine
            .plan
            .blocks
            .iter()
            .find_map(|block| block.grid.clone())
            .expect("a table has a block of its own")
    }

    /// 要件 7.3.2: **every cell of a column begins at one place**, whatever the
    /// cells above it hold and whatever padding the writer typed around the
    /// bars. This is the whole of what the boxes over a table's bars are for
    /// (技術検証 7.7).
    ///
    /// **A cell's own extent is how far its text runs**, which is the screen's
    /// width in one writing direction and its height in the other. Measured on
    /// the wrong one, every cell comes back as the thickness of its own line —
    /// the same for all of them — and the columns have nothing to correct by.
    #[test]
    fn a_column_begins_at_one_place_in_every_row() {
        let source =
            "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | にほへ |\n|狭|とちり |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let text = preview.text.clone();
        let mode = WritingMode::Horizontal;
        let mut engine = engine_set(mode, styled, &plain());

        let first = line_axis_at(&mut engine, mode, &text, "いろは");
        let second = line_axis_at(&mut engine, mode, &text, "にほへ");
        let third = line_axis_at(&mut engine, mode, &text, "とちり");

        assert!(
            (first - second).abs() <= 0.5 && (first - third).abs() <= 0.5,
            "the second column begins at {first}, {second} and {third}"
        );
        // And the first column too, whose cells are padded differently.
        let short = line_axis_at(&mut engine, mode, &text, "短");
        let narrow = line_axis_at(&mut engine, mode, &text, "狭");
        assert!(
            (short - narrow).abs() <= 0.5,
            "the first column begins at {short} and {narrow}"
        );
    }

    /// 要件 7.3.2: **a table sets the same way down a column as across a
    /// page.** The geometry is the same arithmetic in both — a column is as
    /// wide as its widest cell, and the box before a cell carries the rest —
    /// because every distance in it is along the line axis, which is what
    /// [`WritingMode::to_axes`] names (技術検証 7.7).
    ///
    /// **This is the whole of what `supportsSideways: false` bought** (4.14).
    /// The two panes are compared against each other rather than against
    /// numbers of their own: what has to hold is that a writer who turns the
    /// page sees the same table.
    #[test]
    fn a_vertical_pane_sets_a_table_the_way_the_horizontal_one_does() {
        // Narrow enough that the columns are shrunk and a cell's tail is cut,
        // which is the case that used to differ: a cut cell's box begins on the
        // character it hides, and an upright character read the box's `across`.
        let source = "| 左 | 中央 | 右 |\n| :--- | :---: | ---: |\n| a | b | c |\n\
                      | ながいながい内容 | ながいながい内容 | ながいながい内容 |\n";
        let (preview, styles) = preview_of(source);
        let text = preview.text.clone();

        let mut along = Vec::new();
        for mode in [WritingMode::Horizontal, WritingMode::Vertical] {
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers());
            let mut engine = engine_set(mode, styled, &plain());
            let places = ["左", "中央", "右", "a", "b", "c"]
                .map(|word| line_axis_at(&mut engine, mode, &text, word));
            along.push(places);
        }

        for (index, (page, column)) in along[0].iter().zip(&along[1]).enumerate() {
            assert!(
                (page - column).abs() <= 0.5,
                "cell {index} is at {page} across the page and {column} down the column"
            );
        }

        // **And the row is still one line.** A box that advanced short would
        // leave the row fitting; one that advanced long would push the last
        // cell onto a second line, where it would line up with nothing. The
        // cells of a row all sit on the same line or none of the above holds.
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mode = WritingMode::Vertical;
        let mut engine = engine_set(mode, styled, &plain());
        let first = cross_axis_at(&mut engine, mode, &text, "a");
        for word in ["b", "c"] {
            let cross = cross_axis_at(&mut engine, mode, &text, word);
            assert!(
                (first - cross).abs() <= 0.5,
                "the row runs on lines {first} and {cross}"
            );
        }
    }

    /// And the table down a column really is a table: the three cells of a
    /// column begin at one place (要件 7.3.2).
    #[test]
    fn a_column_of_a_vertical_table_begins_at_one_place() {
        let source = "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | にほへ |\n\
                      | 狭 | とちり |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let text = preview.text.clone();
        let mode = WritingMode::Vertical;
        let mut engine = engine_set(mode, styled, &plain());

        let first = line_axis_at(&mut engine, mode, &text, "いろは");
        let second = line_axis_at(&mut engine, mode, &text, "にほへ");
        let third = line_axis_at(&mut engine, mode, &text, "とちり");
        assert!(
            (first - second).abs() <= 0.5 && (first - third).abs() <= 0.5,
            "the second column begins at {first}, {second} and {third}"
        );
    }

    /// 要件 7.3.2: **a table reaches as far as its own columns do, not as far
    /// as the page.** A grid drawn to the page's edge under a two-word table is
    /// not a table; it is a rule with words above it (技術検証 7.7).
    #[test]
    fn a_table_reaches_as_far_as_its_columns() {
        let source = "| 短 | いろは |\n| --- | --- |\n| 狭 | とちり |\n";
        let mode = WritingMode::Horizontal;
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let engine = engine_set(mode, styled, &plain());
        let grid = grid_of(&engine);

        let page = LINE_EXTENT as f32 - margin_for(plain().font_size) * 2.0;
        assert!(
            grid.reach > plain().font_size && grid.reach < page * 0.5,
            "the table reaches {}, against a page of {page}",
            grid.reach
        );
        // And the rules down it stop there too.
        let far = grid.columns.last().copied().expect("a table is ruled");
        assert!((far - grid.reach).abs() <= 0.5, "{:?}", grid.columns);
    }

    /// Where the caret sits along the line axis just past `word`, which is
    /// where a column set with `---:` has to end.
    fn line_axis_after(engine: &mut TextEngine, mode: WritingMode, text: &str, word: &str) -> f32 {
        let byte = text.find(word).expect("the word is in the preview") + word.len();
        let caret = engine
            .caret_geometry(utf16_units(&text[..byte]))
            .expect("DirectWrite hit test");
        mode.to_axes(caret.x, caret.y).1
    }

    /// 要件 7.3.2: `---:` sets a column against its far edge, so what lines up
    /// is where its cells **end**. The slack a cell has over its column is the
    /// same slack either way; all the delimiter row decides is which box
    /// carries it (技術検証 7.7).
    #[test]
    fn a_column_set_against_its_end_lines_its_cells_up_there() {
        let source = "| 右 | 見出し |\n| ---: | --- |\n| あ | いち |\n| ああああ | に |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mut engine = engine_set(WritingMode::Horizontal, styled, &plain());
        let text = preview.text.clone();

        let short = line_axis_after(&mut engine, WritingMode::Horizontal, &text, "あ");
        let long = line_axis_after(&mut engine, WritingMode::Horizontal, &text, "ああああ");
        assert!(
            (short - long).abs() <= 0.5,
            "the column ends at {short} and {long}"
        );
        // And the second column, which the same row sets at its head, still
        // begins at one place.
        let first = line_axis_at(&mut engine, WritingMode::Horizontal, &text, "いち");
        let second = line_axis_at(&mut engine, WritingMode::Horizontal, &text, "に");
        assert!(
            (first - second).abs() <= 0.5,
            "the second column begins at {first} and {second}"
        );
    }

    /// 要件 7.3.2: `:---:` gives a cell's room over to **both** its sides in
    /// equal halves, so what lines up is neither the head nor the tail but the
    /// middle. It is the same room the other two alignments hand to one side
    /// whole; centring is the only one that splits it (技術検証 7.7).
    #[test]
    fn a_column_set_at_its_middle_shares_the_slack_between_its_sides() {
        let source = "| 中 | 見出し |\n| :---: | --- |\n| あ | いち |\n| ああああ | に |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mode = WritingMode::Horizontal;
        let mut engine = engine_set(mode, styled, &plain());
        let text = preview.text.clone();

        let short_head = line_axis_at(&mut engine, mode, &text, "あ");
        let short_tail = line_axis_after(&mut engine, mode, &text, "あ");
        let long_head = line_axis_at(&mut engine, mode, &text, "ああああ");
        let long_tail = line_axis_after(&mut engine, mode, &text, "ああああ");

        // The two halves of the short cell's room over, one on each side of it.
        // **Both have to be there**: either one alone at nothing is `:---` or
        // `---:` wearing the other's name.
        let before = short_head - long_head;
        let after = long_tail - short_tail;
        assert!(
            (before - after).abs() <= 0.5 && before > 1.0,
            "the short cell has {before} before it and {after} after it"
        );
    }

    /// 要件 7.3.2: **a rule stands between each pair of columns**, and it
    /// stands in the gutter — not near it, and not on a cell.
    #[test]
    fn a_rule_stands_in_the_gutter_between_two_columns() {
        let source = "| あ | いい | ううう |\n| --- | --- | --- |\n| ええ | おおお | かかかか |\n";
        let mode = WritingMode::Horizontal;
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let engine = engine_set(mode, styled, &plain());
        let grid = grid_of(&engine);

        // Two edges and two boundaries, in order along the line.
        assert_eq!(grid.columns.len(), 4, "{:?}", grid.columns);
        assert!(grid.columns[0].abs() < 0.5, "{:?}", grid.columns);
        assert!(
            grid.columns.windows(2).all(|pair| pair[0] < pair[1]),
            "{:?}",
            grid.columns
        );
        // Each boundary falls between the column that ends before it and the
        // one that begins after it.
        for (index, at) in grid.columns[1..3].iter().enumerate() {
            let before = grid
                .cells
                .iter()
                .filter(|cell| cell.column == index)
                .map(|cell| cell.line_start + cell.line_size)
                .fold(0.0_f32, f32::max);
            let after = grid
                .cells
                .iter()
                .filter(|cell| cell.column == index + 1)
                .map(|cell| cell.line_start)
                .fold(f32::INFINITY, f32::min);
            assert!(
                before < *at && *at < after,
                "a rule at {at} for the gutter from {before} to {after}"
            );
        }
    }

    /// 要件 7.3.2: **a rule above every row and one under the last**, so every
    /// cell of the table is boxed. A single rule under the header reads as a
    /// heading with a line under it; what a reader knows as a table is the
    /// grid.
    #[test]
    fn a_table_is_ruled_above_every_row_and_under_the_last() {
        let source = "| あ | いい |\n| --- | --- |\n| ええ | おおお |\n| か | き |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let engine = engine_set(WritingMode::Horizontal, styled, &plain());
        let grid = grid_of(&engine);

        // Three rows: one rule above each, and one under the last.
        assert_eq!(grid.rules.len(), 4, "{:?}", grid.rules);
        assert!(
            grid.rules.windows(2).all(|pair| pair[0] < pair[1]),
            "{:?}",
            grid.rules
        );
        // And no rule crosses a cell: every one of them stands between two.
        for cell in &grid.cells {
            let end = cell.flow_start + cell.flow_size;
            assert!(
                grid.rules
                    .iter()
                    .all(|at| *at <= cell.flow_start + 0.5 || *at >= end - 0.5),
                "a rule crosses the cell at {}..{end}: {:?}",
                cell.flow_start,
                grid.rules
            );
        }
    }

    /// 要件 7.3.2: **the delimiter row keeps no more height than the rule it
    /// stands for.** A row's worth of empty paper between a header and the
    /// table under it is not what a table looks like — and the box alone does
    /// not do it, because the line break the box does not cover carries the
    /// format's own font metrics.
    #[test]
    fn the_delimiter_row_keeps_no_height_of_its_own() {
        let source = "| あ | いい |\n| --- | --- |\n| ええ | おおお |\n| か | き |\n";
        let (preview, styles) = preview_of(source);
        let text = preview.text.clone();
        let mode = WritingMode::Horizontal;
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mut engine = engine_set(mode, styled, &plain());

        let header = cross_axis_at(&mut engine, mode, &text, "あ");
        let first = cross_axis_at(&mut engine, mode, &text, "ええ");
        let second = cross_axis_at(&mut engine, mode, &text, "か");
        // Two body rows in a row give the height of one line. The header sits
        // one line above the first of them, plus the rule and nothing else.
        let line = second - first;
        assert!(line > 1.0, "the rows are {line} apart");
        assert!(
            first - header < line * 1.5,
            "the header is {} above the body, against a line of {line}",
            first - header
        );
    }

    /// 要件 7.3.2: **the first row is the header and is set bold**, which is
    /// what the delimiter row under it says the row is.
    #[test]
    fn the_header_row_is_set_bold() {
        let source = "| あ | いい |\n| --- | --- |\n| ええ | おおお |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let engine = engine_set(WritingMode::Horizontal, styled, &plain());
        let grid = grid_of(&engine);

        let bold = |cell: &GridCell| cell.marks.iter().any(|run| run.marks.bold);
        let header = grid
            .cells
            .iter()
            .filter(|cell| cell.row == 0)
            .collect::<Vec<&GridCell>>();
        assert_eq!(header.len(), 2, "one per cell of the header row");
        assert!(header.iter().all(|cell| bold(cell)), "{header:?}");
        assert!(
            grid.cells
                .iter()
                .filter(|cell| cell.row > 1)
                .all(|cell| !bold(cell)),
            "{:?}",
            grid.cells
        );
    }

    /// 要件 7.3.2: **and those rules are drawn.** The same trap the rule under
    /// the header fell into — a mark that the layout knows about and the ink
    /// pass has never heard of — and the same answer: only the pixels say it.
    ///
    /// **Asked of the vertical pane**, where a column rule runs across the page
    /// while the rule under the header runs down it. One long unbroken run of
    /// ink along a row of pixels can therefore only be a column rule.
    #[test]
    fn the_rules_between_columns_reach_the_pixels() {
        let source = "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | とちり |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mut engine = engine_set(WritingMode::Vertical, styled, &plain());
        let tiles = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );

        // **A lighter threshold than the rule's.** A stroke a little over one
        // pixel wide lands on two of them when it falls between their centres,
        // and each is then only part dark; the ink is the same, spread. What
        // tells a rule from a glyph's edge is still the length.
        let mut longest = 0_u32;
        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&tiles, None, &mut drawn)
            .expect("vertical tile render");
        for (_, width, _, bgra) in &drawn.tiles {
            for row in bgra.chunks_exact(*width as usize * 4) {
                let mut run = 0;
                for pixel in row.chunks_exact(4) {
                    run = if pixel[2] < 245 { run + 1 } else { 0 };
                    longest = longest.max(run);
                }
            }
        }

        // No glyph is 60 pixels across at this size, and a column rule reaches
        // from the header row to the last one.
        assert!(longest >= 60, "the longest run of ink is {longest} pixels");
    }

    /// 要件 7.3.1: **the row the caret is on shows its bars.** Nothing stands
    /// over it, so its cells sit where the writer typed them rather than where
    /// their columns are — which is the whole of what makes the active line the
    /// source line.
    ///
    /// **And the rest of the table does not move.** The row is still measured,
    /// so the column it is widest in keeps its width; a table that shifted
    /// every time the caret walked into a row would be unusable to type in.
    #[test]
    fn the_row_the_caret_is_on_shows_its_bars() {
        let source = "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | にほへ |\n\
                      | 狭 | とちり |\n";
        let mode = WritingMode::Horizontal;
        let styles = crate::document::line_styles(source);
        let places = |preview: &crate::document::PreviewDocument| {
            let text = preview.text.clone();
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers())
                .with_source_line(preview.active_line());
            let mut engine = engine_set(mode, styled, &plain());
            ["いろは", "にほへ", "とちり"].map(|word| line_axis_at(&mut engine, mode, &text, word))
        };

        let quiet = crate::document::PreviewDocument::from_source(source);
        let settled = places(&quiet);
        // The caret on the last row, whose cells are the narrowest in the
        // table: the further its column carries them, the plainer the test.
        let at = source.find("| 狭").expect("the row is in the source");
        let active =
            crate::document::PreviewDocument::from_source_with_active_line(source, Some(at));
        assert_eq!(
            active.active_line(),
            Some(3),
            "the caret is on the last row"
        );
        let now = places(&active);

        assert!(
            (settled[0] - now[0]).abs() <= 0.5 && (settled[1] - now[1]).abs() <= 0.5,
            "the other rows moved: {settled:?} became {now:?}"
        );
        assert!(
            now[2] < settled[2] - 20.0,
            "the caret's row is still set in its columns: {settled:?} became {now:?}"
        );

        // **And an engine that has already set this table has to set it
        // again.** Moving the caret from one row to another changes not one
        // character, not one mark and not one marker — so an engine that
        // compared only those would answer with the boxes it had.
        let text = quiet.text.clone();
        let styled = StyledText::marked(&quiet.text, &styles, quiet.marks())
            .with_markers(quiet.markers())
            .with_source_line(quiet.active_line());
        let mut engine = engine_set(mode, styled, &plain());
        let before = line_axis_at(&mut engine, mode, &text, "とちり");
        let styled = StyledText::marked(&active.text, &styles, active.marks())
            .with_markers(active.markers())
            .with_source_line(active.active_line());
        engine
            .update(styled, LineFit::Extent(LINE_EXTENT), &plain())
            .expect("DirectWrite block measurement");
        let after = line_axis_at(&mut engine, mode, &text, "とちり");
        assert!(
            after < before - 20.0,
            "the engine kept the boxes it had: {before} then {after}"
        );
    }

    /// 要件 7.3.1: the delimiter row shows its own `| --- | --- |` when the
    /// caret is on it, and takes a row's worth of room to do it. **Otherwise it
    /// is not on the page at all** — the line it stands for is the boundary
    /// between the two rows either side of it, and that is ruled like every
    /// other boundary in the table.
    #[test]
    fn the_delimiter_row_comes_back_for_the_caret() {
        let source = "| 短 | いろは |\n| --- | --- |\n| 狭 | とちり |\n";
        let styles = crate::document::line_styles(source);
        let at = source
            .find("| ---")
            .expect("the delimiter row is in the source");
        let mode = WritingMode::Horizontal;

        let quiet = crate::document::PreviewDocument::from_source(source);
        let styled =
            StyledText::marked(&quiet.text, &styles, quiet.marks()).with_markers(quiet.markers());
        let engine = engine_set(mode, styled, &plain());
        let settled = grid_of(&engine);
        assert!(
            settled.cells.iter().all(|cell| cell.row != 1),
            "the delimiter row shows nothing: {:?}",
            settled.cells
        );

        let active =
            crate::document::PreviewDocument::from_source_with_active_line(source, Some(at));
        let styled = StyledText::marked(&active.text, &styles, active.marks())
            .with_markers(active.markers())
            .with_source_line(active.active_line());
        let engine = engine_set(mode, styled, &plain());
        let shown = grid_of(&engine);
        let row = shown
            .cells
            .iter()
            .filter(|cell| cell.row == 1)
            .collect::<Vec<&GridCell>>();
        assert_eq!(row.len(), 1, "one cell holding the whole line: {row:?}");
        assert!(row[0].flow_size > 0.0, "{row:?}");
    }

    /// Where the rules that cross a table stand, as flow-axis pixel positions.
    ///
    /// **Read off the tile, because that is the only place they exist.** A rule
    /// runs the length of the line axis, which is a row of the tile's image
    /// across a page and a column of it down a column — the one place in these
    /// tests where the two directions are not the same arithmetic.
    fn rules_across(engine: &mut TextEngine, mode: WritingMode) -> Vec<usize> {
        let tiles = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
        let mut found = Vec::new();
        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&tiles, None, &mut drawn)
            .expect("tile render");
        for (span, width, height, bgra) in &drawn.tiles {
            let (across, along) = match mode {
                WritingMode::Horizontal => (*width as usize, *height as usize),
                WritingMode::Vertical => (*height as usize, *width as usize),
            };
            let ink = |flow: usize, line: usize| {
                let (x, y) = match mode {
                    WritingMode::Horizontal => (line, flow),
                    WritingMode::Vertical => (flow, line),
                };
                bgra[(y * *width as usize + x) * 4 + 2] < 245
            };
            for flow in 0..along {
                let mut run = 0;
                let mut longest = 0;
                for line in 0..across {
                    run = if ink(flow, line) { run + 1 } else { 0 };
                    longest = longest.max(run);
                }
                // Longer than any glyph, which is what tells a rule from
                // a row of text.
                if longest > 60 {
                    found.push(span.flow_start as usize + flow);
                }
            }
        }
        // A stroke a little over one pixel wide lands on two of them.
        found.dedup_by(|left, right| *left <= *right + 1);
        found
    }

    /// 要件 7.3.2: **a table is ruled into even rows in both directions.**
    ///
    /// Down a column the flow axis runs the other way (`FlowOrder`), so a rule
    /// drawn at a row's near coordinate without asking which way lands on the
    /// far side of it. That put two of the three rules of a two-row table one
    /// pixel apart and left the table's far side unruled — **and only the
    /// pixels say so**, because every extent the layout reports is right.
    #[test]
    fn a_table_rules_its_rows_evenly_in_both_directions() {
        let source = "| 上 | 下 |\n| --- | --- |\n| あ | い |\n";
        for mode in [WritingMode::Horizontal, WritingMode::Vertical] {
            let (preview, styles) = preview_of(source);
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers());
            let mut engine = engine_set(mode, styled, &plain());
            let rules = rules_across(&mut engine, mode);

            assert_eq!(rules.len(), 3, "{mode:?} ruled at {rules:?}");
            let first = rules[1] - rules[0];
            let second = rules[2] - rules[1];
            assert!(
                first.abs_diff(second) <= 3,
                "{mode:?} rows are {first} and {second} apart"
            );
        }
    }

    /// 要件 7.3.2: **a cell's text is not pressed against the rule above it.**
    /// The space a line's spacing adds is set below the line, so rules drawn at
    /// the lines' own edges leave every row hard against its top rule with a
    /// gap under it. Half of that space belongs above the row.
    #[test]
    fn a_cell_leaves_room_above_its_text() {
        let source = "| 上 | 下 |\n| --- | --- |\n| あ | い |\n";
        let mode = WritingMode::Horizontal;
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mut engine = engine_set(mode, styled, &plain());
        let rules = rules_across(&mut engine, mode);
        let top = *rules.first().expect("a table is ruled");

        // The first row of pixels holding a glyph. **Rows the rules run along
        // are passed over**: where two rules cross, the paper is painted twice
        // and comes out as dark as ink.
        let tiles = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
        let mut first_glyph = usize::MAX;
        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&tiles, None, &mut drawn)
            .expect("tile render");
        for (span, width, height, bgra) in &drawn.tiles {
            for y in 0..*height as usize {
                let flow = span.flow_start as usize + y;
                if rules.iter().any(|at| at.abs_diff(flow) <= 2) {
                    continue;
                }
                let dark =
                    (0..*width as usize).any(|x| bgra[(y * *width as usize + x) * 4 + 2] < 150);
                if dark {
                    first_glyph = first_glyph.min(flow);
                }
            }
        }

        assert!(
            first_glyph > top + 2,
            "the text starts at {first_glyph} under a rule at {top}"
        );
    }

    /// 要件 7.3.2: **the caret and the click agree inside a table.** Every
    /// question about a position now goes through the cell that holds it, and
    /// there are five places that ask; one of them still answering from the
    /// block's own layout would put the caret in a different cell from the one
    /// the reader pointed at. **Asked in both directions**, because the two
    /// disagree about which way the flow axis runs.
    #[test]
    fn a_click_in_a_table_lands_where_the_caret_is() {
        let source = "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | にほへ |\n";
        for mode in [WritingMode::Horizontal, WritingMode::Vertical] {
            let (preview, styles) = preview_of(source);
            let text = preview.text.clone();
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers());
            let mut engine = engine_set(mode, styled, &plain());
            for word in ["短", "いろは", "とても長い見出しの語", "にほへ"] {
                let byte = text.find(word).expect("the word is in the preview");
                let at = utf16_units(&text[..byte]);
                let caret = engine.caret_geometry(at).expect("caret geometry");
                // A hair inside the character the caret stands before, so the
                // answer is that character and not the edge of the one before.
                let (flow, line) = mode.to_axes(caret.x, caret.y);
                let (x, y) = mode.to_screen(flow + caret.height * 0.5, line + 1.0);
                let hit = engine.hit_test(x, y).expect("hit test");
                assert_eq!(
                    hit.utf16_position, at,
                    "{mode:?}: a click on {word:?} answered {}",
                    hit.utf16_position
                );
            }
        }
    }

    /// 要件 7.3.2: **the rules across a table are actually drawn.** The list of
    /// runs the layout is built from and the list the ink is drawn from were
    /// once built in two different places, and the result was a table whose
    /// columns lined up under a rule nobody ever drew — the boxes were on the
    /// layout and the pass that draws into them had never heard of them. **Only
    /// the pixels say this one**, which is why it is asked for here and not of
    /// `table_marks`.
    #[test]
    fn the_rules_across_a_table_reach_the_pixels() {
        let source = "| 短 | いろは |\n| --- | --- |\n| 狭 | とちり |\n";
        let (preview, styles) = preview_of(source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let mut engine = engine_set(WritingMode::Horizontal, styled, &plain());
        let tiles = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );

        // The longest unbroken run of ink along one row of pixels. **The
        // threshold is the lighter one** for the reason the column rules' test
        // gives: a stroke a little over one pixel wide lands on two of them
        // when it falls between their centres. What tells a rule from an
        // anti-aliased glyph edge is the length, not the strength.
        let mut longest = 0_u32;
        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&tiles, None, &mut drawn)
            .expect("horizontal tile render");
        for (_, width, _, bgra) in &drawn.tiles {
            for row in bgra.chunks_exact(*width as usize * 4) {
                let mut run = 0;
                for pixel in row.chunks_exact(4) {
                    run = if pixel[2] < 245 { run + 1 } else { 0 };
                    longest = longest.max(run);
                }
            }
        }

        // No glyph at this size is 60 pixels across in one unbroken row, and
        // the document holds no `---`. **And the rules stop at the table**: a
        // page-wide one would reach most of the line box, which is 454 here.
        assert!(
            (60..250).contains(&longest),
            "the longest unbroken run of ink is {longest} pixels"
        );
    }

    /// 要件 7.3.2: **a table wider than the pane is brought inside it**, and
    /// what a narrowed column has no width for **wraps inside the column**
    /// rather than coming off the page. The row is then as tall as its tallest
    /// cell — which is the whole of why a table is set cell by cell.
    #[test]
    fn a_table_wider_than_the_pane_wraps_inside_its_columns() {
        let wide = "あ".repeat(40);
        let source = format!("| {wide} | いろは |\n| --- | --- |\n| 狭 | にほへ |\n");
        let mode = WritingMode::Horizontal;
        let (preview, styles) = preview_of(&source);
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let engine = engine_set(mode, styled, &plain());
        let grid = grid_of(&engine);

        let page = LINE_EXTENT as f32 - margin_for(plain().font_size) * 2.0;
        assert!(
            grid.reach <= page + 0.5,
            "the table reaches {} on a page of {page}",
            grid.reach
        );
        // **Nothing is lost**: the long cell holds all of its text, and it is
        // taller than the short one beside it because it wrapped.
        let long = grid
            .cells
            .iter()
            .find(|cell| cell.utf16_len as usize == wide.chars().count())
            .expect("the long cell is in the grid");
        let short = grid
            .cells
            .iter()
            .find(|cell| cell.row == long.row && cell.column != long.column)
            .expect("the cell beside it");
        assert_eq!(
            long.utf16_len as usize,
            wide.chars().count(),
            "the long cell keeps all of its text"
        );
        // And it wrapped: a cell of one line is one font size tall.
        assert!(
            long.flow_size > plain().font_size * 2.0,
            "the long cell is {} tall, against a line of {}",
            long.flow_size,
            plain().font_size
        );
        let _ = short;
    }

    /// 要件 7.3.2: **a keystroke somewhere else does not re-measure a table**
    /// (2026-09-06).
    ///
    /// Tables used to be measured ahead of the cache, so every table in the
    /// document was laid out cell by cell on every keystroke wherever the
    /// writer was typing — 1.13ms per 25-row table, 19.3ms of a keystroke for a
    /// plan holding sixteen of them. **The grid is the same `Arc`**, which says
    /// the measurement was not merely equal but never taken again.
    #[test]
    fn a_keystroke_away_from_a_table_does_not_measure_it() {
        let table = "| 章 | 場面 | 視点 |\n| --- | --- | --- |\n\
                     | 第1章 | No.1 | 少年 |\n| 第2章 | No.2 | 隊 |\n";
        let head = "書き出しの段落。\n\n";
        let source = format!("{head}{table}\n終わりの段落。\n");
        let lay_out = |engine: &mut TextEngine, source: &str| {
            let (preview, styles) = preview_of(source);
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers());
            engine
                .update(styled, LineFit::Extent(LINE_EXTENT), &plain())
                .expect("update")
        };

        let mut engine = TextEngine::new(WritingMode::Horizontal);
        lay_out(&mut engine, &source);
        let before = engine
            .plan
            .blocks
            .iter()
            .find_map(|block| block.grid.clone())
            .expect("the table was measured");

        // One character typed in the paragraph above the table.
        let edited = source.replacen("書き出し", "書き出しの", 1);
        let cost = lay_out(&mut engine, &edited);
        let after = engine
            .plan
            .blocks
            .iter()
            .find_map(|block| block.grid.clone())
            .expect("the table is still there");

        assert_eq!(cost.blocks, 1, "only the edited paragraph is measured");
        assert!(
            Arc::ptr_eq(&before, &after),
            "the table was measured again for a keystroke outside it"
        );
    }

    /// 要件 7.3.1: **but the caret walking into a row does measure it again.**
    ///
    /// A row reads the same either way — a bar is a bar — so the block's text
    /// and its runs are identical, and the boxes that cover the bars are not
    /// made from the runs but from what `measure_table` works out. Nothing in
    /// the key would carry it, which is why `measure_key` takes the row.
    #[test]
    fn the_caret_walking_into_a_row_measures_the_table_again() {
        let source = "| 短 | いろは |\n| --- | --- |\n| とても長い見出しの語 | にほへ |\n";
        let styles = crate::document::line_styles(source);
        let lay_out = |engine: &mut TextEngine, row: Option<usize>| {
            let preview = crate::document::PreviewDocument::from_source(source);
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers())
                .with_source_line(row);
            engine
                .update(styled, LineFit::Extent(LINE_EXTENT), &plain())
                .expect("update")
        };

        let mut engine = TextEngine::new(WritingMode::Horizontal);
        lay_out(&mut engine, None);
        let quiet = engine
            .plan
            .blocks
            .iter()
            .find_map(|block| block.grid.clone())
            .expect("the table was measured");

        let cost = lay_out(&mut engine, Some(2));
        let active = engine
            .plan
            .blocks
            .iter()
            .find_map(|block| block.grid.clone())
            .expect("the table is still there");

        assert_eq!(cost.blocks, 1, "the table is measured again");
        assert!(
            !Arc::ptr_eq(&quiet, &active),
            "the caret moved into a row and the grid did not change"
        );
    }

    /// 要件 7.3.1: **a source pane sets a table as text**, bars and all — the
    /// same signal every other stand-in for markup reads. There is no grid at
    /// all there, so a block of it is one layout like any other.
    #[test]
    fn a_source_pane_sets_a_table_as_text() {
        let source = "| 短 | いろは |\n| --- | --- |\n";
        let styles = crate::document::line_styles(source);
        let styled = StyledText::new(source, &styles);
        let engine = engine_set(WritingMode::Horizontal, styled, &plain());

        assert!(engine.plan.blocks.iter().all(|block| block.grid.is_none()));
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
    /// 要件 7.1: **the rectangle is cut out of the lines on the screen.**
    ///
    /// One paragraph is one line in the file however far it wraps, so a
    /// rectangle built out of file lines could not cross it at all. This walks
    /// the lines the engine laid out, and there is one run for each of them.
    #[test]
    fn a_rectangle_takes_one_run_out_of_every_line_it_crosses() {
        let text = format!("{}\n", "あいうえおかきくけこ".repeat(40));

        for mode in [WritingMode::Horizontal, WritingMode::Vertical] {
            let styled = StyledText::plain(&text);
            let mut engine = engine_set(mode, styled, &plain());
            let end = engine.utf16_len() / 2;
            let (first, last) = (
                engine.plan.locate(0).expect("a line at the start"),
                engine.plan.locate(end).expect("a line in the middle"),
            );
            // The lines the walk has to cover, counted the way the plan reads
            // them rather than the way the rectangle does.
            let mut expected = 1;
            let mut at = first;
            while at != last {
                at = engine
                    .plan
                    .step_line(at.0, at.1, true)
                    .expect("a next line");
                expected += 1;
            }
            assert!(expected > 3, "{mode:?}: the paragraph has to wrap");

            let from = engine.caret_geometry(0).expect("caret at the start");
            let to = engine.caret_geometry(end).expect("caret in the middle");
            let (lo, hi) = match mode {
                WritingMode::Vertical => (from.y, to.y),
                WritingMode::Horizontal => (from.x, to.x),
            };
            let runs = engine
                .rectangle_runs(0, end, lo, hi)
                .expect("the rectangle's runs");

            assert_eq!(runs.len(), expected, "{mode:?}: one run per line");
            // Every run stays inside the line it was cut from, and they come
            // out in reading order.
            let mut at = first;
            for (index, (start, end)) in runs.iter().enumerate() {
                let line_start = engine
                    .plan
                    .line_utf16_start(at.0, at.1)
                    .expect("the line's start");
                let line_end = engine
                    .plan
                    .line_utf16_text_end(at.0, at.1)
                    .expect("the line's end");
                assert!(
                    line_start <= *start && *start <= *end && *end <= line_end,
                    "{mode:?}: run {index} {start}..{end} left its line {line_start}..{line_end}"
                );
                if at != last {
                    at = engine
                        .plan
                        .step_line(at.0, at.1, true)
                        .expect("a next line");
                }
            }
        }
    }

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
        let all = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
        assert!(all.len() > 2, "the sample must produce several tiles");
        let tile = all[1];

        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&[tile], None, &mut drawn)
            .expect("visible tile render");

        assert_eq!(drawn.tiles.len(), 1);
        let (span, width, height, ref pixels) = drawn.tiles[0];
        let bytes = pixels.len();
        assert_eq!(span, tile);
        assert_eq!(
            (width, height),
            (tile.flow_size, engine.line_extent()),
            "a vertical tile is its flow extent wide and the pane tall"
        );
        assert_eq!(bytes, width as usize * height as usize * 4);
    }

    /// 要件 9: a line the writer did not let wrap is longer than the pane, so
    /// the page is cut across the flow as well — and the slice past the first
    /// has to hold what is past it. **A missing offset draws the head of the
    /// line twice; a doubled one draws blank paper.**
    #[test]
    fn a_slice_past_the_first_holds_the_far_end_of_the_line() {
        let text = "あ".repeat(200);
        let mut engine = TextEngine::new(WritingMode::Horizontal);
        engine
            .update(StyledText::plain(&text), LineFit::Free, &plain())
            .expect("free layout");
        assert!(
            engine.line_extent() > MAX_TILE_CROSS,
            "the sample must be wider than one slice: {}",
            engine.line_extent()
        );

        // A pane looking at the far end of the line.
        let far = engine.line_extent() as f32 - 600.0;
        let tiles = engine.visible_tiles(0.0, engine.total_flow_size() as f32, 0, -far, 600.0);
        assert!(
            tiles.iter().all(|tile| tile.cross_index > 0),
            "the near slice is not wanted at the far end: {tiles:?}"
        );

        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&tiles[..1], None, &mut drawn)
            .expect("far slice render");
        let (span, width, height, pixels) = &drawn.tiles[0];
        assert_eq!(
            (*width, *height),
            (span.cross_size, span.flow_size),
            "a horizontal tile is its slice wide and its flow extent tall"
        );
        let paper = &pixels[0..4];
        assert!(
            pixels.chunks_exact(4).any(|point| point != paper),
            "the far slice holds ink"
        );
    }

    /// The horizontal tile is the same slice turned a quarter: as wide as the
    /// pane and as tall as the tile reaches along the flow axis.
    #[test]
    fn renders_a_horizontal_tile_across_the_pane() {
        let text = "横書きのタイルを描画する行です。\n".repeat(60);
        let mut engine = engine_in(WritingMode::Horizontal, &text, 22.0);
        assert!(engine.block_count() > 1, "the sample must span many blocks");
        let all = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
        assert!(all.len() > 2, "the sample must produce several tiles");
        let tile = all[1];

        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&[tile], None, &mut drawn)
            .expect("horizontal tile render");

        let (span, width, height, ref pixels) = *drawn.tiles.first().expect("one tile rendered");
        let bytes = pixels.len();
        let ink = pixels
            .chunks_exact(4)
            .filter(|pixel| pixel[0] < 180 && pixel[1] < 180 && pixel[2] < 180)
            .count();
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
        assert!(!Ornament::Indent.draws_ink());
        assert!(Ornament::Bullet.draws_ink());
        assert!(Ornament::Number.draws_ink());
    }

    /// **Only a box over a whole line of marks keeps the room it covered**
    /// (要件 7.3.2). A rule and a fence leave their line behind as blank space,
    /// which is what gives a code block its padding; everything else a box
    /// covers stands where an indent will be, and the indent is the block's —
    /// a box that also took a step would set the line in twice, and only on the
    /// first line it wrapped to.
    #[test]
    fn only_a_whole_line_box_keeps_the_room_it_covered() {
        assert!(Ornament::Hidden.keeps_room());
        assert!(!Ornament::Indent.keeps_room());
        assert!(!Ornament::Bullet.keeps_room());
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
            grid: None,
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
        let all = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
        assert_eq!(all.len(), 1);
        let mut drawn = DrawnTiles::default();
        engine
            .render_tiles(&all, None, &mut drawn)
            .expect("tile render");
        let ink = drawn
            .tiles
            .iter()
            .flat_map(|(_, _, _, bgra)| bgra.chunks_exact(4))
            .filter(|pixel| pixel[0] < 180 && pixel[1] < 180 && pixel[2] < 180)
            .count();

        assert!(ink > 100, "expected visible glyph pixels");
    }

    /// 要件 7.3.2: **and the comment's ink reaches the pixels.**
    ///
    /// The same trap the rule under a table's header fell into (7.7): a mark
    /// the layout knows about and the drawing has never heard of. **Only the
    /// pixels say it** — and the way to ask is to draw the same code twice,
    /// once in a block that names its language and once in one that does not.
    /// The glyphs and their coverage are identical; the ink is the only thing
    /// that differs.
    #[test]
    fn a_comment_is_drawn_in_its_own_ink() {
        let code = "let a = 1; // これは説明です。日本語の注釈が続きます。\n";
        let counted = |fence: &str| {
            let source = format!("```{fence}\n{code}```\n");
            let (preview, styles) = preview_of(&source);
            let styled = StyledText::marked(&preview.text, &styles, preview.marks())
                .with_markers(preview.markers());
            let mut engine = engine_set(WritingMode::Horizontal, styled, &plain());
            let tiles = engine.visible_tiles(
                0.0,
                engine.total_flow_size() as f32,
                0,
                0.0,
                LINE_EXTENT as f32,
            );
            let mut drawn = DrawnTiles::default();
            engine
                .render_tiles(&tiles, None, &mut drawn)
                .expect("tile render");

            let wanted = plain().comment_ink();
            let byte = |value: f32| (value * 255.0).round() as i32;
            let near = |pixel: &[u8], of: [f32; 3]| {
                // BGRA, and the ink is given in RGB order.
                (pixel[2] as i32 - byte(of[0])).abs() <= 3
                    && (pixel[1] as i32 - byte(of[1])).abs() <= 3
                    && (pixel[0] as i32 - byte(of[2])).abs() <= 3
            };
            let mut faded = 0;
            let mut full = 0;
            for (_, _, _, bgra) in &drawn.tiles {
                for pixel in bgra.chunks_exact(4) {
                    if near(pixel, wanted) {
                        faded += 1;
                    }
                    if near(pixel, plain().ink) {
                        full += 1;
                    }
                }
            }
            (faded, full)
        };

        let (faded_named, full_named) = counted("rust");
        let (faded_plain, full_plain) = counted("");

        // **The body's ink is the sharper of the two**, because a pixel is
        // either the ink or it is not; the faded count also picks up the
        // half-covered edge of every body glyph, which passes through the
        // comment's colour on its way from paper to ink.
        assert!(
            full_named * 3 < full_plain,
            "naming the language should take most of the ink off the body's \
             colour: {full_named} against {full_plain}"
        );
        assert!(
            faded_named > faded_plain * 2,
            "and put it on the comment's: {faded_named} against {faded_plain}"
        );
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
        let all = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );
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
            .visible_tiles(
                0.0,
                engine.total_flow_size() as f32,
                0,
                0.0,
                LINE_EXTENT as f32,
            )
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
        let after = engine.visible_tiles(
            0.0,
            engine.total_flow_size() as f32,
            0,
            0.0,
            LINE_EXTENT as f32,
        );

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
            .update(StyledText::plain(&text), LineFit::Extent(1_000), &huge)
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

/// The terminal's cells (追加要件 Terminal).
///
/// **The same stack, a different layout.** Direct2D, DirectWrite, the render
/// target and the pixels handed to Slint are the document's; what is not shared
/// is the arrangement — a terminal has no blocks, no wrapping and no measuring,
/// because every cell is exactly where its row and column say (技術検証 9.4).
///
/// Nothing in the binary calls this until the pane does, the same as
/// [`crate::pty`]; the allow goes when the pane arrives.
#[allow(dead_code)]
pub mod cells {
    use super::*;

    // --- the terminal's cells (追加要件 Terminal) ---------------------------------
    //
    // **The same stack, a different layout.** Direct2D, DirectWrite, the render
    // target and the pixels handed to Slint are the document's; what is not shared
    // is the arrangement — a terminal has no blocks, no wrapping and no measuring,
    // because every cell is exactly where its row and column say (技術検証 9.4).

    /// How a terminal is set (要件 9 will own these; the defaults are here).
    #[derive(Clone, Debug)]
    pub struct TerminalLook {
        /// A monospaced family. Anything else lays out fine and lines up wrong.
        pub family: String,
        pub font_size: f32,
        /// Multiplier on the font's own line height. **Terminals are set tight** —
        /// the grid is the reading aid, not the leading.
        pub line_spacing: f32,
        pub paper: [f32; 3],
        pub ink: [f32; 3],
        /// The 16 named colours, in the order SGR gives them: black, red, green,
        /// yellow, blue, magenta, cyan, white, then the bright eight.
        pub palette: [[f32; 3]; 16],
    }

    /// The 16 colours, mixed for this editor's ivory paper rather than for a black
    /// screen: **the same hues, brought down far enough to be read on paper.**
    const TERMINAL_PALETTE: [[f32; 3]; 16] = [
        [0.20, 0.19, 0.18], // black — the paper's ink, near enough
        [0.70, 0.16, 0.16], // red
        [0.18, 0.45, 0.20], // green
        [0.63, 0.45, 0.09], // yellow
        [0.16, 0.32, 0.66], // blue
        [0.53, 0.22, 0.62], // magenta
        [0.11, 0.45, 0.48], // cyan
        [0.42, 0.40, 0.37], // white (the dim one)
        [0.42, 0.40, 0.37], // bright black
        [0.83, 0.26, 0.24],
        [0.24, 0.58, 0.27],
        [0.76, 0.57, 0.15],
        [0.24, 0.44, 0.80],
        [0.65, 0.32, 0.74],
        [0.16, 0.57, 0.60],
        [0.20, 0.19, 0.18], // bright white — ink again, so bold text stays read
    ];

    impl Default for TerminalLook {
        fn default() -> Self {
            Self {
                family: crate::text_blocks::DEFAULT_CODE_FONT.to_owned(),
                font_size: 15.0,
                line_spacing: 1.0,
                paper: crate::text_blocks::DEFAULT_PAPER,
                ink: DEFAULT_INK,
                palette: TERMINAL_PALETTE,
            }
        }
    }

    /// One cell's size in pixels. **Everything about a terminal's geometry is these
    /// two numbers**: how many columns fit, where a click lands, what the shell is
    /// told its screen is.
    #[derive(Clone, Copy, PartialEq, Debug)]
    pub struct CellSize {
        pub advance: f32,
        pub line: f32,
    }

    impl CellSize {
        /// The columns and rows a pane of this size holds. **At least one of each**:
        /// a console of no size is one no program can draw on.
        pub fn grid_for(&self, width: f32, height: f32) -> (usize, usize) {
            let columns = (width / self.advance.max(1.0)).floor().max(1.0) as usize;
            let rows = (height / self.line.max(1.0)).floor().max(1.0) as usize;
            (columns, rows)
        }
    }

    /// Measure the font the terminal is set in.
    ///
    /// **Measured, not assumed.** A family's advance is its own business, and the
    /// writer chooses the family (要件 9). Ten digits are measured rather than one
    /// character, so the answer is not one glyph's rounding.
    pub fn terminal_cell_size(look: &TerminalLook) -> Result<CellSize> {
        with_graphics(|graphics| {
            let key = (look.font_size.to_bits(), look.family.clone());
            if let Some((held, size)) = &graphics.cell_size {
                if *held == key {
                    return Ok(*size);
                }
            }
            let format = graphics.cell_format(look, false)?;
            let utf16 = "0000000000".encode_utf16().collect::<Vec<u16>>();
            // SAFETY: the buffer and the format outlive the call.
            let layout = unsafe {
                graphics
                    .dwrite
                    .CreateTextLayout(&utf16, &format, f32::MAX, f32::MAX)?
            };
            // SAFETY: the layout is alive for the call.
            let metrics = unsafe {
                let mut metrics = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut metrics)?;
                metrics
            };
            let advance = (metrics.widthIncludingTrailingWhitespace / 10.0).max(1.0);
            let line = (metrics.height * look.line_spacing.max(0.5)).max(1.0);
            // **The advance is not rounded.** A run of cells is drawn as one string,
            // so the font's own advance and the column pitch have to be the same
            // number — round the pitch up and every character in the run lands a
            // fraction of a pixel further left than its column, which by the end of
            // a line is several columns' worth and puts a TUI's frame out of true.
            // The line advance is rounded, because rows are drawn one at a time and
            // nothing accumulates across them.
            let size = CellSize {
                advance,
                line: line.ceil(),
            };
            graphics.cell_size = Some((key, size));
            Ok(size)
        })
    }

    impl Graphics {
        fn cell_format(&mut self, look: &TerminalLook, bold: bool) -> Result<IDWriteTextFormat> {
            let size = look.font_size.max(1.0);
            let key = (size.to_bits(), look.family.clone(), bold);
            if let Some(format) = self.cell_formats.get(&key) {
                return Ok(format.clone());
            }
            let family = HSTRING::from(look.family.as_str());
            let weight = if bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            };
            // SAFETY: the factory outlives this struct and the name outlives the call.
            let format = unsafe {
                self.dwrite.CreateTextFormat(
                    &family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("ja-JP"),
                )?
            };
            self.cell_formats.insert(key, format.clone());
            Ok(format)
        }
    }

    /// A colour the shell named, as pixels.
    ///
    /// **The 256-colour cube is arithmetic, not a table** — 16 named, then a 6×6×6
    /// cube, then 24 greys. Writing the table out would be 240 numbers nobody could
    /// check.
    fn cell_colour(colour: CellColor, look: &TerminalLook, foreground: bool) -> [f32; 3] {
        match colour {
            CellColor::Default => {
                if foreground {
                    look.ink
                } else {
                    look.paper
                }
            }
            CellColor::Rgb(red, green, blue) => [
                red as f32 / 255.0,
                green as f32 / 255.0,
                blue as f32 / 255.0,
            ],
            CellColor::Indexed(index) => match index {
                0..=15 => look.palette[index as usize],
                16..=231 => {
                    let index = index - 16;
                    let step = |value: u8| {
                        if value == 0 {
                            0.0
                        } else {
                            (55.0 + value as f32 * 40.0) / 255.0
                        }
                    };
                    [step(index / 36), step((index % 36) / 6), step(index % 6)]
                }
                _ => {
                    let grey = (8 + (index as u32 - 232) * 10) as f32 / 255.0;
                    [grey, grey, grey]
                }
            },
        }
    }

    /// A run of cells drawn in one go: same attributes, same selectedness, and
    /// none of them wide.
    struct CellRun {
        column: usize,
        columns: usize,
        text: String,
        attrs: CellAttrs,
        selected: bool,
    }

    /// Cut one row into runs.
    ///
    /// **Only plain ASCII joins a run; everything else is drawn one cell at a
    /// time.** A run is laid out by DirectWrite with the font's own advances, and
    /// the only font whose advance is the cell is the monospaced one the terminal
    /// was set in. The moment a character falls back to another family — a kanji, a
    /// box-drawing rule, an arrow — the run drifts out of its columns by the
    /// difference, and a TUI's frame stops meeting itself. Placed by its own
    /// column, a glyph of any width lands where the grid says it does.
    ///
    /// The spike drew `┌──┬──┐` with its uprights out of true before this rule was
    /// here, which is how the rule was found.
    fn cell_runs(line: &CellLine, selection: Option<(usize, usize)>) -> Vec<CellRun> {
        let selected =
            |column: usize| selection.is_some_and(|(from, to)| (from..to).contains(&column));
        let mut runs: Vec<CellRun> = Vec::new();
        for (column, cell) in line.cells.iter().enumerate() {
            if cell.trailing {
                continue;
            }
            let wide = character_width(cell.text) == 2;
            let plain = |text: char| text.is_ascii() && !text.is_ascii_control();
            let joins = plain(cell.text)
                && !wide
                && runs.last().is_some_and(|run| {
                    run.attrs == cell.attrs
                        && run.selected == selected(column)
                        && run.column + run.columns == column
                        && run.text.chars().next_back().is_some_and(plain)
                });
            if joins {
                let run = runs.last_mut().expect("checked above");
                run.text.push(cell.text);
                run.columns += 1;
            } else {
                runs.push(CellRun {
                    column,
                    columns: if wide { 2 } else { 1 },
                    text: cell.text.to_string(),
                    attrs: cell.attrs,
                    selected: selected(column),
                });
            }
        }
        runs
    }

    /// Draw a terminal screen into `into` (BGRA, `width` × `height`).
    ///
    /// `cursor` is where the block caret goes, if it is showing. **The rows are
    /// given rather than the screen** so that the same drawing serves the
    /// scrollback, which is the same cells one screenful earlier.
    /// `preedit` is what the IME is composing: drawn at the cursor and
    /// underlined, and **not in the grid** — the shell has not been told about
    /// it and must not be until it is committed, so it is painted over whatever
    /// is underneath.
    pub fn draw_terminal(
        lines: &[CellLine],
        selection: &[Option<(usize, usize)>],
        cursor: Option<(usize, usize)>,
        preedit: &str,
        look: &TerminalLook,
        cell: CellSize,
        into: &mut [u8],
        width: u32,
        height: u32,
    ) -> Result<()> {
        with_graphics(|graphics| {
            let plain = graphics.cell_format(look, false)?;
            let bold = graphics.cell_format(look, true)?;
            let (target, brush, bitmap) = {
                let cache = graphics.render_target(width, height)?;
                (
                    cache.target.clone(),
                    cache.brush.clone(),
                    cache.bitmap.clone(),
                )
            };
            // SAFETY: the target, brush and bitmap live as long as the cache entry,
            // and BeginDraw/EndDraw are paired below.
            unsafe {
                target.BeginDraw();
                target.Clear(Some(&colour(look.paper)));
            }
            for (row, line) in lines.iter().enumerate() {
                let top = row as f32 * cell.line;
                if top >= height as f32 {
                    break;
                }
                for run in cell_runs(line, selection.get(row).copied().flatten()) {
                    let left = run.column as f32 * cell.advance;
                    let right = left + run.columns as f32 * cell.advance;
                    let rect = D2D_RECT_F {
                        left,
                        top,
                        right,
                        bottom: top + cell.line,
                    };
                    // 反転（SGR 7）は色を入れ替えるだけ。**解決してから入れ替える**
                    // ので、既定の紙と墨も正しく裏返る。
                    let mut foreground = cell_colour(run.attrs.foreground, look, true);
                    let mut background = cell_colour(run.attrs.background, look, false);
                    // **Selected cells are drawn the way the shell draws its
                    // own selection**: the two colours change places. Two
                    // reversals cancel, which is right — a cell the program
                    // already reversed is shown unreversed when it is picked.
                    if run.attrs.reverse != run.selected {
                        std::mem::swap(&mut foreground, &mut background);
                    }
                    if run.attrs.faint {
                        for channel in &mut foreground {
                            *channel = *channel * 0.6 + look.paper[0] * 0.4;
                        }
                    }
                    // SAFETY: the brush belongs to the target and the rect is read
                    // before the call returns.
                    unsafe {
                        if background != look.paper {
                            brush.SetColor(&colour(background));
                            target.FillRectangle(&rect, &brush);
                        }
                        if run.attrs.hidden || run.text.trim().is_empty() {
                            if run.attrs.underline {
                                brush.SetColor(&colour(foreground));
                                let line_rect = D2D_RECT_F {
                                    top: top + cell.line - 2.0,
                                    bottom: top + cell.line - 1.0,
                                    ..rect
                                };
                                target.FillRectangle(&line_rect, &brush);
                            }
                            continue;
                        }
                        brush.SetColor(&colour(foreground));
                        let utf16 = run.text.encode_utf16().collect::<Vec<u16>>();
                        let format = if run.attrs.bold { &bold } else { &plain };
                        // **A cell is not a line box.** The rectangle is the run's
                        // own columns, so a glyph wider than its cell is clipped to
                        // where the grid says it ends rather than pushing the row.
                        target.DrawText(
                            &utf16,
                            format,
                            &rect,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                        if run.attrs.underline {
                            let line_rect = D2D_RECT_F {
                                top: top + cell.line - 2.0,
                                bottom: top + cell.line - 1.0,
                                ..rect
                            };
                            target.FillRectangle(&line_rect, &brush);
                        }
                    }
                }
            }
            if let (Some((row, column)), false) = (cursor, preedit.is_empty()) {
                // **変換中の字はカーソルの場所に立つ**——自分の紙の上に、下線つきで。
                // どの端末もその2つで「まだ行に入っていない」と言う。
                let top = row as f32 * cell.line;
                let cells: usize = preedit.chars().map(character_width).sum();
                let left = column as f32 * cell.advance;
                let right = (left + cells as f32 * cell.advance).min(width as f32);
                let rect = D2D_RECT_F {
                    left,
                    top,
                    right,
                    bottom: top + cell.line,
                };
                // SAFETY: the brush belongs to the target, and each rectangle is
                // read before the call it is given to returns.
                unsafe {
                    brush.SetColor(&colour(look.paper));
                    target.FillRectangle(&rect, &brush);
                    brush.SetColor(&colour(look.ink));
                    let mut at = column;
                    for text in preedit.chars() {
                        let step = character_width(text).max(1);
                        let left = at as f32 * cell.advance;
                        if left >= width as f32 {
                            break;
                        }
                        let glyph = D2D_RECT_F {
                            left,
                            top,
                            right: left + step as f32 * cell.advance,
                            bottom: top + cell.line,
                        };
                        let utf16 = text.to_string().encode_utf16().collect::<Vec<u16>>();
                        target.DrawText(
                            &utf16,
                            &plain,
                            &glyph,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                        at += step;
                    }
                    let underline = D2D_RECT_F {
                        top: top + cell.line - 2.0,
                        bottom: top + cell.line - 1.0,
                        ..rect
                    };
                    target.FillRectangle(&underline, &brush);
                }
            } else if let Some((row, column)) = cursor {
                let left = column as f32 * cell.advance;
                let top = row as f32 * cell.line;
                let rect = D2D_RECT_F {
                    left,
                    top,
                    right: left + cell.advance,
                    bottom: top + cell.line,
                };
                let under = lines
                    .get(row)
                    .and_then(|line| line.cells.get(column))
                    .map(|cell| cell.text)
                    .unwrap_or(' ');
                // SAFETY: as above.
                unsafe {
                    brush.SetColor(&colour(look.ink));
                    target.FillRectangle(&rect, &brush);
                    if under != ' ' {
                        // **The character under the block is drawn back in the
                        // paper's colour**, so the caret never hides what it is on.
                        brush.SetColor(&colour(look.paper));
                        let utf16 = under.to_string().encode_utf16().collect::<Vec<u16>>();
                        target.DrawText(
                            &utf16,
                            &plain,
                            &rect,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                    }
                }
            }
            // SAFETY: paired with BeginDraw above.
            unsafe { target.EndDraw(None, None)? };

            let stride = width * 4;
            let needed = stride as usize * height as usize;
            let Some(pixels) = into.get_mut(..needed) else {
                return Err(Error::new(
                    E_FAIL,
                    "the terminal buffer is smaller than the terminal",
                ));
            };
            let rect = WICRect {
                X: 0,
                Y: 0,
                Width: width as i32,
                Height: height as i32,
            };
            // SAFETY: the rectangle lies inside the bitmap and the buffer matches
            // the stride and height asked for.
            unsafe {
                let source: IWICBitmapSource = bitmap.cast()?;
                source.CopyPixels(&rect, stride, pixels)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::cells::*;
    use crate::terminal::Terminal;

    /// The pixel columns that hold ink, as clusters of adjacent columns.
    fn ink_columns(pixels: &[u8], width: u32, rows: std::ops::Range<u32>) -> Vec<(u32, u32)> {
        let mut columns: Vec<u32> = Vec::new();
        for row in rows {
            for column in 0..width {
                let at = ((row * width + column) * 4) as usize;
                let (blue, green, red) = (pixels[at], pixels[at + 1], pixels[at + 2]);
                if blue < 140 && green < 140 && red < 140 && !columns.contains(&column) {
                    columns.push(column);
                }
            }
        }
        columns.sort_unstable();
        let mut clusters: Vec<(u32, u32)> = Vec::new();
        for column in columns {
            match clusters.last_mut() {
                Some(last) if column == last.1 + 1 => last.1 = column,
                _ => clusters.push((column, column)),
            }
        }
        clusters
    }

    fn draw(it: &Terminal, look: &TerminalLook, cell: CellSize) -> (Vec<u8>, u32, u32) {
        let width = (it.screen.columns() as f32 * cell.advance).ceil() as u32;
        let height = (it.screen.rows() as f32 * cell.line).ceil() as u32;
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        draw_terminal(
            it.screen.lines(),
            &[],
            None,
            "",
            look,
            cell,
            &mut pixels,
            width,
            height,
        )
        .expect("draw the terminal");
        (pixels, width, height)
    }

    /// **The one thing a terminal cannot get slightly wrong.**
    ///
    /// A run of cells is handed to DirectWrite as a string, and it lays that
    /// string out with the font's advances rather than with the grid's. If the
    /// two differ at all, the difference multiplies by the length of the run —
    /// so the last character of a 40-column run has to land exactly where the
    /// same character lands when it is the only thing on its line.
    #[test]
    fn a_long_run_lands_on_the_same_columns_as_a_single_character() {
        let look = TerminalLook::default();
        let cell = terminal_cell_size(&look).expect("measure the cell");
        let mut it = Terminal::new(48, 3);
        it.feed(b"########################################\r\n");
        it.feed(b"\x1b[40G#");
        let (pixels, width, _) = draw(&it, &look, cell);
        let row = cell.line as u32;
        let in_run = *ink_columns(&pixels, width, 0..row)
            .last()
            .expect("the run drew something");
        let alone = ink_columns(&pixels, width, row..row * 2);
        assert_eq!(
            alone.len(),
            1,
            "only one character was written on the second line"
        );
        assert_eq!(
            in_run, alone[0],
            "the 40th character of a run drifted out of its column"
        );
    }

    /// 追加要件 Terminal: 選んだセルは色が入れ替わる。
    #[test]
    fn a_selected_cell_swaps_its_colours() {
        let look = TerminalLook::default();
        let cell = terminal_cell_size(&look).expect("measure the cell");
        let mut it = Terminal::new(8, 1);
        it.feed(b"abcdefgh");
        let width = (8.0 * cell.advance).ceil() as u32;
        let height = cell.line as u32;
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        // 3桁目から5桁目までを選ぶ。
        draw_terminal(
            it.screen.lines(),
            &[Some((2, 5))],
            None,
            "",
            &look,
            cell,
            &mut pixels,
            width,
            height,
        )
        .expect("draw");
        let ink_at = |column: usize| {
            let x = (column as f32 * cell.advance + cell.advance * 0.5) as u32;
            let at = ((1 * width + x) * 4) as usize;
            (pixels[at], pixels[at + 1], pixels[at + 2])
        };
        let (blue, green, red) = ink_at(3);
        assert!(
            blue < 140 && green < 140 && red < 140,
            "選ばれたセルの背景は墨で塗られる ({red},{green},{blue})"
        );
        let (blue, green, red) = ink_at(6);
        assert!(
            blue > 200 && green > 200 && red > 200,
            "選ばれていないセルは紙のまま ({red},{green},{blue})"
        );
    }

    /// 追加要件 Terminal: 変換中の字はカーソルの場所に、下線つきで立つ。
    #[test]
    fn a_composition_stands_at_the_cursor_and_is_underlined() {
        let look = TerminalLook::default();
        let cell = terminal_cell_size(&look).expect("measure the cell");
        let mut it = Terminal::new(12, 1);
        it.feed(b"ab");
        let width = (12.0 * cell.advance).ceil() as u32;
        let height = cell.line as u32;
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        draw_terminal(
            it.screen.lines(),
            &[],
            Some((0, 2)),
            "あい",
            &look,
            cell,
            &mut pixels,
            width,
            height,
        )
        .expect("draw");
        let inked = |x: u32, y: u32| {
            let at = ((y * width + x) * 4) as usize;
            pixels[at] < 160 && pixels[at + 1] < 160 && pixels[at + 2] < 160
        };
        let under = height - 2;
        // **2桁目から4セル**（全角2文字）に下線が引かれ、その手前には無い。
        assert!(inked((2.5 * cell.advance) as u32, under), "下線がある");
        assert!(inked((5.5 * cell.advance) as u32, under), "4セルぶん続く");
        assert!(
            !inked((1.5 * cell.advance) as u32, under),
            "変換の手前には引かれない"
        );
    }

    /// Reverse video (SGR 7) is the shell's way of showing a selection, and it
    /// has to paint the cell, not just the glyph.
    #[test]
    fn reverse_video_paints_the_cell_behind_the_character() {
        let look = TerminalLook::default();
        let cell = terminal_cell_size(&look).expect("measure the cell");
        let mut it = Terminal::new(8, 2);
        it.feed(b"\x1b[7m  \x1b[m");
        let (pixels, width, _) = draw(&it, &look, cell);
        let corner = ((1 * width + 1) * 4) as usize;
        let (blue, green, red) = (pixels[corner], pixels[corner + 1], pixels[corner + 2]);
        assert!(
            blue < 140 && green < 140 && red < 140,
            "the cell behind reversed spaces is still paper ({red},{green},{blue})"
        );
    }
}
