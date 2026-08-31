use crate::config::{LiteParseConfig, parse_target_pages};
#[cfg(not(target_arch = "wasm32"))]
use crate::conversion;
use crate::error::LiteParseError;
use crate::extract;
use crate::ocr::OcrEngine;
#[cfg(not(target_arch = "wasm32"))]
use crate::ocr::http_simple::HttpOcrEngine;
#[cfg(feature = "tesseract")]
use crate::ocr::tesseract::TesseractOcrEngine;
use crate::ocr_merge;
use crate::output::markdown;
use crate::projection;
#[cfg(not(target_arch = "wasm32"))]
use crate::render;
use crate::types::{
    DocumentMetadata, ExtractedImage, OutlineTarget, Page, ParsedPage, PdfInput, ScreenshotRect,
    XfaPacket,
};
use pdfium::Library;

/// Result of parsing a document.
pub struct ParseResult {
    /// Parsed pages with projected text layout.
    pub pages: Vec<ParsedPage>,
    /// Full document text, concatenated from all pages.
    pub text: String,
    /// Document outline (bookmarks) when present. Used by the markdown
    /// emitter as a high-priority heading source on untagged PDFs.
    pub outline: Vec<OutlineTarget>,
    /// Raster images extracted from the document. Empty unless the parser
    /// was configured with `extract_images`. Each entry carries the same
    /// `id` and `format` the markdown emitter referenced, so the caller can
    /// match them up without parsing markdown.
    pub images: Vec<ExtractedImage>,
    /// Number of embedded image objects that could not be extracted. A bad
    /// image does not fail the rest of the document parse.
    pub image_error_count: u32,
    /// PDFium form type (0 none, 1 AcroForm, 2 XFA full, 3 XFA foreground),
    /// present only when form-field extraction is enabled.
    pub form_type: Option<i32>,
    /// The document's `/Info` `Creator` entry, when present.
    pub creator: Option<String>,
    /// The document's `/Info` `Producer` entry, when present.
    pub producer: Option<String>,
    /// Document provenance metadata (dates, version/security, signatures,
    /// incremental-save markers, trailer IDs, raw XMP, and source size).
    /// Present only when `extract_document_metadata` is enabled, and `None`
    /// for inputs converted from a non-PDF format.
    pub doc_meta: Option<DocumentMetadata>,
    /// Raw XFA packets, present only when `extract_xfa_packets` is enabled.
    /// `Some([])` means extraction ran on a non-XFA document.
    pub xfa_packets: Option<Vec<XfaPacket>>,
}

/// Result of rendering a single page screenshot.
#[derive(Debug, Clone)]
pub struct ScreenshotResult {
    pub page_num: u32,
    pub width: u32,
    pub height: u32,
    pub image_bytes: Vec<u8>,
    /// True when every pixel has the same color (blank page after render).
    pub is_solid_fill: bool,
    /// Solid rectangles/lines detected in the raster (viewport coords).
    /// Populated only when `LiteParseConfig::detect_screenshot_rects` is on.
    pub rects: Vec<ScreenshotRect>,
}

/// Env var pointing at a fragmented glyph-outline → unicode font database
/// directory (`%02x%02x.msgpack` shards). When set, [`LiteParse::new`]
/// auto-wires a [`crate::FontDbResolver`] so buggy/obfuscated-font glyphs are
/// recovered without any extra wiring. Unset (default) leaves the hook dormant.
#[cfg(not(target_arch = "wasm32"))]
const FONT_DB_DIR_ENV: &str = "LITEPARSE_FONT_DB_DIR";

#[cfg(not(target_arch = "wasm32"))]
fn write_extracted_images(
    output_dir: &str,
    images: &mut [ExtractedImage],
) -> Result<(), LiteParseError> {
    use std::collections::HashMap;
    use std::path::Path;

    std::fs::create_dir_all(output_dir)?;
    // Platform contract (mirrors the LlamaParse C extractor, which the worker
    // pipeline is built around): only the canonical file is written; every
    // duplicate placement keeps its own `name` but points `path` at the
    // canonical file. Markdown figure references are rewritten to the
    // canonical name (`rewrite_duplicate_image_refs`) so they only ever
    // reference files that exist.
    let mut written: HashMap<String, String> = HashMap::new();
    for image in images {
        if let Some(canonical) = image.duplicate_of.as_ref()
            && let Some(path) = written.get(canonical)
        {
            image.path = Some(path.clone());
            continue;
        }

        let path = Path::new(output_dir).join(&image.name);
        std::fs::write(&path, image.bytes.as_slice())?;
        let path = path.to_string_lossy().into_owned();
        image.path = Some(path.clone());
        written.insert(image.id.clone(), path);
    }
    Ok(())
}

/// Rewrite markdown figure references for deduplicated images to the
/// canonical entry's file name. The markdown emitter references each figure
/// by its own placement id (`![](img_p2_1.jpg)`), but only the canonical
/// file is written to disk (see `write_extracted_images`), so duplicate
/// placements must reference the canonical name, matching the resolution the
/// LlamaParse worker applies via `resolveOutputImageName`.
fn rewrite_duplicate_image_refs(
    pages: &mut [ParsedPage],
    full_text: &mut String,
    images: &[ExtractedImage],
) {
    use std::collections::HashMap;

    let by_id: HashMap<&str, &ExtractedImage> = images
        .iter()
        .map(|image| (image.id.as_str(), image))
        .collect();
    let renames: Vec<(String, String)> = images
        .iter()
        .filter_map(|image| {
            let canonical = by_id.get(image.duplicate_of.as_ref()?.as_str())?;
            Some((
                format!("![](img_{}.{})", image.id, image.format),
                format!("![]({})", canonical.name),
            ))
        })
        .collect();
    if renames.is_empty() {
        return;
    }

    for markdown in pages
        .iter_mut()
        .map(|page| &mut page.markdown)
        .chain(std::iter::once(full_text))
    {
        for (from, to) in &renames {
            if markdown.contains(from.as_str()) {
                *markdown = markdown.replace(from.as_str(), to);
            }
        }
    }
}

/// Build the default glyph resolver from the environment, if configured.
#[cfg(not(target_arch = "wasm32"))]
fn default_glyph_resolver() -> Option<std::sync::Arc<dyn crate::GlyphResolver>> {
    let dir = std::env::var_os(FONT_DB_DIR_ENV)?;
    if dir.is_empty() {
        return None;
    }
    Some(std::sync::Arc::new(crate::FontDbResolver::new(dir)))
}

#[cfg(target_arch = "wasm32")]
fn default_glyph_resolver() -> Option<std::sync::Arc<dyn crate::GlyphResolver>> {
    None
}

/// Main LiteParse orchestrator.
///
/// ### Thread safety
///
/// `LiteParse` is `Send + Sync` and safe to share across threads (e.g.
/// behind an `Arc`, or used concurrently from a multi-threaded `tokio`
/// runtime).
///
/// PDFium itself is **not** thread-safe, so all PDFium FFI work — document
/// loading, page rendering, text extraction — is serialized through a
/// process-global lock held by [`pdfium::Library`]. From a caller's
/// perspective, this means concurrent `parse_*` / `screenshot*` calls are
/// safe but their PDFium portions run sequentially. The OCR pass and grid
/// projection (which dominate runtime for OCR-heavy documents) run outside
/// the lock and remain fully concurrent.
pub struct LiteParse {
    config: LiteParseConfig,
    /// Optional caller-provided OCR engine. When set, this overrides the
    /// built-in selection logic (HTTP OCR / Tesseract). This is the primary
    /// mechanism for plugging an OCR engine in environments without the
    /// built-ins (e.g. WASM, where the JS side supplies a callback engine).
    ocr_engine_override: Option<std::sync::Arc<dyn OcrEngine>>,
    /// Optional caller-provided glyph recovery hook. When set, it is consulted
    /// as a last resort for buggy/obfuscated-font glyphs that liteparse's
    /// built-in cmap/AGL recovery could not decode. The published package ships
    /// none; the platform build injects an outline → unicode font-DB resolver.
    glyph_resolver: Option<std::sync::Arc<dyn crate::GlyphResolver>>,
}

impl LiteParse {
    pub fn new(config: LiteParseConfig) -> Self {
        Self {
            config,
            ocr_engine_override: None,
            glyph_resolver: default_glyph_resolver(),
        }
    }

    /// Override the OCR engine. When set, the engine is used regardless of
    /// `ocr_server_url` / built-in Tesseract availability.
    pub fn with_ocr_engine(mut self, engine: std::sync::Arc<dyn OcrEngine>) -> Self {
        self.ocr_engine_override = Some(engine);
        self
    }

    /// Inject a glyph recovery hook. When set, glyphs that liteparse considers
    /// untrusted and cannot decode with its built-in cmap/AGL recovery are
    /// passed to the resolver as vector-outline segments for a final attempt.
    pub fn with_glyph_resolver(
        mut self,
        resolver: std::sync::Arc<dyn crate::GlyphResolver>,
    ) -> Self {
        self.glyph_resolver = Some(resolver);
        self
    }

    /// Parse the configured `target_pages` string (e.g. `"1-5,10"`) into an
    /// explicit page list, or `None` when no selection was configured.
    fn resolve_target_pages(&self) -> Result<Option<Vec<u32>>, LiteParseError> {
        self.config
            .target_pages
            .as_ref()
            .map(|s| parse_target_pages(s))
            .transpose()
            .map_err(|e| format!("invalid --target-pages: {}", e).into())
    }

    fn validate_output_config(&self) -> Result<(), LiteParseError> {
        if self.config.image_output_dir.is_some() && !self.config.effective_extract_images() {
            return Err(LiteParseError::Config(
                "image_output_dir requires extract_images = true (or image_mode = embed)"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Determine the complexity of each page in a document, returning a vector
    /// of `PageComplexityStats` for each page. This is useful for deciding
    /// whether to enable OCR on a per-page basis, or for other heuristics.
    ///
    /// Besides the OCR-need signals, each entry carries `layout` signals
    /// (multi-column, ruled tables, dense graphics) computed by running the
    /// real grid-projection pass — useful for routing pages to a
    /// higher-accuracy pipeline even when no OCR is needed.
    pub async fn is_complex(
        &self,
        input: PdfInput,
    ) -> Result<Vec<ocr_merge::PageComplexityStats>, LiteParseError> {
        let log = |msg: &str| {
            if !self.config.quiet {
                eprintln!("{}", msg);
            }
        };

        let t0 = web_time::Instant::now();

        #[cfg(not(target_arch = "wasm32"))]
        let (validated_input, _guard) =
            conversion::resolve_pdf_input(input, self.config.password.as_deref(), false).await?;

        #[cfg(target_arch = "wasm32")]
        let validated_input = input;

        // Determine which pages to extract
        let target_pages = self.resolve_target_pages()?;

        // Load the document and extract text items. Complexity signals derive
        // from the text layer and page objects only — embedded image rasters
        // and hyperlinks are irrelevant here, so both are skipped to keep this
        // pass fast (its whole purpose is a cheap pre-OCR check).
        let password = self.config.password.as_deref();

        let (pages, mut page_complexities) = {
            let lib = Library::init();
            let document = extract::load_document_from_input(&lib, &validated_input, password)?;

            let (pages, _, _) = extract::extract_pages_and_images(
                &document,
                target_pages.as_deref(),
                self.config.max_pages,
                false, // extract_links: irrelevant for complexity stats
                self.glyph_resolver.as_deref(),
                extract::ExtractionOutputOptions::default(),
            )?;
            let t_extract = web_time::Instant::now();
            log(&format!(
                "[liteparse] extract: {:.1}ms ({} pages)",
                t_extract.duration_since(t0).as_secs_f64() * 1000.0,
                pages.len()
            ));

            let page_complexities = pages
                .iter()
                .map(|page| {
                    let page_obj = document.page((page.page_number - 1) as i32)?;
                    ocr_merge::calculate_page_complexity(page, &page_obj)
                })
                .collect::<Result<Vec<_>, _>>()?;
            log(&format!(
                "[liteparse] complexity: {:.1}ms",
                web_time::Instant::now()
                    .duration_since(t_extract)
                    .as_secs_f64()
                    * 1000.0
            ));
            // `lib` is dropped here, releasing the PDFium lock; the layout
            // pass below is pure CPU over the already-extracted items.
            (pages, page_complexities)
        };

        // Layout signals come from the real projection pass so they match
        // what a full parse will decide.
        let t_layout = web_time::Instant::now();
        let parsed_pages = projection::project_pages_to_grid(pages);
        for (stats, page) in page_complexities.iter_mut().zip(&parsed_pages) {
            stats.layout = Some(ocr_merge::calculate_layout_complexity(page));
        }
        log(&format!(
            "[liteparse] layout: {:.1}ms",
            web_time::Instant::now()
                .duration_since(t_layout)
                .as_secs_f64()
                * 1000.0
        ));

        Ok(page_complexities)
    }

    /// Parse a document from a file path, returning structured results.
    ///
    /// Non-PDF files are automatically converted to PDF first (requires
    /// LibreOffice/ImageMagick on the system).
    ///
    /// Not available on `wasm32` — the browser has no filesystem. Use
    /// [`LiteParse::parse_input`] with [`PdfInput::Bytes`] instead.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn parse(&self, input: &str) -> Result<ParseResult, LiteParseError> {
        self.parse_input(PdfInput::Path(input.to_string())).await
    }

    /// Parse a document from either a file path or raw bytes.
    ///
    /// Use `PdfInput::Path` for files on disk or `PdfInput::Bytes` for
    /// in-memory PDF data (e.g. from a network response or Node.js Buffer).
    pub async fn parse_input(&self, input: PdfInput) -> Result<ParseResult, LiteParseError> {
        let log = |msg: &str| {
            if !self.config.quiet {
                eprintln!("{}", msg);
            }
        };

        let t0 = web_time::Instant::now();

        self.validate_output_config()?;

        // Native DOCX path: parse + resolve + layout in-process, no
        // LibreOffice, no PDFium, no OCR. Fail-open only for *engine*
        // failures: any error (or panic — the layout engine is young) logs
        // and falls through to the conversion path, as does a text-sparse
        // document when OCR is on. A config asking for something the native
        // path cannot honor is a hard error instead — the user picks the
        // engine, the engine never silently swaps itself out.
        #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
        if self.config.office_native {
            if let Some(reason) = self.native_docx_ineligible_reason() {
                // Unsupported option + native mode = a hard error, not a
                // silent engine swap: falling back to LibreOffice here would
                // hand back different geometry/pagination than every other
                // parse of the same document. The user opts into the
                // conversion path explicitly instead.
                if crate::office::docx_bytes(&input).is_some() {
                    return Err(LiteParseError::Config(format!(
                        "`{reason}` is not supported by the native DOCX path; \
                         disable it, or set office_native = false \
                         (CLI: --no-office-native) to parse via the conversion path"
                    )));
                }
            } else if let Some(bytes) = crate::office::docx_bytes(&input) {
                match self.try_parse_docx_native(&bytes) {
                    Ok(Some(result)) => {
                        let total =
                            web_time::Instant::now().duration_since(t0).as_secs_f64() * 1000.0;
                        log(&format!("[liteparse] native docx total: {:.1}ms", total));
                        return Ok(result);
                    }
                    Ok(None) => {
                        log(
                            "[liteparse] office_native: text-sparse docx, falling back to conversion path for OCR",
                        );
                    }
                    Err(e) => {
                        log(&format!(
                            "[liteparse] office_native failed ({e}); falling back to conversion path"
                        ));
                    }
                }
            }
        }

        // Native PPTX path, same contract as the DOCX one above: engine
        // failures fall through to LibreOffice, an unsupported option is a hard
        // error rather than a silent engine swap.
        #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
        if self.config.office_native {
            if let Some(reason) = self.native_pptx_ineligible_reason() {
                if crate::office::pptx_bytes(&input).is_some() {
                    return Err(LiteParseError::Config(format!(
                        "`{reason}` is not supported by the native PPTX path; \
                         disable it, or set office_native = false \
                         (CLI: --no-office-native) to parse via the conversion path"
                    )));
                }
            } else if let Some(bytes) = crate::office::pptx_bytes(&input) {
                match self.try_parse_pptx_native(&bytes) {
                    Ok(Some(result)) => {
                        let total =
                            web_time::Instant::now().duration_since(t0).as_secs_f64() * 1000.0;
                        log(&format!("[liteparse] native pptx total: {:.1}ms", total));
                        return Ok(result);
                    }
                    Ok(None) => {
                        log(
                            "[liteparse] office_native: text-sparse pptx, falling back to conversion path for OCR",
                        );
                    }
                    Err(e) => {
                        log(&format!(
                            "[liteparse] office_native failed ({e}); falling back to conversion path"
                        ));
                    }
                }
            }
        }

        // Native XLSX path, same contract again. The workbook states its own
        // geometry, so this one has no layout engine to fail — the fallback
        // exists for reader errors (and the text-sparse OCR case, where a
        // pasted scan is only reachable by rendering through LibreOffice).
        #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
        if self.config.office_native {
            if let Some(reason) = self.native_xlsx_ineligible_reason() {
                if crate::office::xlsx_bytes(&input).is_some() {
                    return Err(LiteParseError::Config(format!(
                        "`{reason}` is not supported by the native XLSX path; \
                         disable it, or set office_native = false \
                         (CLI: --no-office-native) to parse via the conversion path"
                    )));
                }
            } else if let Some(bytes) = crate::office::xlsx_bytes(&input) {
                match self.try_parse_xlsx_native(&bytes) {
                    Ok(Some(result)) => {
                        let total =
                            web_time::Instant::now().duration_since(t0).as_secs_f64() * 1000.0;
                        log(&format!("[liteparse] native xlsx total: {:.1}ms", total));
                        return Ok(result);
                    }
                    Ok(None) => {
                        log(
                            "[liteparse] office_native: text-sparse xlsx, falling back to conversion path for OCR",
                        );
                    }
                    Err(e) => {
                        log(&format!(
                            "[liteparse] office_native failed ({e}); falling back to conversion path"
                        ));
                    }
                }
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        let (validated_input, _guard) =
            conversion::resolve_pdf_input(input, self.config.password.as_deref(), false).await?;

        // Provenance facts describe the file on disk, so they are meaningless
        // for a PDF we generated ourselves from a DOCX/XLSX/image.
        #[cfg(not(target_arch = "wasm32"))]
        let want_doc_meta = self.config.extract_document_metadata && !_guard.is_converted();
        #[cfg(target_arch = "wasm32")]
        let want_doc_meta = self.config.extract_document_metadata;

        #[cfg(target_arch = "wasm32")]
        let validated_input = input;

        // Determine which pages to extract
        let target_pages = self.resolve_target_pages()?;

        // Extract text (and pre-render OCR pages in one PDF load when OCR is on).
        // The PDFium lock is acquired for this entire critical section and
        // released before any `.await` below — OCR (network / CPU) and grid
        // projection (pure Rust) do not touch PDFium, so they can run
        // concurrently with other `LiteParse` calls.
        let password = self.config.password.as_deref();
        // Build the OCR engine up front so the renderer knows whether to emit a
        // grayscale buffer (cheaper, for engines that binarize internally) or RGB.
        let ocr_engine: Option<std::sync::Arc<dyn OcrEngine>> = if self.config.ocr_enabled {
            Some(if let Some(e) = self.ocr_engine_override.clone() {
                e
            } else {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    if let Some(ref url) = self.config.ocr_server_url {
                        std::sync::Arc::new(
                            HttpOcrEngine::with_headers(
                                url.clone(),
                                self.config.ocr_server_headers.clone(),
                            )
                            .with_retry(
                                crate::ocr::http_simple::OcrRetryConfig {
                                    hedge_delays_ms: self.config.ocr_hedge_delays_ms.clone(),
                                    ..Default::default()
                                },
                            ),
                        )
                    } else {
                        #[cfg(feature = "tesseract")]
                        {
                            std::sync::Arc::new(TesseractOcrEngine::new(
                                self.config.tessdata_path.clone(),
                            ))
                        }
                        #[cfg(not(feature = "tesseract"))]
                        {
                            return Err("OCR enabled but no --ocr-server-url provided and tesseract feature is disabled".into());
                        }
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    return Err(
                        "OCR enabled but no `ocrEngine` callback was provided (WASM builds have no built-in OCR engine)".into(),
                    );
                }
            })
        } else {
            None
        };
        let ocr_grayscale = ocr_engine.as_ref().is_some_and(|e| e.prefers_grayscale());

        #[allow(unused_mut)] // mutated only by the native image-output writer
        let (
            pages,
            ocr_rendered,
            outline,
            mut images,
            image_error_count,
            complexity,
            form_type,
            creator,
            producer,
            doc_meta,
            xfa_packets,
        ) = {
            let lib = Library::init();
            #[cfg(not(target_arch = "wasm32"))]
            let repaired_input = self
                .config
                .extract_form_fields
                .then(|| {
                    crate::acroform_repair::repair_orphaned_widgets(
                        &lib,
                        &validated_input,
                        password,
                    )
                })
                .flatten();
            #[cfg(not(target_arch = "wasm32"))]
            let document_input = repaired_input.as_ref().unwrap_or(&validated_input);
            #[cfg(target_arch = "wasm32")]
            let document_input = &validated_input;
            let document = extract::load_document_from_input(&lib, document_input, password)?;
            let form_type = self
                .config
                .extract_form_fields
                .then(|| document.form_type());
            let creator = document.meta_text("Creator");
            let producer = document.meta_text("Producer");
            let doc_meta = want_doc_meta.then(|| {
                // AcroForm repair rewrites the file, so provenance has to come
                // from the original document; fall back if it no longer loads.
                #[cfg(not(target_arch = "wasm32"))]
                if repaired_input.is_some()
                    && let Ok(source) =
                        extract::load_document_from_input(&lib, &validated_input, password)
                {
                    return crate::document_metadata::extract(&validated_input, &source);
                }
                crate::document_metadata::extract(&validated_input, &document)
            });
            let xfa_packets = self.config.extract_xfa_packets.then(|| {
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
                    .collect::<Vec<_>>()
            });
            let outline = extract::extract_outline(&document);
            let (pages, images, image_error_count) = extract::extract_pages_and_images(
                &document,
                target_pages.as_deref(),
                self.config.max_pages,
                self.config.extract_links
                    && self.config.output_format == crate::config::OutputFormat::Markdown,
                self.glyph_resolver.as_deref(),
                extract::ExtractionOutputOptions {
                    extract_content_bounds: self.config.extract_content_bounds,
                    extract_images: self.config.effective_extract_images(),
                    // The markdown table detector splits PDFium's merged
                    // multi-cell runs on real word geometry, so it needs word
                    // boxes even when the caller didn't ask for them.
                    emit_word_boxes: self.config.emit_word_boxes
                        || self.config.output_format == crate::config::OutputFormat::Markdown,
                    extract_text_metadata: self.config.extract_text_metadata,
                    extract_vector_graphics: self.config.extract_vector_graphics,
                    extract_annotations: self.config.extract_annotations,
                    extract_form_fields: self.config.extract_form_fields,
                    extract_structure_tree: self.config.extract_structure_tree,
                },
            )?;
            let t_extract = web_time::Instant::now();
            log(&format!(
                "[liteparse] extract: {:.1}ms ({} pages)",
                t_extract.duration_since(t0).as_secs_f64() * 1000.0,
                pages.len()
            ));
            let rendered = if self.config.ocr_enabled {
                let r = ocr_merge::render_pages_for_ocr(
                    &document,
                    &pages,
                    self.config.dpi,
                    ocr_grayscale,
                    self.config.render_form_fields,
                )?;
                log(&format!(
                    "[liteparse] ocr render: {:.1}ms ({} pages)",
                    web_time::Instant::now()
                        .duration_since(t_extract)
                        .as_secs_f64()
                        * 1000.0,
                    r.len()
                ));
                r
            } else {
                Vec::new()
            };

            let complexity = if self.config.include_complexity {
                pages
                    .iter()
                    .map(|page| {
                        let page_obj = document.page((page.page_number - 1) as i32)?;
                        ocr_merge::calculate_page_complexity(page, &page_obj)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                Vec::new()
            };
            // `lib` is dropped here, releasing the PDFium lock.
            (
                pages,
                rendered,
                outline,
                images,
                image_error_count,
                complexity,
                form_type,
                creator,
                producer,
                doc_meta,
                xfa_packets,
            )
        };
        let mut pages = pages;
        let t1 = web_time::Instant::now();

        // OCR pass (engine resolved before the render block above).
        if let Some(engine) = ocr_engine {
            ocr_merge::ocr_and_merge_rendered(
                &mut pages,
                ocr_rendered,
                engine,
                &self.config.ocr_language,
                self.config.num_workers,
                self.config.ocr_failure_fatal,
            )
            .await?;
        }
        let t_ocr = web_time::Instant::now();
        log(&format!(
            "[liteparse] ocr: {:.1}ms",
            t_ocr.duration_since(t1).as_secs_f64() * 1000.0
        ));

        // Caller-requested content filters (page-region crop, diagonal-text
        // removal). Runs after OCR merge so it also drops OCR text outside the
        // crop region, and before projection so filtered items never surface.
        extract::apply_content_filters(
            &mut pages,
            self.config.crop_box.as_ref(),
            self.config.skip_diagonal_text,
        );

        // Grid projection
        let mut parsed_pages = projection::project_pages_to_grid(pages);

        // Attach per-page complexity signals, including the layout signals
        // that need the projected page (same as `is_complex()` reports).
        for (page, mut stats) in parsed_pages.iter_mut().zip(complexity) {
            stats.layout = Some(ocr_merge::calculate_layout_complexity(page));
            page.complexity = Some(stats);
        }
        let t2 = web_time::Instant::now();
        log(&format!(
            "[liteparse] project: {:.1}ms",
            t2.duration_since(t_ocr).as_secs_f64() * 1000.0
        ));

        let mut full_text = if self.config.output_format == crate::config::OutputFormat::Markdown {
            let page_md = markdown::format_markdown_pages(
                &parsed_pages,
                &outline,
                self.config.image_mode,
                self.config.keep_headers_footers,
            );
            let md = page_md.join("\n\n-----\n\n");
            for (page, md) in parsed_pages.iter_mut().zip(page_md) {
                page.markdown = md;
            }
            let t3 = web_time::Instant::now();
            log(&format!(
                "[liteparse] markdown: {:.1}ms",
                t3.duration_since(t2).as_secs_f64() * 1000.0
            ));
            md
        } else {
            parsed_pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        if self.config.output_format == crate::config::OutputFormat::Markdown {
            rewrite_duplicate_image_refs(&mut parsed_pages, &mut full_text, &images);
        }

        let total = web_time::Instant::now().duration_since(t0).as_secs_f64() * 1000.0;
        log(&format!("[liteparse] total: {:.1}ms", total));

        #[cfg(not(target_arch = "wasm32"))]
        if self.config.effective_extract_images()
            && let Some(output_dir) = self.config.image_output_dir.as_deref()
        {
            write_extracted_images(output_dir, &mut images)?;
        }

        Ok(ParseResult {
            pages: parsed_pages,
            text: full_text,
            outline,
            images,
            image_error_count,
            form_type,
            creator,
            producer,
            doc_meta,
            xfa_packets,
        })
    }

    /// Parse from pre-extracted pages, skipping PDFium text extraction.
    ///
    /// The caller supplies `Page`s already populated with text items (and,
    /// optionally, graphics / struct nodes / image refs) in viewport space
    /// (top-left origin, 72 DPI). This runs only grid projection and the
    /// configured output formatter, so it touches neither PDFium nor OCR and
    /// is fully synchronous. Used when an external extractor (e.g. with its
    /// own font-recovery pipeline) owns text extraction.
    pub fn parse_from_pages(&self, pages: Vec<Page>, outline: Vec<OutlineTarget>) -> ParseResult {
        let mut parsed_pages = projection::project_pages_to_grid(pages);

        let full_text = if self.config.output_format == crate::config::OutputFormat::Markdown {
            let page_md = markdown::format_markdown_pages(
                &parsed_pages,
                &outline,
                self.config.image_mode,
                self.config.keep_headers_footers,
            );
            let md = page_md.join("\n\n-----\n\n");
            for (page, md) in parsed_pages.iter_mut().zip(page_md) {
                page.markdown = md;
            }
            md
        } else {
            parsed_pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        ParseResult {
            pages: parsed_pages,
            text: full_text,
            outline,
            images: Vec::new(),
            image_error_count: 0,
            form_type: None,
            creator: None,
            producer: None,
            doc_meta: None,
            xfa_packets: None,
        }
    }

    /// Config options the native DOCX path cannot honor. Hitting one with
    /// `office_native` on is a hard `Config` error, not a fallback — see the
    /// check in `parse_input`. Deliberately absent (the native path honors
    /// these): `extract_images`, `extract_xfa_packets` (empty — DOCX has no
    /// XFA), `extract_annotations` (hyperlink/internal-link rects as `link`
    /// annotations), `include_complexity`, and `doc_meta` (`None`, as on the
    /// conversion path).
    #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
    fn native_docx_ineligible_reason(&self) -> Option<&'static str> {
        let c = &self.config;
        if c.extract_form_fields {
            Some("extract_form_fields")
        } else if c.extract_structure_tree {
            Some("extract_structure_tree")
        } else if c.extract_vector_graphics {
            Some("extract_vector_graphics")
        } else if c.crop_box.is_some() {
            Some("crop_box")
        } else if c.skip_diagonal_text {
            Some("skip_diagonal_text")
        } else {
            None
        }
    }

    /// Config options the native PPTX path cannot honor. Hitting one with
    /// `office_native` on is a hard `Config` error, not a fallback.
    ///
    /// `extract_annotations` is PPTX-specific: the geometry adapter builds
    /// fragments with no `LinkTarget`, so there are no link rects to merge.
    /// Markdown links (`extract_links`) are unaffected — those come from the
    /// emitter, which resolves `a:hlinkClick`.
    #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
    fn native_pptx_ineligible_reason(&self) -> Option<&'static str> {
        let c = &self.config;
        if c.extract_form_fields {
            Some("extract_form_fields")
        } else if c.extract_structure_tree {
            Some("extract_structure_tree")
        } else if c.extract_vector_graphics {
            Some("extract_vector_graphics")
        } else if c.crop_box.is_some() {
            Some("crop_box")
        } else if c.skip_diagonal_text {
            Some("skip_diagonal_text")
        } else if c.extract_annotations {
            Some("extract_annotations")
        } else {
            None
        }
    }

    /// Config options the native XLSX path cannot honor. Hitting one with
    /// `office_native` on is a hard `Config` error, not a fallback.
    ///
    /// `extract_annotations` is XLSX-specific: hyperlinks reach
    /// `TextItem::link`, but no `DocumentAnnotation` list is built. Images
    /// are honored — the drawing layer's pictures extract, though charts and
    /// drawing shapes carry no image bytes and are out of scope.
    #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
    fn native_xlsx_ineligible_reason(&self) -> Option<&'static str> {
        let c = &self.config;
        if c.extract_form_fields {
            Some("extract_form_fields")
        } else if c.extract_structure_tree {
            Some("extract_structure_tree")
        } else if c.extract_vector_graphics {
            Some("extract_vector_graphics")
        } else if c.crop_box.is_some() {
            Some("crop_box")
        } else if c.skip_diagonal_text {
            Some("skip_diagonal_text")
        } else if c.extract_annotations {
            Some("extract_annotations")
        } else {
            None
        }
    }

    /// Run the native XLSX pipeline. Contract matches the DOCX/PPTX siblings:
    /// `Ok(None)` is an eligible fallback (text-sparse workbook with OCR on),
    /// and panics map to `Err` so the caller degrades to LibreOffice.
    #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
    fn try_parse_xlsx_native(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parse_xlsx_native_inner(data)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native xlsx layout panicked".to_string(),
            ))
        })
    }

    #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
    fn parse_xlsx_native_inner(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        use crate::office::{xlsx, xlsx_layout};

        let wb = liteparse_ooxml::xlsx::read(data)
            .map_err(|e| LiteParseError::Conversion(format!("xlsx parse failed: {e}")))?;
        // Same two gates as the PDF, DOCX and PPTX paths: figures interleave
        // when `image_mode != Off`, pixel bytes surface under
        // `effective_extract_images`.
        let nx = xlsx_layout::workbook_to_pages(
            &wb,
            xlsx::EmitOptions {
                links: self.config.extract_links,
                figures: self.config.image_mode != crate::config::ImageMode::Off,
                images: self.config.effective_extract_images(),
                // A parse wants items, not ink: the grid painter runs for the
                // screenshot path only.
                paint: false,
            },
            None,
        );

        // A text-sparse workbook with OCR on is likely a pasted scan; only
        // the conversion path can render and OCR it.
        if self.config.ocr_enabled {
            let total_chars: usize = nx
                .pages
                .iter()
                .flat_map(|p| &p.text_items)
                .map(|i| i.text.trim().len())
                .sum();
            if total_chars < Self::NATIVE_TEXT_SPARSE_CHARS {
                return Ok(None);
            }
        }

        // Per-page complexity from native facts: the page's table blocks
        // (exact, not detected), the drawing layer's picture rects, and a
        // column count of 1 — a spreadsheet has no section columns.
        let complexity: Vec<Option<crate::ocr_merge::PageComplexityStats>> =
            if self.config.include_complexity {
                nx.pages
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let tables = nx.page_blocks[i]
                            .iter()
                            .filter(|b| {
                                matches!(
                                    b,
                                    crate::markdown_layout::Block::Table { .. }
                                        | crate::markdown_layout::Block::MergedTable { .. }
                                )
                            })
                            .count();
                        Some(crate::ocr_merge::calculate_native_page_complexity(
                            p,
                            nx.pic_rects.get(i).map(Vec::as_slice).unwrap_or(&[]),
                            tables,
                            1,
                        ))
                    })
                    .collect()
            } else {
                vec![None; nx.pages.len()]
            };

        // Page selection mirrors the other native paths: `target_pages`
        // filters by 1-based number, then `max_pages` truncates. The outline
        // stays whole-document.
        let (pages, page_blocks, page_stats, page_filtered) =
            self.select_native_pages(nx.pages, nx.page_blocks, complexity)?;

        let mut result = self.parse_from_native_blocks(
            pages,
            page_blocks,
            (!page_filtered).then_some(nx.all_blocks),
            nx.outline,
            nx.images,
            page_stats,
        );

        // Same post-passes as the PDF, DOCX and PPTX paths, same order:
        // duplicate figure refs point at the canonical name, then only
        // canonical files are written.
        if self.config.output_format == crate::config::OutputFormat::Markdown {
            rewrite_duplicate_image_refs(&mut result.pages, &mut result.text, &result.images);
        }
        if self.config.effective_extract_images()
            && let Some(output_dir) = self.config.image_output_dir.as_deref()
        {
            write_extracted_images(output_dir, &mut result.images)?;
        }
        Ok(Some(result))
    }

    /// Run the native PPTX pipeline. Contract matches the DOCX sibling:
    /// `Ok(None)` is an eligible fallback (text-sparse deck with OCR on), and
    /// panics map to `Err` so the caller degrades to LibreOffice.
    #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
    fn try_parse_pptx_native(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parse_pptx_native_inner(data)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native pptx layout panicked".to_string(),
            ))
        })
    }

    #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
    fn parse_pptx_native_inner(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        use crate::office::{pptx, pptx_layout};

        // A deck embeds no fonts, so unlike the DOCX path there is nothing to
        // register — but the registry must still be the *same* one the geometry
        // pass measures with, since the converter re-measures each item.
        let registry = liteparse_ooxml::render::fonts::FontRegistry::build(&[], &[])
            .map_err(|e| LiteParseError::Conversion(format!("font registry: {e}")))?;
        let geo = pptx_layout::slides_to_pages(data, &registry)?;

        if self.config.ocr_enabled {
            let total_chars: usize = geo
                .pages
                .iter()
                .flat_map(|p| &p.text_items)
                .map(|i| i.text.trim().len())
                .sum();
            if total_chars < Self::NATIVE_TEXT_SPARSE_CHARS {
                return Ok(None);
            }
        }

        // Same two gates as the PDF and DOCX paths: figures interleave when
        // `image_mode != Off`, pixel bytes surface under
        // `effective_extract_images`.
        let want_figures = self.config.image_mode != crate::config::ImageMode::Off;
        let want_images = self.config.effective_extract_images();

        let deck = pptx::emit_with_sources(
            data,
            pptx::EmitOptions {
                links: self.config.extract_links,
                // Speaker notes land in the slide's markdown and contribute
                // no `TextItem`s: the text is not on the slide itself. The
                // LibreOffice conversion path drops nearly all of them.
                notes: true,
                figures: want_figures,
                images: want_images,
            },
        )?;
        let images = deck.images;
        let tagged = deck.blocks;
        let n_pages = geo.pages.len();
        let page_blocks = split_pptx_blocks_by_page(&tagged, n_pages);
        let outline = pptx_slide_outline(&page_blocks);
        let all_blocks: Vec<crate::markdown_layout::Block> =
            tagged.into_iter().map(|(b, _)| b).collect();

        let complexity: Vec<Option<crate::ocr_merge::PageComplexityStats>> =
            if self.config.include_complexity {
                let img_rects = pptx_layout::image_rects_per_page(&geo.layouts);
                geo.pages
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let tables = page_blocks[i]
                            .iter()
                            .filter(|b| {
                                matches!(
                                    b,
                                    crate::markdown_layout::Block::Table { .. }
                                        | crate::markdown_layout::Block::MergedTable { .. }
                                )
                            })
                            .count();
                        // A slide's column count is 1 — PPTX has no section
                        // columns, and shapes are not columns.
                        Some(crate::ocr_merge::calculate_native_page_complexity(
                            p,
                            img_rects.get(i).map(Vec::as_slice).unwrap_or(&[]),
                            tables,
                            1,
                        ))
                    })
                    .collect()
            } else {
                vec![None; n_pages]
            };

        // Page selection mirrors the PDF and DOCX paths: `target_pages` filters
        // by 1-based number, then `max_pages` truncates. A slide *is* a page,
        // so a page number is a slide number.
        let (pages, page_blocks, page_stats, page_filtered) =
            self.select_native_pages(geo.pages, page_blocks, complexity)?;

        let mut result = self.parse_from_native_blocks(
            pages,
            page_blocks,
            (!page_filtered).then_some(all_blocks),
            outline,
            images,
            page_stats,
        );

        // Same post-passes as the PDF and DOCX paths, same order: duplicate
        // figure refs point at the canonical name, then only canonical files
        // are written. Decks reuse images heavily, so this dedup matters more
        // here than for documents.
        if self.config.output_format == crate::config::OutputFormat::Markdown {
            rewrite_duplicate_image_refs(&mut result.pages, &mut result.text, &result.images);
        }
        if want_images && let Some(output_dir) = self.config.image_output_dir.as_deref() {
            write_extracted_images(output_dir, &mut result.images)?;
        }
        Ok(Some(result))
    }

    /// Run the native DOCX pipeline. `Ok(None)` means "eligible fallback":
    /// the document is text-sparse and OCR is enabled, so the conversion path
    /// (which can OCR embedded images) serves the caller better. Panics from
    /// the vendored layout engine are caught and mapped to `Err` so the
    /// caller degrades to LibreOffice instead of aborting.
    #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
    fn try_parse_docx_native(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parse_docx_native_inner(data)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native docx layout panicked".to_string(),
            ))
        })
    }

    /// Total text length below which a DOCX counts as text-sparse (likely a
    /// scanned document pasted into Word) when OCR is enabled.
    #[cfg(all(feature = "office-native", not(target_arch = "wasm32")))]
    const NATIVE_TEXT_SPARSE_CHARS: usize = 32;

    #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
    fn parse_docx_native_inner(&self, data: &[u8]) -> Result<Option<ParseResult>, LiteParseError> {
        use crate::office::{docx, docx_layout};

        let parsed = liteparse_ooxml::docx::parse(data)
            .map_err(|e| LiteParseError::Conversion(format!("docx parse failed: {e}")))?;
        let resolved = liteparse_ooxml::render::resolve::resolve(parsed);
        // Build the registry directly rather than via `resolve_and_layout`,
        // whose `.expect` panics on a font-less host; here that is a clean
        // fallback to the conversion path instead.
        let registry = liteparse_ooxml::render::fonts::FontRegistry::build(
            &resolved.embedded_fonts,
            &resolved.font_families,
        )
        .map_err(|e| LiteParseError::Conversion(format!("font registry: {e}")))?;
        let layouted = liteparse_ooxml::render::layout_document(&resolved, &registry);
        let native = docx_layout::layout_to_pages(
            &layouted,
            &registry,
            self.config.emit_word_boxes,
            self.config.extract_annotations,
        );

        if self.config.ocr_enabled {
            let total_chars: usize = native
                .pages
                .iter()
                .flat_map(|p| &p.text_items)
                .map(|i| i.text.trim().len())
                .sum();
            if total_chars < Self::NATIVE_TEXT_SPARSE_CHARS {
                return Ok(None);
            }
        }

        // Figures mirror the PDF path's gate (interleaved only when
        // `image_mode != Off`); pixel bytes surface only under
        // `effective_extract_images`, also as on the PDF path.
        let want_figures = self.config.image_mode != crate::config::ImageMode::Off;
        let want_images = self.config.effective_extract_images();
        let native_images = if want_figures || want_images {
            docx_layout::collect_images(&layouted)
        } else {
            docx_layout::NativeImages {
                images: Vec::new(),
                figure_ids: std::collections::HashMap::new(),
            }
        };
        let images = if want_images {
            native_images.images
        } else {
            Vec::new()
        };

        let tagged = docx::emit_with_sources(
            &resolved,
            docx::EmitOptions {
                links: self.config.extract_links,
                figures: want_figures.then_some(native_images.figure_ids),
            },
        );
        let n_pages = native.pages.len();
        let mut page_blocks = split_blocks_by_page(&tagged, &native.block_pages, n_pages);
        let all_blocks: Vec<crate::markdown_layout::Block> =
            tagged.into_iter().map(|(b, _)| b).collect();

        // Headers/footers: the layout records each page's selected slot
        // (`LayoutedPage::header_blocks`/`footer_blocks`; the raster and
        // `text_items` always carry them). Markdown mirrors the PDF path's
        // default of suppressing running chrome, so the blocks join the
        // markdown stream only under `keep_headers_footers` — header at the
        // page top, footer at the page bottom. The doc-level text then has to
        // be assembled per page (a whole-document block walk has no page
        // boundaries to repeat them at), which `all_blocks: None` selects.
        let keep_hf = self.config.keep_headers_footers;
        if keep_hf {
            for (i, blocks) in page_blocks.iter_mut().enumerate() {
                let Some(lp) = layouted.get(i) else { break };
                let header = docx::emit_header_footer(
                    &resolved,
                    &lp.header_blocks,
                    self.config.extract_links,
                );
                let footer = docx::emit_header_footer(
                    &resolved,
                    &lp.footer_blocks,
                    self.config.extract_links,
                );
                blocks.splice(0..0, header);
                blocks.extend(footer);
            }
        }

        // Per-page complexity from native facts: placed-media rects, the
        // page's table blocks (exact, not detected), and the section-declared
        // column count — see `calculate_native_page_complexity` for the
        // documented divergences from the PDF pair.
        let complexity: Vec<Option<crate::ocr_merge::PageComplexityStats>> =
            if self.config.include_complexity {
                let col_counts =
                    docx_layout::page_column_counts(&resolved, &native.block_pages, n_pages);
                let img_rects = docx_layout::image_rects_per_page(&layouted);
                native
                    .pages
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let tables = page_blocks[i]
                            .iter()
                            .filter(|b| {
                                matches!(
                                    b,
                                    crate::markdown_layout::Block::Table { .. }
                                        | crate::markdown_layout::Block::MergedTable { .. }
                                )
                            })
                            .count();
                        Some(crate::ocr_merge::calculate_native_page_complexity(
                            p,
                            img_rects.get(i).map(Vec::as_slice).unwrap_or(&[]),
                            tables,
                            col_counts[i],
                        ))
                    })
                    .collect()
            } else {
                vec![None; n_pages]
            };

        // Page selection, mirroring the PDF path: `target_pages` filters by
        // 1-based page number, then `max_pages` truncates. The outline stays
        // whole-document, as it does on the PDF path. A page selection also
        // narrows the doc-level text to the surviving pages' blocks.
        let (pages, page_blocks, page_stats, page_filtered) =
            self.select_native_pages(native.pages, page_blocks, complexity)?;

        let mut result = self.parse_from_native_blocks(
            pages,
            page_blocks,
            (!page_filtered && !keep_hf).then_some(all_blocks),
            native.outline,
            images,
            page_stats,
        );

        // Same post-passes as the PDF path, same order: duplicate figure refs
        // point at the canonical name, then only canonical files are written.
        if self.config.output_format == crate::config::OutputFormat::Markdown {
            rewrite_duplicate_image_refs(&mut result.pages, &mut result.text, &result.images);
        }
        if want_images && let Some(output_dir) = self.config.image_output_dir.as_deref() {
            write_extracted_images(output_dir, &mut result.images)?;
        }
        Ok(Some(result))
    }

    /// Native DOCX screenshot: rasterize the layout's own draw commands
    /// (tiny-skia + skrifa) instead of converting through LibreOffice.
    ///
    /// Error contract, relied on by the `screenshot_input` call site:
    /// `Conversion` means the engine failed and the caller should fall back
    /// to the conversion path; any other error (bad page range, PNG encode)
    /// is the caller's and surfaces directly. Panics from the vendored
    /// engine map to `Conversion` like the parse side.
    #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
    fn try_screenshot_docx_native(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.screenshot_docx_native_inner(data, page_numbers)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native docx layout panicked".to_string(),
            ))
        })
    }

    #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
    fn screenshot_docx_native_inner(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        let parsed = liteparse_ooxml::docx::parse(data)
            .map_err(|e| LiteParseError::Conversion(format!("docx parse failed: {e}")))?;
        let resolved = liteparse_ooxml::render::resolve::resolve(parsed);
        let registry = liteparse_ooxml::render::fonts::FontRegistry::build(
            &resolved.embedded_fonts,
            &resolved.font_families,
        )
        .map_err(|e| LiteParseError::Conversion(format!("font registry: {e}")))?;
        let layouted = liteparse_ooxml::render::layout_document(&resolved, &registry);

        self.rasterize_native_pages(&layouted, &registry, page_numbers, ("document", "pages"))
    }

    /// Shared tail of every native-office screenshot: select pages, rasterize
    /// the layout's draw commands, and wrap each raster in the same
    /// `ScreenshotResult` shape the PDFium path produces.
    ///
    /// `unit` names the page population in the out-of-range message so a deck
    /// says "slides" where a document says "pages"; it is the only thing that
    /// differs between the DOCX, PPTX and XLSX callers, because by this point
    /// all three are just a `[LayoutedPage]`.
    #[cfg(all(
        any(
            feature = "docx-native",
            feature = "pptx-native",
            feature = "xlsx-native"
        ),
        not(target_arch = "wasm32")
    ))]
    fn rasterize_native_pages(
        &self,
        layouted: &[liteparse_ooxml::render::layout::draw_command::LayoutedPage],
        registry: &liteparse_ooxml::render::fonts::FontRegistry,
        page_numbers: Option<&[u32]>,
        unit: (&str, &str),
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        let (container, noun) = unit;
        let page_count = layouted.len() as u32;
        // A zero-page layout is an engine failure, not an empty document:
        // every reader here gives a sheet, a slide or a section at least one
        // page, so nothing to draw means nothing was read. Degrade to the
        // conversion path (LibreOffice may recover a malformed file) rather
        // than return a "page 1 out of range (0 pages)" the caller cannot act
        // on.
        if page_count == 0 {
            return Err(LiteParseError::Conversion(format!(
                "native layout produced no {noun}"
            )));
        }
        let pages: Vec<u32> = match page_numbers {
            Some(nums) => nums.to_vec(),
            None => (1..=page_count).collect(),
        };
        let scale = self.config.dpi / 72.0;
        let mut results = Vec::with_capacity(pages.len());
        for page_num in pages {
            if page_num < 1 || page_num > page_count {
                return Err(LiteParseError::Other(format!(
                    "page {page_num} out of range ({container} has {page_count} {noun})"
                )));
            }
            let raster = liteparse_ooxml::render::raster::rasterize_page(
                &layouted[(page_num - 1) as usize],
                registry,
                scale,
            )
            .map_err(LiteParseError::Conversion)?;
            let (w, h) = (raster.width as usize, raster.height as usize);
            let is_solid_fill = render::is_solid_fill_rgba(&raster.rgba, w, h);
            // Same skip rule as the PDFium path: a solid-fill page has no
            // structure to find.
            let rects = if self.config.detect_screenshot_rects && !is_solid_fill {
                render::find_solid_rects_rgba(
                    &raster.rgba,
                    w,
                    h,
                    raster.page_width_pt,
                    raster.page_height_pt,
                )
            } else {
                Vec::new()
            };
            let png = crate::extract::encode_png(&raster.rgba, raster.width, raster.height)?;
            results.push(ScreenshotResult {
                page_num,
                width: raster.width,
                height: raster.height,
                image_bytes: png,
                is_solid_fill,
                rects,
            });
        }
        Ok(results)
    }

    /// Native PPTX screenshot: the DOCX sibling's contract, one slide per
    /// page. `Conversion` means the engine failed and the caller should fall
    /// back to LibreOffice; anything else is the caller's error.
    #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
    fn try_screenshot_pptx_native(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.screenshot_pptx_native_inner(data, page_numbers)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native pptx layout panicked".to_string(),
            ))
        })
    }

    #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
    fn screenshot_pptx_native_inner(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        // Same registry construction as `parse_pptx_native_inner` — a deck
        // embeds no fonts, and the raster must measure with the registry the
        // geometry pass laid the text out with or the glyphs land off their
        // own boxes.
        let registry = liteparse_ooxml::render::fonts::FontRegistry::build(&[], &[])
            .map_err(|e| LiteParseError::Conversion(format!("font registry: {e}")))?;
        let geo = crate::office::pptx_layout::slides_to_pages(data, &registry)?;
        self.rasterize_native_pages(&geo.layouts, &registry, page_numbers, ("deck", "slides"))
    }

    /// Native XLSX screenshot: the DOCX/PPTX contract over the grid painter's
    /// own commands. `Conversion` means the engine failed and the caller
    /// should fall back to LibreOffice; anything else is the caller's error.
    #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
    fn try_screenshot_xlsx_native(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.screenshot_xlsx_native_inner(data, page_numbers)
        }))
        .unwrap_or_else(|_| {
            Err(LiteParseError::Conversion(
                "native xlsx layout panicked".to_string(),
            ))
        })
    }

    #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
    fn screenshot_xlsx_native_inner(
        &self,
        data: &[u8],
        page_numbers: Option<&[u32]>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        use crate::office::{xlsx, xlsx_layout};

        let wb = liteparse_ooxml::xlsx::read(data)
            .map_err(|e| LiteParseError::Conversion(format!("xlsx parse failed: {e}")))?;
        // A workbook embeds no fonts, so the registry is the host's — same
        // construction as the PPTX path, including its no-faces check.
        let registry = liteparse_ooxml::render::fonts::FontRegistry::build(&[], &[])
            .map_err(|e| LiteParseError::Conversion(format!("font registry: {e}")))?;
        // The one call that differs from the parse side: `paint: true` plus a
        // registry, which is what turns the pass's grid, cell values and
        // pictures into draw commands. `figures` and `images` stay off —
        // those are the *emission* side (markdown refs and extracted byte
        // entries), which a raster has no use for; `paint` reaches the
        // picture bytes on its own.
        let nx = xlsx_layout::workbook_to_pages(
            &wb,
            xlsx::EmitOptions {
                links: false,
                figures: false,
                images: false,
                paint: true,
            },
            Some(&registry),
        );
        self.rasterize_native_pages(&nx.layouts, &registry, page_numbers, ("workbook", "pages"))
    }

    /// `parse_from_pages`'s native sibling: no projection — the block model
    /// arrives from the DOCX source, so markdown is `render_blocks` per page
    /// and page text is a straight reading-order join of the text items.
    ///
    /// Doc-level markdown renders from `all_blocks` when given, NOT by
    /// joining the page renders: `render_blocks` is context-sensitive across
    /// block boundaries (tight lists join with `\n`, soft-hyphen paragraph
    /// splices join with nothing), so a page-boundary join would perturb
    /// whitespace wherever a page break falls inside a list. Rendering the
    /// whole sequence keeps `ParseResult.text` byte-identical to the pure
    /// structure path; the per-page markdown is the paged *view* of the same
    /// blocks. `None` (page-filtered parse) falls back to joining the
    /// surviving pages.
    /// Page selection shared by the native DOCX, PPTX and XLSX paths:
    /// `target_pages` filters by 1-based page number, then `max_pages`
    /// truncates. Returns the aligned `(pages, page_blocks, page_stats)`
    /// triple plus whether any filtering occurred — callers drop the
    /// doc-level `all_blocks` when it did.
    #[cfg(all(feature = "office-native", not(target_arch = "wasm32")))]
    fn select_native_pages(
        &self,
        pages: Vec<Page>,
        page_blocks: Vec<Vec<crate::markdown_layout::Block>>,
        complexity: Vec<Option<crate::ocr_merge::PageComplexityStats>>,
    ) -> Result<
        (
            Vec<Page>,
            Vec<Vec<crate::markdown_layout::Block>>,
            Vec<Option<crate::ocr_merge::PageComplexityStats>>,
            bool,
        ),
        LiteParseError,
    > {
        let mut selected: Vec<((Page, Vec<crate::markdown_layout::Block>), _)> =
            pages.into_iter().zip(page_blocks).zip(complexity).collect();
        let mut page_filtered = false;
        if let Some(targets) = self.resolve_target_pages()? {
            let keep: std::collections::HashSet<usize> =
                targets.iter().map(|p| *p as usize).collect();
            selected.retain(|((p, _), _)| keep.contains(&p.page_number));
            page_filtered = true;
        }
        if selected.len() > self.config.max_pages {
            selected.truncate(self.config.max_pages);
            page_filtered = true;
        }
        let mut pages = Vec::with_capacity(selected.len());
        let mut page_blocks = Vec::with_capacity(selected.len());
        let mut page_stats = Vec::with_capacity(selected.len());
        for ((p, b), s) in selected {
            pages.push(p);
            page_blocks.push(b);
            page_stats.push(s);
        }
        Ok((pages, page_blocks, page_stats, page_filtered))
    }

    #[cfg(all(feature = "office-native", not(target_arch = "wasm32")))]
    fn parse_from_native_blocks(
        &self,
        pages: Vec<Page>,
        page_blocks: Vec<Vec<crate::markdown_layout::Block>>,
        all_blocks: Option<Vec<crate::markdown_layout::Block>>,
        outline: Vec<OutlineTarget>,
        images: Vec<crate::types::ExtractedImage>,
        page_stats: Vec<Option<crate::ocr_merge::PageComplexityStats>>,
    ) -> ParseResult {
        let markdown_out = self.config.output_format == crate::config::OutputFormat::Markdown;
        let parsed_pages: Vec<ParsedPage> = pages
            .into_iter()
            .zip(page_blocks)
            .zip(page_stats)
            .map(|((p, blocks), stats)| {
                let text = native_page_text(&p.text_items);
                let markdown = if markdown_out {
                    crate::markdown_layout::render_blocks(&blocks)
                } else {
                    String::new()
                };
                ParsedPage {
                    page_number: p.page_number,
                    page_width: p.page_width,
                    page_height: p.page_height,
                    content_bounds: self
                        .config
                        .extract_content_bounds
                        .then_some(p.content_bounds)
                        .flatten(),
                    text,
                    markdown,
                    text_items: p.text_items,
                    projected_lines: Vec::new(),
                    regions: crate::types::Region::default(),
                    graphics: Vec::new(),
                    vector_graphics: None,
                    figures: Vec::new(),
                    struct_nodes: Vec::new(),
                    image_refs: Vec::new(),
                    complexity: stats,
                    annotations: p.annotations,
                    form_fields: None,
                    structure_tree: None,
                }
            })
            .collect();

        let full_text = if markdown_out {
            match all_blocks {
                Some(blocks) => crate::markdown_layout::render_blocks(&blocks),
                None => parsed_pages
                    .iter()
                    .map(|p| p.markdown.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n-----\n\n"),
            }
        } else {
            parsed_pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        ParseResult {
            pages: parsed_pages,
            text: full_text,
            outline,
            images,
            image_error_count: 0,
            form_type: None,
            creator: None,
            producer: None,
            doc_meta: None,
            // XFA is a PDF/LiveCycle container; a DOCX cannot carry packets.
            // `Some([])` keeps the JSON shape identical to the conversion
            // path, which also finds none in a LibreOffice-produced PDF.
            xfa_packets: self.config.extract_xfa_packets.then(Vec::new),
        }
    }

    /// Generate screenshots of document pages as PNG bytes.
    ///
    /// Non-PDF files are automatically converted to PDF first (requires
    /// LibreOffice/ImageMagick on the system). Plain-text formats cannot be
    /// rendered and return a clear error.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn screenshot(
        &self,
        input: &str,
        page_numbers: Option<Vec<u32>>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        self.screenshot_input(PdfInput::Path(input.to_string()), page_numbers)
            .await
    }

    /// Generate screenshots from a file path or raw bytes.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn screenshot_input(
        &self,
        input: PdfInput,
        page_numbers: Option<Vec<u32>>,
    ) -> Result<Vec<ScreenshotResult>, LiteParseError> {
        let log = |msg: &str| {
            if !self.config.quiet {
                eprintln!("{}", msg);
            }
        };

        // Native DOCX raster: same layout engine as the native parse, so the
        // screenshot shares the native TextItem coordinate space — the
        // LibreOffice raster never did, which is why highlight-on-screenshot
        // was blocked. Engine failures (`Conversion` errors / panics) fall
        // back to the conversion path, same net as the parse side; other
        // errors (bad page range) are the caller's and surface directly.
        // `render_form_fields` is irrelevant here: the LibreOffice-converted
        // PDF carries no interactive fields either.
        #[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
        if self.config.office_native
            && let Some(bytes) = crate::office::docx_bytes(&input)
        {
            match self.try_screenshot_docx_native(&bytes, page_numbers.as_deref()) {
                Ok(results) => return Ok(results),
                Err(LiteParseError::Conversion(e)) => log(&format!(
                    "[liteparse] native docx screenshot failed ({e}); falling back to conversion"
                )),
                Err(e) => return Err(e),
            }
        }

        // Native PPTX raster, same contract as the DOCX branch above: the
        // slide is drawn from the same commands the geometry pass laid out,
        // so a screenshot and the native `TextItem`s share one coordinate
        // space. `office_native` gates it; engine failures degrade.
        #[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
        if self.config.office_native
            && let Some(bytes) = crate::office::pptx_bytes(&input)
        {
            match self.try_screenshot_pptx_native(&bytes, page_numbers.as_deref()) {
                Ok(results) => return Ok(results),
                Err(LiteParseError::Conversion(e)) => log(&format!(
                    "[liteparse] native pptx screenshot failed ({e}); falling back to conversion"
                )),
                Err(e) => return Err(e),
            }
        }

        // Native XLSX raster, same contract again: the grid painter draws the
        // page the geometry pass already numbered, so a screenshot and the
        // native `TextItem`s share one coordinate space. LibreOffice paginates
        // a sheet by its own print rules, so the converted PDF never did.
        #[cfg(all(feature = "xlsx-native", not(target_arch = "wasm32")))]
        if self.config.office_native
            && let Some(bytes) = crate::office::xlsx_bytes(&input)
        {
            match self.try_screenshot_xlsx_native(&bytes, page_numbers.as_deref()) {
                Ok(results) => return Ok(results),
                Err(LiteParseError::Conversion(e)) => log(&format!(
                    "[liteparse] native xlsx screenshot failed ({e}); falling back to conversion"
                )),
                Err(e) => return Err(e),
            }
        }

        let (validated_input, _guard) =
            conversion::resolve_pdf_input(input, self.config.password.as_deref(), true).await?;

        if let PdfInput::Path(ref path) = validated_input
            && !conversion::is_pdf(path)
        {
            log("[liteparse] converted input to PDF for screenshot rendering");
        }

        let rendered = render::render_pages_to_png(
            &validated_input,
            page_numbers.as_deref(),
            self.config.dpi,
            self.config.password.as_deref(),
            self.config.detect_screenshot_rects,
            self.config.render_form_fields,
        )?;

        Ok(rendered
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

    pub fn config(&self) -> &LiteParseConfig {
        &self.config
    }
}

/// Distribute a deck's blocks across slides.
///
/// Trivial next to the DOCX sibling, and for a structural reason: a DOCX block
/// has to be *located* by the layout stage, because a paragraph's page is
/// whatever pagination decided. A `BlockSource` already names its slide — the
/// index is intrinsic, not recovered — so this is a bucket sort.
///
/// All three sources land on the same page. Notes and SmartArt contribute no
/// `TextItem`s (their text is not on the slide, and a diagram's runs have no
/// rectangle), but they *are* content of that slide and belong in its markdown.
/// Concatenating the buckets reproduces the doc-level markdown exactly, since
/// the emitter already walks slides in presentation order.
#[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
fn split_pptx_blocks_by_page(
    tagged: &[(
        crate::markdown_layout::Block,
        crate::office::pptx::BlockSource,
    )],
    n_pages: usize,
) -> Vec<Vec<crate::markdown_layout::Block>> {
    use crate::office::pptx::BlockSource;

    let mut out = vec![Vec::new(); n_pages];
    for (block, src) in tagged {
        let idx = match src {
            BlockSource::Slide(n) | BlockSource::Notes(n) | BlockSource::Diagram(n) => *n,
        };
        // A slide the geometry pass skipped still has a page, so this only
        // guards against a deck whose emitter and layout disagree on slide
        // count — which would be a bug, not a document property.
        if let Some(bucket) = out.get_mut(idx) {
            bucket.push(block.clone());
        }
    }
    out
}

/// The document outline, one entry per slide title.
///
/// Every entry is level 1. PPTX has no heading hierarchy — `outlineLvl` is a
/// DOCX concept and a body placeholder's indent level is a bullet depth, not a
/// rank — so a flat outline is the honest shape, matching the emitter's own
/// `TITLE_HEADING_LEVEL`.
///
/// `y_pdf` is `None`. The DOCX path fills it from the first text command inside
/// an outline bracket; here the title's position is knowable but the outline is
/// derived from blocks rather than from draw commands, and a slide title is
/// found by its page, not by scrolling to an offset within it.
#[cfg(all(feature = "pptx-native", not(target_arch = "wasm32")))]
fn pptx_slide_outline(page_blocks: &[Vec<crate::markdown_layout::Block>]) -> Vec<OutlineTarget> {
    use crate::markdown_layout::Block;

    let mut out = Vec::new();
    for (idx, blocks) in page_blocks.iter().enumerate() {
        for block in blocks {
            if let Block::Heading { level, text } = block
                && !text.trim().is_empty()
            {
                out.push(OutlineTarget {
                    level: *level,
                    title: text.clone(),
                    page_index: idx as i32,
                    y_pdf: None,
                });
            }
        }
    }
    out
}

/// Distribute the emitted blocks across pages using the layout's block→page
/// map. A forward cursor plus `max` keeps the per-page lists a partition of
/// the input in its original order — concatenating them reproduces the
/// doc-level markdown exactly — while filling forward for blocks the layout
/// recorded nowhere (empty paragraphs) and clamping any out-of-order page
/// assignment a float relocation might produce. `Note` blocks land on the
/// last page: doc-level output must keep notes at the very end, and many
/// emitted notes (referenced only from headers or textboxes) have no
/// physical body page at all.
#[cfg(all(feature = "docx-native", not(target_arch = "wasm32")))]
fn split_blocks_by_page(
    tagged: &[(
        crate::markdown_layout::Block,
        crate::office::docx::BlockSource,
    )],
    block_pages: &std::collections::HashMap<usize, usize>,
    n_pages: usize,
) -> Vec<Vec<crate::markdown_layout::Block>> {
    use crate::office::docx::BlockSource;
    let n_pages = n_pages.max(1);
    let mut out: Vec<Vec<crate::markdown_layout::Block>> =
        (0..n_pages).map(|_| Vec::new()).collect();
    let mut current = 0usize;
    for (block, source) in tagged {
        let page = match source {
            BlockSource::Body(i) => block_pages
                .get(i)
                .copied()
                .map_or(current, |p| current.max(p)),
            BlockSource::Note => n_pages - 1,
        };
        let page = page.min(n_pages - 1);
        current = page;
        out[page].push(block.clone());
    }
    out
}

/// Reading-order plain text from native text items: emission order is the
/// engine's layout order, so lines are rebuilt by baseline proximity and
/// word gaps rather than by re-sorting (which would interleave columns).
#[cfg(all(feature = "office-native", not(target_arch = "wasm32")))]
fn native_page_text(items: &[crate::types::TextItem]) -> String {
    /// Fraction of the font size an x-gap must exceed to count as a missing
    /// inter-word space (word commands carry no trailing spaces). Adjacent
    /// same-word run splits sit at ~0 gap; a real space is ~0.25em.
    const WORD_GAP_EM: f32 = 0.15;
    let mut out = String::new();
    let mut prev: Option<&crate::types::TextItem> = None;
    for item in items.iter().filter(|i| !i.text.is_empty()) {
        if let Some(p) = prev {
            let font_size = p.font_size.unwrap_or(12.0).max(1.0);
            let baseline = item.y + item.font_ascent.unwrap_or(item.height);
            let prev_baseline = p.y + p.font_ascent.unwrap_or(p.height);
            if (baseline - prev_baseline).abs() < 0.5 * font_size {
                if item.x - (p.x + p.width) > WORD_GAP_EM * font_size {
                    out.push(' ');
                }
            } else {
                out.push('\n');
            }
        }
        out.push_str(&item.text);
        prev = Some(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Page, TextItem};

    fn page_with_text_metadata() -> Page {
        Page {
            page_number: 1,
            page_width: 100.0,
            page_height: 100.0,
            content_bounds: None,
            text_items: vec![TextItem {
                text: "hello".into(),
                width: 20.0,
                height: 10.0,
                font_name: Some("Helvetica".into()),
                font_size: Some(10.0),
                font_height: Some(10.0),
                font_ascent: Some(8.0),
                font_descent: Some(-2.0),
                font_weight: Some(700),
                text_width: Some(19.0),
                font_is_buggy: true,
                mcid: Some(3),
                fill_color: Some("ff112233".into()),
                stroke_color: Some("ff445566".into()),
                char_codes: vec![104, 101, 108, 108, 111],
                trailing_space_generated: true,
                ..Default::default()
            }],
            graphics: vec![],
            vector_graphics: None,
            struct_nodes: vec![],
            image_refs: vec![],
            annotations: None,
            form_fields: None,
            structure_tree: None,
        }
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_new_stores_config() {
        let mut cfg = LiteParseConfig::default();
        cfg.ocr_enabled = false;
        cfg.max_pages = 7;
        let lp = LiteParse::new(cfg);
        assert!(!lp.config().ocr_enabled);
        assert_eq!(lp.config().max_pages, 7);
    }

    #[test]
    fn parse_from_pages_preserves_internal_text_metadata() {
        let result = LiteParse::new(LiteParseConfig::default())
            .parse_from_pages(vec![page_with_text_metadata()], vec![]);
        let item = &result.pages[0].text_items[0];
        assert_eq!(item.font_name.as_deref(), Some("Helvetica"));
        assert_eq!(item.font_size, Some(10.0));
        assert_eq!(item.font_height, Some(10.0));
        assert_eq!(item.font_ascent, Some(8.0));
        assert_eq!(item.font_descent, Some(-2.0));
        assert_eq!(item.font_weight, Some(700));
        assert_eq!(item.text_width, Some(19.0));
        assert!(item.font_is_buggy);
        assert_eq!(item.mcid, Some(3));
        assert_eq!(item.fill_color.as_deref(), Some("ff112233"));
        assert_eq!(item.stroke_color.as_deref(), Some("ff445566"));
        assert_eq!(item.char_codes, vec![104, 101, 108, 108, 111]);
        assert!(item.trailing_space_generated);
    }

    #[test]
    fn image_extraction_is_opt_in_but_embed_mode_implies_it() {
        let default = LiteParseConfig::default();
        assert!(!default.effective_extract_images());

        // `image_mode = embed` predates `extract_images` and must keep
        // extracting bytes for existing callers.
        let embed = LiteParseConfig {
            image_mode: crate::config::ImageMode::Embed,
            ..Default::default()
        };
        assert!(embed.effective_extract_images());

        let explicit = LiteParseConfig {
            extract_images: true,
            ..Default::default()
        };
        assert!(explicit.effective_extract_images());
    }

    #[test]
    fn image_output_dir_requires_image_extraction() {
        let parser = LiteParse::new(LiteParseConfig {
            image_output_dir: Some("images".into()),
            ..Default::default()
        });
        assert_eq!(
            parser.validate_output_config().unwrap_err().to_string(),
            "invalid config: image_output_dir requires extract_images = true (or image_mode = embed)"
        );

        let embed = LiteParse::new(LiteParseConfig {
            image_mode: crate::config::ImageMode::Embed,
            image_output_dir: Some("images".into()),
            ..Default::default()
        });
        assert!(embed.validate_output_config().is_ok());
    }

    #[test]
    fn image_output_writes_duplicates_from_canonical_bytes() {
        fn image(id: &str, duplicate_of: Option<&str>, bytes: &[u8]) -> ExtractedImage {
            ExtractedImage {
                id: id.into(),
                name: format!("img_{id}.png"),
                path: None,
                page: 1,
                bbox: crate::types::Rect::default(),
                width: 2,
                height: 2,
                rotation: 0.0,
                format: "png".into(),
                duplicate_of: duplicate_of.map(str::to_owned),
                bytes: std::sync::Arc::new(bytes.to_vec()),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut images = vec![
            image("p1_1", None, b"canonical"),
            image("p2_1", Some("p1_1"), b"canonical"),
        ];
        write_extracted_images(dir.path().to_str().unwrap(), &mut images).unwrap();

        // Platform contract: one file on disk; the duplicate keeps its own
        // placement `name` but shares the canonical file's `path`.
        assert_eq!(images[0].name, "img_p1_1.png");
        assert_eq!(images[1].name, "img_p2_1.png");
        assert_eq!(images[0].path, images[1].path);
        assert_eq!(
            std::fs::read(images[0].path.as_ref().unwrap()).unwrap(),
            b"canonical"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn duplicate_image_markdown_refs_are_rewritten_to_canonical() {
        fn image(id: &str, format: &str, duplicate_of: Option<&str>) -> ExtractedImage {
            ExtractedImage {
                id: id.into(),
                name: format!("img_{id}.{format}"),
                path: None,
                page: 1,
                bbox: crate::types::Rect::default(),
                width: 2,
                height: 2,
                rotation: 0.0,
                format: format.into(),
                duplicate_of: duplicate_of.map(str::to_owned),
                bytes: std::sync::Arc::new(Vec::new()),
            }
        }

        let mut pages = vec![ParsedPage {
            page_number: 1,
            page_width: 612.0,
            page_height: 792.0,
            content_bounds: None,
            text: String::new(),
            markdown: "intro\n\n![](img_p2_1.jpg)\n\noutro".into(),
            text_items: vec![],
            projected_lines: vec![],
            regions: crate::types::Region::default(),
            graphics: vec![],
            vector_graphics: None,
            figures: vec![],
            struct_nodes: vec![],
            image_refs: vec![],
            complexity: None,
            annotations: None,
            form_fields: None,
            structure_tree: None,
        }];
        let mut full_text = pages[0].markdown.clone();
        let images = vec![
            image("p1_1", "jpg", None),
            image("p2_1", "jpg", Some("p1_1")),
        ];

        rewrite_duplicate_image_refs(&mut pages, &mut full_text, &images);

        // The duplicate's ref now points at the canonical file; canonical
        // refs and surrounding text are untouched.
        assert_eq!(pages[0].markdown, "intro\n\n![](img_p1_1.jpg)\n\noutro");
        assert_eq!(full_text, pages[0].markdown);
    }
}
