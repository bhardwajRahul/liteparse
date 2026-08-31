//! Per-run PPTX geometry: `p:txBody` → measured lines → [`TextItem`]s.
//!
//! [`super::pptx`] needs only shape rectangles, and PPTX hands those over
//! directly — `<a:off>`/`<a:ext>` are absolute EMU and `pptx::geometry` has
//! already composed group coordinate spaces into `Shape::slide_rect`. **Below
//! the shape there are no coordinates at all.** A paragraph does not say where
//! its lines fall, and a run does not say how wide it is; both are derived by
//! measuring the text and wrapping it inside the rectangle the shape gives it.
//!
//! That derivation is not written here. `render::layout` already does it for
//! DOCX text boxes, and the reusable part reaches further than expected:
//! `stack_blocks` names no `w:` type, and `shape_body::layout_shape_body` —
//! carved out of the DOCX text-box path for this — does insets, anchor and
//! `@vertOverflow` off a plain `a:bodyPr`, which is DrawingML and identical in
//! both formats. So this module is an **adapter**: it turns a `TextParagraph`
//! plus its resolved text style into the `LayoutBlock`s that stack expects, and
//! hands the resulting draw commands to the same `DrawCommand` → `TextItem`
//! converter the DOCX path uses.
//!
//! What is genuinely new here, and why each cannot be assumed:
//!
//! | input | why it cannot be assumed |
//! |---|---|
//! | theme font default | many runs name no font. DrawingML's answer is `+mn-lt`/`+mj-lt`; guessing the host default gives the wrong face *and* the wrong width |
//! | `a:bodyPr` insets | declared on every text shape, and often differ from the spec default |
//! | `a:bodyPr` anchor | frequently not `top` |
//! | `a:normAutofit` | rarely shrinks, but when it does it can go as low as 25% — a body laid out at 4x its intended size is not a subtle error |
//! | shape rotation | a minority of text shapes rotate, some at a right angle. The DOCX path hardcodes `rotation = 0.0`, which is honest for a page and not here |
//!
//! Rotation is applied here rather than inside the shared converter because
//! `DrawCommand::Text` carries no rotation field — it is emitted pre-shifted but
//! un-rotated, and the DOCX painter rotates the whole shape instead. Adding one
//! would touch an enum that is matched exhaustively across the vendored crate by
//! design. Each shape is therefore converted on its own and its items are
//! rotated about the shape centre on the way onto the slide.
//!
//! Reading order and the text cascade both come from [`super::pptx`], through
//! `Deck::prepare`, so a `TextItem` is a box for exactly the text the markdown
//! emitter saw, resolved the same way.
//!
//! # Two walks, one prepare
//!
//! Text is emitted in reading order; **fills and outlines are emitted in
//! document order**, by [`paint_shapes`], because §19.3.1.45 makes document
//! order z-order and reading order sequences a significant fraction of
//! overlapping fills the wrong way round. The two walks share one
//! `Deck::prepare`, so they cannot disagree about a rectangle or a cascade;
//! what they deliberately do not share is the chrome filter, the traversal
//! order, and the placement mechanism — a text body is bracketed, a path
//! places itself.

use liteparse_ooxml::model::dimension::Dimension;
use liteparse_ooxml::model::{
    Alignment, ColorMap, DrawingFill, PresetGeometryDef, PresetShapeType, ShapeGeometry,
    StyleMatrixRef, Theme,
};
use liteparse_ooxml::pptx::{
    self, Group, PresentationPackage, ResolvedTextStyle, Shape, ShapeKind, Spacing, TextBody,
    TextCascade, TextParagraph,
};
use liteparse_ooxml::render::dimension::Pt;
use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::geometry::{PtOffset, PtRect, PtSize};
use liteparse_ooxml::render::layout::ShapeAutoFit;
use liteparse_ooxml::render::layout::draw_command::{
    DrawCommand, LayoutedPage, ResolvedFill, ShapeTransform, TransformMark,
};
use liteparse_ooxml::render::layout::fragment::{
    Fragment, emit_run_fragments, font_props_from_run,
};
use liteparse_ooxml::render::layout::measurer::TextMeasurer;
use liteparse_ooxml::render::layout::paragraph::{LineSpacingRule, ParagraphStyle};
use liteparse_ooxml::render::layout::section::LayoutBlock;
use liteparse_ooxml::render::layout::shape_body::{layout_shape_body, measure_shape_body};
use liteparse_ooxml::render::resolve::color::{RgbColor, rgb_from_u32};
use liteparse_ooxml::render::resolve::drawing_color::{DrawingColorContext, resolve_drawing_color};
use liteparse_ooxml::render::resolve::fonts::resolve_font_set_themes;
use liteparse_ooxml::render::resolve::images::PartMedia;
use liteparse_ooxml::render::resolve::shape_geometry::build_geometry;
use liteparse_ooxml::render::resolve::shape_visuals::{
    resolve_blip_fill, resolve_fill, resolve_shape_visuals,
};

use std::collections::HashMap;

use crate::error::LiteParseError;
use crate::office::docx_layout;
use crate::office::pptx::{self as pptx_emit, Deck, is_title, reading_order};
use crate::types::{Page, TextItem};

/// EMU per point (§20.1.2.1: 914400 EMU/inch ÷ 72 pt/inch).
const EMU_PER_POINT: f32 = 12700.0;

/// §20.1.10.60 `@rot` is in 60000ths of a degree.
const ANGLE_UNITS_PER_DEGREE: f32 = 60000.0;

/// What a deck's geometry pass produced, plus what it could not place.
///
/// The counters are not diagnostics — they are the honest half of the result.
/// Two content classes carry text that this pass does not yet position, and a
/// consumer that saw only `pages` would read their absence as "the slide had no
/// such text" rather than "this pass cannot place it yet".
pub struct SlideGeometry {
    /// One page per slide, in presentation order. Page count always equals
    /// slide count, so a page index is a slide index.
    pub pages: Vec<Page>,
    /// The same slides as draw commands rather than boxes — one page per
    /// slide, parallel to `pages`, ready for `render::raster::rasterize_page`.
    ///
    /// Not a second derivation: each shape contributes the *same*
    /// `layout_shape_body` output that became its [`TextItem`]s, bracketed by
    /// the `DrawCommand::Transform` that places it on the slide. So a
    /// screenshot and the geometry it would highlight come from one layout, by
    /// construction — which is the property the LibreOffice screenshot path
    /// could never offer.
    pub layouts: Vec<LayoutedPage>,
    /// Per page, the shape each run of items came from. Parallel to `pages`.
    ///
    /// Derived geometry has no external oracle — PPTX declares no coordinate
    /// below the shape, so there is nothing to diff a line box against. The
    /// one check that *is* available is that a shape's text lands inside the
    /// box that shape gave it, and that requires knowing which shape each item
    /// came from. Kept on the result rather than recomputed by a probe because
    /// a probe that re-derived the association would be re-deriving the thing
    /// under test.
    pub placements: Vec<Vec<ShapePlacement>>,
    /// Table cells whose text this pass placed.
    pub placed_table_cells: usize,
    /// Table cells carrying text that could not be placed: a frame with no
    /// rectangle, or a table whose `a:tblGrid` gives its columns no width.
    /// The markdown emitter still emits their text, so this is a geometry gap
    /// for those cells rather than a content drop.
    pub unplaced_table_cells: usize,
    /// SmartArt text bodies. The frame has a rect, but the text lives in
    /// `ppt/diagrams/data*.xml` with its own layout algorithm; the markdown
    /// emitter places the *frame* in reading order, which is a weaker claim
    /// than a per-run box.
    pub unplaced_diagram_bodies: usize,
    /// Shapes the paint walk emitted a [`DrawCommand::Path`] for.
    pub painted_shapes: usize,
    /// Shapes that resolve to a fill or a stroke but whose geometry this pass
    /// cannot build — an unimplemented preset, or none declared. Their text is
    /// still laid out; only their ink is missing.
    pub unpainted_shapes: usize,
    /// Painted shapes whose stroke is black because **nothing** named a
    /// colour — not the `<a:ln>`, and not the theme line style its
    /// `p:style/a:lnRef` points at.
    ///
    /// Still the one place this pass paints something *wrong* rather than
    /// painting nothing, but it now means what it says: before `p:style` was
    /// parsed it also counted every outline whose colour was sitting in the
    /// theme, unread.
    pub outlines_defaulted_black: usize,
    /// Shapes carrying a `p:style` that the paint walk consulted.
    pub shapes_with_style: usize,
    /// Shapes whose fill came from the theme's `fillStyleLst` via `a:fillRef`,
    /// because `spPr` declared no fill element at all. Counted after
    /// resolution, so an `idx="0"` reference (the "no reference" sentinel) is
    /// not counted here.
    pub fills_from_style_ref: usize,
    /// Shapes stroked entirely from the theme's `lnStyleLst`, having no
    /// `<a:ln>` of their own. This is ink the pass did not put down before,
    /// which makes it the one counter here that can only grow the raster.
    pub strokes_from_style_ref: usize,
    /// Shapes that ask a style matrix for a fill or an outline — a non-zero
    /// `idx`, with nothing declared locally — and get **nothing back**: the
    /// theme part is missing, or its `fillStyleLst`/`lnStyleLst` is shorter
    /// than the index.
    ///
    /// The honest residue of the two counters above. Without it a theme that
    /// resolves to an empty matrix is indistinguishable from input that never
    /// asked, and the difference is a shape PowerPoint paints and we do not.
    pub style_refs_unresolved: usize,
    /// Per page, the command index of the full-slide background
    /// [`DrawCommand::Path`], when one was emitted. Parallel to `pages`.
    ///
    /// An explicit link for the same reason [`ShapePlacement::bracket`] is
    /// one: the background is a `Path` among the painter's other `Path`s, and
    /// a consumer that wanted to separate "the slide has a backdrop" from "the
    /// shapes put ink on it" would otherwise have to guess by position and
    /// extent. It also makes the z-order claim checkable — a present index
    /// that is not 0 is a background painted over its own slide.
    pub background_commands: Vec<Option<usize>>,
    /// Slides that emitted a full-slide background [`DrawCommand::Path`].
    pub painted_backgrounds: usize,
    /// Slides whose resolved background is deliberately transparent
    /// (`<a:noFill/>`). Not a gap: nothing is the correct paint, and it is
    /// separated from the next two so that "no ink" and "could not make ink"
    /// stay different claims.
    pub transparent_backgrounds: usize,
    /// Slides that declare a background this pass cannot colour: `blip`,
    /// `pattern`, or a `bgRef` into a matrix entry of one of those kinds.
    /// Distinguished from the transparent case on the *declared* arm, because
    /// `resolve_fill` collapses all of them to
    /// [`ResolvedFill::None`] — so the resolved fill alone would report a
    /// missing photograph as a slide that correctly has no backdrop.
    pub unrenderable_backgrounds: usize,
    /// Slides where no part in the slide → layout → master chain declares a
    /// background at all. Normally zero, so any number here is a signal about
    /// the package rather than a normal outcome.
    pub undeclared_backgrounds: usize,
    /// Runs whose colour came from a declared `a:solidFill` on some rung of
    /// the text cascade.
    pub runs_colour_declared: usize,
    /// Runs with no colour anywhere in the cascade, painted black by §21.1.2.3
    /// default. The remaining gap here is `p:style/a:fontRef`, which supplies
    /// a shape-level text colour this pass does not parse.
    pub runs_colour_defaulted: usize,
    /// Shapes painted from a *layout* or *master* part rather than the slide's
    /// own tree — the panels, rules and logo strips a deck authors once and
    /// every slide under that rung inherits.
    ///
    /// Counted apart from `painted_shapes` rather than pooled with it, because
    /// the two answer different questions: this one is per *slide occurrence*,
    /// so one layout panel over 40 slides is 40 here and 1 in its part. Pooling
    /// them would make the slide-level number move whenever a deck happened to
    /// reuse a layout more.
    pub inherited_shapes_painted: usize,
    /// Inherited shapes that resolve to ink but whose geometry cannot be built
    /// — the same class as `unpainted_shapes`, counted separately for the same
    /// reason as above.
    pub inherited_shapes_unpainted: usize,
    /// Occurrences of an inherited shape that **carries text at all**.
    ///
    /// Not a gap, and not a target. Far fewer distinct strings sit under these
    /// occurrences than the count suggests — a layout shape is drawn on every
    /// slide that uses the layout, so one speaker banner authored once lands on
    /// many pages, and many occurrences are the literal `‹#›` of a slide
    /// number. This counts the population; `inherited_text_laid_out` counts the
    /// part of it a reader gets.
    pub inherited_shapes_with_text: usize,
    /// Occurrences of inherited text this pass **did** lay out: the strings
    /// that land on exactly one slide of their deck. See
    /// [`PreparedSlide::inherited_text`] for the rule.
    ///
    /// Counted in the text walk rather than the paint walk, so it is an
    /// occurrence of *emitted* text and not of a shape that happened to paint.
    /// The residue is furniture, and dropping it is the feature.
    ///
    /// [`PreparedSlide::inherited_text`]: crate::office::pptx::PreparedSlide::inherited_text
    pub inherited_text_laid_out: usize,
    /// Slides that decline an inherited rung via `@showMasterSp="0"`. Counted
    /// because "painted nothing here" and "asked to paint nothing here" are
    /// different claims.
    pub slides_declining_layout: usize,
    pub slides_declining_master: usize,
    /// `p:pic` shapes the paint walk reached, on any rung.
    pub pictures_seen: usize,
    /// Pictures whose blip resolves to no image: an empty picture placeholder
    /// (all in layouts and masters — correct, not a gap), an external `r:link`,
    /// or media with no decoder (EMF/WMF references).
    ///
    /// Pooled deliberately with the correct cases, because the *ratio* is the
    /// claim this pass can make honestly; splitting it further would need the
    /// resolver to report a reason, which is its own change.
    pub pictures_unresolved: usize,
    /// Pictures that declare no `prstGeom` and took the schema's implicit
    /// `rect`.
    pub pictures_implicit_rect: usize,
    /// Shapes painted with an image fill — pictures and `spPr` blip fills
    /// together. The number that says the emitter put photographs on slides,
    /// as opposed to merely resolving them.
    pub blips_painted: usize,
    /// Slides whose background is an image this pass paints. Carved out of
    /// `unrenderable_backgrounds`.
    pub blip_backgrounds_painted: usize,
    /// Shapes whose `spPr` fill is `a:grpFill` and which took a real fill from
    /// an enclosing group (§20.1.8.35).
    pub fills_from_group: usize,
    /// Shapes declaring `a:grpFill` where the chain of enclosing groups ends
    /// without one naming a fill. **Not a defect**: a group with `noFill`, or
    /// with no fill element at all, is the file saying "inherit nothing", and
    /// the shape is correctly left unpainted. Counted to keep that case
    /// visible and distinguishable from a lookup that missed.
    pub group_fills_unanswered: usize,
    /// Runs whose resolved colour is **exactly the colour of the slide's own
    /// background**, i.e. text that cannot be read.
    ///
    /// The gate this step exists for. It over-counts by design — a run over an
    /// opaque shape fill is legible whatever the backdrop says — so the number
    /// is a ceiling on invisibility, and the claim it supports is the
    /// direction it moved, not its absolute value.
    pub runs_invisible_on_background: usize,
}

/// Where one text body's items landed on its slide — a shape's, or a single
/// table cell's.
pub struct ShapePlacement {
    /// Which of the two layout paths produced this box.
    ///
    /// Not decoration: a table cell's rectangle is *derived* (grid prefix sums,
    /// grown rows) where a shape's is *declared*, so the two have different
    /// failure modes and a check that pooled them would let a cell-only bug —
    /// a dropped `a:tcPr` anchor, say — hide inside the far larger set of
    /// correct shapes.
    pub kind: PlacementKind,
    /// The box's unrotated rectangle in slide Pt: `(x, y, width, height)`.
    pub rect: (f32, f32, f32, f32),
    /// Counter-clockwise degrees, matching [`TextItem::rotation`].
    pub rotation: f32,
    /// Whether the body's `a:normAutofit` shrinks it (`@fontScale` < 100%).
    pub shrunk: bool,
    /// Half-open range into the page's `text_items`.
    pub items: std::ops::Range<usize>,
    /// Index into the page's `commands` of the `Transform(Begin)` that placed
    /// this box.
    ///
    /// An explicit link, not an ordinal one. The command list also carries the
    /// paint walk's shapes — four in five of which have no text body at all —
    /// so "the *n*th bracket is the *n*th placement" was only ever true by
    /// accident of the text walk running alone, and a check built on it would
    /// silently stop testing anything the moment a second producer appeared.
    pub bracket: usize,
}

/// Which layout path a [`ShapePlacement`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKind {
    /// A `p:sp` text body, in the rectangle the file declares for the shape.
    Shape,
    /// One `a:tc`, in a rectangle derived from the table's grid and rows.
    TableCell,
}

/// Lay every slide out and return one [`Page`] per slide.
///
/// `registry` must be the registry the caller intends to measure with; it is
/// threaded to both the measurer used here and the converter, which re-measures
/// to recover each item's box. Passing two different registries would produce
/// boxes that do not match the layout that placed them.
pub fn slides_to_pages(
    data: &[u8],
    registry: &FontRegistry,
) -> Result<SlideGeometry, LiteParseError> {
    let pkg = pptx::walk(data)
        .map_err(|e| LiteParseError::Conversion(format!("pptx parse failed: {e}")))?;
    Ok(layout_deck(&pkg, registry))
}

fn layout_deck(pkg: &PresentationPackage, registry: &FontRegistry) -> SlideGeometry {
    // Parsed once per *theme part*, not once per deck and not once per slide.
    // A deck-wide theme was tolerable while the theme only supplied fonts; a
    // fill resolves its colour through the theme's colour matrix, and a slide
    // under a second master would then take a real, wrong colour from the
    // first master's palette — which is worse than no colour at all, because
    // nothing about the output says it came from the wrong place.
    let mut themes: HashMap<String, Option<Theme>> = HashMap::new();

    let measurer = TextMeasurer::new(registry);
    let (slide_w, slide_h) = pkg.info.slide_size_pt();
    let page_size = PtSize {
        width: Pt::new(slide_w as f32),
        height: Pt::new(slide_h as f32),
    };

    let mut deck = Deck::new(pkg);
    let mut out = SlideGeometry {
        pages: Vec::with_capacity(pkg.slides.len()),
        layouts: Vec::with_capacity(pkg.slides.len()),
        placements: Vec::with_capacity(pkg.slides.len()),
        placed_table_cells: 0,
        unplaced_table_cells: 0,
        unplaced_diagram_bodies: 0,
        painted_shapes: 0,
        unpainted_shapes: 0,
        outlines_defaulted_black: 0,
        shapes_with_style: 0,
        fills_from_style_ref: 0,
        strokes_from_style_ref: 0,
        style_refs_unresolved: 0,
        inherited_shapes_painted: 0,
        inherited_shapes_unpainted: 0,
        inherited_shapes_with_text: 0,
        inherited_text_laid_out: 0,
        slides_declining_layout: 0,
        slides_declining_master: 0,
        background_commands: Vec::with_capacity(pkg.slides.len()),
        painted_backgrounds: 0,
        transparent_backgrounds: 0,
        unrenderable_backgrounds: 0,
        undeclared_backgrounds: 0,
        runs_colour_declared: 0,
        runs_colour_defaulted: 0,
        runs_invisible_on_background: 0,
        pictures_seen: 0,
        pictures_unresolved: 0,
        pictures_implicit_rect: 0,
        blips_painted: 0,
        blip_backgrounds_painted: 0,
        fills_from_group: 0,
        group_fills_unanswered: 0,
    };

    for (idx, slide) in pkg.slides.iter().enumerate() {
        let mut items = Vec::new();
        let mut placements = Vec::new();
        let mut commands = Vec::new();
        let mut background_at = None;
        let theme = slide_theme(pkg, slide, &mut themes);
        // A slide whose shape tree will not parse still yields a page. Page
        // count must equal slide count — a consumer indexes pages by slide.
        if let Some(prepared) = deck.prepare(pkg, slide) {
            let cascade = prepared.cascade();
            let mut ctx = ShapeCtx {
                cascade,
                theme,
                color_map: prepared.color_map,
                measurer: &measurer,
                registry,
                items: &mut items,
                commands: &mut commands,
                placements: &mut placements,
                placed_table_cells: &mut out.placed_table_cells,
                unplaced_table_cells: &mut out.unplaced_table_cells,
                unplaced_diagram_bodies: &mut out.unplaced_diagram_bodies,
                painted_shapes: &mut out.painted_shapes,
                unpainted_shapes: &mut out.unpainted_shapes,
                outlines_defaulted_black: &mut out.outlines_defaulted_black,
                shapes_with_style: &mut out.shapes_with_style,
                fills_from_style_ref: &mut out.fills_from_style_ref,
                strokes_from_style_ref: &mut out.strokes_from_style_ref,
                style_refs_unresolved: &mut out.style_refs_unresolved,
                inherited_shapes_painted: &mut out.inherited_shapes_painted,
                inherited_shapes_unpainted: &mut out.inherited_shapes_unpainted,
                inherited_shapes_with_text: &mut out.inherited_shapes_with_text,
                inherited_text_laid_out: &mut out.inherited_text_laid_out,
                painted_backgrounds: &mut out.painted_backgrounds,
                transparent_backgrounds: &mut out.transparent_backgrounds,
                unrenderable_backgrounds: &mut out.unrenderable_backgrounds,
                undeclared_backgrounds: &mut out.undeclared_backgrounds,
                background_rgb: None,
                runs_colour_declared: &mut out.runs_colour_declared,
                runs_colour_defaulted: &mut out.runs_colour_defaulted,
                runs_invisible_on_background: &mut out.runs_invisible_on_background,
                pictures_seen: &mut out.pictures_seen,
                pictures_unresolved: &mut out.pictures_unresolved,
                pictures_implicit_rect: &mut out.pictures_implicit_rect,
                blips_painted: &mut out.blips_painted,
                blip_backgrounds_painted: &mut out.blip_backgrounds_painted,
                fills_from_group: &mut out.fills_from_group,
                group_fills_unanswered: &mut out.group_fills_unanswered,
            };
            // Two walks over one `prepare`, in the order the raster wants
            // them. See [`paint_shape`] for why they cannot be one walk. The
            // background goes first because the command list *is* the z-order.
            background_at = paint_background(
                prepared.background.as_ref(),
                prepared.background_media,
                page_size,
                &mut ctx,
            );
            // ...and so is this: master under layout under slide. §19.3.1.39
            // builds the page by drawing each rung over the one it inherits
            // from, which is why an inherited panel is a backdrop for the
            // slide's own shapes rather than a lid over them.
            for (rung, media) in prepared.inherited.into_iter().zip(prepared.inherited_media) {
                paint_shapes(rung, Source::inherited(media), None, &mut ctx);
            }
            paint_shapes(
                &prepared.shapes,
                Source::slide(prepared.media),
                None,
                &mut ctx,
            );
            // One order over the slide's own shapes *and* the inherited text
            // the furniture rule kept — the same list the markdown emitter
            // walks, from the same `prepare`. A `TextItem` is a faithful box
            // only for markdown a reader actually gets, so the two walks take
            // the same traversal or the claim is not true.
            for read in pptx_emit::slide_reading_order(&prepared) {
                // `!figure`: the same list also carries the rungs' pictures,
                // which this counter does not name and must not absorb.
                if read.rung.is_some() && !read.figure {
                    *ctx.inherited_text_laid_out += 1;
                }
                layout_shape(read.shape, &mut ctx);
            }
            out.slides_declining_master += usize::from(prepared.declined[0]);
            out.slides_declining_layout += usize::from(prepared.declined[1]);
        }

        out.placements.push(placements);
        out.background_commands.push(background_at);
        out.layouts.push(LayoutedPage {
            commands,
            page_size,
            // DOCX-only side-channels: `block_starts` indexes the flattened
            // body blocks a page starts (a slide is one page with no such
            // flattening), and slides have no OOXML headers/footers.
            block_starts: Vec::new(),
            header_blocks: Vec::new(),
            footer_blocks: Vec::new(),
        });
        let content_bounds = union_bounds(&items);
        out.pages.push(Page {
            page_number: idx + 1,
            page_width: page_size.width.raw(),
            page_height: page_size.height.raw(),
            content_bounds,
            text_items: items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations: None,
            form_fields: None,
            structure_tree: None,
        });
    }

    out
}

struct ShapeCtx<'a, 'r> {
    cascade: TextCascade<'a>,
    theme: Option<&'a Theme>,
    /// §19.3.1.6 the slide's effective colour map, resolved by
    /// [`Deck::prepare`]. Every scheme colour on the slide — run, shape fill,
    /// outline, background — goes through this one value, which is the point:
    /// the three paths must not disagree about what `tx1` names.
    color_map: Option<ColorMap>,
    measurer: &'a TextMeasurer<'r>,
    registry: &'a FontRegistry,
    items: &'a mut Vec<TextItem>,
    /// The slide's draw commands, in slide coordinates once each shape's run
    /// is read through the bracket that opens it.
    commands: &'a mut Vec<DrawCommand>,
    placements: &'a mut Vec<ShapePlacement>,
    placed_table_cells: &'a mut usize,
    unplaced_table_cells: &'a mut usize,
    unplaced_diagram_bodies: &'a mut usize,
    painted_shapes: &'a mut usize,
    unpainted_shapes: &'a mut usize,
    outlines_defaulted_black: &'a mut usize,
    shapes_with_style: &'a mut usize,
    fills_from_style_ref: &'a mut usize,
    strokes_from_style_ref: &'a mut usize,
    style_refs_unresolved: &'a mut usize,
    inherited_shapes_painted: &'a mut usize,
    inherited_shapes_unpainted: &'a mut usize,
    inherited_shapes_with_text: &'a mut usize,
    inherited_text_laid_out: &'a mut usize,
    painted_backgrounds: &'a mut usize,
    transparent_backgrounds: &'a mut usize,
    unrenderable_backgrounds: &'a mut usize,
    undeclared_backgrounds: &'a mut usize,
    /// The slide backdrop's resolved sRGB, filled in by `paint_background`
    /// before either walk runs. Only the invisible-text count reads it.
    background_rgb: Option<u32>,
    runs_colour_declared: &'a mut usize,
    runs_colour_defaulted: &'a mut usize,
    runs_invisible_on_background: &'a mut usize,
    pictures_seen: &'a mut usize,
    pictures_unresolved: &'a mut usize,
    pictures_implicit_rect: &'a mut usize,
    blips_painted: &'a mut usize,
    blip_backgrounds_painted: &'a mut usize,
    fills_from_group: &'a mut usize,
    group_fills_unanswered: &'a mut usize,
}

/// Which part a painted shape came from.
///
/// Threaded as an argument rather than kept as a flag on [`ShapeCtx`], because
/// a flag would have to be set and cleared around each walk and the failure
/// mode of forgetting is a tally that silently attributes a layout's panels to
/// the slide.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rung {
    /// The slide's own `p:spTree`.
    Slide,
    /// A layout's or master's, drawn under it.
    Inherited,
}

/// The part a paint walk is reading, and the media that part can reach.
///
/// The two travel together because they are answers about the *same* part, and
/// separating them is how an inherited picture ends up resolved against the
/// slide's relationship table — a real image from the wrong `rId1`, which no
/// count in the probe could distinguish from the right one.
#[derive(Clone, Copy)]
struct Source<'a> {
    rung: Rung,
    media: &'a PartMedia,
}

impl<'a> Source<'a> {
    fn slide(media: &'a PartMedia) -> Self {
        Self {
            rung: Rung::Slide,
            media,
        }
    }

    fn inherited(media: &'a PartMedia) -> Self {
        Self {
            rung: Rung::Inherited,
            media,
        }
    }
}

impl ShapeCtx<'_, '_> {
    /// Record one shape's outcome against the rung it came from.
    fn tally(&mut self, rung: Rung, painted: bool) {
        let counter = match (rung, painted) {
            (Rung::Slide, true) => &mut *self.painted_shapes,
            (Rung::Slide, false) => &mut *self.unpainted_shapes,
            (Rung::Inherited, true) => &mut *self.inherited_shapes_painted,
            (Rung::Inherited, false) => &mut *self.inherited_shapes_unpainted,
        };
        *counter += 1;
    }

    /// Add one body's commands to the slide, wrapped in the bracket that
    /// places them.
    ///
    /// `commands` stay in the body-local space `layout_shape_body` emitted
    /// them in — the same values that go on to become [`TextItem`]s — so the
    /// bracket is the *only* place a slide coordinate is introduced, and the
    /// two consumers cannot drift apart by one of them shifting and the other
    /// not.
    /// Returns the index of the `Begin` mark, which is the placement's link
    /// back into the command list.
    fn push_bracketed(&mut self, placement: ShapeTransform, commands: Vec<DrawCommand>) -> usize {
        let at = self.commands.len();
        self.commands
            .push(DrawCommand::Transform(TransformMark::Begin(placement)));
        self.commands.extend(commands);
        self.commands
            .push(DrawCommand::Transform(TransformMark::End));
        at
    }
}

/// The slide's background, as a full-slide [`DrawCommand::Path`] at the head
/// of the command list.
///
/// **First, and unconditionally first.** §19.3.1.1 puts `p:bg` before
/// `p:spTree` in `p:cSld` for the same reason this call precedes
/// [`paint_shapes`]: the command list is the z-order, and a background emitted
/// anywhere else paints over the slide it is supposed to sit under.
///
/// The rectangle is the *page*, not any shape's box. A background is not a
/// shape — it has no `a:xfrm`, no rotation and no geometry of its own — so it
/// is built as a `rect` preset at the slide's full extent, through the same
/// [`build_geometry`] every shape uses rather than by hand-rolling four verbs.
///
/// The cascade and the colour lookup are both already shared and neither is
/// re-derived here: [`Deck::prepare`] resolves which of slide/layout/master
/// wins, and [`pptx::background_fill`] is the single place that knows a
/// `bgRef@idx` of 1001 selects `bgFillStyleLst[0]` and not `fillStyleLst`.
/// `ctx.theme` is this slide's own master's theme, which is what makes a slide
/// resolving a `bgRef` under a second master come out in its own palette.
///
/// [`Deck::prepare`]: crate::office::pptx::Deck::prepare
fn paint_background(
    background: Option<&(pptx::BackgroundSource, pptx::Background)>,
    media: &PartMedia,
    page_size: PtSize,
    ctx: &mut ShapeCtx<'_, '_>,
) -> Option<usize> {
    let Some((_, bg)) = background else {
        *ctx.undeclared_backgrounds += 1;
        return None;
    };
    let fill = pptx::background_fill(bg, ctx.theme, ctx.color_map, Some(media));
    if matches!(fill, ResolvedFill::None) {
        // Two very different slides land here, and the resolved fill cannot
        // tell them apart: `resolve_fill` collapses blip, pattern and `grpFill`
        // to `None` alongside a genuine `<a:noFill/>`. Splitting them on the
        // *declared* arm is the difference between "this slide has no
        // backdrop, correctly" and "this slide's photograph backdrop is
        // missing" — reporting the second as the first is exactly the
        // silent-wrongness this pass counts its way out of.
        if matches!(bg, pptx::Background::Properties(DrawingFill::None)) {
            *ctx.transparent_backgrounds += 1;
        } else {
            *ctx.unrenderable_backgrounds += 1;
        }
        return None;
    }
    // The only way a `rect` fails to build is a zero-extent page, which has no
    // slide to be the backdrop of. Uncounted for that reason.
    let path = build_geometry(
        &ShapeGeometry::Preset(PresetGeometryDef {
            preset: PresetShapeType::Rect,
            adjust_values: Vec::new(),
        }),
        page_size,
    )?;
    *ctx.painted_backgrounds += 1;
    if matches!(fill, ResolvedFill::Blip(_)) {
        *ctx.blip_backgrounds_painted += 1;
    }
    if let ResolvedFill::Solid(rgba) = fill {
        ctx.background_rgb = Some(rgba.to_rgb24());
    }
    let at = ctx.commands.len();
    ctx.commands.push(DrawCommand::Path {
        origin: PtOffset::new(Pt::ZERO, Pt::ZERO),
        rotation: Dimension::ZERO,
        flip_h: false,
        flip_v: false,
        extent: page_size,
        paths: path.paths,
        fill,
        // A background has no `a:ln`. The `rect` preset marks its subpath
        // stroked, so passing anything here would outline the whole slide.
        stroke: None,
        effects: Vec::new(),
    });
    Some(at)
}

/// Paint every shape on the slide, in document order.
///
/// **A second traversal, and it has to be.** The text walk uses
/// [`reading_order`], which sorts shapes into bands and drops chrome; both
/// halves are right for reading and wrong for painting. §19.3.1.45 makes
/// document order z-order, and where two fills overlap reading order sequences
/// a meaningful share of them the wrong way round. A backdrop panel authored
/// last and read first would paint over the content it belongs under.
///
/// The two walks share one `Deck::prepare`, so the rectangles and the cascade
/// they see are the same ones by construction. What is *not* shared is the
/// chrome filter: this walk paints chrome, the text walk still drops it, and
/// chrome shapes that carry text are a divergence this step leaves open rather
/// than closes. `group_fill` is the fill the *enclosing* group offers its
/// members, already resolved and already chain-collapsed — see
/// [`group_fill_for`].
fn paint_shapes(
    shapes: &[Shape],
    from: Source<'_>,
    group_fill: Option<&ResolvedFill>,
    ctx: &mut ShapeCtx<'_, '_>,
) {
    for shape in shapes {
        paint_shape(shape, from, group_fill, ctx);
        if let ShapeKind::Group(group) = &shape.kind {
            // A group puts no ink down itself: §19.3.1.23 gives it no
            // geometry, so its `grpSpPr` fill exists only as the thing its
            // members' `a:grpFill` names. Its children carry every fill, in
            // their own declaration order.
            let inherited = group_fill_for(group, group_fill, from, ctx);
            paint_shapes(&group.children, from, inherited.as_ref(), ctx);
        }
    }
}

/// What this group offers its members, given what its own parent offered it.
///
/// Collapses the chain here rather than at the point of use so that a
/// `DrawingFill::Group` reaching the resolver is always answerable in one
/// step. Three outcomes, and only the first paints:
///
/// * the group names a fill → that, resolved in this part's colour context
/// * the group's own fill is `a:grpFill` → whatever its parent offered,
///   passed straight through (the chain is load-bearing: nested groups carry
///   many shapes between them)
/// * the group names `a:noFill`, or no fill at all → `None`. The spec's
///   "inherit nothing"; a member is then correctly unpainted.
fn group_fill_for(
    group: &Group,
    parent: Option<&ResolvedFill>,
    from: Source<'_>,
    ctx: &ShapeCtx<'_, '_>,
) -> Option<ResolvedFill> {
    match group.fill.as_ref()? {
        DrawingFill::Group => parent.cloned(),
        fill => {
            let color_ctx = DrawingColorContext::new(ctx.theme).with_color_map(ctx.color_map);
            match resolve_fill(fill, &color_ctx, Some(from.media), parent) {
                ResolvedFill::None => None,
                resolved => Some(resolved),
            }
        }
    }
}

/// One shape's fill and outline as a [`DrawCommand::Path`], if it puts ink on
/// the slide.
///
/// Unlike a text body this needs no bracket: `Path` carries its own
/// origin/rotation/flip/extent and the painter composes them, so the shape is
/// self-placing. That also lets it carry the two flips, which a text bracket
/// deliberately does not — §20.1.7.6 mirrors a shape's *geometry*, and
/// PowerPoint does not mirror the text inside it.
fn paint_shape(
    shape: &Shape,
    from: Source<'_>,
    group_fill: Option<&ResolvedFill>,
    ctx: &mut ShapeCtx<'_, '_>,
) {
    let Some(props) = shape_properties(shape) else {
        return;
    };
    // Counted before the paint decision, not after: the text is missing
    // whether or not the shape also puts ink down, and a gap that only reported
    // itself on shapes that happened to have a fill would under-report exactly
    // where it matters.
    if from.rung == Rung::Inherited && shape_text_len(shape) > 0 {
        *ctx.inherited_shapes_with_text += 1;
    }
    // §19.3.1.46 `p:style` — the theme-matrix references this shape inherits
    // its fill and outline from when `spPr` declares none of its own.
    // `a:fontRef` is deliberately not consulted here, because a run colour
    // resolved in the paint walk and not in the text cascade would make the two
    // disagree.
    let style = shape.style.as_ref();
    let mut visuals = resolve_shape_visuals(
        Some(props),
        style.and_then(|s| s.line_ref.as_ref()),
        style.and_then(|s| s.effect_ref.as_ref()),
        style.and_then(|s| s.fill_ref.as_ref()),
        &DrawingColorContext::new(ctx.theme).with_color_map(ctx.color_map),
        Some(from.media),
        group_fill,
    );
    // §20.1.8.35 — tallied here, not after the paint gate: a `grpFill` that
    // resolved and then failed to build is a geometry gap, and folding it in
    // with the shapes that inherited nothing would blame the wrong step.
    if matches!(props.fill, Some(DrawingFill::Group)) {
        if matches!(visuals.fill, ResolvedFill::None) {
            *ctx.group_fills_unanswered += 1;
        } else {
            *ctx.fills_from_group += 1;
        }
    }
    // Where the ink came from, read *before* the picture override below
    // replaces the fill: a `p:pic` whose frame inherits a theme fill still
    // inherited it, and the photograph that lands on top is a different fact.
    // Both are tallied only once the shape is known to paint, so they stay
    // subsets of `painted_shapes`.
    let fill_from_ref = props.fill.is_none() && !matches!(visuals.fill, ResolvedFill::None);
    let stroke_from_ref = props.outline.is_none() && visuals.stroke.is_some();
    // A reference that named a matrix entry and came back empty. `idx == 0` is
    // the spec's "no reference" sentinel and is not a miss.
    let asked = |r: Option<&StyleMatrixRef>| r.is_some_and(|r| r.idx != 0);
    let missed_fill = props.fill.is_none()
        && asked(style.and_then(|s| s.fill_ref.as_ref()))
        && matches!(visuals.fill, ResolvedFill::None);
    let missed_stroke = props.outline.is_none()
        && asked(style.and_then(|s| s.line_ref.as_ref()))
        && visuals.stroke.is_none();
    if style.is_some() {
        *ctx.shapes_with_style += 1;
    }
    // Counted here rather than beside the two "inherited" tallies below: a
    // shape whose only ink was the reference that missed does not paint at
    // all, and a miss that reported itself only on shapes that painted anyway
    // would hide exactly the case it exists to expose.
    if missed_fill || missed_stroke {
        *ctx.style_refs_unresolved += 1;
    }
    // §19.3.1.37: a `p:pic`'s image is its own `p:blipFill`, a sibling of
    // `spPr` — not an `spPr` fill element. So the picture's fill has to be
    // resolved separately and take precedence; reading `shape_properties`
    // alone finds the frame's *background* (usually absent) and never the
    // photograph.
    if let ShapeKind::Picture(pic) = &shape.kind {
        *ctx.pictures_seen += 1;
        let blip = resolve_blip_fill(&pic.blip_fill, Some(from.media));
        if matches!(blip, ResolvedFill::None) {
            // An empty picture placeholder, an `r:link`, or media we cannot
            // decode (EMF/WMF). Counted rather than silently falling back to
            // the frame's own fill.
            *ctx.pictures_unresolved += 1;
        } else {
            visuals.fill = blip;
        }
    }
    let paints = !matches!(visuals.fill, ResolvedFill::None) || visuals.stroke.is_some();
    if !paints {
        return;
    }

    let Some(slide_rect) = shape.slide_rect else {
        ctx.tally(from.rung, false);
        return;
    };
    // The *bounding* box, not the raw rect: `pptx::geometry` composes group
    // transforms into `slide_rect`, and a rotated or skewed child's ink lives
    // in the box that composition produced.
    let box_ = slide_rect.bounding_box();
    let extent = PtSize {
        width: emu_to_pt(box_.size.width.raw()),
        height: emu_to_pt(box_.size.height.raw()),
    };
    // §19.3.1.37: a picture frame with no `prstGeom` is a rectangle — the
    // image fills its box.
    //
    // This is **not** the "never approximate an unbuildable preset by its
    // bounding box" rule being relaxed. That rule is about a `cloud` we cannot
    // build, where any substitute is a guess at a shape the file named. Here
    // the file names no shape at all, and the schema supplies the default.
    let implicit_rect;
    let geometry = match props.geometry.as_ref() {
        Some(g) => g,
        None if matches!(shape.kind, ShapeKind::Picture(_)) => {
            implicit_rect = ShapeGeometry::Preset(PresetGeometryDef {
                preset: PresetShapeType::Rect,
                adjust_values: Vec::new(),
            });
            *ctx.pictures_implicit_rect += 1;
            &implicit_rect
        }
        None => {
            ctx.tally(from.rung, false);
            return;
        }
    };
    let Some(path) = build_geometry(geometry, extent) else {
        // An unimplemented preset. Counted, never approximated by its bounding
        // box: a `rect` drawn where a `cloud` was asked for is a wrong slide,
        // not a coarse one.
        ctx.tally(from.rung, false);
        return;
    };

    // Asked of the resolver rather than re-derived from `props`: whether a
    // colourless `<a:ln>` is a defect now depends on what the `lnRef` found in
    // the theme, and only the resolver did that lookup.
    if visuals.stroke_color_defaulted {
        *ctx.outlines_defaulted_black += 1;
    }
    if fill_from_ref {
        *ctx.fills_from_style_ref += 1;
    }
    if stroke_from_ref {
        *ctx.strokes_from_style_ref += 1;
    }
    ctx.tally(from.rung, true);
    if matches!(visuals.fill, ResolvedFill::Blip(_)) {
        *ctx.blips_painted += 1;
    }
    ctx.commands.push(DrawCommand::Path {
        origin: PtOffset::new(
            emu_to_pt(box_.origin.x.raw()),
            emu_to_pt(box_.origin.y.raw()),
        ),
        rotation: slide_rect.rotation,
        flip_h: slide_rect.flip_h,
        flip_v: slide_rect.flip_v,
        extent,
        paths: path.paths,
        fill: visuals.fill,
        stroke: visuals.stroke,
        effects: visuals.effects,
    });
}

/// How much text an inherited shape carries that this pass will not lay out.
///
/// Only `p:sp` bodies are counted. A `p:graphicFrame`'s table is a separate,
/// rarer gap; folding the two would make the number harder to act on, not more
/// complete.
fn shape_text_len(shape: &Shape) -> usize {
    let ShapeKind::AutoShape(sp) = &shape.kind else {
        return 0;
    };
    sp.text.as_ref().map_or(0, |body| {
        body.paragraphs.iter().map(|p| p.text().len()).sum()
    })
}

/// The `spPr` a shape paints from. Groups and graphic frames have none of
/// their own — a table's cell fills are `a:tcPr`, which this pass does not
/// paint yet.
fn shape_properties(shape: &Shape) -> Option<&liteparse_ooxml::model::ShapeProperties> {
    match &shape.kind {
        ShapeKind::AutoShape(sp) => sp.properties.as_ref(),
        ShapeKind::Connector(c) => c.properties.as_ref(),
        ShapeKind::Picture(p) => p.shape_properties.as_ref(),
        ShapeKind::Group(_) | ShapeKind::GraphicFrame(_) => None,
    }
}

/// The theme governing one slide, parsed once per theme *part*.
fn slide_theme<'a>(
    pkg: &PresentationPackage,
    slide: &pptx::SlideParts,
    cache: &'a mut HashMap<String, Option<Theme>>,
) -> Option<&'a Theme> {
    let path = slide.theme_path.clone()?;
    cache
        .entry(path)
        .or_insert_with(|| {
            pkg.theme_for(slide)
                .and_then(|bytes| liteparse_ooxml::docx::parse::theme::parse_theme(bytes).ok())
        })
        .as_ref()
}

fn layout_shape(shape: &Shape, ctx: &mut ShapeCtx<'_, '_>) {
    match &shape.kind {
        ShapeKind::AutoShape(sp) => {
            if let Some(body) = &sp.text {
                layout_text_shape(shape, body, ctx);
            }
        }
        ShapeKind::Group(group) => {
            // Children carry composed `slide_rect`s already, so a group needs
            // no coordinate work here — only the same reading order the
            // markdown emitter applies within it.
            for child in reading_order(&group.children) {
                layout_shape(child, ctx);
            }
        }
        ShapeKind::GraphicFrame(frame) => match &frame.payload {
            pptx::GraphicFramePayload::Table(table) => layout_table(shape, table, ctx),
            pptx::GraphicFramePayload::Diagram { .. } => *ctx.unplaced_diagram_bodies += 1,
            pptx::GraphicFramePayload::Unsupported { .. } => {}
        },
        ShapeKind::Picture(_) | ShapeKind::Connector(_) => {}
    }
}

/// Lay one shape's text body out inside its rectangle and append the resulting
/// items, translated and rotated onto the slide.
fn layout_text_shape(shape: &Shape, body: &TextBody, ctx: &mut ShapeCtx<'_, '_>) {
    // A shape with no rectangle cannot be laid out at all: there is nothing to
    // wrap inside. The markdown emitter still emits such a shape's text — so
    // this is a geometry gap for that shape, not a content drop.
    let Some(slide_rect) = shape.slide_rect else {
        return;
    };
    let extent = PtSize {
        width: emu_to_pt(slide_rect.rect.size.width.raw()),
        height: emu_to_pt(slide_rect.rect.size.height.raw()),
    };
    if extent.width <= Pt::ZERO || extent.height <= Pt::ZERO {
        return;
    }
    // §20.1.9.18: the shape's box is not its *text* box. Every preset carries
    // an `<a:rect>` naming where its text goes, and for anything that is not a
    // plain rectangle that rectangle is inset — a `roundRect` by the sagitta of
    // its corner arc, a `cloud` by a third of its width in each direction. The
    // inset rect is always smaller than the box, so this only ever wraps text
    // sooner, never later.
    let body_box = body_rect(shape, extent);
    let body_extent = body_box.size;
    let offset = (body_box.origin.x.raw(), body_box.origin.y.raw());

    let title = is_title(shape.placeholder.as_ref());
    // Rung 2 is the shape's own list style, exactly as the markdown emitter
    // layers it on.
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    let auto_fit = ShapeAutoFit::from_body(body.body_pr.as_ref().and_then(|bp| bp.auto_fit));
    let default_family = theme_family(ctx.theme, title);

    let mut blocks = Vec::with_capacity(body.paragraphs.len());
    for para in &body.paragraphs {
        let resolved = cascade.resolve(&para.properties, shape.placeholder.as_ref());
        blocks.push(paragraph_block(
            para,
            &resolved,
            &default_family,
            auto_fit,
            ctx,
        ));
    }

    // The fallback height for a line that states none — an empty paragraph
    // between two bullets. Scaled by the body's own shrink, for the same reason
    // the DOCX path scales it: otherwise a shrunk body keeps full-size blanks.
    let line_height = auto_fit.scale_font(
        ctx.measurer
            .default_line_height(&default_family, spec_default_size()),
    );

    let commands = layout_shape_body(&blocks, body_extent, body.body_pr.as_ref(), line_height);
    if commands.is_empty() {
        return;
    }

    // The frame is still the *shape's*, not the text rect's: a rotation turns
    // the whole shape about the shape's centre, and an inset body rides along
    // rather than turning about its own — the same reason a table's cells all
    // rotate about the table's centre. `offset` is what puts it back where it
    // belongs, exactly as a cell's is.
    let frame = Frame::new(slide_rect, extent);
    let bracket = ctx.push_bracketed(frame.place(offset, body_extent), commands.clone());
    let items = commands_to_items(commands, body_extent, ctx);
    let first_item = ctx.items.len();
    frame.push_items(items, offset, ctx.items);

    ctx.placements.push(ShapePlacement {
        kind: PlacementKind::Shape,
        rect: (
            frame.origin.0 + offset.0,
            frame.origin.1 + offset.1,
            body_extent.width.raw(),
            body_extent.height.raw(),
        ),
        rotation: frame.item_rotation(),
        shrunk: auto_fit != ShapeAutoFit::NONE,
        items: first_item..ctx.items.len(),
        bracket,
    });
}

/// The rectangle a shape's text body is laid out in — its geometry's
/// `<a:rect>` (§20.1.9.18) when that geometry names one, and the shape's whole
/// box otherwise.
///
/// Built here rather than shared with the paint walk, which resolves the same
/// geometry a few lines earlier. The two cannot share: the painter builds in
/// `slide_rect.bounding_box()`, because a rotated child's ink lives in the box
/// group composition produced, while text is laid out in `slide_rect.rect` and
/// turned about the frame's centre afterwards. Those two extents differ for a
/// rotated shape, and a shared build would give one of the two walks a
/// rectangle scaled for the other. The build itself is cheap.
///
/// Falls back to the full box in three cases, each a silent text drop if taken
/// literally:
///
/// * no geometry declared (the schema's default is a rectangle, whose text
///   rect *is* the box, so the fallback is also the right answer),
/// * an unbuildable preset, i.e. a name §20.1.9.18 does not define,
/// * a degenerate rect. A body given no width lays nothing out, and dropping a
///   shape's text on a guide that evaluated to zero would be a worse answer
///   than the box it came in.
fn body_rect(shape: &Shape, extent: PtSize) -> PtRect {
    let whole = PtRect {
        origin: PtOffset::new(Pt::ZERO, Pt::ZERO),
        size: extent,
    };
    let Some(geometry) = shape_properties(shape).and_then(|p| p.geometry.as_ref()) else {
        return whole;
    };
    let Some(rect) = build_geometry(geometry, extent).and_then(|path| path.text_rect) else {
        return whole;
    };
    if rect.size.width <= Pt::ZERO || rect.size.height <= Pt::ZERO {
        return whole;
    }
    rect
}

/// Convert a laid-out body's draw commands into [`TextItem`]s, in body-local Pt.
///
/// Goes through the DOCX path's own `DrawCommand` → `TextItem` code, on a
/// synthetic page the size of the body's box, so the two formats agree on how a
/// baseline becomes a box (ascent above, descent below, width re-measured).
fn commands_to_items(
    commands: Vec<liteparse_ooxml::render::layout::draw_command::DrawCommand>,
    extent: PtSize,
    ctx: &ShapeCtx<'_, '_>,
) -> Vec<TextItem> {
    if commands.is_empty() {
        return Vec::new();
    }
    let page = LayoutedPage {
        commands,
        page_size: extent,
        block_starts: Vec::new(),
        header_blocks: Vec::new(),
        footer_blocks: Vec::new(),
    };
    let converted = docx_layout::layout_to_pages(&[page], ctx.registry, false, false);
    converted
        .pages
        .into_iter()
        .next()
        .map(|p| p.text_items)
        .unwrap_or_default()
}

/// The rectangle a body's items are placed against: where it sits on the slide,
/// and the rotation the whole frame carries.
///
/// A table needs this separated out from the body layout because one frame
/// holds many boxes — every cell rotates about the **frame's** centre, not its
/// own, or a rotated table would fan its cells apart.
struct Frame {
    origin: (f32, f32),
    /// Frame-local centre of rotation.
    centre: (f32, f32),
    /// Clockwise degrees, as the file declares them.
    rot_deg: f32,
    /// The same angle unconverted, for the draw-command bracket — which takes
    /// 60000ths of a degree, exactly as `a:xfrm@rot` states them.
    rotation: liteparse_ooxml::model::dimension::Dimension<
        liteparse_ooxml::model::dimension::SixtieThousandthDeg,
    >,
}

impl Frame {
    fn new(slide_rect: liteparse_ooxml::pptx::SlideRect, extent: PtSize) -> Self {
        Self {
            origin: (
                emu_to_pt(slide_rect.rect.origin.x.raw()).raw(),
                emu_to_pt(slide_rect.rect.origin.y.raw()).raw(),
            ),
            centre: (extent.width.raw() * 0.5, extent.height.raw() * 0.5),
            // OOXML `@rot` is clockwise-positive; `TextItem::rotation` is
            // counter-clockwise degrees.
            rot_deg: slide_rect.rotation.raw() as f32 / ANGLE_UNITS_PER_DEGREE,
            rotation: slide_rect.rotation,
        }
    }

    /// The placement bracket for a box of size `extent` whose top-left sits at
    /// `offset` within this frame.
    ///
    /// A rotation about the *frame's* centre is not a rotation about the
    /// box's, and a table is where the difference shows: every cell turns with
    /// the table, so charging each one its own centre would fan them apart.
    /// A rigid motion decomposes, though — rotate the box about its own centre
    /// by the frame's angle, then put that centre where the frame's rotation
    /// sends it — and the second half is all this computes. For a shape,
    /// `offset` is `(0, 0)` and the two centres coincide, so it reduces to the
    /// frame origin.
    ///
    /// **Flips are deliberately not carried.** `a:xfrm@flipH/@flipV` mirror a
    /// shape's *geometry*; PowerPoint does not mirror the text inside it, and
    /// the item path ignores them for the same reason.
    fn place(&self, offset: (f32, f32), extent: PtSize) -> ShapeTransform {
        let (half_w, half_h) = (extent.width.raw() * 0.5, extent.height.raw() * 0.5);
        let (cx, cy) = (offset.0 + half_w, offset.1 + half_h);
        let (rx, ry) = if self.rot_deg == 0.0 {
            (cx, cy)
        } else {
            let (sin, cos) = self.rot_deg.to_radians().sin_cos();
            let (dx, dy) = (cx - self.centre.0, cy - self.centre.1);
            (
                self.centre.0 + dx * cos - dy * sin,
                self.centre.1 + dx * sin + dy * cos,
            )
        };
        ShapeTransform {
            origin: PtOffset::new(
                Pt::new(self.origin.0 + rx - half_w),
                Pt::new(self.origin.1 + ry - half_h),
            ),
            rotation: self.rotation,
            flip_h: false,
            flip_v: false,
            extent,
        }
    }

    /// The angle an item placed in this frame reports.
    fn item_rotation(&self) -> f32 {
        -self.rot_deg
    }

    /// Append `items` — in box-local Pt — onto the slide, where the box's own
    /// top-left sits at `offset` within the frame.
    fn push_items(&self, items: Vec<TextItem>, offset: (f32, f32), out: &mut Vec<TextItem>) {
        let rotate = self.rot_deg != 0.0;
        let (sin, cos) = (
            self.rot_deg.to_radians().sin(),
            self.rot_deg.to_radians().cos(),
        );
        for mut item in items {
            item.x += offset.0;
            item.y += offset.1;
            if rotate {
                // Rotate the box's top-left about the frame centre, in
                // frame-local space, then translate. The item keeps its
                // unrotated width and height and states its angle, matching how
                // the PDF path reports a rotated run — a rotated AABB would
                // silently widen every box.
                let (dx, dy) = (item.x - self.centre.0, item.y - self.centre.1);
                item.x = self.centre.0 + dx * cos - dy * sin;
                item.y = self.centre.1 + dx * sin + dy * cos;
                item.rotation = self.item_rotation();
            }
            item.x += self.origin.0;
            item.y += self.origin.1;
            out.push(item);
        }
    }
}

/// Lay a DrawingML table's cells out and append their items.
///
/// **The second layout path.** A cell's rectangle is not declared anywhere: it
/// is derived from `a:gridCol` prefix sums across and `a:tr@h` down, both
/// measured from the frame's own origin. Once the rectangle exists a cell is an
/// ordinary DrawingML text body — `a:tcPr` carries the same four insets and the
/// same anchor as an `a:bodyPr` — so everything below the rectangle is the
/// shape path's code.
///
/// Two facts decide the derivation, and both contradict the obvious
/// implementation:
///
/// - **The frame's `@cx` is not the table's width.** Some producers write a
///   constant frame width for every table, up to 4x wrong. The grid is
///   authoritative and the frame supplies only the origin.
/// - **`a:tr@h` is a minimum, not a height** (§21.1.3.18). A row may declare
///   `h="0"`, which under a literal reading stacks every cell of the table at
///   its top edge. So each row is grown to its tallest cell, which is why cells
///   must be measured before any of them can be placed.
fn layout_table(shape: &Shape, table: &pptx::Table, ctx: &mut ShapeCtx<'_, '_>) {
    let text_cells = || {
        table
            .rows
            .iter()
            .flat_map(|r| r.cells.iter())
            .filter(|c| !c.is_absorbed() && c.text.as_ref().is_some_and(|t| !t.is_empty()))
            .count()
    };
    let Some(slide_rect) = shape.slide_rect else {
        *ctx.unplaced_table_cells += text_cells();
        return;
    };
    let col_edges = prefix_edges(table.grid.iter().map(|w| emu_to_pt(w.raw())));
    if col_edges.len() < 2 || col_edges[col_edges.len() - 1] <= Pt::ZERO {
        // No grid means no cell has a width to wrap in. Counted, not dropped
        // silently — the markdown emitter still emits this table's text.
        *ctx.unplaced_table_cells += text_cells();
        return;
    }

    let default_family = theme_family(ctx.theme, false);
    let line_height = ctx
        .measurer
        .default_line_height(&default_family, spec_default_size());

    // Pass 1: build each cell's blocks and its width, and measure the height it
    // needs. Blocks are kept because pass 2 stacks the very same ones — a
    // rebuild would risk measuring one thing and placing another.
    let mut cells: Vec<CellLayout> = Vec::new();
    for (row_idx, row) in table.rows.iter().enumerate() {
        let mut col = 0usize;
        for cell in &row.cells {
            let span = cell.grid_span.max(1) as usize;
            // An absorbed cell still holds its slot in the grid (§21.1.3.16),
            // so the cursor advances past it even though nothing is drawn.
            let start = col;
            col += span;
            if cell.is_absorbed() || start + 1 >= col_edges.len() {
                continue;
            }
            let Some(body) = &cell.text else { continue };
            let end = col.min(col_edges.len() - 1);
            let width = col_edges[end] - col_edges[start];
            if width <= Pt::ZERO {
                *ctx.unplaced_table_cells += usize::from(!body.is_empty());
                continue;
            }

            let body_pr = cell.properties.text_body_properties();
            let blocks = cell_blocks(body, &default_family, ctx);
            let needed = measure_shape_body(&blocks, width, Some(&body_pr), line_height);
            cells.push(CellLayout {
                row: row_idx,
                row_span: cell.row_span.max(1) as usize,
                left: col_edges[start],
                width,
                blocks,
                body_pr,
                needed,
            });
        }
    }

    let declared: Vec<Pt> = table
        .rows
        .iter()
        .map(|r| {
            r.height
                .map_or(Pt::ZERO, |h| emu_to_pt(h.raw()))
                .max(Pt::ZERO)
        })
        .collect();
    let row_edges = prefix_edges(grown_row_heights(&declared, &cells).into_iter());

    // Pass 2: place. Rotation is the frame's, about the frame's own centre.
    let frame = Frame::new(
        slide_rect,
        PtSize {
            width: emu_to_pt(slide_rect.rect.size.width.raw()),
            height: emu_to_pt(slide_rect.rect.size.height.raw()),
        },
    );
    for cell in cells {
        let bottom = row_edges[(cell.row + cell.row_span).min(row_edges.len() - 1)];
        let extent = PtSize {
            width: cell.width,
            height: bottom - row_edges[cell.row],
        };
        let commands = layout_shape_body(&cell.blocks, extent, Some(&cell.body_pr), line_height);
        if commands.is_empty() {
            continue;
        }
        let offset = (cell.left.raw(), row_edges[cell.row].raw());
        let bracket = ctx.push_bracketed(frame.place(offset, extent), commands.clone());
        let items = commands_to_items(commands, extent, ctx);
        let first_item = ctx.items.len();
        frame.push_items(items, offset, ctx.items);
        if ctx.items.len() == first_item {
            continue;
        }
        *ctx.placed_table_cells += 1;
        // One placement per cell, not per table: the containment check is only
        // worth anything against the box the text was actually wrapped in.
        ctx.placements.push(ShapePlacement {
            kind: PlacementKind::TableCell,
            rect: (
                frame.origin.0 + offset.0,
                frame.origin.1 + offset.1,
                extent.width.raw(),
                extent.height.raw(),
            ),
            rotation: frame.item_rotation(),
            shrunk: false,
            items: first_item..ctx.items.len(),
            bracket,
        });
    }
}

/// Turn a run of lengths into the `n + 1` edges they define, starting at zero.
///
/// Both of a cell's coordinates come from one of these: `a:gridCol` widths
/// across, grown row heights down. `edges[i]` is track `i`'s near edge, so a
/// cell spanning `[i, i + span)` runs from `edges[i]` to `edges[i + span]` and a
/// merge needs no special case.
fn prefix_edges(lengths: impl Iterator<Item = Pt>) -> Vec<Pt> {
    let mut edges = vec![Pt::ZERO];
    let mut acc = Pt::ZERO;
    for len in lengths {
        acc += len;
        edges.push(acc);
    }
    edges
}

/// §21.1.3.18: each row's declared `@h` raised to fit its tallest cell.
///
/// `@h` is a **minimum**, and treating it as the height is not a rounding
/// error: a row declaring `h="0"` would stack every cell of the table at its
/// top edge.
///
/// A row-spanning cell is deliberately excluded from its row's growth. Its
/// content is shared across every row it covers, so charging the whole height
/// to the first of them would push every later row down — the spanning cell
/// instead takes the summed rectangle and may overflow it, which is what
/// `@vertOverflow`'s default already describes.
fn grown_row_heights(declared: &[Pt], cells: &[CellLayout]) -> Vec<Pt> {
    let mut heights = declared.to_vec();
    for cell in cells {
        if cell.row_span == 1
            && let Some(h) = heights.get_mut(cell.row)
        {
            *h = (*h).max(cell.needed);
        }
    }
    heights
}

/// One table cell, measured and waiting for its row's height to be decided.
struct CellLayout {
    row: usize,
    row_span: usize,
    /// Frame-local left edge and the width the grid gives this cell.
    left: Pt,
    width: Pt,
    blocks: Vec<LayoutBlock>,
    body_pr: liteparse_ooxml::model::BodyProperties,
    /// Height this cell's text needs at `width`, insets included.
    needed: Pt,
}

/// A cell's paragraphs as layout blocks.
///
/// Resolved exactly as `pptx::cell_text` resolves them for markdown — the
/// cell's own `a:lstStyle` as the shape rung and **no placeholder**, since a
/// cell fills none. Diverging here would box text the emitter never wrote.
fn cell_blocks(
    body: &TextBody,
    default_family: &str,
    ctx: &mut ShapeCtx<'_, '_>,
) -> Vec<LayoutBlock> {
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    body.paragraphs
        .iter()
        .map(|para| {
            let resolved = cascade.resolve(&para.properties, None);
            paragraph_block(para, &resolved, default_family, ShapeAutoFit::NONE, ctx)
        })
        .collect()
}

/// One `a:p` as a [`LayoutBlock::Paragraph`], with every run measured.
fn paragraph_block(
    para: &TextParagraph,
    resolved: &ResolvedTextStyle,
    default_family: &str,
    auto_fit: ShapeAutoFit,
    ctx: &mut ShapeCtx<'_, '_>,
) -> LayoutBlock {
    let mut fragments = Vec::new();
    collect_run_fragments(
        &para.content,
        resolved,
        default_family,
        auto_fit,
        ctx,
        &mut fragments,
    );

    LayoutBlock::Paragraph {
        fragments,
        style: paragraph_style(resolved, auto_fit),
        page_break_before: false,
        footnotes: Vec::new(),
        floating_images: Vec::new(),
        floating_shapes: Vec::new(),
    }
}

/// §21.1.2.3.9 `a:rPr/a:solidFill` → the sRGB the painter draws with.
///
/// The colour arrives here already merged down the whole text cascade — run,
/// shape `a:lstStyle`, layout/master placeholder, master `p:txStyles`,
/// presentation `defaultTextStyle` — because `drawing_color` is an ordinary
/// `Option` field of [`RunProperties`] and rides `merge_run_properties` like
/// every other one. What could not ride the cascade is the *resolution*: a
/// `schemeClr` needs this slide's theme and its master's `p:clrMap`, and
/// neither is a property of the run.
///
/// Alpha is dropped: `DrawCommand::Text` carries an opaque `RgbColor`, so a
/// half-transparent run paints solid. That is a smaller error than the black
/// it replaces, and it is the same simplification the fill path made.
///
/// Black on no declaration is the §21.1.2.3 default, *not* a guess — but the
/// count still separates the two, because the runs that land here are exactly
/// the ones `p:style/a:fontRef` would colour.
fn run_color(
    props: &liteparse_ooxml::model::RunProperties,
    ctx: &mut ShapeCtx<'_, '_>,
) -> RgbColor {
    let rgb = match props.drawing_color.as_ref() {
        Some(declared) => {
            *ctx.runs_colour_declared += 1;
            let dc = DrawingColorContext::new(ctx.theme).with_color_map(ctx.color_map);
            resolve_drawing_color(declared, &dc).to_rgb24()
        }
        None => {
            *ctx.runs_colour_defaulted += 1;
            0x000000
        }
    };
    // Counted for *both* branches on purpose: a defaulted-black run on a dark
    // backdrop is the exact failure this step exists to remove, and checking
    // only the declared branch would report it as fixed.
    if ctx.background_rgb == Some(rgb) {
        *ctx.runs_invisible_on_background += 1;
    }
    rgb_from_u32(rgb)
}

fn collect_run_fragments(
    inlines: &[liteparse_ooxml::model::Inline],
    resolved: &ResolvedTextStyle,
    default_family: &str,
    auto_fit: ShapeAutoFit,
    ctx: &mut ShapeCtx<'_, '_>,
    fragments: &mut Vec<Fragment>,
) {
    use liteparse_ooxml::model::{Inline, RunElement};

    for inline in inlines {
        match inline {
            Inline::TextRun(run) => {
                let mut props = run.properties.clone();
                resolved.apply_to_run(&mut props);
                // The cascade resolves size but has no font rung, so a run that
                // named `+mn-lt` still carries a `ThemeFontRef` here and a run
                // that named nothing carries neither. Resolve the reference,
                // then let `font_props_from_run` fall back to the theme face
                // for the 26.3% that named no family at all.
                if let Some(theme) = ctx.theme {
                    resolve_font_set_themes(&mut props.fonts, theme);
                }
                let font = font_props_from_run(
                    &props,
                    default_family,
                    // `apply_to_run` guarantees a size — `run_defaults.font_size`
                    // is always `Some` — so this is unreachable in practice and
                    // is the §20.1.2.1 spec default rather than a guess.
                    spec_default_size(),
                    auto_fit,
                );
                let color = run_color(&props, ctx);
                for element in &run.content {
                    match element {
                        RunElement::Text(text) => {
                            emit_run_fragments(text, &font, color, None, ctx.measurer, fragments)
                        }
                        RunElement::Tab => fragments.push(Fragment::Tab {
                            line_height: font.size,
                            font: std::rc::Rc::new(font.clone()),
                            color,
                            fitting_width: None,
                        }),
                        RunElement::LineBreak(_) => fragments.push(Fragment::LineBreak {
                            line_height: font.size,
                        }),
                        _ => {}
                    }
                }
            }
            Inline::Hyperlink(link) => {
                collect_run_fragments(
                    &link.content,
                    resolved,
                    default_family,
                    auto_fit,
                    ctx,
                    fragments,
                );
            }
            _ => {}
        }
    }
}

/// `pub(crate)` for the XLSX shape painter, which lays a DrawingML body out
/// with the same `layout_shape_body` and so needs the same `a:pPr` → style
/// mapping. Everything it reads is on `ResolvedTextStyle`, which is not
/// PPTX-specific — only the *cascade* that fills it is.
pub(crate) fn paragraph_style(
    resolved: &ResolvedTextStyle,
    auto_fit: ShapeAutoFit,
) -> ParagraphStyle {
    let size = resolved
        .run_defaults
        .font_size
        .map(Pt::from)
        .unwrap_or_else(spec_default_size);

    ParagraphStyle {
        alignment: resolved.alignment.unwrap_or(Alignment::Start),
        space_before: spacing_to_pt(resolved.space_before, size),
        space_after: spacing_to_pt(resolved.space_after, size),
        // §21.1.2.2.7 `@marL` is the left edge of the whole paragraph and
        // `@indent` is the first line's offset from it — the same split as
        // §17.3.1.12's `left`/`firstLine`, and a hanging indent is negative in
        // both. So these map across directly.
        indent_left: resolved.margin_left.map(Pt::from).unwrap_or(Pt::ZERO),
        indent_right: resolved.margin_right.map(Pt::from).unwrap_or(Pt::ZERO),
        indent_first_line: resolved.indent.map(Pt::from).unwrap_or(Pt::ZERO),
        line_spacing: match resolved.line_spacing {
            // `a:spcPct` is a multiple of the line's natural height, which is
            // exactly what `Auto` means here.
            Some(Spacing::Percent(p)) => LineSpacingRule::Auto(p.to_fraction()),
            // `a:spcPts` is an absolute height, and DrawingML treats it as a
            // minimum rather than a cap.
            Some(Spacing::Points(p)) => LineSpacingRule::AtLeast(Pt::new(p.to_points_f32())),
            None => LineSpacingRule::Auto(1.0),
        },
        auto_fit,
        default_tab_stop: resolved
            .default_tab_size
            .map(Pt::from)
            .unwrap_or(Pt::new(36.0)),
        ..ParagraphStyle::default()
    }
}

/// `a:spcBef`/`a:spcAft` as a height.
///
/// The percentage form is a percentage **of the font size**, not of the line
/// box — §21.1.2.2.9 defines it against the text size — so it needs the
/// paragraph's resolved size to become a length.
fn spacing_to_pt(spacing: Option<Spacing>, size: Pt) -> Pt {
    match spacing {
        Some(Spacing::Percent(p)) => Pt::new(size.raw() * p.to_fraction()),
        Some(Spacing::Points(p)) => Pt::new(p.to_points_f32()),
        None => Pt::ZERO,
    }
}

/// The theme face a run falls back to when it names no family: `+mj-lt` for a
/// title, `+mn-lt` for everything else (§20.1.4.1.24/§20.1.4.1.26).
///
/// Falls through to the measurer's own generic handling when the deck has no
/// theme or the theme names no latin face — which is host-dependent, and is
/// reported as such by the probe's `ResolveRule` histogram rather than hidden.
fn theme_family(theme: Option<&Theme>, title: bool) -> String {
    let latin = theme.map(|t| {
        if title {
            t.major_font.latin.clone()
        } else {
            t.minor_font.latin.clone()
        }
    });
    match latin {
        Some(f) if !f.is_empty() => f,
        _ => "Arial".to_string(),
    }
}

/// §21.1.2.2.10: the size a run takes when nothing in the cascade supplies one.
/// Matches `pptx::textcascade`'s own spec-default rung (1800 hundredths = 18pt).
fn spec_default_size() -> Pt {
    Pt::new(18.0)
}

fn emu_to_pt(emu: i64) -> Pt {
    Pt::new(emu as f32 / EMU_PER_POINT)
}

/// Placed-image rects per slide, for the native complexity stats.
///
/// Read off the painted command stream rather than the emitter's figure list,
/// so the number does not move when a caller changes `image_mode` or
/// `extract_images` — complexity describes the deck, not the request. This is
/// the same independence `docx_layout::image_rects_per_page` has by walking the
/// layout rather than `collect_images`.
///
/// Wider than the figure list on purpose, and in both directions a figure would
/// not go: a blip **background** and a blip fill behind text are image content
/// on the page for the purpose of "is this slide complex", while neither is a
/// figure a reader gets a ref to.
pub fn image_rects_per_page(layouted: &[LayoutedPage]) -> Vec<Vec<crate::types::Rect>> {
    layouted
        .iter()
        .map(|lp| {
            lp.commands
                .iter()
                .filter_map(|cmd| match cmd {
                    DrawCommand::Path {
                        origin,
                        extent,
                        fill: ResolvedFill::Blip(_),
                        ..
                    } => Some(crate::types::Rect {
                        x: origin.x.raw(),
                        y: origin.y.raw(),
                        width: extent.width.raw(),
                        height: extent.height.raw(),
                    }),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

fn union_bounds(items: &[TextItem]) -> Option<crate::types::Rect> {
    let first = items.first()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.x + first.width;
    let mut bottom = first.y + first.height;
    for item in items.iter().skip(1) {
        left = left.min(item.x);
        top = top.min(item.y);
        right = right.max(item.x + item.width);
        bottom = bottom.max(item.y + item.height);
    }
    Some(crate::types::Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse_ooxml::model::{ThemeFontScheme, dimension::Dimension};

    fn theme_with(major: &str, minor: &str) -> Theme {
        Theme {
            major_font: ThemeFontScheme {
                latin: major.to_string(),
                ..Default::default()
            },
            minor_font: ThemeFontScheme {
                latin: minor.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn theme_font_splits_title_from_body() {
        // The rung that covers runs naming no font. Getting the halves the
        // wrong way round is invisible in markdown and wrong in every width.
        let theme = theme_with("Georgia", "Verdana");
        assert_eq!(theme_family(Some(&theme), true), "Georgia");
        assert_eq!(theme_family(Some(&theme), false), "Verdana");
    }

    #[test]
    fn theme_font_falls_back_when_absent_or_empty() {
        assert_eq!(theme_family(None, false), "Arial");
        let empty = theme_with("", "");
        assert_eq!(theme_family(Some(&empty), true), "Arial");
    }

    #[test]
    fn percent_spacing_is_a_fraction_of_the_font_size() {
        // §21.1.2.2.9 measures `a:spcPct` against the text size, not the line
        // box. Reading it as a line multiple would scale every gap by the
        // wrong base.
        let size = Pt::new(20.0);
        // 50000 thousandths of a percent = 50%.
        let half = Spacing::Percent(Dimension::new(50_000));
        assert_eq!(spacing_to_pt(Some(half), size), Pt::new(10.0));
        assert_eq!(spacing_to_pt(None, size), Pt::ZERO);
    }

    #[test]
    fn points_spacing_is_absolute() {
        // `a:spcPts` is in hundredths of a point.
        let pts = Spacing::Points(Dimension::new(1_200));
        assert_eq!(spacing_to_pt(Some(pts), Pt::new(20.0)), Pt::new(12.0));
    }

    #[test]
    fn emu_converts_at_12700_per_point() {
        assert_eq!(emu_to_pt(914_400), Pt::new(72.0));
        assert_eq!(emu_to_pt(0), Pt::ZERO);
    }

    fn cell(row: usize, row_span: usize, needed: f32) -> CellLayout {
        CellLayout {
            row,
            row_span,
            left: Pt::ZERO,
            width: Pt::new(100.0),
            blocks: Vec::new(),
            body_pr: liteparse_ooxml::pptx::TableCellProperties::default().text_body_properties(),
            needed: Pt::new(needed),
        }
    }

    #[test]
    fn prefix_edges_bracket_every_track() {
        let e = prefix_edges([Pt::new(10.0), Pt::new(20.0), Pt::new(5.0)].into_iter());
        assert_eq!(
            e,
            vec![Pt::ZERO, Pt::new(10.0), Pt::new(30.0), Pt::new(35.0)]
        );
        // A cell spanning columns 1..3 reads its span straight off the edges.
        assert_eq!(e[3] - e[1], Pt::new(25.0));
        // No tracks is one edge, not zero — the caller checks for `< 2`.
        assert_eq!(prefix_edges(std::iter::empty()), vec![Pt::ZERO]);
    }

    #[test]
    fn row_grows_to_its_tallest_cell_but_never_shrinks() {
        // `@h` is a minimum (§21.1.3.18): a taller cell raises the row, a
        // shorter one leaves the declared height alone.
        let declared = vec![Pt::new(20.0), Pt::new(50.0)];
        let grown = grown_row_heights(&declared, &[cell(0, 1, 35.0), cell(1, 1, 10.0)]);
        assert_eq!(grown, vec![Pt::new(35.0), Pt::new(50.0)]);
    }

    #[test]
    fn zero_declared_height_is_grown_not_taken_literally() {
        // A row declaring `h="0"` taken literally makes every row edge zero and
        // the whole table collapses onto its top edge — text that is still
        // inside the frame, and therefore invisible to a containment check.
        let grown = grown_row_heights(&[Pt::ZERO, Pt::ZERO], &[cell(0, 1, 18.0), cell(1, 1, 24.0)]);
        assert_eq!(grown, vec![Pt::new(18.0), Pt::new(24.0)]);
        let edges = prefix_edges(grown.into_iter());
        assert_eq!(edges[1], Pt::new(18.0));
        assert!(edges[2] > edges[1], "rows must not share an edge");
    }

    #[test]
    fn a_row_spanning_cell_does_not_grow_the_row_it_starts_in() {
        // Its content belongs to every row it covers, so charging the height to
        // the first would push all the later rows down the slide.
        let declared = vec![Pt::new(10.0), Pt::new(10.0)];
        let grown = grown_row_heights(&declared, &[cell(0, 2, 500.0)]);
        assert_eq!(grown, declared);
    }

    #[test]
    fn a_cell_naming_a_row_off_the_end_is_ignored() {
        // Malformed input must not panic or silently resize the table.
        let declared = vec![Pt::new(10.0)];
        assert_eq!(grown_row_heights(&declared, &[cell(7, 1, 99.0)]), declared);
    }

    // ── the placement bracket ────────────────────────────────────────────

    fn frame(x: f32, y: f32, w: f32, h: f32, deg: f32) -> Frame {
        use liteparse_ooxml::model::geometry::{Offset, Rect, Size};
        let emu = |pt: f32| Dimension::new((pt * EMU_PER_POINT) as i64);
        let slide_rect = liteparse_ooxml::pptx::SlideRect {
            rect: Rect::new(Offset::new(emu(x), emu(y)), Size::new(emu(w), emu(h))),
            rotation: Dimension::new((deg * ANGLE_UNITS_PER_DEGREE) as i64),
            flip_h: false,
            flip_v: false,
            skewed: false,
        };
        Frame::new(slide_rect, size(w, h))
    }

    fn size(w: f32, h: f32) -> PtSize {
        PtSize {
            width: Pt::new(w),
            height: Pt::new(h),
        }
    }

    /// An unrotated shape's bracket is its own rectangle — the commands inside
    /// it are body-local, so the origin is where the body starts.
    #[test]
    fn an_unrotated_shape_brackets_at_its_own_origin() {
        let placed = frame(100.0, 50.0, 200.0, 80.0, 0.0).place((0.0, 0.0), size(200.0, 80.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (100.0, 50.0)
        );
        assert_eq!(placed.rotation.raw(), 0);
        assert_eq!(placed.extent.width, Pt::new(200.0));
    }

    /// A rotated shape turns about its own centre, so its origin does not move
    /// — only the space inside the bracket rotates. Rotating the origin as
    /// well would slide every rotated body off its shape.
    #[test]
    fn a_rotated_shape_keeps_its_origin() {
        let placed = frame(100.0, 50.0, 200.0, 80.0, 90.0).place((0.0, 0.0), size(200.0, 80.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (100.0, 50.0)
        );
        assert_eq!(placed.rotation.raw(), 5_400_000, "the bracket carries @rot");
    }

    /// A cell turns with its **table**: its centre lands where the frame's
    /// rotation sends it. Charging each cell its own centre would leave every
    /// cell in place and fan the table apart instead of turning it.
    #[test]
    fn a_cell_rotates_about_the_frame_centre_not_its_own() {
        // A 40x20 cell in the top-left of a 200x80 frame: centre (20, 10)
        // against the frame's (100, 40). A quarter turn sends the offset
        // (-80, -30) to (30, -80), so the cell's centre lands at (130, -40)
        // and its box origin half a cell up and left of that.
        let placed = frame(0.0, 0.0, 200.0, 80.0, 90.0).place((0.0, 0.0), size(40.0, 20.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (110.0, -50.0)
        );

        // Unrotated, the same cell is simply at its offset in the frame.
        let flat = frame(0.0, 0.0, 200.0, 80.0, 0.0).place((5.0, 7.0), size(40.0, 20.0));
        assert_eq!((flat.origin.x.raw(), flat.origin.y.raw()), (5.0, 7.0));
    }

    /// §20.1.7.6 flips mirror a shape's *geometry*; PowerPoint does not mirror
    /// the text inside it, and the item path ignores them for the same reason.
    #[test]
    fn a_bracket_never_carries_a_flip() {
        let placed = frame(0.0, 0.0, 10.0, 10.0, 0.0).place((0.0, 0.0), size(10.0, 10.0));
        assert!(!placed.flip_h && !placed.flip_v);
    }

    #[test]
    fn union_bounds_spans_every_item() {
        let item = |x: f32, y: f32, w: f32, h: f32| TextItem {
            x,
            y,
            width: w,
            height: h,
            ..TextItem::default()
        };
        assert!(union_bounds(&[]).is_none());
        let r = union_bounds(&[item(10.0, 20.0, 5.0, 5.0), item(3.0, 40.0, 2.0, 2.0)]).unwrap();
        assert_eq!((r.x, r.y), (3.0, 20.0));
        assert_eq!((r.width, r.height), (12.0, 22.0));
    }

    /// A shape carrying `geometry` and nothing else — `body_rect` reads only
    /// the geometry off `spPr`, and every other field is noise to it.
    fn shape_with(geometry: Option<liteparse_ooxml::model::ShapeGeometry>) -> Shape {
        Shape {
            non_visual: liteparse_ooxml::model::DocProperties {
                id: 1,
                name: String::new(),
                description: None,
                hidden: None,
                title: None,
            },
            placeholder: None,
            transform: None,
            transform_inherited: false,
            slide_rect: None,
            style: None,
            kind: ShapeKind::AutoShape(Box::new(liteparse_ooxml::pptx::AutoShape {
                properties: Some(liteparse_ooxml::model::ShapeProperties {
                    bw_mode: None,
                    transform: None,
                    geometry,
                    fill: None,
                    outline: None,
                    effect_list: None,
                }),
                text: None,
                is_text_box: false,
            })),
        }
    }

    fn preset(
        preset: liteparse_ooxml::model::PresetShapeType,
    ) -> Option<liteparse_ooxml::model::ShapeGeometry> {
        Some(liteparse_ooxml::model::ShapeGeometry::Preset(
            liteparse_ooxml::model::PresetGeometryDef {
                preset,
                adjust_values: Vec::new(),
            },
        ))
    }

    /// §20.1.9.22: a `roundRect`'s text rect is inset by 29.289% of its corner
    /// radius — the sagitta of the 45° arc — on every side. With the default
    /// 16.667% adjustment on a 100x100 box that is 4.88pt; the point of the
    /// test is that it is *neither* zero (a plain rect's answer) nor the full
    /// 16.667pt radius.
    #[test]
    fn a_round_rect_lays_its_body_out_inside_its_corner_arcs() {
        let r = body_rect(
            &shape_with(preset(liteparse_ooxml::model::PresetShapeType::RoundRect)),
            size(100.0, 100.0),
        );
        let inset = r.origin.x.raw();
        assert!(
            (inset - 4.88).abs() < 0.05,
            "expected the 29.289% sagitta, got {inset}"
        );
        assert!((r.origin.y.raw() - inset).abs() < 0.01);
        assert!((r.size.width.raw() - (100.0 - 2.0 * inset)).abs() < 0.05);
        assert!((r.size.height.raw() - (100.0 - 2.0 * inset)).abs() < 0.05);
    }

    /// The 72% case: a plain rectangle's text rect *is* its box, so the
    /// substitution has to be a no-op for it rather than a near-miss.
    #[test]
    fn a_plain_rect_gets_its_whole_box() {
        let r = body_rect(
            &shape_with(preset(liteparse_ooxml::model::PresetShapeType::Rect)),
            size(100.0, 60.0),
        );
        assert_eq!((r.origin.x.raw(), r.origin.y.raw()), (0.0, 0.0));
        assert_eq!((r.size.width.raw(), r.size.height.raw()), (100.0, 60.0));
    }

    /// A body may declare no geometry at all. The schema's default is a
    /// rectangle, so the whole box is the right answer and not merely a
    /// fallback — laying such a body out in nothing would drop its text.
    #[test]
    fn a_shape_declaring_no_geometry_gets_its_whole_box() {
        let r = body_rect(&shape_with(None), size(80.0, 40.0));
        assert_eq!((r.size.width.raw(), r.size.height.raw()), (80.0, 40.0));
    }

    /// A `prst` §20.1.9.18 does not define builds nothing. The paint walk
    /// refuses to approximate it by its bounding box — a `rect` drawn where a
    /// `cloud` was asked for is a wrong slide. Text is the opposite trade: the
    /// box is where the text already lands today, and refusing it would lose
    /// the text rather than draw it coarsely.
    #[test]
    fn an_undefined_preset_still_gets_its_box_to_lay_text_out_in() {
        let r = body_rect(
            &shape_with(preset(liteparse_ooxml::model::PresetShapeType::Other(
                "nonesuch".into(),
            ))),
            size(80.0, 40.0),
        );
        assert_eq!((r.size.width.raw(), r.size.height.raw()), (80.0, 40.0));
    }
}
