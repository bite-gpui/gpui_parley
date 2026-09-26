# gpui_parley

A second implementation of GPUI's `TextSystem`, built on
[Parley](https://github.com/linebender/parley).

`gpui_engine::TextSystem` is the trait the facade swaps: an application picks its
shaping, layout and rasterization by installing one, and `gpui_parley` is the one
that is not the default. It exists because the SPI's surface — a line, a run, a
glyph — cannot express what Parley can do, so the crate offers those features
through an extension trait while still implementing the SPI for everything else.

## Using it

```toml
[dependencies]
gpui_parley = { git = "https://github.com/bite-gpui/gpui_parley" }
```

```rust
use gpui_parley::ParleyTextSystem;

gpui::application()
    .with_text_system(ParleyTextSystem::new())
    .run(...);
```

`ParleyTextSystem::new()` already returns an `Arc<Self>`, which is what
`with_text_system` wants.

The library target is named `gpui_parley`, so `use gpui_parley::…` is the same
whether the crate comes from here or from the workspace it was extracted out of.

## What it adds beyond the SPI

Through `ParleyTextSystem` itself and the `ParleyTextSystemExt` trait
(`as_parley`, for reaching the concrete type through an `Arc<dyn TextSystem>`):

| | |
| --- | --- |
| `layout_with_boxes` | per-line boxes, which the SPI's line model has no place for |
| `layout_indented` | `text-indent` |
| `layout_with_char_count` | breaking on character count rather than width |
| `glyph_triangles` | a glyph's outline tessellated into triangles in em units, for drawing text as vector paths instead of rasterizing it |
| `resolved_family` | which face a family name actually resolved to |
| `LineBox`, `font_context` | the line-box type, and the shaper's own font context |

## Where this code came from

The crate was `crates/gpui_parley` in
[`bite-gpui/bite-gpui`](https://github.com/bite-gpui/bite-gpui), a member of the
rearchitected layer stack, and was published from there as `bite-gp-parley
1.21.0` on crates.io. It is not a layer: it is one of the implementations the
layers are designed to be able to swap, and it is a consumer of `gpui_engine`
rather than a part of it. Hence a repository of its own, and a version requirement
no longer being how it is reached.

The content here is the copy vendored by
[`bite-gpui/bite_gpui_scroll_demo`](https://github.com/bite-gpui/bite_gpui_scroll_demo)
at `patches/gpui_parley`, which is the most complete version of the crate that
exists. That copy records its own base as the fork's workspace at `d418335` (on
`bite_v1.21.0-pre`) and lists the three changes it had already made to it:

1. **The hinting instance is cached.** `HintingInstance::new` runs the font's
   hinting program at a size, which is expensive; the copy builds one per face and
   device size instead of one per rasterized glyph. An application that rasterizes
   the same face at many sizes — a zoom, or a transform that scales each line —
   paid that per glyph before.
2. **Shaping and rasterization read the shaper's own font database**, rather than
   a font table held by the crate. That database is the host's fonts through
   fontique's platform backend plus everything `add_fonts` is handed, so a family
   named here can be one the machine has and a comma-separated *stack* of them
   resolves in order. Because a stack may resolve to more than one face — a glyph
   no family in it can draw is shaped from whatever can — the face a run was
   actually shaped from is recorded as the run is converted, so the face
   rasterized is the face the run's glyph ids belong to.
3. **One clippy nit** in the crate's own tests, which the demo's workspace lints
   more strictly than the fork's.

So it is worth being plain about what is and is not in
`bite-gpui/bite-gpui`: the crate on every branch there — including
`bite_v1.21.0-pre-path-pass-cost` — is the version *without* the hinting cache.
The three changes above were made in the demo's copy and never pushed back into
the fork's crate. Anyone depending on the published `bite-gp-parley` is getting
the un-cached path.

On the way into this repository the crate was made self-contained: its fonts
moved from the demo workspace into `assets/fonts/`, and its manifest was written
against published `bite-gp-*` versions instead of the fork's workspace
inheritance. Nothing in `src/` changed except the four `include_bytes!` paths.

## Examples

| | |
| --- | --- |
| `parley_demo` | a window: the embedded faces and the Parley-native features. Needs the `demo` feature |
| `raster_cost` | a screenful of lines, split into shaping and rasterization, then a sweep of the size a glyph is *rasterized* at |
| `vector_cost` | the other way to draw text: tessellate each outline once, place it per frame |
| `frame_cost` | the whole CPU route the vector path walks in a frame, and what it costs in bytes |

```sh
cargo run -p gpui_parley --example parley_demo --features demo

# The three cost examples print milliseconds per frame; the debug build inflates
# both halves by roughly an order of magnitude.
cargo run --release -p gpui_parley --example raster_cost
cargo run --release -p gpui_parley --example vector_cost
cargo run --release -p gpui_parley --example frame_cost
```

`raster_cost` reads `assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf` relative
to the working directory, so run it from the repository root — which is what
`cargo run` does.

A recorded run, with the numbers these print and what they mean, is in
[`benchmarks/2026-09-26-cost-examples.md`](benchmarks/2026-09-26-cost-examples.md).

## Tests

```sh
cargo test                                         # shaping, rasterization, the caches
cargo test --features test-support                 # ...and that it can be installed
cargo test -p gpui_parley --features test-support  # (same, from inside the workspace)
```

`test-support` pulls in the facade and moves gpui's frame drawing onto its test
path, which is why it is a feature rather than a dev-dependency: a dev-dependency's
features are unified into every dev-context build, and a build that is not running
tests should not be able to pick that path up by accident.

## Licences

The crate is Apache-2.0 — see `LICENSE-APACHE`. The faces under
`assets/fonts/ibm-plex-sans/` are IBM Plex Sans under the SIL Open Font License
1.1 — see the `license.txt` beside them.

## State

Not published to crates.io; `publish = false` in the manifest, and it is reached
by git. The published `bite-gp-parley` on crates.io is this crate's un-cached
predecessor, from before the changes listed above.
