# gpui_parley

A second implementation of GPUI's `TextSystem`, built on
[Parley](https://github.com/linebender/parley).

`gpui_engine::TextSystem` is the trait the facade swaps: an application picks its
shaping, layout and rasterization by installing one, and `gpui_parley` is the one
that is not the default. It exists because the SPI's surface — a line, a run, a
glyph — cannot express what Parley can do, so the crate offers those features
through an extension trait while still implementing the SPI for everything else.

## Using it

The crate is not on crates.io yet, so today it is a git dependency:

```toml
[dependencies]
gpui_parley = { git = "https://github.com/bite-gpui/gpui_parley" }
```

Once a release has been published, a version requirement is the way — see
[Publishing](#publishing) below.

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

## Publishing

The tag **is** the release. `.github/workflows/release.yml` publishes to crates.io,
and it is the publisher that used to live in `bite-gpui/distribution` — where this
crate was one of thirty-one published from a staged checkout of a branch. The part
that moved is the part about uploading safely: a tag that has to agree with the
manifest, a dry run by default, an environment approval before the upload, and the
registry token checked before anything is built. The part that did not move is the
machinery for publishing thirty-one crates in dependency order, which one crate
does not need.

```sh
# what will be released, without publishing it
gh workflow run release.yml -f tag=v0.1.0

# the release itself: push the tag, then approve the environment
git tag v0.1.0 && git push origin v0.1.0
```

Rules the workflow enforces, all of them because a crates.io version is immutable:

- **The tag must name the manifest's version.** `v0.2.0` releases `0.2.0` and
  refuses to release anything else, so a tag cannot publish a commit whose version
  it does not name. Bumping the crate is a manifest edit and a tag, and the
  workflow is what checks the two agree.
- **A dispatch is a dry run unless it says otherwise,** and a real publish has to
  repeat the tag to confirm.
- **A real publish waits on the `crates-io` environment**, which is where the
  token lives and where required reviewers belong. A tag push starts the release;
  a reviewer is what makes it an upload.

Two things have to exist before the first release, and neither is in this
repository:

1. **A `crates-io` environment** on this repository with required reviewers, and
   `CARGO_REGISTRY_TOKEN` as a secret on it. The layer stack's token is scoped to
   the crate name pattern `bite_*`, which does not cover `gpui_parley` — an upload
   with it is refused as `403 Forbidden: this token does not have the required
   permissions to perform this action`. The token has to cover this name.
2. **A version to release.** `0.1.0` is what the manifest says now. The scheme is
   this crate's own; the layer stack's `1.21.0` numbering came from the upstream
   Zed release a target retargeted, which is not a thing this repository has.

The published `bite-gp-parley` on crates.io is this crate's predecessor and stays
where it is: versions `1.20.203` and `1.21.0`, published from the layer stack, both
without the changes listed above. Publishing from here does not continue that name
— it starts this one.
