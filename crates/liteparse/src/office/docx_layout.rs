//! `LayoutedPage` draw commands → [`Page`]/[`TextItem`] geometry.
//!
//! The vendored layout engine ends at per-page [`DrawCommand`] streams in Pt
//! with a top-left origin — already liteparse viewport space, no unit
//! conversion and no y-flip. What a `Text` command does *not* carry is a
//! bounding box: `position` is the baseline origin and there is no width field.
//! Both come from re-measuring with the same [`TextMeasurer`]/[`FontRegistry`]
//! the layout ran with, which reproduces the engine's own advances
//! bit-for-bit.
//!
//! Facts that are geometric inferences on the PDF path arrive here as data:
//! `LinkAnnotation` → [`TextItem::link`], `Outline` marks →
//! [`OutlineTarget`]s, `Image` commands → [`ExtractedImage`]s with the
//! *original* embedded bytes (see [`collect_images`]). `TextItem::strike` stays
//! `false`: the vendored layout never draws strikethrough lines, and markdown
//! strike comes from the block emitter's cascade, which reads the source
//! instead of geometry.

use std::collections::{HashMap, VecDeque};

use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::layout::draw_command::{DrawCommand, LayoutedPage, OutlineMark};
use liteparse_ooxml::render::layout::fragment::FontProps;
use liteparse_ooxml::render::layout::measurer::TextMeasurer;

use crate::types::{
    DocumentAnnotation, ExtractedImage, OutlineTarget, Page, Rect, TextItem, WordBox,
};

/// Everything the native pipeline taps out of a laid-out document.
pub struct NativeLayout {
    /// One [`Page`] per physical page, in order, with real geometry.
    pub pages: Vec<Page>,
    /// Outline entries in reading order, `page_index` zero-based, `y_pdf` in
    /// PDF user space (bottom-left origin) per the [`OutlineTarget`] contract.
    pub outline: Vec<OutlineTarget>,
    /// Flattened body-block index → zero-based physical page where the
    /// block's first content landed (min page on the rare duplicate).
    pub block_pages: HashMap<usize, usize>,
}

/// Horizontal/vertical fraction of a link rectangle an item must cover for
/// the link to attach. Link rects are per-word, so a run-level item normally
/// covers its words' rects fully; the threshold only guards against grazing
/// overlaps from neighbouring lines.
const LINK_MIN_COVER_FRACTION: f32 = 0.5;

/// Convert laid-out pages into liteparse [`Page`]s + outline + block→page map.
///
/// `registry` must be the same registry `layout_document` ran with — the
/// re-measure only reproduces the engine's line breaks against identical font
/// resolution. `emit_word_boxes` mirrors `LiteParseConfig::emit_word_boxes`:
/// when false, `TextItem::words` stays empty and the per-word prefix measures
/// are skipped entirely.
pub fn layout_to_pages(
    layouted: &[LayoutedPage],
    registry: &FontRegistry,
    emit_word_boxes: bool,
    extract_annotations: bool,
) -> NativeLayout {
    let measurer = TextMeasurer::new(registry);
    let mut pages = Vec::with_capacity(layouted.len());
    let mut outline: Vec<OutlineTarget> = Vec::new();
    let mut block_pages: HashMap<usize, usize> = HashMap::new();

    for (page_idx, lp) in layouted.iter().enumerate() {
        let page_height = lp.page_size.height.raw();
        let mut text_items: Vec<TextItem> = Vec::new();
        let mut links: Vec<(Rect, String)> = Vec::new();
        // (target identity, uri, per-word rect) in stream order; one
        // `w:hyperlink`'s words share one `Rc` target, so pointer identity
        // groups the words of a hyperlink instance without comparing URLs
        // (two links to the same URL stay two annotations).
        let mut link_spans: Vec<(usize, Option<String>, Rect)> = Vec::new();
        // Indices into `outline` whose heading bracket is still open and has
        // no y yet — resolved by the first Text command inside the bracket.
        let mut open_headings: Vec<usize> = Vec::new();

        for cmd in &lp.commands {
            match cmd {
                DrawCommand::Text {
                    position,
                    text,
                    font_family,
                    char_spacing,
                    font_size,
                    bold,
                    italic,
                    color,
                    text_scale,
                } => {
                    let props = FontProps {
                        family: font_family.clone(),
                        size: *font_size,
                        bold: *bold,
                        italic: *italic,
                        underline: false,
                        char_spacing: *char_spacing,
                        text_scale: *text_scale,
                        underline_position: liteparse_ooxml::render::dimension::Pt::ZERO,
                        underline_thickness: liteparse_ooxml::render::dimension::Pt::ZERO,
                    };
                    let (advance, metrics) = measurer.measure(text, &props);
                    let ascent = metrics.ascent.raw();
                    let descent = metrics.descent.raw();
                    let x = position.x.raw();
                    let top = position.y.raw() - ascent;
                    let width = advance.raw();
                    let height = ascent + descent;

                    let words = if emit_word_boxes {
                        word_boxes(text, &props, &measurer, x, top, height)
                    } else {
                        Vec::new()
                    };

                    let item = TextItem {
                        text: text.to_string(),
                        x,
                        y: top,
                        width,
                        height,
                        rotation: 0.0,
                        font_name: Some(font_family.to_string()),
                        font_size: Some(font_size.raw()),
                        // The PDF path's font_height is font_size × text-matrix
                        // y-scale; the native path has no CTM, so scale is 1.
                        font_height: Some(font_size.raw()),
                        font_ascent: Some(ascent),
                        // PDFium's convention: descent is negative below the
                        // baseline. `TextMetrics.descent` is positive-down.
                        font_descent: Some(-descent),
                        font_weight: Some(if *bold { 700 } else { 400 }),
                        text_width: Some(width),
                        fill_color: Some(format!(
                            "ff{:02x}{:02x}{:02x}",
                            color.r, color.g, color.b
                        )),
                        words,
                        ..TextItem::default()
                    };

                    for &oi in &open_headings {
                        if outline[oi].y_pdf.is_none() {
                            outline[oi].y_pdf = Some(page_height - top);
                        }
                    }

                    text_items.push(item);
                }
                DrawCommand::EmojiCluster {
                    rect, text, size, ..
                } => {
                    text_items.push(TextItem {
                        text: text.clone(),
                        x: rect.origin.x.raw(),
                        y: rect.origin.y.raw(),
                        width: rect.size.width.raw(),
                        height: rect.size.height.raw(),
                        font_size: Some(size.raw()),
                        ..TextItem::default()
                    });
                }
                DrawCommand::LinkAnnotation { rect, url } => {
                    let r = Rect {
                        x: rect.origin.x.raw(),
                        y: rect.origin.y.raw(),
                        width: rect.size.width.raw(),
                        height: rect.size.height.raw(),
                    };
                    if extract_annotations {
                        link_spans.push((
                            std::rc::Rc::as_ptr(url) as *const u8 as usize,
                            Some(url.to_string()),
                            r.clone(),
                        ));
                    }
                    links.push((r, url.to_string()));
                }
                // Internal links (TOC entries, cross-refs to bookmarks) become
                // uri-less `link` annotations — the same shape a GoTo link has
                // on the PDF path, where the LibreOffice-converted document
                // yields link annotations with no URI. They deliberately do
                // NOT set `TextItem::link` or emit markdown links: bookmark
                // anchors don't exist in the output document.
                DrawCommand::InternalLink { rect, destination } if extract_annotations => {
                    link_spans.push((
                        std::rc::Rc::as_ptr(destination) as *const u8 as usize,
                        None,
                        Rect {
                            x: rect.origin.x.raw(),
                            y: rect.origin.y.raw(),
                            width: rect.size.width.raw(),
                            height: rect.size.height.raw(),
                        },
                    ));
                }
                DrawCommand::Outline(mark) => match mark {
                    OutlineMark::Begin(h) => {
                        outline.push(OutlineTarget {
                            level: h.level.value(),
                            title: h.title.to_string(),
                            page_index: page_idx as i32,
                            y_pdf: None,
                        });
                        open_headings.push(outline.len() - 1);
                    }
                    OutlineMark::End => {
                        open_headings.pop();
                    }
                },
                // Underline/strike: the vendored layout emits underlines but
                // never strikethrough; TextItem has no underline field and
                // strike detection over border/separator lines would only
                // false-positive. Images/paths/rects: deferred (module docs).
                // NamedDestination is the bookmark *target* marker; nothing in
                // the ParseResult contract can hold it (the PDF path drops
                // GoTo destinations the same way — annotations carry only the
                // source rects).
                //
                // `Transform` is the PPTX placement bracket. This converter is
                // only ever handed body-local pages — the DOCX stacker emits
                // no brackets, and `pptx_layout` converts one shape body at a
                // time and applies the placement to the items itself — so a
                // bracket reaching here would mean a slide-wide page was
                // converted without its placements, which is a caller bug this
                // arm cannot repair.
                DrawCommand::Underline { .. }
                | DrawCommand::Line { .. }
                | DrawCommand::Rect { .. }
                | DrawCommand::Image { .. }
                | DrawCommand::Path { .. }
                | DrawCommand::InternalLink { .. }
                | DrawCommand::NamedDestination { .. }
                | DrawCommand::Transform(_)
                | DrawCommand::Float(_) => {}
            }
        }

        assign_links(&mut text_items, &links);
        let annotations = extract_annotations.then(|| merge_link_annotations(&link_spans));

        for &b in &lp.block_starts {
            block_pages
                .entry(b)
                .and_modify(|p| *p = (*p).min(page_idx))
                .or_insert(page_idx);
        }

        let content_bounds = union_bounds(&text_items);
        pages.push(Page {
            page_number: page_idx + 1,
            page_width: lp.page_size.width.raw(),
            page_height,
            content_bounds,
            text_items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations,
            form_fields: None,
            structure_tree: None,
        });
    }

    NativeLayout {
        pages,
        outline,
        block_pages,
    }
}

/// Per-word boxes via prefix re-measure. The measurer's advance arithmetic is
/// linear in the string (cmap advances + per-char spacing, no shaping), so a
/// prefix measure is exact — `width(a+b) == width(a) + width(b)`.
fn word_boxes(
    text: &str,
    props: &FontProps,
    measurer: &TextMeasurer<'_>,
    item_x: f32,
    item_y: f32,
    item_height: f32,
) -> Vec<WordBox> {
    let mut words = Vec::new();
    let mut search_from = 0usize;
    for word in text.split_whitespace() {
        // Locate this word's byte range (split_whitespace loses offsets).
        let rel = match text[search_from..].find(word) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let start = search_from + rel;
        let end = start + word.len();
        search_from = end;
        let x0 = measurer.measure(&text[..start], props).0.raw();
        let x1 = measurer.measure(&text[..end], props).0.raw();
        words.push(WordBox {
            text: word.to_string(),
            x: item_x + x0,
            y: item_y,
            width: x1 - x0,
            height: item_height,
        });
    }
    words
}

/// Attach link URLs to the items covering each link rectangle. Rects come one
/// per word from the layout engine; an item takes the first rect it covers.
fn assign_links(items: &mut [TextItem], links: &[(Rect, String)]) {
    if links.is_empty() {
        return;
    }
    for item in items.iter_mut() {
        if item.link.is_some() {
            continue;
        }
        for (r, url) in links {
            let ox = (item.x + item.width).min(r.x + r.width) - item.x.max(r.x);
            let oy = (item.y + item.height).min(r.y + r.height) - item.y.max(r.y);
            if ox >= r.width * LINK_MIN_COVER_FRACTION && oy >= r.height * LINK_MIN_COVER_FRACTION {
                item.link = Some(url.clone());
                break;
            }
        }
    }
}

/// Fold per-word link rects into annotation-shaped [`DocumentAnnotation`]s.
///
/// Spans arrive in stream (reading) order; a run of consecutive spans sharing
/// one target identity is one hyperlink instance and becomes ONE annotation —
/// `rect` is the union, `quadpoint_rects` hold one merged rect per laid-out
/// line, matching the PDF path's quadpoint convention for multi-line links.
/// Word rects of one line share the line box (same top and height from
/// `cursor_y`/`line_height`), so a new line is detected by the vertical seam.
fn merge_link_annotations(spans: &[(usize, Option<String>, Rect)]) -> Vec<DocumentAnnotation> {
    /// Two same-line rects never differ vertically; this only absorbs f32
    /// noise, not layout differences.
    const LINE_EPSILON: f32 = 0.01;

    let mut out = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        let target = spans[i].0;
        let mut j = i;
        while j < spans.len() && spans[j].0 == target {
            j += 1;
        }

        let mut quads: Vec<Rect> = Vec::new();
        for (_, _, r) in &spans[i..j] {
            match quads.last_mut() {
                Some(q)
                    if (q.y - r.y).abs() <= LINE_EPSILON
                        && (q.height - r.height).abs() <= LINE_EPSILON =>
                {
                    let x0 = q.x.min(r.x);
                    let x1 = (q.x + q.width).max(r.x + r.width);
                    q.x = x0;
                    q.width = x1 - x0;
                }
                _ => quads.push(r.clone()),
            }
        }
        let rect = quads.iter().skip(1).fold(quads[0].clone(), |acc, r| {
            let x0 = acc.x.min(r.x);
            let y0 = acc.y.min(r.y);
            let x1 = (acc.x + acc.width).max(r.x + r.width);
            let y1 = (acc.y + acc.height).max(r.y + r.height);
            Rect {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            }
        });
        // A single-line link needs no quadpoints beyond its rect — PDFium
        // reports quadpoints there too, but one redundant rect adds noise for
        // the consumer; multi-line is where quadpoints carry information.
        let quadpoint_rects = if quads.len() > 1 { quads } else { Vec::new() };

        out.push(DocumentAnnotation {
            subtype: "link".to_string(),
            contents: None,
            created: None,
            modified: None,
            title: None,
            rect: Some(rect),
            quadpoint_rects,
            uri: spans[i].1.clone(),
        });
        i = j;
    }
    out
}

/// Native images tapped from the draw-command stream.
pub struct NativeImages {
    /// One entry per surfaced placement, in (page, draw-order). Duplicate
    /// placements of one media entry carry `duplicate_of` and share the
    /// canonical entry's bytes, matching the PDF path's dedup contract.
    pub images: Vec<ExtractedImage>,
    /// Media identity (`MediaEntry::data` allocation pointer) → FIFO of
    /// `(figure id, extension)` in draw order. The block emitter pops from
    /// these queues to give its `Block::Figure`s the layout-assigned
    /// page-scoped ids, joining the two walks without guessing.
    pub figure_ids: HashMap<usize, VecDeque<(String, String)>>,
}

/// File extension for a media format we surface as an extracted image.
///
/// `None` skips the image entirely: EMF/WMF are vector metafiles and SVG is
/// XML — the PDF path only ever emits raster jpg/png (PDFium rasterizes
/// everything), so surfacing bytes most consumers cannot decode would be a
/// worse contract than omitting them. Word's SVG embeds normally carry a PNG
/// fallback blip, which is the one the layout draws.
///
/// Shared with the PPTX figure emitter rather than mirrored there: two lists of
/// "formats we surface" would compile, run, and disagree the first time one
/// gained a format, and the disagreement would show up as a format that
/// extracts on one office path and silently vanishes on the other.
pub(crate) fn media_extension(format: liteparse_ooxml::model::ImageFormat) -> Option<&'static str> {
    use liteparse_ooxml::model::ImageFormat as F;
    match format {
        F::Png => Some("png"),
        F::Jpeg => Some("jpg"),
        F::Gif => Some("gif"),
        F::Bmp => Some("bmp"),
        F::Tiff => Some("tiff"),
        F::WebP => Some("webp"),
        F::Svg | F::Emf | F::Wmf | F::Unknown => None,
    }
}

/// Collect embedded images from the draw-command stream.
///
/// Ids follow the platform extractor's `p{page}_{n}` naming (1-based, per
/// page, in draw order) so `img_{id}.{ext}` file names line up with the PDF
/// path's. Bytes are the *original* embedded media — no re-encode, unlike the
/// PDF path which rasterizes through PDFium — so a JPEG stays the exact JPEG
/// the author inserted. `src_rect` crops are not applied to the bytes; `bbox`
/// is the placed rectangle either way.
pub fn collect_images(layouted: &[LayoutedPage]) -> NativeImages {
    use std::hash::{Hash, Hasher};

    let mut images: Vec<ExtractedImage> = Vec::new();
    let mut figure_ids: HashMap<usize, VecDeque<(String, String)>> = HashMap::new();
    // Same media Arc ⇒ same bytes, free dedup for the common repeated-rel
    // case (a logo in every page's header is ONE relationship).
    let mut by_ptr: HashMap<usize, usize> = HashMap::new();
    // Distinct rels can still hold identical bytes; hash → candidate
    // canonical indices, confirmed by full compare like the PDF path's cache.
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();

    for (page_idx, lp) in layouted.iter().enumerate() {
        let page_number = (page_idx + 1) as u32;
        let mut n = 0u32;
        for cmd in &lp.commands {
            let DrawCommand::Image {
                rect, image_data, ..
            } = cmd
            else {
                continue;
            };
            let Some(ext) = media_extension(image_data.format) else {
                continue;
            };
            n += 1;
            let id = format!("p{page_number}_{n}");
            let bbox = Rect {
                x: rect.origin.x.raw(),
                y: rect.origin.y.raw(),
                width: rect.size.width.raw(),
                height: rect.size.height.raw(),
            };
            let ptr = image_data.data.as_ptr() as usize;

            let canonical_idx = by_ptr.get(&ptr).copied().or_else(|| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                image_data.data.hash(&mut h);
                by_hash
                    .get(&h.finish())?
                    .iter()
                    .copied()
                    .find(|&i| *images[i].bytes == *image_data.data)
            });

            let entry = if let Some(ci) = canonical_idx {
                let canonical = &images[ci];
                ExtractedImage {
                    id: id.clone(),
                    name: format!("img_{id}.{ext}"),
                    path: None,
                    page: page_number,
                    bbox,
                    width: canonical.width,
                    height: canonical.height,
                    rotation: 0.0,
                    format: ext.to_string(),
                    duplicate_of: Some(canonical.id.clone()),
                    bytes: std::sync::Arc::clone(&canonical.bytes),
                }
            } else {
                let bytes = std::sync::Arc::new(image_data.data.to_vec());
                let (width, height) =
                    image::ImageReader::new(std::io::Cursor::new(bytes.as_slice()))
                        .with_guessed_format()
                        .ok()
                        .and_then(|r| r.into_dimensions().ok())
                        .unwrap_or((0, 0));
                ExtractedImage {
                    id: id.clone(),
                    name: format!("img_{id}.{ext}"),
                    path: None,
                    page: page_number,
                    bbox,
                    width,
                    height,
                    rotation: 0.0,
                    format: ext.to_string(),
                    duplicate_of: None,
                    bytes,
                }
            };

            if canonical_idx.is_none() {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                image_data.data.hash(&mut h);
                by_hash.entry(h.finish()).or_default().push(images.len());
                by_ptr.insert(ptr, images.len());
            }
            figure_ids
                .entry(ptr)
                .or_default()
                .push_back((id, ext.to_string()));
            images.push(entry);
        }
    }

    NativeImages { images, figure_ids }
}

/// Placed-media rects per page, for the native complexity stats. Every
/// `Image` command counts regardless of media format — EMF/SVG placements are
/// visual content even though [`collect_images`] skips their bytes.
pub fn image_rects_per_page(layouted: &[LayoutedPage]) -> Vec<Vec<Rect>> {
    layouted
        .iter()
        .map(|lp| {
            lp.commands
                .iter()
                .filter_map(|cmd| match cmd {
                    DrawCommand::Image { rect, .. } => Some(Rect {
                        x: rect.origin.x.raw(),
                        y: rect.origin.y.raw(),
                        width: rect.size.width.raw(),
                        height: rect.size.height.raw(),
                    }),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

/// Section-declared column count per physical page (§17.6.4 `w:cols`), for
/// the native complexity stats.
///
/// The join reuses the `block_starts` channel: a flattened body-block index
/// locates its section by cumulative section length (the same flattening
/// `emit_with_sources` tags blocks with), and `block_pages` says which page
/// each block started on. A page hosting blocks from several sections (a
/// `Continuous` break) takes the max. Pages with no block start — pure
/// continuation pages — inherit the previous page's count, and anything
/// before the first anchored block falls back to 1.
pub fn page_column_counts(
    resolved: &liteparse_ooxml::render::resolve::ResolvedDocument,
    block_pages: &HashMap<usize, usize>,
    n_pages: usize,
) -> Vec<usize> {
    let sections: Vec<(usize, usize)> = resolved
        .sections
        .iter()
        .map(|s| {
            let cols = s
                .properties
                .columns
                .as_ref()
                .and_then(|c| c.count)
                .unwrap_or(1)
                .max(1) as usize;
            (s.blocks.len(), cols)
        })
        .collect();
    column_counts_from_sections(&sections, block_pages, n_pages)
}

/// Pure core of [`page_column_counts`]: `sections` is `(block count, column
/// count)` per section, in document order.
fn column_counts_from_sections(
    sections: &[(usize, usize)],
    block_pages: &HashMap<usize, usize>,
    n_pages: usize,
) -> Vec<usize> {
    // Flat-index boundary each section ends at.
    let mut sec_end = Vec::with_capacity(sections.len());
    let mut cum = 0usize;
    for (len, _) in sections {
        cum += len;
        sec_end.push(cum);
    }
    let section_of = |flat: usize| sec_end.partition_point(|&end| end <= flat);

    let mut cols = vec![0usize; n_pages];
    for (&flat, &page) in block_pages {
        let s = section_of(flat);
        if let (Some(slot), Some(&(_, c))) = (cols.get_mut(page), sections.get(s)) {
            *slot = (*slot).max(c);
        }
    }
    let mut prev = 1usize;
    for c in &mut cols {
        if *c == 0 {
            *c = prev;
        } else {
            prev = *c;
        }
    }
    cols
}

fn union_bounds(items: &[TextItem]) -> Option<Rect> {
    let mut it = items.iter();
    let first = it.next()?;
    let (mut x0, mut y0) = (first.x, first.y);
    let (mut x1, mut y1) = (first.x + first.width, first.y + first.height);
    for i in it {
        x0 = x0.min(i.x);
        y0 = y0.min(i.y);
        x1 = x1.max(i.x + i.width);
        y1 = y1.max(i.y + i.height);
    }
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse_ooxml::render::dimension::Pt;
    use liteparse_ooxml::render::geometry::{PtOffset, PtRect, PtSize};
    use liteparse_ooxml::render::layout::draw_command::OutlineHeading;
    use liteparse_ooxml::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn registry() -> FontRegistry {
        FontRegistry::new()
    }

    fn text_cmd(x: f32, y: f32, text: &str, size: f32) -> DrawCommand {
        DrawCommand::Text {
            position: PtOffset::new(Pt::new(x), Pt::new(y)),
            text: Rc::from(text),
            font_family: Rc::from("Arial"),
            char_spacing: Pt::ZERO,
            font_size: Pt::new(size),
            bold: false,
            italic: false,
            color: RgbColor::BLACK,
            text_scale: 1.0,
        }
    }

    fn page(commands: Vec<DrawCommand>) -> LayoutedPage {
        LayoutedPage {
            commands,
            page_size: PtSize::new(Pt::new(612.0), Pt::new(792.0)),
            block_starts: Vec::new(),
            header_blocks: Vec::new(),
            footer_blocks: Vec::new(),
        }
    }

    #[test]
    fn text_bbox_hangs_from_the_baseline_by_ascent() {
        let reg = registry();
        let measurer = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let (advance, metrics) = measurer.measure("Hello", &props);

        let out = layout_to_pages(
            &[page(vec![text_cmd(72.0, 100.0, "Hello", 12.0)])],
            &reg,
            true,
            false,
        );
        let item = &out.pages[0].text_items[0];
        assert_eq!(item.x, 72.0);
        assert_eq!(item.y, 100.0 - metrics.ascent.raw());
        assert_eq!(item.width, advance.raw());
        assert_eq!(item.height, metrics.ascent.raw() + metrics.descent.raw());
        assert_eq!(item.font_descent, Some(-metrics.descent.raw()));
        assert_eq!(out.pages[0].page_number, 1);
        // Word boxes partition the advance: two words, gap between them.
        let out2 = layout_to_pages(
            &[page(vec![text_cmd(0.0, 100.0, "ab cd", 12.0)])],
            &reg,
            true,
            false,
        );
        let words = &out2.pages[0].text_items[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].x, 0.0);
        assert!(
            words[1].x > words[0].x + words[0].width,
            "gap for the space"
        );
    }

    #[test]
    fn links_attach_to_covered_items_only() {
        let reg = registry();
        let m = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let (w, metrics) = m.measure("click", &props);
        let link_rect = PtRect::from_xywh(
            Pt::new(72.0),
            Pt::new(100.0 - metrics.ascent.raw()),
            w,
            Pt::new(metrics.ascent.raw() + metrics.descent.raw()),
        );
        let out = layout_to_pages(
            &[page(vec![
                text_cmd(72.0, 100.0, "click", 12.0),
                text_cmd(72.0, 300.0, "plain", 12.0),
                DrawCommand::LinkAnnotation {
                    rect: link_rect,
                    url: Rc::from("https://example.com"),
                },
            ])],
            &reg,
            false,
            false,
        );
        let items = &out.pages[0].text_items;
        assert_eq!(items[0].link.as_deref(), Some("https://example.com"));
        assert_eq!(items[1].link, None);
    }

    #[test]
    fn outline_marks_become_targets_with_pdf_space_y() {
        let reg = registry();
        let m = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let ascent = m.measure("Title", &props).1.ascent.raw();
        let out = layout_to_pages(
            &[page(vec![
                DrawCommand::Outline(OutlineMark::Begin(OutlineHeading {
                    node_id: 1,
                    level: liteparse_ooxml::model::OutlineLevel::new(2),
                    title: Rc::from("Title"),
                })),
                text_cmd(72.0, 100.0, "Title", 12.0),
                DrawCommand::Outline(OutlineMark::End),
            ])],
            &reg,
            false,
            false,
        );
        assert_eq!(out.outline.len(), 1);
        let t = &out.outline[0];
        assert_eq!((t.level, t.page_index), (2, 0));
        assert_eq!(t.title, "Title");
        // Bottom-left PDF space: page_height − viewport top of the heading.
        assert_eq!(t.y_pdf, Some(792.0 - (100.0 - ascent)));
    }

    #[test]
    fn block_starts_become_a_min_page_map() {
        let reg = registry();
        let mut p0 = page(vec![]);
        p0.block_starts = vec![3, 4];
        let mut p1 = page(vec![]);
        p1.block_starts = vec![4, 5];
        let out = layout_to_pages(&[p0, p1], &reg, false, false);
        assert_eq!(out.block_pages[&3], 0);
        assert_eq!(out.block_pages[&4], 0, "duplicate takes the earliest page");
        assert_eq!(out.block_pages[&5], 1);
    }

    fn link_rect(x: f32, y: f32, w: f32, h: f32) -> PtRect {
        PtRect::from_xywh(Pt::new(x), Pt::new(y), Pt::new(w), Pt::new(h))
    }

    /// One two-line hyperlink (one shared `Rc` target, three word rects) must
    /// become ONE annotation whose rect is the union and whose quadpoints
    /// carry one merged rect per line.
    #[test]
    fn multi_line_hyperlink_merges_to_one_annotation() {
        let reg = registry();
        let url: Rc<str> = Rc::from("https://example.com");
        let out = layout_to_pages(
            &[page(vec![
                DrawCommand::LinkAnnotation {
                    rect: link_rect(72.0, 100.0, 30.0, 14.0),
                    url: Rc::clone(&url),
                },
                DrawCommand::LinkAnnotation {
                    rect: link_rect(105.0, 100.0, 40.0, 14.0),
                    url: Rc::clone(&url),
                },
                DrawCommand::LinkAnnotation {
                    rect: link_rect(72.0, 114.0, 25.0, 14.0),
                    url: Rc::clone(&url),
                },
            ])],
            &reg,
            false,
            true,
        );
        let anns = out.pages[0].annotations.as_ref().unwrap();
        assert_eq!(anns.len(), 1);
        let a = &anns[0];
        assert_eq!(a.subtype, "link");
        assert_eq!(a.uri.as_deref(), Some("https://example.com"));
        let r = a.rect.clone().unwrap();
        assert_eq!((r.x, r.y), (72.0, 100.0));
        assert_eq!((r.width, r.height), (73.0, 28.0));
        assert_eq!(a.quadpoint_rects.len(), 2, "one merged rect per line");
        assert_eq!(a.quadpoint_rects[0].width, 73.0);
        assert_eq!(a.quadpoint_rects[1].width, 25.0);
    }

    /// Two hyperlinks to the SAME url are distinct `Rc` targets and must stay
    /// two annotations; a single-line link carries no redundant quadpoints.
    #[test]
    fn distinct_hyperlink_instances_stay_separate_annotations() {
        let reg = registry();
        let out = layout_to_pages(
            &[page(vec![
                DrawCommand::LinkAnnotation {
                    rect: link_rect(72.0, 100.0, 30.0, 14.0),
                    url: Rc::from("https://example.com"),
                },
                DrawCommand::LinkAnnotation {
                    rect: link_rect(200.0, 100.0, 30.0, 14.0),
                    url: Rc::from("https://example.com"),
                },
            ])],
            &reg,
            false,
            true,
        );
        let anns = out.pages[0].annotations.as_ref().unwrap();
        assert_eq!(anns.len(), 2);
        assert!(anns.iter().all(|a| a.quadpoint_rects.is_empty()));
    }

    /// Internal links (TOC/cross-refs) become uri-less `link` annotations —
    /// the GoTo shape — and never attach `TextItem::link`.
    #[test]
    fn internal_links_become_uriless_annotations() {
        let reg = registry();
        let out = layout_to_pages(
            &[page(vec![
                text_cmd(72.0, 100.0, "See chapter 3", 12.0),
                DrawCommand::InternalLink {
                    rect: link_rect(72.0, 88.0, 80.0, 14.0),
                    destination: Rc::from("_Toc123"),
                },
            ])],
            &reg,
            false,
            true,
        );
        let anns = out.pages[0].annotations.as_ref().unwrap();
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].subtype, "link");
        assert_eq!(anns[0].uri, None);
        assert_eq!(
            out.pages[0].text_items[0].link, None,
            "internal anchors must not become TextItem links"
        );
    }

    /// Section columns land on the pages their blocks start on; a page hosting
    /// two sections (Continuous break) takes the max; pure continuation pages
    /// inherit the previous page's count; leading unanchored pages default 1.
    #[test]
    fn column_counts_join_sections_to_pages() {
        // Section 0: 3 blocks, 2 columns. Section 1: 2 blocks, 1 column.
        let sections = [(3usize, 2usize), (2, 1)];
        let block_pages: HashMap<usize, usize> =
            [(0, 0), (2, 1), (3, 1), (4, 2)].into_iter().collect();
        assert_eq!(
            column_counts_from_sections(&sections, &block_pages, 5),
            vec![2, 2, 1, 1, 1],
            "page 1 mixes both sections (max), pages 3-4 inherit"
        );

        // No block anchored before page 1: leading page falls back to 1.
        let late: HashMap<usize, usize> = [(0, 1)].into_iter().collect();
        assert_eq!(
            column_counts_from_sections(&sections, &late, 3),
            vec![1, 2, 2]
        );
    }

    /// Flag off: `annotations` stays `None` (disabled ≠ enabled-but-empty),
    /// and pages with the flag on but no links report `Some([])`.
    #[test]
    fn annotations_none_when_disabled_some_empty_when_enabled() {
        let reg = registry();
        let cmds = vec![text_cmd(72.0, 100.0, "plain", 12.0)];
        let off = layout_to_pages(&[page(cmds.clone())], &reg, false, false);
        assert!(off.pages[0].annotations.is_none());
        let on = layout_to_pages(&[page(cmds)], &reg, false, true);
        assert!(
            on.pages[0]
                .annotations
                .as_ref()
                .is_some_and(|v| v.is_empty())
        );
    }
}
