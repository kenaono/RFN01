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
    hash::{DefaultHasher, Hash, Hasher},
};

use windows::{
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
                D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
                D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Factory,
                ID2D1RenderTarget, ID2D1SolidColorBrush,
            },
            DirectWrite::{
                DWRITE_FACTORY_TYPE_SHARED, DWRITE_FLOW_DIRECTION_RIGHT_TO_LEFT,
                DWRITE_FLOW_DIRECTION_TOP_TO_BOTTOM, DWRITE_FONT_STRETCH_NORMAL,
                DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_HIT_TEST_METRICS,
                DWRITE_LINE_METRICS, DWRITE_READING_DIRECTION_LEFT_TO_RIGHT,
                DWRITE_READING_DIRECTION_TOP_TO_BOTTOM, DWRITE_TEXT_RANGE, DWriteCreateFactory,
                IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
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
    core::{Interface, Result, w},
};

use crate::text_blocks::{
    BlockLayoutPlan, BlockMeasure, FlowOrder, LineInfo, TileSpan, block_flow_bound, cells_per_line,
    place_blocks, split_blocks,
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

const BACKGROUND: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 1.0,
    g: 253.0 / 255.0,
    b: 247.0 / 255.0,
    a: 1.0,
};
const FOREGROUND: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 41.0 / 255.0,
    g: 37.0 / 255.0,
    b: 36.0 / 255.0,
    a: 1.0,
};

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
    formats: HashMap<(u32, WritingMode), IDWriteTextFormat>,
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
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
                d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                wic: CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?,
                formats: HashMap::new(),
                target: None,
                scratch: Vec::new(),
                _apartment: apartment,
            })
        }
    }

    fn text_format(&mut self, font_size: f32, mode: WritingMode) -> Result<IDWriteTextFormat> {
        let font_size = font_size.max(1.0);
        let key = (font_size.to_bits(), mode);
        if let Some(format) = self.formats.get(&key) {
            return Ok(format.clone());
        }

        // SAFETY: The factory is alive for the lifetime of this struct and the
        // string literals are static wide strings.
        let format = unsafe {
            self.dwrite.CreateTextFormat(
                w!("Yu Mincho"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                w!("ja-JP"),
            )?
        };
        mode.apply_to(&format)?;
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
            let (bitmap, target, brush) = unsafe {
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
                let brush = target.CreateSolidColorBrush(&FOREGROUND, None)?;
                (bitmap, target, brush)
            };
            self.target = Some(RenderTargetCache {
                width,
                height,
                bitmap,
                target,
                brush,
            });
        }
        Ok(self.target.as_ref().expect("render target created above"))
    }
}

thread_local! {
    static GRAPHICS: RefCell<Option<Graphics>> = const { RefCell::new(None) };
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

/// The layout of one document in one writing mode, split into independently
/// laid out blocks.
#[derive(Default)]
pub struct TextEngine {
    mode: WritingMode,
    text: String,
    /// The pane's extent along the line axis: its height in vertical writing,
    /// its width in horizontal writing. Every measurement depends on it.
    line_extent: u32,
    font_size: f32,
    margin: f32,
    plan: BlockLayoutPlan,
    /// Measurements keyed by block text. Unchanged blocks survive every edit.
    measures: HashMap<u64, MeasuredBlock>,
    /// Live layouts, most recently used first.
    layouts: Vec<(u64, IDWriteTextLayout)>,
}

/// Identifies a layout object. Two blocks with the same text at the same size
/// share one, whatever their position in the document.
///
/// The mode is not part of the key: these caches belong to one engine, and an
/// engine keeps the mode it was built with.
fn layout_key(text: &str, font_size: f32, line_extent: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    font_size.to_bits().hash(&mut hasher);
    line_extent.hash(&mut hasher);
    hasher.finish()
}

/// Identifies a measurement. Same layout, but the last block of the document
/// keeps a trailing empty line the others give up, so it measures differently.
fn measure_key(
    text: &str,
    font_size: f32,
    line_extent: u32,
    keep_trailing_empty_line: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    layout_key(text, font_size, line_extent).hash(&mut hasher);
    keep_trailing_empty_line.hash(&mut hasher);
    hasher.finish()
}

/// Rounded, because it is the document's starting edge and every block sits a
/// whole number of pixels from it. See `place_blocks`.
fn margin_for(font_size: f32) -> f32 {
    (font_size * 1.5).max(16.0).round()
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

    /// How far a tile reaches along the flow axis, keeping the pixels per tile
    /// roughly constant as the window grows or shrinks.
    pub fn tile_flow_size(&self) -> u32 {
        (TILE_TARGET_PIXELS / self.line_extent()).clamp(MIN_TILE_FLOW_SIZE, MAX_TILE_FLOW_SIZE)
    }

    /// True when the engine already describes exactly this text and geometry.
    pub fn matches(&self, text: &str, line_extent: u32, font_size: f32) -> bool {
        self.line_extent == line_extent && self.font_size == font_size && self.text == text
    }

    /// Re-split and re-measure the document, reusing every block whose text did
    /// not change. Returns the number of blocks that had to be measured.
    pub fn update(&mut self, text: &str, line_extent: u32, font_size: f32) -> Result<usize> {
        let line_extent = line_extent.max(1);
        let font_size = font_size.max(1.0);
        if self.matches(text, line_extent, font_size) {
            return Ok(0);
        }
        if self.line_extent != line_extent || self.font_size != font_size {
            // Both feed into every measurement, so nothing cached survives.
            self.measures.clear();
            self.layouts.clear();
        }

        let mode = self.mode;
        let margin = margin_for(font_size);
        let line_box = (line_extent as f32 - margin * 2.0).max(1.0);
        // The split is charged in line space, so it needs the geometry: the same
        // pane at a different line extent wraps differently and cuts elsewhere.
        let spans = split_blocks(text, cells_per_line(line_extent, font_size));
        let mut measures = Vec::with_capacity(spans.len());
        let mut live_measure_keys = HashSet::with_capacity(spans.len());
        let mut live_layout_keys = HashSet::with_capacity(spans.len());
        let mut fresh_measures = Vec::new();
        let mut fresh_layouts = Vec::new();
        let mut measured = 0;

        with_graphics(|graphics| {
            let format = graphics.text_format(font_size, mode)?;
            let last_index = spans.len().saturating_sub(1);
            for (index, span) in spans.iter().enumerate() {
                let block_text = &text[span.byte_start..span.byte_end];
                let keep_trailing_empty_line = index == last_index;
                let key = measure_key(block_text, font_size, line_extent, keep_trailing_empty_line);
                live_measure_keys.insert(key);
                live_layout_keys.insert(layout_key(block_text, font_size, line_extent));
                if let Some(cached) = self.measures.get(&key)
                    && cached.text == block_text
                    && cached.keep_trailing_empty_line == keep_trailing_empty_line
                {
                    // Cheap now that the line table is shared, and the entry
                    // itself stays put rather than being copied into a new map.
                    measures.push(cached.measure.clone());
                    continue;
                }

                let max_flow_size = block_flow_bound(block_text, line_extent, font_size);
                let utf16 = block_text.encode_utf16().collect::<Vec<u16>>();
                // The layout box is the block's flow bound by the pane's line
                // box, which way round depending on the mode.
                let (max_width, max_height) = mode.to_screen(max_flow_size, line_box);
                // SAFETY: The UTF-16 buffer stays alive across CreateTextLayout,
                // and the layout owns everything it needs afterwards.
                let layout = unsafe {
                    graphics
                        .dwrite
                        .CreateTextLayout(&utf16, &format, max_width, max_height)?
                };
                let measure =
                    measure_block(&layout, max_flow_size, keep_trailing_empty_line, mode)?;
                measured += 1;
                measures.push(measure.clone());
                fresh_measures.push((
                    key,
                    MeasuredBlock {
                        text: block_text.to_owned(),
                        keep_trailing_empty_line,
                        measure,
                    },
                ));
                // Keep the layout that was just built. The caret hit test and the
                // tile render both want this exact block moments from now, and
                // building it again is one of the more expensive things here.
                fresh_layouts.push((layout_key(block_text, font_size, line_extent), layout));
            }
            Ok(())
        })?;

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
        self.line_extent = line_extent;
        self.font_size = font_size;
        self.margin = margin;
        Ok(measured)
    }

    fn layout_for(
        &mut self,
        graphics: &mut Graphics,
        block_index: usize,
    ) -> Result<IDWriteTextLayout> {
        let (byte_start, byte_end, max_flow_size) = {
            let block = &self.plan.blocks[block_index];
            (
                block.span.byte_start,
                block.span.byte_end,
                block.max_flow_size,
            )
        };
        let key = layout_key(
            &self.text[byte_start..byte_end],
            self.font_size,
            self.line_extent,
        );
        if let Some(position) = self.layouts.iter().position(|(cached, _)| *cached == key) {
            let entry = self.layouts.remove(position);
            let layout = entry.1.clone();
            self.layouts.insert(0, entry);
            return Ok(layout);
        }

        let format = graphics.text_format(self.font_size, self.mode)?;
        let line_box = (self.line_extent as f32 - self.margin * 2.0).max(1.0);
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
                let (target, brush, bitmap) = {
                    let cache = graphics.render_target(surface_width, surface_height)?;
                    (
                        cache.target.clone(),
                        cache.brush.clone(),
                        cache.bitmap.clone(),
                    )
                };

                // SAFETY: The target, brush and bitmap are kept alive by the
                // cache for the whole draw, and BeginDraw/EndDraw are paired.
                unsafe {
                    target.BeginDraw();
                    target.Clear(Some(&BACKGROUND));
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
                // The block is drawn at its own offset inside the tile, and the
                // margin sits on the line axis. Both swap with the mode.
                let block_origin = block.draw_origin() - tile_start as f32;
                let (origin_x, origin_y) = mode.to_screen(block_origin, margin);
                let origin = windows_numerics::Vector2 {
                    X: origin_x,
                    Y: origin_y,
                };
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
        self.font_size.to_bits().hash(&mut hasher);
        // Two panes showing the same text at the same size draw different
        // pixels, so a shared tile cache must not confuse them.
        self.mode.hash(&mut hasher);
        if let Some(block) = self.plan.blocks.get(tile.block_index) {
            self.text[block.span.byte_start..block.span.byte_end].hash(&mut hasher);
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
                width: self.font_size,
                height: self.font_size,
            });
        }
        let block_index = self.plan.block_at_utf16(caret_utf16);
        let font_size = self.font_size;
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
            // needs the block's offset applied, the line coordinate the margin.
            let (_, line_point) = mode.to_axes(point_x, point_y);
            let (x, y) = mode.to_screen(
                block.to_global_flow(mode.flow_of(&metrics)),
                margin + line_point,
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
                let (origin_x, origin_y) = mode.to_screen(block.draw_origin(), margin);

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
            let (layout_x, layout_y) = mode.to_screen(block.to_layout_flow(flow), line - margin);
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
            let (layout_x, layout_y) = mode.to_screen(
                block.to_layout_flow(center_flow),
                (target_line - margin).max(0.0),
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
fn measure_block(
    layout: &IDWriteTextLayout,
    max_flow_size: f32,
    keep_trailing_empty_line: bool,
    mode: WritingMode,
) -> Result<BlockMeasure> {
    let mut line_count = 0_u32;
    // SAFETY: The probe call is expected to report an insufficient buffer; only
    // the returned count is used.
    unsafe {
        let _ = layout.GetLineMetrics(None, &mut line_count);
    }
    let mut line_metrics = vec![DWRITE_LINE_METRICS::default(); line_count as usize];
    if line_count > 0 {
        // SAFETY: The buffer holds exactly the count reported above.
        unsafe { layout.GetLineMetrics(Some(&mut line_metrics), &mut line_count)? };
    }
    line_metrics.truncate(line_count as usize);

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
    use crate::text_blocks::visible_flow_range;

    /// The pane extent along the line axis every test lays text out in.
    const LINE_EXTENT: u32 = 520;

    fn engine_for(text: &str, font_size: f32) -> TextEngine {
        engine_in(WritingMode::Vertical, text, font_size)
    }

    fn engine_in(mode: WritingMode, text: &str, font_size: f32) -> TextEngine {
        let mut engine = TextEngine::new(mode);
        engine
            .update(text, LINE_EXTENT, font_size)
            .expect("DirectWrite block measurement");
        engine
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
            assert_split_matches_one_layout(WritingMode::Vertical, repeats);
        }
    }

    /// The same invariant with the lines stacking downwards instead. This is the
    /// one that has to hold before anything else in horizontal writing can: if
    /// the blocks do not add up to the single layout, every line below the first
    /// block is drawn at the wrong height.
    #[test]
    fn horizontal_block_heights_sum_to_the_single_layout_height() {
        for repeats in [37, 40, 41, 53] {
            assert_split_matches_one_layout(WritingMode::Horizontal, repeats);
        }
    }

    fn assert_split_matches_one_layout(mode: WritingMode, repeats: usize) {
        let paragraph = "これは検証用の段落です。句読点、括弧（かっこ）、全角ＡＢＣ、半角ABC123を含みます。\n\n";
        let text = paragraph.repeat(repeats);
        let font_size = 22.0;
        let engine = engine_in(mode, &text, font_size);
        assert!(engine.block_count() > 1, "the sample must span many blocks");

        let margin = margin_for(font_size);
        let line_box = (LINE_EXTENT as f32 - margin * 2.0).max(1.0);
        let bound = block_flow_bound(&text, LINE_EXTENT, font_size);
        let (max_width, max_height) = mode.to_screen(bound, line_box);
        let utf16 = text.encode_utf16().collect::<Vec<u16>>();
        let whole = with_graphics(|graphics| {
            let format = graphics.text_format(font_size, mode)?;
            // SAFETY: The UTF-16 buffer outlives CreateTextLayout.
            let layout = unsafe {
                graphics
                    .dwrite
                    .CreateTextLayout(&utf16, &format, max_width, max_height)?
            };
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
            "splitting must not change the column count ({repeats} paragraphs)"
        );
        // Nothing is estimated any more: both sides are the sum of the same line
        // advances, so the only difference allowed is float summation order.
        let boundaries = (engine.block_count() - 1).max(1) as f32;
        let error = (split_width - whole.flow_size).abs();
        assert!(
            error <= 0.5,
            "{repeats} paragraphs: split blocks total {split_width}px \
             but one layout is {}px, over {} blocks — {:.3}px per boundary",
            whole.flow_size,
            engine.block_count(),
            error / boundaries
        );
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
        engine
            .update(&edited, LINE_EXTENT, 22.0)
            .expect("update after edit");

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
        engine
            .update(&edited, LINE_EXTENT, 22.0)
            .expect("update after Enter");

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
        let measured = engine
            .update(&edited, LINE_EXTENT, 22.0)
            .expect("incremental update");

        assert!(
            measured <= 2,
            "an edit re-measured {measured} of {blocks} blocks; only the block \
             holding the edit (and at most the one its boundary shifted into) should change"
        );
    }

    #[test]
    fn a_repeated_update_measures_nothing() {
        let text = "同じ内容での更新\n\n本文\n";
        let mut engine = engine_for(text, 22.0);

        assert_eq!(
            engine
                .update(text, LINE_EXTENT, 22.0)
                .expect("no-op update"),
            0
        );
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
