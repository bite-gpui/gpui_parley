//! A Parley-backed [`TextSystem`] for GPUI.
//!
//! This crate proves that an out-of-tree crate can implement GPUI's text SPI on
//! top of a different shaping and line-layout engine. It depends on `gpui_engine`
//! (the SPI surface) and `parley`, not on `gpui_engine_default`.
//!
//! The shaping and line layout go through Parley; glyph rasterization uses
//! `skrifa` for outline extraction and hinting and `tiny-skia` for coverage
//! rasterization.

#![warn(missing_docs)]

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use gpui_engine::{
    Font, FontId, FontMetrics, FontRun, GlyphId, LineLayout, LineLayoutIndex, LineWrapper,
    LineWrapperHandle, MissingGlyphReports, RenderGlyphParams, ShapedGlyph, ShapedRun,
    TextRenderingMode, WrapBoundary, WrappedLineLayout,
};

pub use gpui_engine::{PlatformTextSystem, TextSystem};
use gpui_shared_string::SharedString;
use gpui_types::{Bounds, DevicePixels, Hsla, Pixels, Point, Size, px};
use lyon::path::FillRule as LyonFillRule;
use lyon::tessellation::{BuffersBuilder, FillOptions, FillTessellator, FillVertex, VertexBuffers};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontStyle, FontWeight, IndentOptions,
    LayoutContext, PositionedLayoutItem, StyleProperty, YieldData,
};
use skrifa::{
    FontRef, GlyphId as SkrifaGlyphId, MetadataProvider,
    instance::{LocationRef, Size as SkrifaSize},
    outline::{DrawSettings, HintingInstance, HintingOptions, OutlinePen},
    raw::TableProvider,
};
use smallvec::SmallVec;
use tiny_skia::{FillRule, Mask, PathBuilder, Transform};

/// The embedded regular font.
const FONT_DATA: &[u8] =
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
/// The embedded italic font.
const FONT_DATA_ITALIC: &[u8] =
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf");
/// The embedded semibold font.
const FONT_DATA_SEMIBOLD: &[u8] =
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf");
/// The embedded semibold italic font.
const FONT_DATA_SEMIBOLD_ITALIC: &[u8] =
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf");
/// The family name shared by the embedded fonts.
pub const FONT_FAMILY: &str = "IBM Plex Sans";

/// How many hinting instances to keep around.
///
/// The transform asks for a different size on every line of every frame, so this
/// is a cap on growth rather than a working set: the instances are cheap to
/// rebuild (thirty a frame, not one a glyph).
const HINTING_CACHE_LIMIT: usize = 64;

/// How many tessellated glyph outlines to keep.
///
/// The cache key includes the size the outline was flattened for, because the
/// flattening tolerance is a fraction of a *device* pixel: a glyph flattened for
/// 14px has visible facets under an 8x magnifier. Keying on a quantized size
/// keeps the key coarse — a handful of bands, not one entry per size the
/// transform asks for.
const GLYPH_PATH_CACHE_LIMIT: usize = 4096;

/// A glyph outline, flattened and tessellated into triangles.
///
/// Coordinates are in **em units** — a capital is about 0.7 of one — so a caller
/// draws them at any size by multiplying: no tessellation is tied to a size, and
/// one cached outline serves every line of every frame this app will draw. This
/// is what `Window::paint_path` takes, once transformed.
pub type GlyphTriangles = Vec<[Point<Pixels>; 3]>;

/// Builds a [`parley::FontContext`] pre-loaded with the embedded fonts.
///
/// This is exposed so examples and consumers can use Parley's advanced layout
/// features directly, outside the shared [`TextSystem`] shaping boundary.
pub fn font_context() -> parley::FontContext {
    let mut collection = parley::fontique::Collection::new(parley::fontique::CollectionOptions {
        shared: false,
        // The host's fonts, through fontique's platform backend — fontconfig, on a
        // Linux desktop. Without them a family named here is only one of the fonts
        // compiled in, and a glyph the chosen family has not got has nowhere to fall
        // back to; with them, a stack resolves the way the desktop's own font
        // configuration says it should, which is what makes naming a stack
        // worthwhile.
        system_fonts: true,
    });
    for data in [
        FONT_DATA,
        FONT_DATA_ITALIC,
        FONT_DATA_SEMIBOLD,
        FONT_DATA_SEMIBOLD_ITALIC,
    ] {
        collection.register_fonts(parley::fontique::Blob::new(Arc::new(data.to_vec())), None);
    }
    parley::FontContext {
        collection,
        source_cache: parley::fontique::SourceCache::default(),
    }
}

/// Maps GPUI's font weight onto Parley's.
fn map_weight(weight: gpui_engine::FontWeight) -> FontWeight {
    FontWeight::new(weight.0)
}

/// Maps GPUI's font style onto Parley's.
fn map_style(style: gpui_engine::FontStyle) -> FontStyle {
    match style {
        gpui_engine::FontStyle::Normal => FontStyle::Normal,
        gpui_engine::FontStyle::Italic | gpui_engine::FontStyle::Oblique => FontStyle::Italic,
    }
}

/// Converts a Parley layout into a GPUI [`LineLayout`].
fn convert_layout(
    layout: &parley::Layout<[u8; 4]>,
    font_size: Pixels,
    len: usize,
    registry: &Mutex<FontRegistry>,
) -> LineLayout {
    let mut result = LineLayout {
        font_size,
        width: px(layout.width()),
        ascent: px(0.0),
        descent: px(0.0),
        runs: Vec::new(),
        len,
    };

    let mut registry = registry.lock().unwrap();
    for line in layout.lines() {
        let metrics = line.metrics();
        result.ascent = px(metrics.ascent);
        result.descent = px(metrics.descent);

        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                // The face the shaper used for these glyphs — not the one that was
                // asked for, which a fallback glyph would not be in.
                let font_id = registry.resolve_face(glyph_run.run().font());
                let mut glyphs = Vec::new();
                let mut offset = glyph_run.offset();
                let baseline = glyph_run.baseline();
                for cluster in glyph_run.run().visual_clusters() {
                    let byte_index = cluster.text_range().start;
                    let is_emoji = cluster.is_emoji();
                    for mut glyph in cluster.glyphs() {
                        glyph.x += offset;
                        glyph.y += baseline;
                        offset += glyph.advance;
                        glyphs.push(ShapedGlyph {
                            id: GlyphId(glyph.id),
                            position: Point {
                                x: px(glyph.x),
                                y: px(glyph.y),
                            },
                            index: byte_index,
                            is_emoji,
                        });
                    }
                }
                result.runs.push(ShapedRun { font_id, glyphs });
            }
        }
    }

    result
}

/// Returns the wrap boundary whose glyph carries the given byte index.
fn wrap_boundary_for_byte(unwrapped: &LineLayout, byte: usize) -> Option<WrapBoundary> {
    for (run_ix, run) in unwrapped.runs.iter().enumerate() {
        for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
            if glyph.index == byte {
                return Some(WrapBoundary { run_ix, glyph_ix });
            }
        }
    }
    None
}

/// Collects a glyph outline into a [`tiny_skia::Path`], flipping the y-axis so
/// the font's y-up outline coordinates become the y-down coordinates
/// `tiny_skia` rasterizes in.
struct GlyphPathBuilder {
    builder: PathBuilder,
}

impl GlyphPathBuilder {
    fn new() -> Self {
        Self {
            builder: PathBuilder::new(),
        }
    }

    fn build(self) -> Option<tiny_skia::Path> {
        self.builder.finish()
    }
}

impl OutlinePen for GlyphPathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, -y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, -y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder.quad_to(cx0, -cy0, x, -y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_to(cx0, -cy0, cx1, -cy1, x, -y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// A pen that builds a [`lyon`] path, for tessellating an outline.
struct LyonPathBuilder {
    builder: lyon::path::path::Builder,
}

impl LyonPathBuilder {
    fn new() -> Self {
        Self {
            builder: lyon::path::Path::builder(),
        }
    }

    fn build(self) -> lyon::path::Path {
        self.builder.build()
    }
}

/// Glyph outlines arrive y-up and are drawn y-down, as in [`GlyphPathBuilder`].
impl OutlinePen for LyonPathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        // `begin` is this builder's `move_to`.
        self.builder.begin(lyon::math::point(x, -y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(lyon::math::point(x, -y));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder
            .quadratic_bezier_to(lyon::math::point(cx0, -cy0), lyon::math::point(x, -y));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_bezier_to(
            lyon::math::point(cx0, -cy0),
            lyon::math::point(cx1, -cy1),
            lyon::math::point(x, -y),
        );
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// A per-line layout box used by [`ParleyTextSystem::layout_with_boxes`].
#[derive(Clone, Copy, Debug)]
pub struct LineBox {
    /// The x offset of the line's start edge.
    pub x: f32,
    /// The maximum advance (width) of the line.
    pub width: f32,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct LineCacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: Vec<FontRun>,
    force_width: Option<Pixels>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct WrappedCacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: Vec<FontRun>,
    wrap_width: Option<Pixels>,
    max_lines: Option<usize>,
}

/// One frame's worth of cached layouts, with the keys in insertion order.
#[derive(Default)]
struct FrameCache {
    lines: HashMap<Arc<LineCacheKey>, Arc<LineLayout>>,
    wrapped_lines: HashMap<Arc<WrappedCacheKey>, Arc<WrappedLineLayout>>,
    used_lines: Vec<Arc<LineCacheKey>>,
    used_wrapped_lines: Vec<Arc<WrappedCacheKey>>,
}

/// A two-frame line-layout cache.
///
/// Layouts created during a frame live in `current_frame`; advancing a frame
/// rotates it into `previous_frame`, so a layout survives into the next frame.
/// GPUI carries layouts through a frame it does not re-prepaint by calling
/// [`reuse_layouts`](LineLayoutCache::reuse_layouts) with the range it wants to
/// keep.
#[derive(Default)]
struct LineLayoutCache {
    previous_frame: FrameCache,
    current_frame: FrameCache,
}

impl LineLayoutCache {
    fn layout_index(&self) -> LineLayoutIndex {
        LineLayoutIndex {
            // Parley's font set is fixed and `add_fonts` is a no-op, so no
            // generation can advance and the cache never invalidates on one.
            font_generation: 0,
            lines_index: self.current_frame.used_lines.len(),
            wrapped_lines_index: self.current_frame.used_wrapped_lines.len(),
            lines_by_hash_index: 0,
            wrapped_lines_by_hash_index: 0,
        }
    }

    fn reuse_layouts(&mut self, range: Range<LineLayoutIndex>) {
        for key in &self.previous_frame.used_lines[range.start.lines_index..range.end.lines_index] {
            if let Some(layout) = self.previous_frame.lines.remove(key) {
                self.current_frame.lines.insert(key.clone(), layout);
            }
            self.current_frame.used_lines.push(key.clone());
        }
        for key in &self.previous_frame.used_wrapped_lines
            [range.start.wrapped_lines_index..range.end.wrapped_lines_index]
        {
            if let Some(layout) = self.previous_frame.wrapped_lines.remove(key) {
                self.current_frame.wrapped_lines.insert(key.clone(), layout);
            }
            self.current_frame.used_wrapped_lines.push(key.clone());
        }
    }

    fn truncate_layouts(&mut self, index: LineLayoutIndex) {
        self.current_frame.used_lines.truncate(index.lines_index);
        self.current_frame
            .used_wrapped_lines
            .truncate(index.wrapped_lines_index);
    }

    fn finish_frame(&mut self) {
        std::mem::swap(&mut self.previous_frame, &mut self.current_frame);
        let current = &mut self.current_frame;
        current.lines.clear();
        current.wrapped_lines.clear();
        current.used_lines.clear();
        current.used_wrapped_lines.clear();
    }
}

/// A [`TextSystem`] that shapes and lays out text through Parley.
pub struct ParleyTextSystem {
    platform: Arc<ParleyPlatformTextSystem>,
    platform_dyn: Arc<dyn PlatformTextSystem>,
    font_runs_pool: Mutex<Vec<Vec<FontRun>>>,
    wrapper_pool: Mutex<Vec<LineWrapper>>,
    raster_bounds_cache: Mutex<HashMap<RenderGlyphParams, Bounds<DevicePixels>>>,
    layout_cache: Mutex<LineLayoutCache>,
}

impl ParleyTextSystem {
    /// Creates a text system that shapes with the embedded IBM Plex Sans fonts.
    pub fn new() -> Arc<Self> {
        let platform = Arc::new(ParleyPlatformTextSystem::new());
        let platform_dyn: Arc<dyn PlatformTextSystem> = platform.clone();
        Arc::new(Self {
            platform,
            platform_dyn,
            font_runs_pool: Mutex::new(Vec::new()),
            wrapper_pool: Mutex::new(Vec::new()),
            raster_bounds_cache: Mutex::new(HashMap::new()),
            layout_cache: Mutex::new(LineLayoutCache::default()),
        })
    }

    /// Lays out `text` with a first-line indent, using Parley's CSS
    /// `text-indent` support.
    pub fn layout_indented(
        &self,
        text: &str,
        size: f32,
        indent: f32,
        max_width: f32,
    ) -> parley::Layout<[u8; 4]> {
        let mut font_context = self.platform.font_context.lock().unwrap();
        let mut layout_context = self.platform.layout_context.lock().unwrap();
        let mut builder = layout_context.ranged_builder(&mut font_context, text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::from(FONT_FAMILY)));
        builder.push_default(StyleProperty::FontSize(size));
        let mut layout = builder.build(text);
        layout.set_text_indent(indent, IndentOptions::default());
        layout.break_all_lines(Some(max_width));
        layout.align(Alignment::Start, AlignmentOptions::default());
        layout
    }

    /// Lays out `text` with a character-count limit per line, using Parley's
    /// `break_next_with_length`.
    pub fn layout_with_char_count(
        &self,
        text: &str,
        size: f32,
        max_chars: u32,
    ) -> parley::Layout<[u8; 4]> {
        let mut font_context = self.platform.font_context.lock().unwrap();
        let mut layout_context = self.platform.layout_context.lock().unwrap();
        let mut builder = layout_context.ranged_builder(&mut font_context, text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::from(FONT_FAMILY)));
        builder.push_default(StyleProperty::FontSize(size));
        let mut layout = builder.build(text);
        {
            let mut breaker = layout.break_lines();
            while breaker.break_next_with_length(max_chars).is_some() {}
        }
        layout.align(Alignment::Start, AlignmentOptions::default());
        layout
    }

    /// Lays out `text` into per-line boxes (e.g. flowing around excluded
    /// regions). Each box constrains the x offset and maximum advance of one
    /// line; lines past the end of `boxes` use the full width.
    pub fn layout_with_boxes(
        &self,
        text: &str,
        size: f32,
        boxes: &[LineBox],
    ) -> parley::Layout<[u8; 4]> {
        let mut font_context = self.platform.font_context.lock().unwrap();
        let mut layout_context = self.platform.layout_context.lock().unwrap();
        let mut builder = layout_context.ranged_builder(&mut font_context, text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::from(FONT_FAMILY)));
        builder.push_default(StyleProperty::FontSize(size));
        let mut layout = builder.build(text);
        {
            let mut breaker = layout.break_lines();
            breaker.state_mut().set_layout_max_advance(f32::MAX);
            let mut line = 0;
            loop {
                let box_ = boxes.get(line);
                breaker
                    .state_mut()
                    .set_line_max_advance(box_.map_or(f32::MAX, |b| b.width));
                breaker.state_mut().set_line_x(box_.map_or(0.0, |b| b.x));
                match breaker.break_next() {
                    Some(YieldData::LineBreak(_)) => line += 1,
                    Some(_) => {}
                    None => break,
                }
            }
        }
        layout.align(Alignment::Start, AlignmentOptions::default());
        layout
    }

    /// A glyph's outline, flattened and tessellated into triangles.
    ///
    /// The point of this is a glyph drawn as *vectors*: tessellate once for the
    /// size a glyph is roughly drawn at, then scale the triangles to whatever
    /// size each line of a frame asks for, instead of rasterizing a mask per
    /// glyph per size. Coordinates come back in **em units**, so drawing at a
    /// size is one multiply — see [`GlyphTriangles`].
    ///
    /// `device_size` is the size the glyph is actually drawn at (the line's
    /// scale included) and `max_error` the flattening error tolerated there, in
    /// device pixels — a quarter of one, say. Both are part of the cache key, so
    /// a caller that wants hits should quantize `device_size` into bands rather
    /// than passing the transform's size straight through; rounding up to the
    /// next power of two means the flattening is always fine enough to draw.
    pub fn glyph_triangles(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
        device_size: f32,
        max_error: f32,
    ) -> Result<Arc<GlyphTriangles>> {
        self.platform
            .glyph_triangles(font_id, glyph_id, device_size, max_error)
    }

    /// The family a stack actually resolves to.
    ///
    /// The answer to "what font is this, really?" when the name asked for was a
    /// fallback stack rather than one family — see
    /// [`ParleyPlatformTextSystem::resolved_family`].
    pub fn resolved_family(&self, family: &str) -> Option<String> {
        self.platform.resolved_family(family)
    }
}

/// Access to Parley's own layout API from a text system handle.
///
/// `App::text_system` hands out a `dyn TextSystem`, so reaching the native layout
/// methods above needs a downcast. Keeping it behind this trait means a call site
/// asks for the capability instead of naming the concrete type and the cast.
///
/// ```ignore
/// let Some(parley) = cx.text_system().as_parley() else {
///     return;
/// };
/// let layout = parley.layout_indented(text, 16.0, 40.0, 320.0);
/// ```
pub trait ParleyTextSystemExt {
    /// This text system as a [`ParleyTextSystem`], when it is one.
    fn as_parley(&self) -> Option<&ParleyTextSystem>;
}

impl ParleyTextSystemExt for dyn TextSystem + '_ {
    fn as_parley(&self) -> Option<&ParleyTextSystem> {
        self.as_any().downcast_ref::<ParleyTextSystem>()
    }
}

impl TextSystem for ParleyTextSystem {
    /// Parley's platform text system leaves `set_missing_glyph_sink` at its
    /// no-op default, so there is no report stream to hand out.
    fn take_missing_glyph_receiver(&self) -> Option<Box<dyn MissingGlyphReports>> {
        None
    }

    fn enable_missing_glyph_reporting(&self) {}

    fn disable_missing_glyph_reporting(&self) {}

    fn platform_text_system(&self) -> &Arc<dyn PlatformTextSystem> {
        &self.platform_dyn
    }

    fn all_font_names(&self) -> Vec<String> {
        self.platform.all_font_names()
    }

    fn add_fonts(&self, fonts: Vec<std::borrow::Cow<'static, [u8]>>) -> Result<()> {
        self.platform.add_fonts(fonts)
    }

    fn get_font_for_id(&self, id: FontId) -> Option<Font> {
        self.platform.font_for_id(id)
    }

    fn resolve_font(&self, font: &Font) -> FontId {
        self.platform.resolve_font(font)
    }

    fn prewarm_fonts(&self, _fonts: &[Font]) {}

    fn bounding_box(&self, font_id: FontId, font_size: Pixels) -> Bounds<Pixels> {
        self.platform.font_metrics(font_id).bounding_box(font_size)
    }

    fn typographic_bounds(
        &self,
        font_id: FontId,
        font_size: Pixels,
        character: char,
    ) -> Result<Bounds<Pixels>> {
        let glyph_id = self
            .platform
            .glyph_for_char(font_id, character)
            .unwrap_or(GlyphId(0));
        let bounds = self.platform.typographic_bounds(font_id, glyph_id)?;
        let scale = font_size.0 / self.platform.font_metrics(font_id).units_per_em as f32;
        Ok((bounds * scale).map(px))
    }

    fn advance(&self, font_id: FontId, font_size: Pixels, ch: char) -> Result<Size<Pixels>> {
        let glyph_id = self
            .platform
            .glyph_for_char(font_id, ch)
            .unwrap_or(GlyphId(0));
        let units = self.platform.advance(font_id, glyph_id)?;
        let scale = font_size.0 / self.platform.font_metrics(font_id).units_per_em as f32;
        Ok(Size {
            width: px(units.width * scale),
            height: px(units.height * scale),
        })
    }

    fn layout_width(&self, font_id: FontId, font_size: Pixels, ch: char) -> Pixels {
        self.advance(font_id, font_size, ch)
            .map(|size| size.width)
            .unwrap_or(font_size)
    }

    fn em_width(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.layout_width(font_id, font_size, 'm'))
    }

    fn em_advance(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        self.advance(font_id, font_size, 'm').map(|size| size.width)
    }

    fn ch_width(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        Ok(self.layout_width(font_id, font_size, '0'))
    }

    fn ch_advance(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels> {
        self.advance(font_id, font_size, '0').map(|size| size.width)
    }

    fn units_per_em(&self, font_id: FontId) -> u32 {
        self.platform.font_metrics(font_id).units_per_em
    }

    fn cap_height(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.platform.font_metrics(font_id).cap_height(font_size)
    }

    fn x_height(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.platform.font_metrics(font_id).x_height(font_size)
    }

    fn ascent(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.platform.font_metrics(font_id).ascent(font_size)
    }

    fn descent(&self, font_id: FontId, font_size: Pixels) -> Pixels {
        self.platform.font_metrics(font_id).descent(font_size)
    }

    fn baseline_offset(&self, font_id: FontId, font_size: Pixels, line_height: Pixels) -> Pixels {
        let ascent = self.ascent(font_id, font_size);
        let descent = self.descent(font_id, font_size);
        (line_height - (ascent - descent)) / 2.0 + ascent
    }

    fn take_font_runs(&self) -> Vec<FontRun> {
        self.font_runs_pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_default()
    }

    fn recycle_font_runs(&self, font_runs: Vec<FontRun>) {
        self.font_runs_pool.lock().unwrap().push(font_runs);
    }

    fn line_wrapper(self: Arc<Self>, font: Font, font_size: Pixels) -> LineWrapperHandle {
        let font_id = self.resolve_font(&font);
        let wrapper = self
            .wrapper_pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LineWrapper::new(font_id, font_size, self.clone()));
        let this = self;
        LineWrapperHandle::new(wrapper, move |wrapper| {
            this.wrapper_pool.lock().unwrap().push(wrapper);
        })
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let mut cache = self.raster_bounds_cache.lock().unwrap();
        if let Some(bounds) = cache.get(params) {
            return Ok(*bounds);
        }
        let bounds = self.platform.glyph_raster_bounds(params)?;
        cache.insert(params.clone(), bounds);
        Ok(bounds)
    }

    fn rasterize_glyph(&self, params: &RenderGlyphParams) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let bounds = self.platform.glyph_raster_bounds(params)?;
        self.platform.rasterize_glyph(params, bounds)
    }

    fn glyph_dilation_for_color(&self, _color: Hsla) -> u8 {
        0
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::PlatformDefault
    }

    fn layout_index(&self) -> LineLayoutIndex {
        self.layout_cache.lock().unwrap().layout_index()
    }

    fn reuse_layouts(&self, range: Range<LineLayoutIndex>) {
        self.layout_cache.lock().unwrap().reuse_layouts(range);
    }

    fn truncate_layouts(&self, index: LineLayoutIndex) {
        self.layout_cache.lock().unwrap().truncate_layouts(index);
    }

    fn finish_frame(&self) {
        self.layout_cache.lock().unwrap().finish_frame();
    }

    fn layout_wrapped_line(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
    ) -> Arc<WrappedLineLayout> {
        let mut cache = self.layout_cache.lock().unwrap();
        let cache = &mut *cache;

        let key = Arc::new(WrappedCacheKey {
            text: SharedString::from(text),
            font_size,
            runs: runs.to_vec(),
            wrap_width,
            max_lines,
        });

        if let Some(layout) = cache.current_frame.wrapped_lines.get(&key).cloned() {
            return layout;
        }
        if let Some(layout) = cache.previous_frame.wrapped_lines.remove(&key) {
            cache
                .current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            cache.current_frame.used_wrapped_lines.push(key);
            return layout;
        }

        let width = wrap_width.unwrap_or(Pixels::MAX);
        let (unwrapped_layout, wrap_boundaries) =
            self.platform.layout_wrapped(text, font_size, runs, width);
        let layout = Arc::new(WrappedLineLayout {
            unwrapped_layout: Arc::new(unwrapped_layout),
            wrap_boundaries,
            wrap_width,
        });
        cache
            .current_frame
            .wrapped_lines
            .insert(key.clone(), layout.clone());
        cache.current_frame.used_wrapped_lines.push(key);
        layout
    }

    fn layout_line(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout> {
        let mut cache = self.layout_cache.lock().unwrap();
        let cache = &mut *cache;

        let key = Arc::new(LineCacheKey {
            text: SharedString::from(text),
            font_size,
            runs: runs.to_vec(),
            force_width,
        });

        if let Some(layout) = cache.current_frame.lines.get(&key).cloned() {
            return layout;
        }
        if let Some(layout) = cache.previous_frame.lines.remove(&key) {
            cache
                .current_frame
                .lines
                .insert(key.clone(), layout.clone());
            cache.current_frame.used_lines.push(key);
            return layout;
        }

        let layout = Arc::new(self.platform.layout_line(text, font_size, runs));
        cache
            .current_frame
            .lines
            .insert(key.clone(), layout.clone());
        cache.current_frame.used_lines.push(key);
        layout
    }

    fn try_layout_line_by_hash(
        &self,
        _text_hash: u64,
        _text_len: usize,
        _font_size: Pixels,
        _runs: &[FontRun],
        _force_width: Option<Pixels>,
    ) -> Option<Arc<LineLayout>> {
        None
    }

    fn layout_line_by_hash(
        &self,
        _text_hash: u64,
        _text_len: usize,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
        materialize_text: Box<dyn FnOnce() -> SharedString>,
    ) -> Arc<LineLayout> {
        let text = materialize_text();
        self.layout_line(&text, font_size, runs, force_width)
    }
}

/// The Parley shaping backend behind [`ParleyTextSystem`].
struct ParleyPlatformTextSystem {
    font_context: Mutex<FontContext>,
    layout_context: Mutex<LayoutContext>,
    font_registry: Mutex<FontRegistry>,
    /// Hinting instances, keyed by face and device size.
    hinting: Mutex<HashMap<(usize, u32), Arc<HintingInstance>>>,
    /// Tessellated glyph outlines, keyed by face, glyph, and the size they were
    /// flattened for.
    glyph_paths: Mutex<HashMap<(usize, u32, u32), Arc<GlyphTriangles>>>,
}

/// A [`lyon`] point as this crate's geometry, in em units.
fn pixels(point: lyon::math::Point, units_per_em: f32) -> Point<Pixels> {
    Point {
        x: px(point.x / units_per_em),
        y: px(point.y / units_per_em),
    }
}

/// Maps GPUI [`Font`]s to stable [`FontId`]s and back, and records the faces the
/// shaper chose along the way.
///
/// Both get ids from the one counter. The second is what makes a fallback chain safe
/// to name: a glyph the requested family has not got is shaped from whatever the
/// stack has next, and the glyph ids in that run only mean anything to *that* face —
/// so it is recorded as the shaper makes it, and rasterization reads it back rather
/// than resolving the requested family a second time and getting a different answer.
#[derive(Default)]
struct FontRegistry {
    ids_by_font: HashMap<Font, FontId>,
    fonts_by_id: HashMap<FontId, Font>,
    ids_by_face: HashMap<(u64, u32), FontId>,
    faces_by_id: HashMap<FontId, (parley::fontique::Blob<u8>, u32)>,
    next_id: usize,
}

impl FontRegistry {
    fn allocate_id(&mut self) -> FontId {
        let id = FontId(self.next_id);
        self.next_id += 1;
        id
    }

    fn resolve(&mut self, font: &Font) -> FontId {
        if let Some(id) = self.ids_by_font.get(font) {
            return *id;
        }
        let id = self.allocate_id();
        self.ids_by_font.insert(font.clone(), id);
        self.fonts_by_id.insert(id, font.clone());
        id
    }

    /// The id for the face the shaper picked, registering it if it is new.
    fn resolve_face(&mut self, face: &parley::FontData) -> FontId {
        let key = (face.data.id(), face.index);
        if let Some(id) = self.ids_by_face.get(&key) {
            return *id;
        }
        let id = self.allocate_id();
        self.ids_by_face.insert(key, id);
        self.faces_by_id.insert(id, (face.data.clone(), face.index));
        id
    }

    fn font_for_id(&self, id: FontId) -> Option<Font> {
        self.fonts_by_id.get(&id).cloned()
    }

    fn face_for_id(&self, id: FontId) -> Option<(parley::fontique::Blob<u8>, u32)> {
        self.faces_by_id.get(&id).cloned()
    }
}

impl ParleyPlatformTextSystem {
    fn new() -> Self {
        Self {
            font_context: Mutex::new(font_context()),
            layout_context: Mutex::new(LayoutContext::new()),
            font_registry: Mutex::new(FontRegistry::default()),
            hinting: Mutex::new(HashMap::new()),
            glyph_paths: Mutex::new(HashMap::new()),
        }
    }

    /// A glyph's outline as triangles, flattened finely enough for the size it
    /// will be drawn at.
    ///
    /// The caller passes the *device* size (the size the glyph is actually drawn
    /// at, line scale included) and the error it will tolerate in device pixels;
    /// the outline comes back in font units, so it can be scaled to that size —
    /// or any other — at paint time. Ask for the same size twice and the second
    /// answer is the first.
    fn glyph_triangles(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
        device_size: f32,
        max_error: f32,
    ) -> Result<Arc<GlyphTriangles>> {
        let key = (font_id.0, glyph_id.0, device_size.to_bits());
        if let Some(triangles) = self.glyph_paths.lock().unwrap().get(&key) {
            return Ok(triangles.clone());
        }

        let (data, index) = self.font_data_for_id(font_id).context("unknown font")?;
        let font_ref = FontRef::from_index(data.data(), index).context("invalid font data")?;
        let units_per_em = font_ref
            .head()
            .context("the font should have a head table")?
            .units_per_em() as f32;
        let outlines = font_ref.outline_glyphs();
        let glyph = outlines
            .get(SkrifaGlyphId::new(glyph_id.0))
            .context("missing glyph outline")?;

        let mut pen = LyonPathBuilder::new();
        glyph
            .draw(
                DrawSettings::unhinted(SkrifaSize::new(units_per_em), LocationRef::default()),
                &mut pen,
            )
            .context("unable to draw glyph outline")?;
        let path = pen.build();

        // The path is in font units and the caller's budget is in device pixels,
        // so the tolerance converts through the size the glyph is drawn at.
        let tolerance = (max_error * units_per_em / device_size.max(0.001)).max(0.01);
        let mut buffers: VertexBuffers<lyon::math::Point, u32> = VertexBuffers::new();
        FillTessellator::new()
            .tessellate_path(
                &path,
                // Nonzero, because a glyph's counters are holes and this pipeline
                // has no stencil to resolve them with: the triangles it is handed
                // have to be wound correctly before they get there.
                &FillOptions::tolerance(tolerance).with_fill_rule(LyonFillRule::NonZero),
                &mut BuffersBuilder::new(&mut buffers, |vertex: FillVertex| vertex.position()),
            )
            .context("unable to tessellate glyph outline")?;

        let (triangles, _) = buffers.indices.as_chunks::<3>();
        let triangles: GlyphTriangles = triangles
            .iter()
            .map(|[a, b, c]| {
                [
                    pixels(buffers.vertices[*a as usize], units_per_em),
                    pixels(buffers.vertices[*b as usize], units_per_em),
                    pixels(buffers.vertices[*c as usize], units_per_em),
                ]
            })
            .collect();
        let triangles = Arc::new(triangles);

        let mut cache = self.glyph_paths.lock().unwrap();
        if cache.len() >= GLYPH_PATH_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, triangles.clone());
        Ok(triangles)
    }

    fn resolve_font(&self, font: &Font) -> FontId {
        self.font_registry.lock().unwrap().resolve(font)
    }

    fn font_for_id(&self, id: FontId) -> Option<Font> {
        self.font_registry.lock().unwrap().font_for_id(id)
    }

    /// The face to draw `font` with: its bytes, and where the face sits in them.
    ///
    /// This asks the shaper's own database — the family by the name it was asked
    /// for, then that family's own matcher for the nearest face to the weight and
    /// slant that were asked for — so what is rasterized is the face that was
    /// shaped with, and none of it needs to know what faces a font ships. A family
    /// that was never loaded answers with the compiled-in one, which is why asking
    /// for a font that is not here draws something rather than nothing.
    fn font_data_for(&self, font: &Font) -> Option<(parley::fontique::Blob<u8>, u32)> {
        let mut context = self.font_context.lock().unwrap();
        let collection = &mut context.collection;
        let family = collection
            .family_by_name(&font.family)
            .or_else(|| collection.family_by_name(FONT_FAMILY))?;
        let face = family.match_font(
            parley::fontique::FontWidth::default(),
            map_style(font.style),
            map_weight(font.weight),
            false,
        )?;
        Some((face.load(None)?, face.index()))
    }

    /// The face to draw an id with: the one the shaper used for it, if it shaped
    /// anything with it, and otherwise the family it was asked for, resolved.
    fn font_data_for_id(&self, id: FontId) -> Option<(parley::fontique::Blob<u8>, u32)> {
        if let Some(face) = self.font_registry.lock().unwrap().face_for_id(id) {
            return Some(face);
        }
        let font = self.font_for_id(id)?;
        self.font_data_for(&font)
    }

    /// The family a stack actually resolves to.
    ///
    /// Walks the comma-separated names in order, as the shaper does, and answers
    /// with the first the database can name a *concrete* family for. That is the
    /// family the text will be set in, and so the honest answer to "what font is
    /// this?" when the name that was asked for was a fallback stack. A generic
    /// (`sans-serif`, and the rest) is not a family of its own but whatever this
    /// platform maps it to, so it answers with the first family it stands for.
    fn resolved_family(&self, family: &str) -> Option<String> {
        let mut context = self.font_context.lock().unwrap();
        let collection = &mut context.collection;
        for name in family.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if let Some(generic) = parley::fontique::GenericFamily::parse(name) {
                let first = collection.generic_families(generic).next();
                if let Some(resolved) = first.and_then(|id| collection.family_name(id)) {
                    return Some(resolved.to_string());
                }
                continue;
            }
            if collection.family_id(name).is_some() {
                return Some(name.to_string());
            }
        }
        None
    }

    /// The hinting instance for a face at a device size, built once and reused.
    ///
    /// `skrifa` builds one by running the font's hinting program at a size, which
    /// came to a third of the cost of rasterizing the glyph it was then used on —
    /// and because this app asks for a different size on every line of every
    /// frame, an un-cached version builds thousands of them a frame.
    fn hinting_instance(
        &self,
        params: &RenderGlyphParams,
        device_size: f32,
        outlines: &skrifa::outline::OutlineGlyphCollection<'_>,
    ) -> Result<Arc<HintingInstance>> {
        let key = (params.font_id.0, device_size.to_bits());
        let mut cache = self.hinting.lock().unwrap();
        if let Some(instance) = cache.get(&key) {
            return Ok(instance.clone());
        }

        let instance = Arc::new(
            HintingInstance::new(
                outlines,
                SkrifaSize::new(device_size),
                LocationRef::default(),
                HintingOptions::default(),
            )
            .context("unable to create hinting instance")?,
        );

        // Every frame brings sizes that have never been asked for before, so
        // this has to be bounded.
        if cache.len() >= HINTING_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, instance.clone());
        Ok(instance)
    }

    fn rasterize_outline(
        &self,
        params: &RenderGlyphParams,
    ) -> Result<(Bounds<DevicePixels>, Vec<u8>)> {
        let (data, index) = self
            .font_data_for_id(params.font_id)
            .context("unknown font")?;
        let font_ref = FontRef::from_index(data.data(), index).context("invalid font data")?;

        let device_size = params.font_size.0 * params.scale_factor;
        let outlines = font_ref.outline_glyphs();
        let hinting = self.hinting_instance(params, device_size, &outlines)?;
        let glyph_id = SkrifaGlyphId::new(params.glyph_id.0);
        let glyph = outlines.get(glyph_id).context("missing glyph outline")?;

        let mut path_builder = GlyphPathBuilder::new();
        glyph
            .draw(DrawSettings::hinted(&hinting, false), &mut path_builder)
            .context("unable to draw glyph outline")?;
        let Some(path) = path_builder.build() else {
            return Ok((Bounds::default(), Vec::new()));
        };

        let bounds = path.bounds();
        let left = bounds.left().floor();
        let top = bounds.top().floor();
        let right = bounds.right().ceil();
        let bottom = bounds.bottom().ceil();
        let width = (right - left).max(0.0) as u32;
        let height = (bottom - top).max(0.0) as u32;
        if width == 0 || height == 0 {
            return Ok((Bounds::default(), Vec::new()));
        }

        let mut mask = Mask::new(width, height).context("unable to allocate glyph mask")?;
        mask.fill_path(
            &path,
            FillRule::Winding,
            true,
            Transform::from_translate(-left, -top),
        );

        let bounds = Bounds {
            origin: Point {
                x: DevicePixels(left as i32),
                y: DevicePixels(top as i32),
            },
            size: Size {
                width: DevicePixels(width as i32),
                height: DevicePixels(height as i32),
            },
        };
        Ok((bounds, mask.data().to_vec()))
    }

    fn build_layout(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
    ) -> parley::Layout<[u8; 4]> {
        let mut font_context = self.font_context.lock().unwrap();
        let mut layout_context = self.layout_context.lock().unwrap();

        // Which runs name something the shaper can resolve. A stack is expected to
        // end in a generic and so to resolve itself, and is taken as given; a single
        // name that is not a family anywhere is the one case worth refusing, since
        // shaping it would give an empty line rather than a fallback. This has to be
        // asked before the builder is made, because the builder takes the font
        // context for the rest of the function.
        let families: Vec<Option<String>> = runs
            .iter()
            .map(|run| {
                self.font_for_id(run.font_id)
                    .map(|font| font.family.to_string())
                    .filter(|family| {
                        family.contains(',')
                            || font_context.collection.family_by_name(family).is_some()
                    })
            })
            .collect();

        let mut builder = layout_context.ranged_builder(&mut font_context, text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::from(FONT_FAMILY)));
        builder.push_default(StyleProperty::FontSize(font_size.0));

        let mut byte = 0;
        for (run, family) in runs.iter().zip(&families) {
            let end = byte + run.len;
            if let Some(font) = self.font_for_id(run.font_id) {
                // The run's own family, which is what makes a family loaded at
                // runtime the one the text is shaped in.
                if let Some(family) = family {
                    // Pushed as a source string, not as one name, so a whole stack is
                    // what the shaper is handed and it resolves the list the way the
                    // desktop's own font configuration says it should.
                    builder.push(
                        StyleProperty::FontFamily(FontFamily::Source(std::borrow::Cow::Owned(
                            family.clone(),
                        ))),
                        byte..end,
                    );
                }
                builder.push(
                    StyleProperty::FontWeight(map_weight(font.weight)),
                    byte..end,
                );
                builder.push(StyleProperty::FontStyle(map_style(font.style)), byte..end);
            }
            byte = end;
        }

        builder.build(text)
    }

    fn layout_wrapped(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        wrap_width: Pixels,
    ) -> (LineLayout, SmallVec<[WrapBoundary; 1]>) {
        let mut layout = self.build_layout(text, font_size, runs);

        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());
        let unwrapped = convert_layout(&layout, font_size, text.len(), &self.font_registry);

        layout.break_all_lines(Some(wrap_width.0));
        let mut boundaries = SmallVec::new();
        for (i, line) in layout.lines().enumerate() {
            if i == 0 {
                continue;
            }
            if let Some(boundary) = wrap_boundary_for_byte(&unwrapped, line.text_range().start) {
                boundaries.push(boundary);
            }
        }

        (unwrapped, boundaries)
    }
}

impl PlatformTextSystem for ParleyPlatformTextSystem {
    /// Adds faces to the collection the shaper reads.
    ///
    /// It is the same collection [`Self::font_data_for`] asks when a glyph is
    /// rasterized, so a font loaded here is shaped and cut from its own bytes, and
    /// nothing has to be told what it is: family, weight and slant all come out of
    /// the font. The collection already carries the host's fonts (fontique's
    /// platform backend), so this is additive to them rather than instead of them,
    /// and a family named by a run may be one of either.
    fn add_fonts(&self, fonts: Vec<std::borrow::Cow<'static, [u8]>>) -> Result<()> {
        let mut context = self.font_context.lock().unwrap();
        for data in fonts {
            context.collection.register_fonts(
                parley::fontique::Blob::new(Arc::new(data.into_owned())),
                None,
            );
        }
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        self.font_context
            .lock()
            .unwrap()
            .collection
            .family_names()
            .map(str::to_string)
            .collect()
    }

    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        Ok(self.resolve_font(descriptor))
    }

    fn font_metrics(&self, _font_id: FontId) -> FontMetrics {
        FontMetrics {
            units_per_em: 1000,
            ascent: 1025.0,
            descent: -275.0,
            line_gap: 0.0,
            underline_position: -95.0,
            underline_thickness: 60.0,
            cap_height: 698.0,
            x_height: 516.0,
            bounding_box: Bounds {
                origin: Point {
                    x: -260.0,
                    y: -245.0,
                },
                size: Size {
                    width: 1501.0,
                    height: 1364.0,
                },
            },
        }
    }

    fn typographic_bounds(&self, _font_id: FontId, _glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(Bounds {
            origin: Point { x: 54.0, y: 0.0 },
            size: Size {
                width: 392.0,
                height: 528.0,
            },
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let (data, index) = self.font_data_for_id(font_id).context("unknown font")?;
        let font_ref = FontRef::from_index(data.data(), index).context("invalid font data")?;
        let units_per_em = font_ref.head().map(|head| head.units_per_em())?;
        let glyph_metrics =
            font_ref.glyph_metrics(SkrifaSize::new(units_per_em as f32), LocationRef::default());
        let glyph_id = SkrifaGlyphId::new(glyph_id.0);
        let width = glyph_metrics
            .advance_width(glyph_id)
            .context("glyph out of range")?;
        Ok(Size { width, height: 0.0 })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let (data, index) = self.font_data_for_id(font_id)?;
        let font_ref = FontRef::from_index(data.data(), index).ok()?;
        let glyph_id = font_ref.charmap().map(ch)?;
        (glyph_id.to_u32() != 0).then_some(GlyphId(glyph_id.to_u32()))
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.rasterize_outline(params).map(|(bounds, _)| bounds)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        _raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.rasterize_outline(params)
            .map(|(bounds, data)| (bounds.size, data))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut layout = self.build_layout(text, font_size, runs);
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());
        convert_layout(&layout, font_size, text.len(), &self.font_registry)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::PlatformDefault
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{ParleyTextSystem, ParleyTextSystemExt, TextSystem};
    use gpui_engine::{FontRun, RenderGlyphParams, font};
    use gpui_types::{Point, px};

    #[test]
    fn shapes_a_line_through_parley() {
        let text_system = ParleyTextSystem::new();
        let layout = text_system.layout_line("hello", px(16.0), &[], None);

        assert_eq!(layout.len, 5);
        assert!(layout.width > px(0.0));
        assert!(!layout.runs.is_empty());
        assert!(!layout.runs[0].glyphs.is_empty());
    }

    #[test]
    fn parley_width_grows_with_font_size() {
        let text_system = ParleyTextSystem::new();
        let small = text_system.layout_line("hello", px(12.0), &[], None);
        let large = text_system.layout_line("hello", px(24.0), &[], None);

        assert!(large.width > small.width);
    }

    #[test]
    fn parley_native_layout_is_reachable_from_a_text_system_handle() {
        let text_system = ParleyTextSystem::new();
        let handle: &dyn TextSystem = &*text_system;
        let parley = handle
            .as_parley()
            .expect("the accessor should recognise a Parley text system");

        let layout =
            parley.layout_indented("hello world this is a longer string", 16.0, 32.0, 120.0);
        assert!(layout.lines().len() > 1);
    }

    #[test]
    fn resolves_distinct_fonts_to_distinct_ids_and_back() {
        let text_system = ParleyTextSystem::new();
        let regular = font("IBM Plex Sans");
        let bold = font("IBM Plex Sans").bold();
        let regular_id = text_system.resolve_font(&regular);
        let bold_id = text_system.resolve_font(&bold);

        assert_ne!(regular_id, bold_id);
        assert_eq!(
            text_system.get_font_for_id(regular_id),
            Some(regular.clone())
        );
        assert_eq!(text_system.get_font_for_id(bold_id), Some(bold));
        assert_eq!(text_system.resolve_font(&regular), regular_id);
    }

    #[test]
    fn shapes_mixed_weight_runs() {
        let text_system = ParleyTextSystem::new();
        let regular = font("IBM Plex Sans");
        let bold = font("IBM Plex Sans").bold();
        let regular_id = text_system.resolve_font(&regular);
        let bold_id = text_system.resolve_font(&bold);

        let runs = [
            FontRun {
                len: 7,
                font_id: regular_id,
            },
            FontRun {
                len: 4,
                font_id: bold_id,
            },
        ];
        let layout = text_system.layout_line("regularbold", px(16.0), &runs, None);

        assert_eq!(layout.len, 11);
        assert!(layout.width > px(0.0));
        assert!(!layout.runs.is_empty());
        // A run carries the face the shaper actually used, not the id of the family
        // it was asked for, so the two weights come back as two faces of their own
        // rather than as the two ids they were requested with.
        let mut faces: Vec<_> = layout.runs.iter().map(|run| run.font_id).collect();
        faces.dedup();
        assert_eq!(
            faces.len(),
            2,
            "the two weights should be shaped from two faces, not {faces:?}"
        );
    }

    #[test]
    fn wraps_a_line_into_multiple_lines() {
        let text_system = ParleyTextSystem::new();
        let wrapped = text_system.layout_wrapped_line(
            "hello world this is a long line that should wrap",
            px(16.0),
            &[],
            Some(px(100.0)),
            None,
        );

        assert_eq!(wrapped.wrap_width, Some(px(100.0)));
        assert!(
            !wrapped.wrap_boundaries.is_empty(),
            "expected at least one wrap boundary"
        );
        assert!(wrapped.width() <= px(100.0));
    }

    #[test]
    fn rasterizes_a_glyph() {
        let text_system = ParleyTextSystem::new();
        let font_id = text_system.resolve_font(&font("IBM Plex Sans"));
        let glyph_id = text_system
            .platform_text_system()
            .glyph_for_char(font_id, 'A')
            .expect("the embedded font should map 'A'");
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: px(16.0),
            subpixel_variant: Point { x: 0, y: 0 },
            scale_factor: 1.0,
            is_emoji: false,
            subpixel_rendering: false,
            dilation: 0,
        };

        let bounds = text_system.raster_bounds(&params).unwrap();
        assert!(bounds.size.width.0 > 0);
        assert!(bounds.size.height.0 > 0);

        let (size, data) = text_system.rasterize_glyph(&params).unwrap();
        assert_eq!(size.width, bounds.size.width);
        assert_eq!(size.height, bounds.size.height);
        assert!(!data.is_empty());
    }

    #[test]
    fn parley_native_layout_methods_terminate() {
        let text_system = ParleyTextSystem::new();
        let text = "The quick brown fox jumps over the lazy dog while the bright \
                    sun shines down on the quiet meadow near the river.";

        let indented = text_system.layout_indented(text, 16.0, 40.0, 320.0);
        assert!(indented.lines().len() > 1);

        let boxes = [
            crate::LineBox {
                x: 0.0,
                width: 320.0,
            },
            crate::LineBox {
                x: 80.0,
                width: 160.0,
            },
            crate::LineBox {
                x: 0.0,
                width: 320.0,
            },
        ];
        let boxed = text_system.layout_with_boxes(text, 16.0, &boxes);
        assert!(boxed.lines().len() > 1);

        let counted = text_system.layout_with_char_count(text, 16.0, 16);
        assert!(counted.lines().len() > 1);
    }

    #[test]
    fn rasterizes_a_full_line_of_glyphs() {
        let text_system = ParleyTextSystem::new();
        let layout = text_system.layout_line(
            "The quick brown fox jumps over the lazy dog",
            px(16.0),
            &[],
            None,
        );

        let mut count = 0;
        for run in &layout.runs {
            for glyph in &run.glyphs {
                let params = RenderGlyphParams {
                    font_id: run.font_id,
                    glyph_id: glyph.id,
                    font_size: px(16.0),
                    subpixel_variant: Point { x: 0, y: 0 },
                    scale_factor: 1.0,
                    is_emoji: glyph.is_emoji,
                    subpixel_rendering: false,
                    dilation: 0,
                };
                let _ = text_system.rasterize_glyph(&params).unwrap();
                count += 1;
            }
        }
        assert!(count > 10);
    }

    #[test]
    fn line_layout_cache_reuses_layouts_across_frames() {
        let text_system = ParleyTextSystem::new();

        let first = text_system.layout_line("hello", px(16.0), &[], None);
        let same_frame = text_system.layout_line("hello", px(16.0), &[], None);
        assert!(Arc::ptr_eq(&first, &same_frame));

        text_system.finish_frame();
        let next_frame = text_system.layout_line("hello", px(16.0), &[], None);
        assert!(Arc::ptr_eq(&first, &next_frame));

        let different = text_system.layout_line("world", px(16.0), &[], None);
        assert!(!Arc::ptr_eq(&first, &different));
    }

    #[test]
    fn wrapped_line_layout_cache_reuses_layouts_across_frames() {
        let text_system = ParleyTextSystem::new();

        let first =
            text_system.layout_wrapped_line("hello world", px(16.0), &[], Some(px(100.0)), None);
        let same_frame =
            text_system.layout_wrapped_line("hello world", px(16.0), &[], Some(px(100.0)), None);
        assert!(Arc::ptr_eq(&first, &same_frame));

        text_system.finish_frame();
        let next_frame =
            text_system.layout_wrapped_line("hello world", px(16.0), &[], Some(px(100.0)), None);
        assert!(Arc::ptr_eq(&first, &next_frame));
    }

    #[test]
    fn reuse_layouts_carries_layouts_through_a_reused_frame() {
        let text_system = ParleyTextSystem::new();

        // Frame 1 prepaints and shapes a line.
        let start = text_system.layout_index();
        let first = text_system.layout_line("hello", px(16.0), &[], None);
        let end = text_system.layout_index();
        text_system.finish_frame();

        // Frame 2 reuses the previous prepaint: no layout is requested, but the
        // range is carried forward.
        text_system.reuse_layouts(start..end);
        text_system.finish_frame();

        // Frame 3 prepaints again; the line should still be cached.
        let third = text_system.layout_line("hello", px(16.0), &[], None);
        assert!(Arc::ptr_eq(&first, &third));
    }
}
