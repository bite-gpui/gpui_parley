//! Times the CPU half of the app's vector mode, a frame at a time, and prints
//! what it would cost in bytes.
//!
//! `vector_cost` measures the two halves the app's vector mode rests on —
//! tessellate once, place per frame. This measures the *whole* per-frame path the
//! app actually walks, including the parts that are not obvious from the outside:
//! building a `gpui_engine::Path` a triangle at a time (`push_triangle` unions a
//! `Bounds` for every corner it is handed, three to a triangle), the `Path::scale`
//! that `Window::paint_path` does to every path before it reaches the scene, and
//! the expansion `gpui_wgpu` then does — one `PathRasterizationVertex` per vertex,
//! which is 104 bytes, because each one carries the whole `Background` and the
//! path's bounds along with its position.
//!
//!     cargo run --release -p gpui_parley --example frame_cost
//!
//! Still the CPU half only: what the GPU does with 24MB of vertices a frame is
//! not something a headless example can answer. What it can say is which of the
//! CPU-side copies is worth removing and what the vertex traffic is.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_engine::{FontId, GlyphId, Path, PathVertex, TextSystem, font};
use gpui_parley::{GlyphTriangles, ParleyTextSystem};
use gpui_types::{Background, Bounds, Pixels, Point, ScaledPixels, point, px, size};

/// Body prose, about as wide as the app's 800px column at 14px.
const LINE: &str = "The quick brown fox jumps over the lazy dog while the bright sun shines \
                    down on the quiet meadow near the river bank and the road beyond it";

/// The app's body font size.
const SIZE: f32 = 14.0;

/// How many lines of the document a 1000x700 window shows at the app's 30px
/// line spacing.
const LINES: usize = 24;

/// The app's flattening budget, in device pixels.
const GLYPH_ERROR: f32 = 0.25;

/// A screenful's worth of shaped lines, at the size the document asks for.
struct Screenful {
    /// Each line's glyphs: font, id, and where the shaper put them.
    lines: Vec<Vec<(FontId, GlyphId, Pixels, Pixels)>>,
}

impl Screenful {
    fn shape(text_system: &ParleyTextSystem) -> Self {
        let mut lines = Vec::new();
        for _ in 0..LINES {
            let layout = text_system.layout_line(LINE, px(SIZE), &[], None);
            let mut line = Vec::new();
            for run in &layout.runs {
                for glyph in &run.glyphs {
                    line.push((run.font_id, glyph.id, glyph.position.x, glyph.position.y));
                }
            }
            lines.push(line);
        }
        Self { lines }
    }
}

/// The size an outline should be tessellated for, given the size it is drawn at:
/// the app rounds up to a power of two, so one outline serves a band of sizes.
fn band(size: f32) -> f32 {
    2f32.powf(size.max(0.001).log2().ceil())
}

/// Where a triangle's corners sit to be filled solid, as the renderer's path
/// shader reads them.
const SOLID: Point<f32> = point(0.0, 1.0);

/// The per-vertex record `gpui_wgpu` expands a path into. A copy of its own
/// `PathRasterizationVertex` (`crates/gpui_wgpu/src/wgpu_renderer.rs`), which is
/// private to that crate, so that the expansion below can be timed and its size
/// asserted without reaching into the renderer.
#[derive(Clone, Copy)]
#[repr(C)]
struct RasterVertex {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

/// What a screenful's glyphs flatten into, at a range of flattening budgets.
///
/// The triangles are the whole of everything downstream — the placement, the
/// renderer's expansion, the upload, the rasterization — so this is the size of
/// that knob, before any question of what it looks like.
///
/// A tessellation is cached by size and not by tolerance, so each row gets a text
/// system of its own; a `FontId` belongs to the system that resolved it, so each
/// row resolves the face again rather than borrowing the screenful's.
fn flattening_sweep(screenful: &Screenful) {
    let glyphs: Vec<GlyphId> = screenful
        .lines
        .iter()
        .flat_map(|line| line.iter().map(|(_, glyph_id, _, _)| *glyph_id))
        .collect();

    println!("flattening error   triangles   vertices   records a frame");
    for error in [0.25, 0.5, 0.75, 1.0, 1.5] {
        let text_system = ParleyTextSystem::new();
        let font_id = text_system.resolve_font(&font("IBM Plex Sans"));
        let mut counted: HashMap<u32, usize> = HashMap::new();
        let mut triangles = 0usize;
        for glyph_id in &glyphs {
            let count = match counted.get(&glyph_id.0) {
                Some(count) => *count,
                None => {
                    let count = text_system
                        .glyph_triangles(font_id, *glyph_id, band(SIZE), error)
                        .expect("the embedded font should tessellate")
                        .len();
                    counted.insert(glyph_id.0, count);
                    count
                }
            };
            triangles += count;
        }
        let vertices = triangles * 3;
        println!(
            "{:>8.2}          {triangles:>9}  {vertices:>10}  {:>6.1} MB",
            error,
            vertices as f32 * RASTER_VERTEX_BYTES as f32 / (1024.0 * 1024.0),
        );
    }
    println!();
}

/// How long a screenful's frame takes, and what it moved.
#[derive(Default)]
struct Frame {
    triangles: usize,
    vertices: usize,
    /// Building the paths the way the app does, triangle by triangle.
    place: Duration,
    /// The same triangles pushed straight onto the vertex list, with the path's
    /// bounds gathered in plain floats instead of a `Bounds` union per corner,
    /// which is what the app does now.
    place_direct: Duration,
    /// `Path::scale`, which `Window::paint_path` does to every path it is handed.
    scale: Duration,
    /// Expanding the paths into per-vertex records, as the renderer does.
    expand: Duration,
}

/// The size of the record the renderer expands a path into, from a copy of its
/// own struct — the fork's own test asserts the same number.
const RASTER_VERTEX_BYTES: usize = size_of::<RasterVertex>();

impl Frame {
    fn total(&self) -> Duration {
        self.place_direct + self.scale + self.expand
    }

    fn bytes(&self) -> usize {
        self.vertices * RASTER_VERTEX_BYTES
    }

    /// What the app's own route costs, in bytes moved per frame: the path's own
    /// vertices, the scaled copy of them, and the renderer's expansion of those.
    fn traffic(&self) -> usize {
        self.vertices * size_of::<PathVertex<Pixels>>() + self.bytes() * 2
    }
}

fn main() {
    assert_eq!(
        RASTER_VERTEX_BYTES, 104,
        "the record should still be the renderer's own 26 words"
    );

    let text_system = ParleyTextSystem::new();
    let font_id = text_system.resolve_font(&font("IBM Plex Sans"));

    // One line, to report the shape of the thing being measured.
    let layout = text_system.layout_line(LINE, px(SIZE), &[], None);

    // What the screenful needs: which glyph, at which band.
    let mut wanted: HashSet<(u32, u32)> = HashSet::new();
    let mut outlines = 0usize;
    for run in &layout.runs {
        for glyph in &run.glyphs {
            wanted.insert((glyph.id.0, band(SIZE).to_bits()));
            outlines += 1;
        }
    }

    // Shaping a screenful: the app does this once per line, ever.
    let start = Instant::now();
    let screenful = Screenful::shape(&text_system);
    let shaping = start.elapsed();

    // Tessellating every glyph the screenful uses. Once, ever.
    let start = Instant::now();
    let mut cache: HashMap<(u32, u32), std::sync::Arc<GlyphTriangles>> = HashMap::new();
    let mut triangles_available = 0usize;
    for key in &wanted {
        let triangles = text_system
            .glyph_triangles(font_id, GlyphId(key.0), f32::from_bits(key.1), GLYPH_ERROR)
            .expect("the embedded font should tessellate");
        triangles_available += triangles.len();
        cache.insert(*key, triangles);
    }
    let tessellation = start.elapsed();

    println!(
        "{LINES} lines, {} glyphs, {}px wide at {SIZE}px, {} glyph outlines in {} band",
        layout
            .runs
            .iter()
            .map(|run| run.glyphs.len())
            .sum::<usize>(),
        f32::from(layout.width),
        wanted
            .iter()
            .map(|(glyph, _)| glyph)
            .collect::<HashSet<_>>()
            .len(),
        wanted.len(),
    );
    println!();

    flattening_sweep(&screenful);

    // The window is 1000 wide, so the app's column sits at x = 100.
    let left = 100.0f32;
    let frame = |screenful: &Screenful| -> Frame {
        let mut frame = Frame::default();

        // The app's route: one `Path` per line, a triangle at a time.
        let start = Instant::now();
        let mut paths = Vec::with_capacity(LINES);
        for (index, line) in screenful.lines.iter().enumerate() {
            let top = index as f32 * 30.0;
            let mut path = Path::new(point(px(left), px(top)));
            for (_, id, x, y) in line {
                let key = (id.0, band(SIZE).to_bits());
                let Some(triangles) = cache.get(&key) else {
                    continue;
                };
                // The glyph's place in the line, at the size the line is drawn
                // at: the outline is in em units, so this is the whole transform.
                let (glyph_x, glyph_y) = (left + f32::from(*x), top + f32::from(*y));
                for triangle in triangles.iter() {
                    let corner = |p: &Point<Pixels>| {
                        point(px(glyph_x + p.x.0 * SIZE), px(glyph_y + p.y.0 * SIZE))
                    };
                    path.push_triangle(
                        (
                            corner(&triangle[0]),
                            corner(&triangle[1]),
                            corner(&triangle[2]),
                        ),
                        (SOLID, SOLID, SOLID),
                    );
                }
                frame.triangles += triangles.len();
            }
            frame.vertices += path.vertices.len();
            paths.push(path);
        }
        frame.place = start.elapsed();

        // The app's route now: the same triangles pushed straight onto the
        // vertex list, with the path's bounds gathered in the same pass as plain
        // floats rather than a `Bounds` union per corner.
        let start = Instant::now();
        let mut direct: Vec<Path<Pixels>> = Vec::with_capacity(LINES);
        for (index, line) in screenful.lines.iter().enumerate() {
            let top = index as f32 * 30.0;
            let mut path = Path::new(point(px(left), px(top)));
            path.vertices.reserve(line.len() * 3 * 18);
            let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
            let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
            for (_, id, x, y) in line {
                let key = (id.0, band(SIZE).to_bits());
                let Some(triangles) = cache.get(&key) else {
                    continue;
                };
                let (glyph_x, glyph_y) = (left + f32::from(*x), top + f32::from(*y));
                for triangle in triangles.iter() {
                    for corner in triangle {
                        let x = glyph_x + corner.x.0 * SIZE;
                        let y = glyph_y + corner.y.0 * SIZE;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                        path.vertices.push(PathVertex {
                            xy_position: point(px(x), px(y)),
                            st_position: SOLID,
                            // Inert in the published 1.21.0 (the renderer clips by
                            // `clipped_bounds()`), but the field exists and must be
                            // filled; see the note in `src/main.rs`.
                            content_mask: Default::default(),
                        });
                    }
                }
            }
            if !path.vertices.is_empty() {
                path.bounds = Bounds::new(
                    point(px(min_x), px(min_y)),
                    size(px(max_x - min_x), px(max_y - min_y)),
                );
            }
            direct.push(path);
        }
        frame.place_direct = start.elapsed();
        drop(direct);

        // What `paint_path` does before the path reaches the scene.
        let start = Instant::now();
        let scaled: Vec<Path<ScaledPixels>> = paths.iter().map(|path| path.scale(1.0)).collect();
        frame.scale = start.elapsed();

        // And what the renderer does with them: one record per vertex, each
        // carrying the colour and the bounds it is drawn with.
        let start = Instant::now();
        let mut records: Vec<RasterVertex> = Vec::new();
        for path in &scaled {
            let bounds = path.clipped_bounds();
            let color = path.color;
            records.extend(path.vertices.iter().map(|vertex| RasterVertex {
                xy_position: vertex.xy_position,
                st_position: vertex.st_position,
                color,
                bounds,
            }));
        }
        frame.expand = start.elapsed();
        assert_eq!(records.len(), frame.vertices);

        frame
    };

    // Cold: nothing tessellated yet that this frame needs... but the cache is
    // warm by now, which is the steady state the numbers below are about.
    let first = frame(&screenful);
    println!(
        "cold, one frame              {:>9.1?}  {} triangles",
        first.total(),
        first.triangles
    );
    println!();

    let runs = 20;
    let mut place = Duration::ZERO;
    let mut place_triangle = Duration::ZERO;
    let mut scale = Duration::ZERO;
    let mut expand = Duration::ZERO;
    let mut last = Frame::default();
    for _ in 0..runs {
        last = frame(&screenful);
        place += last.place_direct;
        place_triangle += last.place;
        scale += last.scale;
        expand += last.expand;
    }
    let mean = |total: Duration| total / runs as u32;
    let per = mean(place + scale + expand);

    println!("one frame ({LINES} lines, 1000x700 window)");
    println!(
        "  place (the app's route)    {:>9.1?}  {} triangles, {} vertices",
        mean(place),
        last.triangles,
        last.vertices
    );
    println!(
        "  place (push_triangle)      {:>9.1?}  the same, through the per-corner bounds unions",
        mean(place_triangle)
    );
    println!(
        "  scale (paint_path)         {:>9.1?}  a second copy of every vertex",
        mean(scale)
    );
    println!(
        "  expand (104B records)      {:>9.1?}  {} MB, what the renderer is handed",
        mean(expand),
        last.bytes() as f32 / (1024.0 * 1024.0)
    );
    println!("  the app's route, whole    {per:>9.1?}  placement, scaled copy and expansion");
    println!();
    println!(
        "  bytes a frame              {:>9}  ({:.1} MB, {:.2} GB/s at 60fps)",
        last.traffic(),
        last.traffic() as f32 / (1024.0 * 1024.0),
        last.traffic() as f32 * 60.0 / (1024.0 * 1024.0 * 1024.0)
    );
    println!(
        "  shape (once)               {:>9.1?}  for {LINES} lines",
        shaping
    );
    println!(
        "  tessellate (once)          {:>9.1?}  for {} outlines, {triangles_available} triangles",
        tessellation,
        wanted.len()
    );

    // What scrolling into fresh text costs: one new line's glyphs, tessellated.
    let fresh: Vec<(u32, u32)> = wanted.iter().copied().collect();
    let start = Instant::now();
    for key in &fresh {
        text_system
            .glyph_triangles(font_id, GlyphId(key.0), f32::from_bits(key.1), GLYPH_ERROR)
            .expect("cached");
    }
    println!(
        "  tessellate (cached)        {:>9.1?}  for {} outlines, which is what a lookup costs",
        start.elapsed(),
        fresh.len()
    );
    let _ = outlines;
}
