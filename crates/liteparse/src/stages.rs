//! The parse pipeline as public stage functions.
//!
//! [`LiteParse::parse`](crate::LiteParse::parse) is a fixed sequence of
//! stages: open → orientation → extract → complexity → OCR render →
//! recognition → OCR merge → content filters → grid projection → block
//! classification → markdown rendering. This module exposes each stage as a
//! function over serializable types so a caller can run the same sequence
//! itself and insert its own inputs between stages — its own OCR engine, a
//! page selection, extra blocks — or put a process boundary between any two.
//!
//! Two invariants hold this together:
//!
//! * `parse()` is implemented *only* through these functions (see
//!   `parser.rs`), and an integration test composes them by hand and asserts
//!   the result equals `parse()`. A step cannot be added to `parse()` without
//!   becoming a public stage.
//! * Every type crossing a stage boundary is `Clone + Serialize +
//!   Deserialize`, and the serialization is lossless for everything a later
//!   stage reads.
//!
//! Stages split into two kinds. **pdfium-bound** stages take an open
//! [`Document`] and must run while the [`Library`] that opened it is alive
//! (PDFium is single-threaded behind a process-global lock, so keep these
//! sections short and never hold a `Document` across an `.await`).
//! **Pure** stages take and return plain data and can run anywhere.
//!
//! | Stage | Function | Kind |
//! | --- | --- | --- |
//! | open + orientation | [`open`], [`apply_orientation`] | pdfium |
//! | document facts | [`outline`], [`xfa_packets`], [`document_metadata`] | pdfium |
//! | extract | [`extract`] | pdfium |
//! | complexity | [`page_complexity`], [`layout_complexity`] | pdfium / pure |
//! | OCR | [`render_for_ocr`] → [`recognize`] → [`merge_ocr`] | pdfium / any / pure |
//! | content filters | [`apply_content_filters`] | pure |
//! | projection | [`project`] | pure |
//! | markdown | [`document_signals`] → [`extract_blocks`] → [`render_page_markdown`] | pure |
//! | screenshots | [`screenshots`] | pdfium |
//!
//! Everything else in the crate that is not re-exported here is unstable.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{CropBox, ImageMode, PageOrientationCorrection};
use crate::error::LiteParseError;
use crate::extract;
use crate::markdown_layout;
use crate::ocr::OcrEngine;
use crate::ocr_merge;
use crate::projection;
use crate::render;
use crate::types::{
    DocumentMetadata, ExtractedImage, OutlineTarget, Page, ParsedPage, PdfInput, XfaPacket,
};

pub use crate::extract::{ExtractedPages, ExtractionOutputOptions};
pub use crate::layout::{LayoutBlock, LayoutCell};
pub use crate::markdown_layout::{Block, Cell, PositionedBlock, SpanCell, render_blocks};
pub use crate::ocr::OcrResult;
pub use crate::ocr_merge::{
    ComplexityReason, LayoutComplexityReason, LayoutComplexityStats, OcrRaster, OcrRenderOptions,
    PageComplexityStats, PageOcrOutcome,
};
pub use crate::parser::ScreenshotResult;
/// The PDFium handle types the pdfium-bound stages take. Re-exported so a
/// caller needs no direct dependency on the pdfium crate.
pub use pdfium::{Document, Library};

// ── Open ───────────────────────────────────────────────────────────────

/// Open `input` and apply per-page orientation corrections.
///
/// `/Rotate` rewrites live in the open document only, so every reopen of the
/// same input (extraction, OCR rounds, screenshots) must go through this
/// function with the same `corrections` to see the same page geometry.
pub fn open<'lib>(
    lib: &'lib Library,
    input: &PdfInput,
    password: Option<&str>,
    corrections: &[PageOrientationCorrection],
) -> Result<Document<'lib>, LiteParseError> {
    let document = extract::load_document_from_input(lib, input, password)?;
    apply_orientation(&document, corrections)?;
    Ok(document)
}

/// Counter-rotate the pages named in `corrections` so their content reads
/// upright. [`open`] already does this; call it directly only on a document
/// opened some other way.
pub fn apply_orientation(
    document: &Document,
    corrections: &[PageOrientationCorrection],
) -> Result<(), LiteParseError> {
    extract::apply_page_orientation_corrections(document, corrections)
}

/// Rewrite an AcroForm whose widgets are orphaned from their pages so
/// form-field extraction can see them. Returns the repaired input when a
/// rewrite was needed, `None` when the document is fine as is. The result is
/// what to [`open`] for extraction; provenance facts should still come from
/// the original input.
#[cfg(not(target_arch = "wasm32"))]
pub fn repair_acroform(
    lib: &Library,
    input: &PdfInput,
    password: Option<&str>,
) -> Option<PdfInput> {
    crate::acroform_repair::repair_orphaned_widgets(lib, input, password)
}

// ── Document facts ─────────────────────────────────────────────────────

/// Walk the document outline (bookmarks). Document-level, and several times
/// more expensive than opening the document, so a caller parsing in page
/// batches should do it once.
pub fn outline(document: &Document) -> Vec<OutlineTarget> {
    extract::extract_outline(document)
}

/// Raw XFA packets from the document's `/XFA` array. Empty for a non-XFA
/// document.
pub fn xfa_packets(document: &Document) -> Vec<XfaPacket> {
    document
        .xfa_packets()
        .into_iter()
        .map(|packet| XfaPacket {
            index: packet.index.max(0) as u32,
            name: packet.name,
            content_length: packet
                .content
                .as_ref()
                .map_or(0, |content| content.len() as u32),
            content: packet
                .content
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        })
        .collect()
}

/// Provenance metadata (dates, security, signatures, incremental-save
/// markers, XMP). `input` must be the file the facts describe: pass the
/// original input, not an AcroForm-repaired or format-converted copy.
pub fn document_metadata(input: &PdfInput, document: &Document) -> DocumentMetadata {
    crate::document_metadata::extract(input, document)
}

// ── Extract ────────────────────────────────────────────────────────────

/// What [`extract`] pulls out of the document.
#[derive(Clone, Copy)]
pub struct ExtractRequest<'a> {
    /// 1-based pages to extract; `None` means every page.
    pub target_pages: Option<&'a [u32]>,
    /// Upper bound on extracted pages, applied after `target_pages`.
    pub max_pages: usize,
    /// Resolve link annotations onto the text items they cover.
    pub extract_links: bool,
    /// Last-resort glyph recovery for buggy fonts (see [`crate::GlyphResolver`]).
    pub glyph_resolver: Option<&'a dyn crate::GlyphResolver>,
    /// Which optional outputs to produce (images, word boxes, form fields…).
    pub output: ExtractionOutputOptions,
}

impl Default for ExtractRequest<'_> {
    fn default() -> Self {
        Self {
            target_pages: None,
            max_pages: usize::MAX,
            extract_links: false,
            glyph_resolver: None,
            output: ExtractionOutputOptions::default(),
        }
    }
}

/// Extract text items, graphics, structure nodes and (optionally) images
/// from an open document. This is the only stage that reads page content
/// through PDFium; everything downstream works on the returned [`Page`]s.
pub fn extract(
    document: &Document,
    request: &ExtractRequest<'_>,
) -> Result<ExtractedPages, LiteParseError> {
    extract::extract_pages_and_images(
        document,
        request.target_pages,
        request.max_pages,
        request.extract_links,
        request.glyph_resolver,
        request.output,
    )
}

// ── Complexity ─────────────────────────────────────────────────────────

/// Score one extracted page: does it need OCR, and why. Reads page objects
/// (images, filled paths) through PDFium, so it runs against the document
/// the page was extracted from. `layout` is left unset; see
/// [`layout_complexity`] for the post-projection half.
pub fn page_complexity(
    document: &Document,
    page: &Page,
) -> Result<PageComplexityStats, LiteParseError> {
    let page_obj = document.page((page.page_number - 1) as i32)?;
    ocr_merge::calculate_page_complexity(page, &page_obj)
}

/// Layout-difficulty signals (columns, tables, dense graphics) for a
/// projected page. Pure; fills `PageComplexityStats::layout`.
pub fn layout_complexity(page: &ParsedPage) -> LayoutComplexityStats {
    ocr_merge::calculate_layout_complexity(page)
}

// ── OCR ────────────────────────────────────────────────────────────────

/// Rasterize the pages in `pages[start..]` that need OCR (or that
/// `options.selection` names), stopping after `options.max_rasters`. Returns
/// the rasters and the index to resume from, so a long document runs in
/// bounded rounds. Drop the [`Document`] before awaiting recognition.
pub fn render_for_ocr(
    document: &Document,
    pages: &[Page],
    start: usize,
    options: &OcrRenderOptions,
) -> Result<(Vec<OcrRaster>, usize), LiteParseError> {
    ocr_merge::render_pages_for_ocr(document, pages, start, options)
}

/// Run an [`OcrEngine`] over rasters, at most `num_workers` at a time. Pure
/// recognition; failures are per-outcome, not errors. A caller with its own
/// OCR service can skip this and build [`PageOcrOutcome`]s directly from
/// the [`OcrRaster`] facts and its engine's word boxes (raster pixel space).
pub async fn recognize(
    rasters: Vec<OcrRaster>,
    engine: Arc<dyn OcrEngine>,
    language: &str,
    num_workers: usize,
) -> Vec<PageOcrOutcome> {
    ocr_merge::recognize_rasters(rasters, engine, language, num_workers).await
}

/// Merge recognition outcomes into `pages` in place: drop unusable native
/// text, filter engine artifacts, append the surviving results as `OCR`
/// text items in viewport points. Errors only when every outcome failed and
/// one of the failed pages had no usable native text (`ocr_failure_fatal`).
pub fn merge_ocr(
    pages: &mut [Page],
    outcomes: Vec<PageOcrOutcome>,
    ocr_failure_fatal: bool,
) -> Result<(), LiteParseError> {
    ocr_merge::merge_ocr_results(pages, outcomes, ocr_failure_fatal)
}

// ── Content filters ────────────────────────────────────────────────────

/// Caller-requested item filters applied between OCR merge and projection.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContentFilters<'a> {
    /// Keep only items lying entirely inside the surviving page region.
    pub crop_box: Option<&'a CropBox>,
    /// Drop skewed text (watermarks, rotated stamps).
    pub skip_diagonal_text: bool,
}

/// Apply [`ContentFilters`] to `pages` in place. Runs after [`merge_ocr`] so
/// OCR text is filtered too, and before [`project`] so removed items never
/// reach the output. No-op when no filter is requested.
pub fn apply_content_filters(pages: &mut [Page], filters: &ContentFilters<'_>) {
    extract::apply_content_filters(pages, filters.crop_box, filters.skip_diagonal_text);
}

// ── Projection ─────────────────────────────────────────────────────────

/// Grid projection: turn each page's items into plain text with spatial
/// layout, plus the per-line structure the markdown stages read. Pure and
/// per page, so it can run over any page subset.
pub fn project(pages: Vec<Page>) -> Vec<ParsedPage> {
    projection::project_pages_to_grid(pages)
}

// ── Markdown ───────────────────────────────────────────────────────────

/// Whole-document signals the block classifier needs: computed once over
/// every page, then applied per page by [`extract_blocks`]. Heading levels
/// are ranked against the document's body font size, and running
/// headers/footers are the lines that repeat across pages — neither is
/// knowable from one page alone. A caller that classifies pages in batches
/// must compute this over the whole document first, or heading levels drift
/// between batches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentSignals {
    /// Dominant body font size in points; `0.0` when no page has sized lines.
    pub body_size: f32,
    /// `(font_size, heading_level)` pairs, largest size first, levels 1..=6.
    pub heading_map: Vec<(f32, u8)>,
    /// Normalized texts of running headers/footers to suppress. Empty when
    /// `keep_headers_footers` was set or the document is too short to tell.
    pub header_footer: HashSet<String>,
}

/// Compute [`DocumentSignals`] over every page of the document.
pub fn document_signals(pages: &[ParsedPage], keep_headers_footers: bool) -> DocumentSignals {
    if pages.is_empty() {
        return DocumentSignals::default();
    }
    let body_size = markdown_layout::compute_body_size(pages);
    let heading_map = markdown_layout::build_heading_map(pages, body_size);
    let header_footer = if keep_headers_footers {
        HashSet::new()
    } else {
        markdown_layout::compute_header_footer_set(pages)
    };
    DocumentSignals {
        body_size,
        heading_map,
        header_footer,
    }
}

/// Per-document inputs to [`extract_blocks`] other than the signals.
#[derive(Clone, Copy)]
pub struct BlockOptions<'a> {
    /// Document outline; entries for the page act as a high-priority heading
    /// source on untagged PDFs.
    pub outline: &'a [OutlineTarget],
    /// Whether and how figures appear in the block list.
    pub image_mode: ImageMode,
    /// Disable running-chrome suppression (both the cross-page set in the
    /// signals and the single-page detector).
    pub keep_headers_footers: bool,
}

/// Classify one projected page into its final block sequence: headings,
/// paragraphs, lists, code, tables, rules, figures — after chrome
/// suppression, rule dedup and soft-hyphen splicing. This is exactly the
/// decomposition [`render_page_markdown`] renders and
/// `ParsedPage::blocks` reports.
///
/// Returns `None` for a page with no projected lines (blank, or fully OCR
/// without font metadata): there is no structure to report, and rendering
/// falls back to the projection text in a fence.
pub fn extract_blocks(
    page: &ParsedPage,
    signals: &DocumentSignals,
    options: &BlockOptions<'_>,
) -> Option<Vec<PositionedBlock>> {
    if page.projected_lines.is_empty() {
        return None;
    }

    // Filter outline entries to this page so the classifier's y/title match
    // is a O(entries_on_page) scan per line, not O(whole doc).
    let target_index = (page.page_number as i32).saturating_sub(1);
    let page_outline: Vec<OutlineTarget> = options
        .outline
        .iter()
        .filter(|e| e.page_index == target_index)
        .cloned()
        .collect();
    let chrome_indices = if options.keep_headers_footers {
        HashSet::new()
    } else {
        markdown_layout::detect_single_page_chrome(page, signals.body_size)
    };
    let mut blocks = markdown_layout::classify_page_with_filters(
        page,
        &signals.heading_map,
        &signals.header_footer,
        &page_outline,
        options.image_mode,
        &chrome_indices,
    );
    // A page whose only surviving blocks are horizontal rules (all its text
    // was stripped as chrome) should render empty, not as a stack of bare
    // `---` separators.
    let has_content = blocks
        .iter()
        .any(|b| !matches!(b.block, Block::HorizontalRule | Block::Figure { .. }));
    if !has_content {
        blocks.retain(|b| !matches!(b.block, Block::HorizontalRule));
    }
    crate::output::markdown::dedupe_rules(&mut blocks);
    Some(markdown_layout::splice_soft_hyphens(blocks))
}

/// Render one page's markdown from its blocks. A page that classified to
/// `None` renders its projection text inside a ```` ```text ```` fence so
/// nothing is silently dropped.
pub fn render_page_markdown(page: &ParsedPage, blocks: Option<&[PositionedBlock]>) -> String {
    match blocks {
        Some(blocks) => render_blocks(blocks),
        None => {
            let mut out = String::from("```text\n");
            out.push_str(&page.text);
            if !page.text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```");
            out
        }
    }
}

/// The public, flat view of a page's blocks (`ParsedPage::blocks`), with
/// block boxes mapped back from the projected frame into viewport space.
pub fn layout_blocks(page: &ParsedPage, blocks: &[PositionedBlock]) -> Vec<LayoutBlock> {
    crate::layout::blocks_for_page(page, blocks)
}

/// Point duplicate-image figure references at the canonical image's file
/// name, in every page's markdown and in `full_text`. Only the canonical
/// image is written to disk, so a `![](img_p2_1.jpg)` for a duplicate
/// placement would otherwise reference a file that does not exist.
pub fn canonicalize_image_refs(
    pages: &mut [ParsedPage],
    full_text: &mut String,
    images: &[ExtractedImage],
) {
    crate::parser::rewrite_duplicate_image_refs(pages, full_text, images)
}

// ── Screenshots ────────────────────────────────────────────────────────

/// How [`screenshots`] renders pages.
#[derive(Debug, Clone, Copy)]
pub struct ScreenshotOptions {
    pub dpi: f32,
    /// Detect solid rectangles/lines in the raster (`ScreenshotResult::rects`).
    pub detect_rects: bool,
    /// Paint form-field appearances (runs document actions).
    pub render_form_fields: bool,
    /// Skip pages that fail to render instead of failing the call.
    pub continue_on_page_error: bool,
}

/// Render pages to PNG. `page_numbers` is 1-based; `None` renders every
/// page.
pub fn screenshots(
    document: &Document,
    page_numbers: Option<&[u32]>,
    options: &ScreenshotOptions,
) -> Result<Vec<ScreenshotResult>, LiteParseError> {
    Ok(render::render_document_pages(
        document,
        page_numbers,
        options.dpi,
        options.detect_rects,
        options.render_form_fields,
        options.continue_on_page_error,
    )?
    .into_iter()
    .map(|page| ScreenshotResult {
        page_num: page.page_num,
        width: page.width,
        height: page.height,
        image_bytes: page.png_bytes,
        is_solid_fill: page.is_solid_fill,
        rects: page.rects,
    })
    .collect())
}
