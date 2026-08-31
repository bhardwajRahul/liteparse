# Attribution

This crate is a vendored subset of **[dxpdf](https://github.com/nerdy-pro/dxpdf)
v0.4.0** by nerdy.pro, used under the MIT License (see `LICENSE`).

Upstream is a DOCX→PDF engine (`parse → resolve → layout → subset → paint`)
built on Skia. We vendor only the first two stages, which contain no Skia
references, so the result is pure Rust and builds on musl (rust-skia ships no
musl prebuilts, and `skia-safe` on musl falls back to a ~1hr from-source build).

## What was copied verbatim

```
src/docx/                 src/model/
src/field/                src/render/resolve/
src/error.rs
src/render/{dimension,geometry,error}.rs
src/render/layout/draw_command.rs   (referenced by resolve::shape_visuals)
src/render/emoji/cluster.rs         (referenced by draw_command)
```

## What was changed

1. `src/render/emf.rs` — **dropped**. The subset's only genuine `skia_safe`
   import; converts Windows metafiles for the painter, unreachable from the
   structure path.
2. `src/render/fonts.rs` — **replaced with a shim.** Upstream is a Skia
   `FontRegistry`; only `TypefaceEntry` is named here (one field of
   `DrawCommand::Text`). This is the seam for a future harfrust+skrifa port.
3. `src/lib.rs`, `src/render/mod.rs`, `src/render/layout/mod.rs`,
   `src/render/emoji/mod.rs` — **rewritten** to declare only the copied modules.
4. Edition-2024 pattern fixes (`ref mut` in an implicit borrow) in
   `render/resolve/properties.rs` and `docx/parse/rel_rewrite.rs` — upstream
   pins an older toolchain.
5. Unknown-element tolerance — see below.

## Fail-open parsing (the main deliberate divergence)

**Upstream fails closed at every level. We fail open.** A single malformed
value anywhere in a file must never cost the whole document — that is a
standing requirement of this copy, not a one-off fix. Three distinct classes,
all of which upstream treats as fatal:

| class | upstream | here |
|---|---|---|
| **unknown elements** (`commentReference`, …) | aborts document | `#[serde(other)]` catch-all, element skipped |
| **unknown attribute *values*** (`w:jc val="bogus"`) | aborts document | `lenient::` deserializers → unspecified |
| **malformed scalars** (colours, measurements, ids) | aborts document | dropped / spec default |

The mechanism lives in `docx/parse/primitives/lenient.rs`:

- `opt_attr` — `Option<T>` attribute → `None`. ECMA-376 §17.17 says an invalid
  value is treated as absent, and absent means *inherit from the style chain*,
  so this is both the spec-correct and the least-destructive degradation.
- `opt_val_attr` — same for `<w:x w:val="…"/>` elements; collapses the
  `ValAttr<T>` wrapper so the field is plain `Option<T>`.
- `or_default` — required attributes whose type has a *spec* default.
- `nonneg_or_default` — required non-negative measurements → zero, keeping the
  non-negativity guard.

Two rules that are easy to get wrong and were both hit during the port:

1. **Never invent a value to keep an infallible `From`.** Where the model has
   no "unspecified" variant (colours, adjust handles, gradient stops) the
   conversion was made fallible and callers drop the item. Substituting black
   or zero would be silent corruption.
2. **`AttrValueDeserializer` must coerce, not just visit strings.** serde's own
   string deserializer refuses to produce an integer, so routing numeric
   attributes through it would silently turn every *valid* `gridSpan`, `numId`,
   `ilvl` and `outlineLvl` into `None` — corrupting documents rather than
   merely being strict. Its unit tests guard exactly this.

Upstream tests that asserted the strict behaviour were rewritten to assert the
new contract *plus* the guarantee they originally protected (e.g. `"1.0"` is
dropped as a numbering id, but must still never be coerced to `1`).

### Verifying

```
cargo test -p liteparse-docx
cargo build -p liteparse-docx --example parse_probe
python3 bench/docx_native_spike/fuzz_attribute_values.py --mode all
```

The fuzz harness corrupts every attribute value in the `docx_files` corpus
(623k values) and requires all 48 documents to still parse.

## How much of this copy is actually used (measured 2026-08-05)

The headline "37k LOC" overstates it, and the answer changes once layout lands,
so this is a snapshot with a reproducible recipe — not a standing fact.

| | |
|---|---|
| total | 38,098 |
| dxpdf's own `#[cfg(test)]` blocks | 13,643 |
| **production LOC** | **24,455** |

**Statically reachable** from the two entry points this crate exists to provide
(`docx::parse`, `render::resolve::resolve`): 156 items are not, and they form
two islands — `field/` (2,340 L), which nothing outside `field/` references at
all, and the `layout/draw_command.rs` cluster (+ `resolve::shape_geometry`,
`shape_visuals`, `drawing_color`, `emoji::cluster`; 3,960 L), which is reachable
only from itself. Note `dead_code` cannot see the rest: a serde schema struct
counts as "used" merely by being deserialized into, whether or not any field is
ever read.

**Actually executed** over the 48-doc `docx_files` corpus — 47.3% of
instrumented lines. The core parse path is genuinely exercised
(`docx/parse/*` 86.6%, `properties` 88.6%); `render/resolve` is 24.7%,
`docx/parse/drawing` 36.4%, `field/` 16.3%, and these are flat zero:
`emoji`, `render/{geometry,dimension}`, `layout/draw_command`,
`resolve/{shape_geometry,shape_visuals,drawing_color,conditional,locale,images,header_footer,color}`.

Three buckets, and only the first is a trim candidate:

1. **Painting-only, dead for us permanently (~4-6k)** — preset shape geometry,
   shape fills/strokes/effects, DrawingML colour transforms, emoji clusters,
   VML path commands/formulas.
2. **Dead today, live under C′ (~6k)** — `draw_command` *is* the geometry tap
   (`Text{position}` / `LinkAnnotation` / `Image`), `render/{dimension,geometry}`
   are its Pt types, and `field/` is how `PAGE`/`REF` become text, which matters
   as soon as headers/footers paginate.
3. **Low coverage but load-bearing** — `parse/drawing` and `parse/vml` schemas
   are what make the parser fail *open* on drawings, and are the raw material
   for the two known content gaps (textbox content, image rects).

**Decision: do not trim.** The trimmable set is ~4-6k of 24k, and deleting it
forfeits the clean re-sync this file exists to protect. Re-measure after the
layout vendor, when bucket 2 is a measured fact rather than a projection.

Recipe: `bench/docx_vendor_coverage.sh`.

Keeping this list current matters: it is what makes a future re-sync against a
newer dxpdf tractable.
