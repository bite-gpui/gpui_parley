# the four cost examples — 2026-09-26

What `raster_cost`, `vector_cost` and `frame_cost` print, recorded in the crate's
own repository rather than in the application that used to vendor it.

```sh
cd <this repository>
cargo run --release --example raster_cost
cargo run --release --example vector_cost
cargo run --release --example frame_cost
```

| | |
| --- | --- |
| toolchain | `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1` — this repository's own `rust-toolchain.toml` |
| host | Intel i7-8750H, 12 threads, linux x86_64 — **not a controlled benchmarking host**: a laptop CPU, no pinning, no fixed clocks, other load present |
| corpus | the constants in each example: 30 lines / 3,480 glyphs for the first two, 24 lines at 1000x700 for the third |

Re-run the same day, same host and toolchain: every count identical, the clocks
within ~10% (the lattice frame 3.6 → 3.3 ms, its `0.5px` step 44.1 → 41.1 µs).
The blocks below are that re-run. That spread is the point of the paragraph after
them: the counts are what to trust, the clocks are a machine's.

These are the same examples the scroll demo carried before the crate was
extracted, and earlier numbers were recorded there — in `patches/README.md`, by
the author of the crate, without naming a host or a compiler. Those are 0.8×–2.6×
away from these on identical counts. The counts are what to trust; the clocks are
a machine's.

## `raster_cost`

```
one frame: 30 lines, 3480 glyphs, 840 distinct rasters
  layout         4.0ms   (133.2µs a line)
  raster        22.7ms   (27.0µs a glyph)
  total         26.7ms

  1000 rasters of one glyph at one size: 18.250613ms
  1000 hinting instances at one size:    31.215162ms
  1000 font loads + outline collections: 336.536µs
  1000 hinted outline draws at one size: 1.906337ms
  1000 unhinted outline draws at one size: 259.464µs

  a frame asks for one size per line, so: 30 instances, not 3480

steady over 200 frames of a flick, sizes bounded and oscillating:
ladder          atlas   misses/frm   layout/frm   raster/frm
continuous      32508          154      284.6µs        3.3ms
0.5px             644            0      241.0µs       41.1µs
0.25px           1260            0      276.2µs       44.8µs
0.125px          2548            0      249.2µs       42.7µs
0.0625px         5068            0      255.4µs       45.0µs
```

The first block is a frame asked for the way an animated transform asks for one —
a different font size on every line, so nothing in either cache can be reused. The
second sweeps what rounding the size a glyph is *rasterized* at would buy, over 200
frames whose sizes stay inside that range. Two things come out of it: a lattice
stops missing once it is warm, and every step in it is the same ~43 µs a frame
against 3.3 ms at a continuous size — **~80×, for free**. So the step is not a
speed knob but a smoothing one, and what it costs is atlas entries, in proportion.

That is the measurement behind the second change this repository carries: the
hinting instance cached per face and size, rather than built once per rasterized
glyph. `1000 hinting instances at one size: 31.2ms` is what one size costs when it
is built 1000 times.

## `vector_cost`

```
one frame: 30 lines, 3480 glyphs
  tessellation (once)       638.7µs  for 56 outlines of 28 glyphs across 2 bands, 1271 triangles
  transform (a frame)        47.7µs  for 3480 glyphs, 218322 vertices
  vertex bytes              1746576  (99.9 MB a frame at 60fps)
```

The other way to draw text: tessellate each glyph's outline once, in em units, then
place the same triangles every frame at whatever size each line asks for. A
screenful is ~0.7 ms of CPU — 639 µs of tessellation once, then 48 µs a frame — to
place 3,480 glyphs as 218,322 vertices.

This is the CPU half only. What the GPU does with those vertices is not something a
headless example can answer.

## `frame_cost`

```
24 lines, 139 glyphs, 887.7682px wide at 14px, 28 glyph outlines in 28 band

flattening error   triangles   vertices   records a frame
    0.25              62040      186120    18.5 MB
    0.50              60552      181656    18.0 MB
    0.75              60024      180072    17.9 MB
    1.00              60024      180072    17.9 MB
    1.50              59880      179640    17.8 MB

cold, one frame                 15.7ms  62040 triangles

one frame (24 lines, 1000x700 window)
  place (the app's route)      977.6µs  62040 triangles, 186120 vertices
  place (push_triangle)          1.2ms  the same, through the per-corner bounds unions
  scale (paint_path)           841.2µs  a second copy of every vertex
  expand (104B records)          2.0ms  18.459778 MB, what the renderer is handed
  the app's route, whole        3.8ms  placement, scaled copy and expansion

  bytes a frame               44668800  (42.6 MB, 2.50 GB/s at 60fps)
  shape (once)                  85.7µs  for 24 lines
  tessellate (once)            339.3µs  for 28 outlines, 587 triangles
  tessellate (cached)            3.8µs  for 28 outlines, which is what a lookup costs
```

The whole per-frame route the vector path walks, including the parts that are not
obvious from outside: building a `Path` a corner at a time, the `Path::scale` that
`paint_path` does to every path, and the expansion into the renderer's 104-byte
per-vertex records. 42.6 MB moves per frame, 2.50 GB/s at 60 fps.

Most of the traffic is the expansion: one 104-byte record per vertex, each carrying
a whole `Background` and the path's bounds beside its position. The scene-side
half — the 186,120 `PathVertex`es from `gpui_engine` — is 6 MB, so what is left to
attack is the expansion and not the scene.
