//! Times the *other* way to draw the document: as vector triangles, tessellated
//! once per glyph and transformed per line, instead of rasterized per glyph per
//! size.
//!
//! `raster_cost` measures what the app does now, and finds rasterization is the
//! whole frame. This measures what it would cost to never rasterize: each glyph's
//! outline is tessellated once into triangles in em units, and a frame is then
//! only a multiply-and-add per triangle — the same triangles all frame, at
//! whatever size each line of the transform asks for.
//!
//!     cargo run --release -p gpui_parley --example vector_cost
//!
//! What comes out is the CPU half only: the triangles go to the GPU, and how fast
//! the path pipeline then rasterizes them is not something a headless example can
//! answer.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use gpui_engine::{GlyphId, TextSystem, font};
use gpui_parley::ParleyTextSystem;
use gpui_types::{Point, px};

/// One line's worth of prose, about as wide as the app's 800px column.
const LINE: &str = "The quick brown fox jumps over the lazy dog while the bright sun \
                    shines down on the quiet meadow near the river bank";

/// How many lines a screenful covers.
const LINES: usize = 30;

/// The flattening error tolerated, in device pixels.
const MAX_ERROR: f32 = 0.25;

/// The size an outline should be tessellated for, given the size it is drawn at.
///
/// Rounding *up* to a power of two means the flattening is always fine enough at
/// the size drawn — and it makes the cache key coarse, which is the whole point:
/// a handful of bands rather than one entry per size the transform asks for.
fn band(size: f32) -> f32 {
    2f32.powf(size.max(0.001).log2().ceil())
}

fn main() {
    let text_system = ParleyTextSystem::new();
    let font_id = text_system.resolve_font(&font("IBM Plex Sans"));

    // A line's worth of shaping for each line of a screenful, at the size the
    // transform asks for on that line. This part the vector route still has to
    // do once — or it could be done once for the whole frame instead, by shaping
    // at one size and scaling the glyph positions along with the glyphs.
    let sizes: Vec<f32> = (0..LINES).map(|line| 14.0 + (line as f32) * 0.37).collect();
    let layouts: Vec<(f32, _)> = sizes
        .iter()
        .map(|size| (*size, text_system.layout_line(LINE, px(*size), &[], None)))
        .collect();

    // What the screenful needs: which glyph, at which band.
    let mut wanted: HashSet<(u32, u32)> = HashSet::new();
    let mut glyphs = 0usize;
    for (size, layout) in &layouts {
        for run in &layout.runs {
            for glyph in &run.glyphs {
                wanted.insert((glyph.id.0, band(*size).to_bits()));
                glyphs += 1;
            }
        }
    }

    // One-time: tessellate every one of them.
    let start = Instant::now();
    let mut outlines: HashMap<(u32, u32), _> = HashMap::new();
    let mut triangle_count = 0usize;
    for (glyph, band_bits) in &wanted {
        let triangles = text_system
            .glyph_triangles(
                font_id,
                GlyphId(*glyph),
                f32::from_bits(*band_bits),
                MAX_ERROR,
            )
            .expect("the embedded font should tessellate");
        triangle_count += triangles.len();
        outlines.insert((*glyph, *band_bits), triangles);
    }
    let tessellation_time = start.elapsed();

    // A frame: for each line, scale the triangles of each glyph to that line's
    // size and offset them to where the glyph sits. This is all the CPU work
    // text would cost per frame, and it is the same work at any magnification:
    // one multiply per coordinate.
    let start = Instant::now();
    let mut placed = 0usize;
    let mut vertices = 0usize;
    for (size, layout) in &layouts {
        for run in &layout.runs {
            for glyph in &run.glyphs {
                let key = (glyph.id.0, band(*size).to_bits());
                let outline = outlines
                    .get(&key)
                    .expect("every glyph of the screenful was tessellated");
                // Where the shaper put the glyph, at the size of this line: the
                // outline is in em units, so this is the whole transform.
                let origin = glyph.position;
                for triangle in outline.iter() {
                    for corner in triangle {
                        let _ = Point {
                            x: px(corner.x.0.mul_add(*size, origin.x.0)),
                            y: px(corner.y.0.mul_add(*size, origin.y.0)),
                        };
                    }
                }
                vertices += outline.len() * 3;
                placed += 1;
            }
        }
    }
    let frame_time = start.elapsed();

    println!("one frame: {LINES} lines, {glyphs} glyphs");
    println!(
        "  tessellation (once)  {tessellation_time:>12.1?}  for {} outlines of {} glyphs across {} bands, {triangle_count} triangles",
        outlines.len(),
        wanted
            .iter()
            .map(|(glyph, _)| glyph)
            .collect::<HashSet<_>>()
            .len(),
        wanted
            .iter()
            .map(|(_, band)| band)
            .collect::<HashSet<_>>()
            .len(),
    );
    println!(
        "  transform (a frame)  {frame_time:>12.1?}  for {placed} glyphs, {vertices} vertices"
    );
    println!(
        "  vertex bytes         {:>12}  ({:.1} MB a frame at 60fps)",
        vertices * 8,
        (vertices * 8) as f32 * 60.0 / (1024.0 * 1024.0),
    );
}
