//! Times the two halves of a text frame: laying lines out, and rasterizing the
//! glyphs they produce.
//!
//! The demo's transform asks for a *different font size on every line*, and a
//! different one again next frame, so nothing in either cache can be reused
//! between frames: every line is shaped again, and every glyph is rasterized
//! again — the atlas is keyed on the font size, so a continuous zoom misses on
//! every glyph of every line.
//!
//! It then sweeps what rounding the size a glyph is *rasterized* at would buy,
//! over a run of frames and with a cache that persists across them the way the
//! renderer's atlas does. The lines are still shaped at the exact sizes they are
//! drawn at — that is what positions the glyphs and spaces the line — but the size
//! in the cache key is a lattice, which is what `Window::GLYPH_RASTER_STEP` rounds
//! to. A continuous size keeps missing for as long as the document moves; a lattice
//! fills the cache with a finite set of sizes and then answers from it.
//!
//! This measures a screenful of that, standing in for one frame of the app.
//!
//!     cargo run --release -p gpui_parley --example raster_cost
//!
//! Run it in release too: the interesting number is milliseconds per frame, and
//! the debug build inflates both halves by roughly an order of magnitude.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_engine::{LineLayout, RenderGlyphParams, TextSystem, font};
use gpui_parley::ParleyTextSystem;
use gpui_types::{Point, px};
use skrifa::{
    FontRef, MetadataProvider,
    instance::{LocationRef, Size as SkrifaSize},
    outline::{DrawSettings, HintingInstance, HintingOptions, OutlinePen},
};

/// A pen that throws the outline away, so the draw can be timed on its own.
struct Nothing;

impl OutlinePen for Nothing {
    fn move_to(&mut self, _x: f32, _y: f32) {}

    fn line_to(&mut self, _x: f32, _y: f32) {}

    fn quad_to(&mut self, _cx0: f32, _cy0: f32, _x: f32, _y: f32) {}

    fn curve_to(&mut self, _cx0: f32, _cy0: f32, _cx1: f32, _cy1: f32, _x: f32, _y: f32) {}

    fn close(&mut self) {}
}

/// One line's worth of prose, about as wide as the app's 800px column.
const LINE: &str = "The quick brown fox jumps over the lazy dog while the bright sun \
                    shines down on the quiet meadow near the river bank";

/// How many lines a screenful covers.
const LINES: usize = 30;

/// One frame's worth of text: the lines are *shaped* at the exact sizes they are
/// drawn at, and the glyphs are *rasterized* at those sizes rounded onto a lattice
/// of `step` pixels (or exactly, when `step` is zero), which is what the renderer
/// keys its atlas on.
///
/// `atlas` stands in for that cache: a key already in it is a hit and is not
/// rasterized again. It persists across the frames of the sweep, which is the point
/// — whether a key comes back next frame is what decides whether the app pays for a
/// glyph once or every frame.
///
/// Returns what the frame had to rasterize, what shaping cost, and what rasterizing
/// cost.
fn rasterize_frame(
    text_system: &ParleyTextSystem,
    sizes: &[f32],
    step: f32,
    atlas: &mut HashSet<(u32, u32)>,
) -> (usize, Duration, Duration) {
    let start = Instant::now();
    let layouts: Vec<Arc<LineLayout>> = sizes
        .iter()
        .map(|size| text_system.layout_line(LINE, px(*size), &[], None))
        .collect();
    let layout_time = start.elapsed();

    let mut misses = 0usize;
    let start = Instant::now();
    for (size, layout) in sizes.iter().zip(&layouts) {
        let raster_size = if step == 0.0 {
            *size
        } else {
            (*size / step).round() * step
        };
        for run in &layout.runs {
            for glyph in &run.glyphs {
                if !atlas.insert((glyph.id.0, raster_size.to_bits())) {
                    continue;
                }
                let params = RenderGlyphParams {
                    font_id: run.font_id,
                    glyph_id: glyph.id,
                    font_size: px(raster_size),
                    subpixel_variant: Point { x: 0, y: 0 },
                    scale_factor: 1.0,
                    is_emoji: glyph.is_emoji,
                    subpixel_rendering: false,
                    dilation: 0,
                };
                text_system
                    .rasterize_glyph(&params)
                    .expect("the embedded font should rasterize");
                misses += 1;
            }
        }
    }
    (misses, layout_time, start.elapsed())
}

fn main() {
    let text_system = ParleyTextSystem::new();
    let font_id = text_system.resolve_font(&font("IBM Plex Sans"));

    // A screenful of the document, each line at the size the transform asks for
    // on it. Distinct sizes on purpose: that is what defeats both caches.
    let sizes: Vec<f32> = (0..LINES).map(|line| 14.0 + (line as f32) * 0.37).collect();

    // Half one: laying the lines out. This is shaping plus line breaking, and
    // it is what Parley does.
    let start = Instant::now();
    let layouts: Vec<(f32, _)> = sizes
        .iter()
        .map(|size| (*size, text_system.layout_line(LINE, px(*size), &[], None)))
        .collect();
    let layout_time = start.elapsed();

    // Half two: rasterizing the glyphs, as paint does on an atlas miss. The
    // atlas deduplicates within a frame, so this counts distinct glyphs per
    // size — which, with one size per line, is nearly every glyph on its own.
    let mut every_glyph = 0usize;
    let mut distinct = HashSet::new();
    for (size, layout) in &layouts {
        for run in &layout.runs {
            for glyph in &run.glyphs {
                every_glyph += 1;
                distinct.insert((glyph.id.0, size.to_bits()));
            }
        }
    }

    let mut rasterized = 0usize;
    let start = Instant::now();
    for (size, layout) in &layouts {
        for run in &layout.runs {
            for glyph in &run.glyphs {
                if !distinct.remove(&(glyph.id.0, size.to_bits())) {
                    continue;
                }
                let params = RenderGlyphParams {
                    font_id: run.font_id,
                    glyph_id: glyph.id,
                    font_size: px(*size),
                    subpixel_variant: Point { x: 0, y: 0 },
                    scale_factor: 1.0,
                    is_emoji: glyph.is_emoji,
                    subpixel_rendering: false,
                    dilation: 0,
                };
                text_system
                    .rasterize_glyph(&params)
                    .expect("the embedded font should rasterize");
                rasterized += 1;
            }
        }
    }
    let raster_time = start.elapsed();

    let per_glyph = raster_time / rasterized as u32;
    println!("one frame: {LINES} lines, {every_glyph} glyphs, {rasterized} distinct rasters");
    println!(
        "  layout  {layout_time:>12.1?}   ({:.1?} a line)",
        layout_time / LINES as u32
    );
    println!("  raster  {raster_time:>12.1?}   ({per_glyph:.1?} a glyph)");
    println!("  total   {:>12.1?}", layout_time + raster_time);

    // Where the rasterization goes, on one glyph, with the hinting instance
    // built the way `rasterize_outline` builds it today.
    let params = RenderGlyphParams {
        font_id,
        glyph_id: text_system
            .platform_text_system()
            .glyph_for_char(font_id, 'H')
            .expect("the embedded font should map 'H'"),
        font_size: px(14.0),
        subpixel_variant: Point { x: 0, y: 0 },
        scale_factor: 1.0,
        is_emoji: false,
        subpixel_rendering: false,
        dilation: 0,
    };
    let start = Instant::now();
    for _ in 0..1000 {
        text_system.rasterize_glyph(&params).unwrap();
    }
    println!(
        "\n  1000 rasters of one glyph at one size: {:?}",
        start.elapsed()
    );

    // Attribution. `rasterize_outline` builds a hinting instance for every
    // glyph it draws; skrifa builds one by running the font's hinting program at
    // a size, which is expensive. This is that cost on its own, for the same
    // size, without any of the outline work it is then used for.
    let data = std::fs::read("assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf")
        .expect("the font should be readable from the workspace root");
    let font = FontRef::from_index(&data, 0).expect("the embedded face should load");
    let outlines = font.outline_glyphs();
    let size = SkrifaSize::new(14.0);

    let start = Instant::now();
    for _ in 0..1000 {
        let _ = HintingInstance::new(
            &outlines,
            size,
            LocationRef::default(),
            HintingOptions::default(),
        )
        .expect("the embedded font should be hintable");
    }
    println!(
        "  1000 hinting instances at one size:    {:?}",
        start.elapsed()
    );

    // And the per-call setup the raster path repeats for every single glyph:
    // locating the face and asking for its outlines.
    let start = Instant::now();
    for _ in 0..1000 {
        let font = FontRef::from_index(&data, 0).expect("the embedded face should load");
        let _ = font.outline_glyphs();
    }
    println!(
        "  1000 font loads + outline collections: {:?}",
        start.elapsed()
    );

    // And the outline draw that instance is then used for, with the instance
    // built once — which is what a cache would turn the raster path into.
    let hinting = HintingInstance::new(
        &outlines,
        size,
        LocationRef::default(),
        HintingOptions::default(),
    )
    .expect("the embedded font should be hintable");
    let glyph = outlines
        .get(
            font.charmap()
                .map('H')
                .expect("the embedded font should map 'H'"),
        )
        .expect("the glyph should have an outline");
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = glyph.draw(DrawSettings::hinted(&hinting, false), &mut Nothing);
    }
    println!(
        "  1000 hinted outline draws at one size: {:?}",
        start.elapsed()
    );

    // The same draw without hinting, which is what the outline would cost if the
    // raster path skipped it: hinted outlines are snapped to a pixel grid, which
    // is worth paying for on static text and arguably wrong for text that is
    // being rescaled on every frame.
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = glyph.draw(
            DrawSettings::unhinted(size, LocationRef::default()),
            &mut Nothing,
        );
    }
    println!(
        "  1000 unhinted outline draws at one size: {:?}",
        start.elapsed()
    );
    println!(
        "\n  a frame asks for one size per line, so: {} instances, not {}",
        LINES, every_glyph
    );

    // Whether snapping the size to a ladder rescues the cache.
    //
    // A flick walks every line's size upward between frames, by roughly a pixel
    // at the speeds that matter. Continuous sizes make that a new key for every
    // glyph every frame — the atlas never holds anything long enough to be
    // asked for it twice. A ladder makes the key space finite: the frame still
    // *asks* for every glyph it draws, but it asks at one of a fixed set of
    // sizes, so once the ladder is warm the atlas answers. What that costs is
    // capacity — every rung in use holds its own copy of every glyph.
    //
    // The atlas starts at 1024x1024 and grows to the device's texture limit, so
    // the question a ladder has to answer is how many rungs it can afford.
    #[allow(clippy::type_complexity)]
    let ladders: [(&str, f32); 5] = [
        ("continuous", 0.0),
        ("0.5px", 0.5),
        ("0.25px", 0.25),
        ("0.125px", 0.125),
        ("0.0625px", 0.0625),
    ];
    const FRAMES: usize = 200;

    // The transform's sizes are bounded: the scale a line is drawn at is a
    // function of where it is, so a line's size oscillates within a range rather
    // than sliding away, and the same range is handed out frame after frame. That
    // is the whole question for a ladder — whether its rungs are revisited — so
    // the sizes here oscillate over a little more than the body text's span.
    let size_of = |line: usize, frame: usize| {
        let phase = frame as f32 * 0.15 + line as f32 * 0.2;
        14.0 * (1.0 + 0.4 * phase.sin())
    };

    // The first frames are the cache filling, not what a frame settles to, so
    // only the back half is averaged.
    let warm_frames = FRAMES / 2;

    println!("\nsteady over {FRAMES} frames of a flick, sizes bounded and oscillating:");
    println!(
        "{:<11} {:>9} {:>12} {:>12} {:>12}",
        "ladder", "atlas", "misses/frm", "layout/frm", "raster/frm"
    );
    for (name, step) in ladders {
        // A fresh text system per row, so that what a row's frames spend shaping is
        // the row's own: the system caches laid-out lines by size, and sharing one
        // across the sweep would leave every row after the first reading the first
        // row's cache. The atlas is modelled by `atlas` instead, which is the thing
        // the step is supposed to change.
        let text_system = ParleyTextSystem::new();
        let mut atlas = HashSet::new();
        let mut misses = 0usize;
        let mut layout_time = Duration::ZERO;
        let mut raster_time = Duration::ZERO;
        for frame in 0..FRAMES {
            let sizes: Vec<f32> = (0..LINES).map(|line| size_of(line, frame)).collect();
            let (frame_misses, frame_layout, frame_raster) =
                rasterize_frame(&text_system, &sizes, step, &mut atlas);
            if frame >= FRAMES - warm_frames {
                misses += frame_misses;
                layout_time += frame_layout;
                raster_time += frame_raster;
            }
        }
        println!(
            "{name:<11} {:>9} {:>12} {:>12.1?} {:>12.1?}",
            atlas.len(),
            misses / warm_frames,
            layout_time / warm_frames as u32,
            raster_time / warm_frames as u32,
        );
    }
}
