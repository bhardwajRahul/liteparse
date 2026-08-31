//! DOCX → [`Block`] using only `liteparse-ooxml`'s `parse` + `resolve` stages.
//!
//! Deliberately avoids the vendored crate's `render::layout` stage: structure
//! alone — headings, lists, emphasis, merged cells, notes — is recoverable
//! without a layout engine, and layout is where the font/measurement
//! dependencies live. Geometry is a separate concern (see `docx_layout`); this
//! module produces no bounding boxes.
//!
//! What `resolve()` hands us is *not* fully cascaded: it walks `basedOn` chains
//! and flattens numbering (including `lvlOverride`/`startOverride`), but leaves
//! the per-run merge to the caller. That merge is [`Emitter::effective_fmt`].

use std::collections::{HashMap, VecDeque};

use liteparse_ooxml::field::FieldInstruction;
use liteparse_ooxml::model::{
    Block as DocxBlock, GraphicContent, HyperlinkTarget, Inline, NoteId, NumId, NumberFormat,
    OutlineLevel, Paragraph, ParagraphProperties, RunElement, RunProperties, StrikeStyle, StyleId,
    Table, VerticalMerge,
};
use liteparse_ooxml::render::resolve::ResolvedDocument;

use crate::error::LiteParseError;
use crate::markdown_layout::{Block, Cell, escape_inline};
use crate::office::inline::{Chunk, Fmt, render_chunks};

/// What the emitter should produce beyond the always-on structure.
#[derive(Default)]
pub struct EmitOptions {
    /// Render external hyperlinks as `[text](url)`. Mirrors
    /// `LiteParseConfig::extract_links`, which the PDF path honors too.
    pub links: bool,
    /// Layout-assigned figure ids: media identity (`MediaEntry::data`
    /// allocation pointer) → FIFO of `(id, extension)` in draw order, from
    /// `docx_layout::collect_images`. `Some` enables `Block::Figure`
    /// emission; `None` (structure-only callers, `image_mode == Off`) keeps
    /// the block stream figure-free. Ids must come from the layout because
    /// they are page-scoped (`p{page}_{n}`) and the structure walk does not
    /// know pages.
    pub figures: Option<HashMap<usize, VecDeque<(String, String)>>>,
}

/// Parse a DOCX and emit the shared block model.
///
/// `extract_links` mirrors `LiteParseConfig::extract_links`: when true,
/// external hyperlinks render as `[text](url)`, matching the PDF path.
///
/// Structure-only: no layout runs, so no `Block::Figure`s are emitted (their
/// ids are page-scoped and require layout — see [`EmitOptions::figures`]).
///
/// Errors only when the archive itself is unreadable — the vendored parser is
/// fail-open on unknown elements, unknown attribute values and malformed
/// scalars, so a document with unmodeled markup still yields blocks.
pub fn docx_to_blocks(data: &[u8], extract_links: bool) -> Result<Vec<Block>, LiteParseError> {
    let parsed = liteparse_ooxml::docx::parse(data)
        .map_err(|e| LiteParseError::Conversion(format!("docx parse failed: {e}")))?;
    let resolved = liteparse_ooxml::render::resolve::resolve(parsed);
    Ok(emit(&resolved, extract_links))
}

/// Emit blocks for an already-resolved document, structure-only (no figures).
pub fn emit(doc: &ResolvedDocument, extract_links: bool) -> Vec<Block> {
    emit_with_sources(
        doc,
        EmitOptions {
            links: extract_links,
            figures: None,
        },
    )
    .into_iter()
    .map(|(b, _)| b)
    .collect()
}

/// Where an emitted [`Block`] came from, for the layout block→page join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSource {
    /// Index into the flattened concatenation of every section's blocks
    /// (section breaks included) — the same flattening whose indices the
    /// layout engine records in `LayoutedPage::block_starts`.
    Body(usize),
    /// Footnote/endnote bodies, and the `HorizontalRule` that precedes them.
    Note,
}

/// [`emit`], with each block tagged by the source body block that produced it.
///
/// One source block maps to zero or one emitted blocks today (empty paragraphs
/// and section breaks emit nothing; nothing merges across source blocks), but
/// the tagging tolerates zero-or-more either way.
pub fn emit_with_sources(doc: &ResolvedDocument, opts: EmitOptions) -> Vec<(Block, BlockSource)> {
    let mut em = Emitter::new(doc, opts);
    let mut out: Vec<(Block, BlockSource)> = Vec::new();
    // One Emitter across all blocks: list counters must persist so an ordered
    // list interrupted by prose continues its count.
    let mut scratch = Vec::new();
    let mut flat_idx = 0usize;
    for section in &doc.sections {
        for block in &section.blocks {
            em.blocks(std::slice::from_ref(block), &mut scratch);
            for b in scratch.drain(..) {
                out.push((b, BlockSource::Body(flat_idx)));
            }
            flat_idx += 1;
        }
    }

    // Note bodies are appended after the document, behind a rule. Emit *all* of
    // them, not just those whose reference we walked past: notes are frequently
    // referenced from headers, footers and textboxes, and content-completeness
    // is what matters here. Note ids 0 and 1 are the separator and
    // continuationSeparator — chrome, never real content.
    let mut notes = Vec::new();
    for map in [&doc.footnotes, &doc.endnotes] {
        let mut ids: Vec<NoteId> = map.keys().copied().filter(|i| i.value() > 1).collect();
        ids.sort_by_key(|i| i.value());
        for id in ids {
            em.blocks(&map[&id], &mut notes);
        }
    }
    if !notes.is_empty() {
        out.push((Block::HorizontalRule, BlockSource::Note));
        out.extend(notes.into_iter().map(|b| (b, BlockSource::Note)));
    }
    out
}

struct Emitter<'a> {
    doc: &'a ResolvedDocument,
    /// `(numId, level)` → next counter, so ordered lists number correctly and
    /// a list interrupted by prose continues its count.
    counters: HashMap<(i64, u8), u32>,
    /// True while emitting the content of a table cell. Cell text must stay
    /// *unescaped*: `render_blocks` escapes it per dialect on the way out (`|`
    /// for pipe tables, `&<>` for HTML), and markdown-escaping it here would
    /// either double the backslashes (`\\*`) or surface them literally inside
    /// an HTML block, where markdown escapes mean nothing.
    in_cell: bool,
    /// Render external hyperlinks as `[text](url)`. Mirrors
    /// `LiteParseConfig::extract_links`, which the PDF path honors too.
    links: bool,
    /// Figure-id queues from the layout (see [`EmitOptions::figures`]).
    figures: Option<HashMap<usize, VecDeque<(String, String)>>>,
    /// Figures encountered during the current paragraph/table walk, flushed
    /// as standalone [`Block::Figure`]s after the enclosing block — images
    /// are inline in the model but block-level in markdown, same as the PDF
    /// path's y-interleaved figures.
    pending_figures: Vec<(String, String)>,
    /// Blocks harvested from text boxes met during the current paragraph/table
    /// walk, flushed after the enclosing block. §17.17.1 `wps:txbx` and
    /// §14.1.2.22 `v:textbox` hold *block* content — the corpus's boxes are
    /// multi-paragraph callouts with their own heading and bullets — so they
    /// cannot be folded into the host paragraph's chunk stream without fusing
    /// that structure into one line.
    pending_boxes: Vec<Block>,
    /// Recursion guard: a text box can hold a shape that holds a text box.
    box_depth: u8,
}

/// Nesting depth past which a text box's interior is not walked. Bounds a
/// pathological or cyclic file; real documents do not nest text boxes.
const MAX_TEXT_BOX_DEPTH: u8 = 4;

impl<'a> Emitter<'a> {
    fn new(doc: &'a ResolvedDocument, opts: EmitOptions) -> Self {
        Self {
            doc,
            counters: HashMap::new(),
            in_cell: false,
            links: opts.links,
            figures: opts.figures,
            pending_figures: Vec::new(),
            pending_boxes: Vec::new(),
            box_depth: 0,
        }
    }

    /// Drain the figures and text-box blocks gathered while walking the current
    /// block's inlines. Figures first: an image and a text box in the same
    /// paragraph is a captioned-figure shape, and the caption reads after the
    /// picture.
    fn flush_pending(&mut self, out: &mut Vec<Block>) {
        for (id, format) in self.pending_figures.drain(..) {
            out.push(Block::Figure { id, format });
        }
        out.append(&mut self.pending_boxes);
    }

    fn style_run(&self, id: Option<&StyleId>) -> Option<&RunProperties> {
        let id = id.or(self.doc.default_paragraph_style_id.as_ref())?;
        self.doc.styles.get(id).map(|s| &s.run)
    }

    fn style_para(&self, id: Option<&StyleId>) -> Option<&ParagraphProperties> {
        let id = id.or(self.doc.default_paragraph_style_id.as_ref())?;
        self.doc.styles.get(id).map(|s| &s.paragraph)
    }

    /// The cascade `resolve()` intentionally leaves to the caller:
    /// doc defaults → paragraph style → run's character style → direct.
    ///
    /// `Option<bool>` distinguishes "inherit" (`None`) from "explicitly off"
    /// (`Some(false)`), so a `NotBold` character style correctly overrides a
    /// bold paragraph style rather than being ignored.
    fn effective_fmt(
        &self,
        para: &Paragraph,
        run_style: Option<&StyleId>,
        direct: &RunProperties,
    ) -> Fmt {
        let mut bold = self.doc.doc_defaults_run.bold;
        let mut italic = self.doc.doc_defaults_run.italic;
        let mut strike = self.doc.doc_defaults_run.strike;

        let mut apply = |p: &RunProperties| {
            if p.bold.is_some() {
                bold = p.bold;
            }
            if p.italic.is_some() {
                italic = p.italic;
            }
            if p.strike.is_some() {
                strike = p.strike;
            }
        };

        if let Some(r) = self.style_run(para.style_id.as_ref()) {
            apply(r);
        }
        if let Some(s) = run_style.and_then(|id| self.doc.styles.get(id)) {
            apply(&s.run);
        }
        apply(direct);

        Fmt {
            bold: bold.unwrap_or(false),
            italic: italic.unwrap_or(false),
            strike: !matches!(strike.unwrap_or(StrikeStyle::None), StrikeStyle::None),
        }
    }

    /// Escape markdown specials unless we are inside a table cell.
    fn escape(&self, s: &str) -> String {
        if self.in_cell {
            s.to_string()
        } else {
            escape_inline(s)
        }
    }

    /// Cascade the two paragraph-level facts this emitter reads. Direct
    /// formatting wins; the paragraph style fills in what it leaves unset.
    fn para_props(&self, para: &Paragraph) -> ParagraphProperties {
        let mut out = para.properties.clone();
        if let Some(sp) = self.style_para(para.style_id.as_ref()) {
            if out.outline_level.is_none() {
                out.outline_level = sp.outline_level;
            }
            if out.numbering.is_none() {
                out.numbering = sp.numbering;
            }
        }
        out
    }

    /// Flatten a paragraph's inlines into formatting-tagged text chunks, in
    /// document order. Figures encountered along the way accumulate in
    /// `pending_figures` for the caller to flush after its block.
    fn chunks(&mut self, para: &Paragraph, content: &[Inline]) -> Vec<Chunk> {
        let mut out = Vec::new();
        self.collect(para, content, None, &mut out);
        out
    }

    fn collect(
        &mut self,
        para: &Paragraph,
        content: &[Inline],
        link: Option<&str>,
        out: &mut Vec<Chunk>,
    ) {
        // Links are emitted only outside table cells: cell text travels
        // unescaped and is escaped per dialect by the renderer, which would
        // mangle a baked-in `[text](url)`. The PDF path's cells are plain
        // text too, so this keeps the two pipelines rendering cells alike.
        let emit_links = self.links && !self.in_cell;
        for inline in content {
            match inline {
                Inline::TextRun(run) => {
                    let fmt = self.effective_fmt(para, run.style_id.as_ref(), &run.properties);
                    let mut text = String::new();
                    for el in &run.content {
                        match el {
                            RunElement::Text(t) => text.push_str(t),
                            // A tab or a break inside a paragraph is a
                            // *within-line* separator once the paragraph
                            // becomes one markdown line; a space is the only
                            // faithful rendering.
                            RunElement::Tab | RunElement::LineBreak(_) => text.push(' '),
                            _ => {}
                        }
                    }
                    if !text.is_empty() {
                        out.push(Chunk {
                            fmt,
                            link: link.map(str::to_string),
                            text,
                        });
                    }
                }
                Inline::Hyperlink(h) => {
                    // Only resolved external URLs become markdown links.
                    // Internal bookmark anchors (TOC entries, cross-refs) have
                    // no addressable target in a markdown document, and an
                    // unresolved rel id is not a URL.
                    let url = match &h.target {
                        HyperlinkTarget::ExternalUrl(u) if emit_links => Some(u.as_str()),
                        _ => None,
                    };
                    self.collect(para, &h.content, url.or(link), out);
                }
                Inline::Field(f) => {
                    // HYPERLINK fields carry the URL in the instruction
                    // itself; an empty target with `\l` is an internal
                    // anchor, skipped for the same reason as above.
                    let url = match &f.instruction {
                        FieldInstruction::Hyperlink { target, .. }
                            if emit_links && !target.is_empty() =>
                        {
                            Some(target.as_str())
                        }
                        _ => None,
                    };
                    self.collect(para, &f.content, url.or(link), out);
                }
                // §17.16.7: a FORMCHECKBOX field renders as its ffData state
                // (☐/☒) — it has no result-zone runs to pick up. Other field
                // markers stay invisible: the instruction is InstrText (never
                // a TextRun) and result runs flow through on their own.
                Inline::FieldChar(fc) => {
                    if let Some(glyph) = fc.form_checkbox_glyph() {
                        out.push(Chunk {
                            fmt: Fmt::default(),
                            link: link.map(str::to_string),
                            text: glyph.to_string(),
                        });
                    }
                }
                Inline::Image(img) => {
                    // Pop the layout-assigned id for this media's next
                    // placement. The queue is keyed by media identity and
                    // both walks visit body images in document order, so the
                    // nth structure occurrence pairs with the nth drawn
                    // placement. A miss (queue absent or drained — e.g. a
                    // format the image collector skips) emits nothing rather
                    // than a dangling reference.
                    if let Some(queues) = self.figures.as_mut()
                        && let Some(rel) =
                            liteparse_ooxml::render::resolve::images::extract_image_rel_id(img)
                        && let Some(media) = self.doc.media.get(rel)
                        && let Some(fig) = queues
                            .get_mut(&(media.data.as_ptr() as usize))
                            .and_then(|q| q.pop_front())
                    {
                        self.pending_figures.push(fig);
                    }
                    // §14.5 wps:wsp — a shape's `wps:txbx` body. Reached for
                    // both placements: an inline box and an anchored callout
                    // are the same content to a reader, and markdown has no
                    // float to distinguish them with.
                    if let Some(GraphicContent::WordProcessingShape(wsp)) = img.graphic.as_ref() {
                        self.harvest_text_box(&wsp.txbx_content);
                    }
                }
                // §17.3.3.19: legacy VML drawing. Every primitive variant
                // admits a `<v:textbox>`, so the walk is over `common()`
                // rather than the `Shape` variant alone — the same rule
                // `fragment::collect` applies on the layout side.
                Inline::Pict(pict) => {
                    for primitive in &pict.primitives {
                        if let Some(tb) = primitive.common().text_box.as_ref() {
                            self.harvest_text_box(&tb.content);
                        }
                    }
                }
                // MCE §M.2.1. A shape is normally written as a `wps` choice
                // with a VML fallback carrying *the same text*, so exactly one
                // branch may be walked or the box is emitted twice.
                Inline::AlternateContent(ac) => {
                    let before = self.pending_boxes.len();
                    for choice in &ac.choices {
                        self.harvest_boxes(&choice.content);
                        if self.pending_boxes.len() > before {
                            break;
                        }
                    }
                    if self.pending_boxes.len() == before
                        && let Some(fallback) = ac.fallback.as_ref()
                    {
                        self.harvest_boxes(fallback);
                    }
                }
                _ => {}
            }
        }
    }

    /// Text boxes only, for the interior of an `mc:AlternateContent`.
    ///
    /// Deliberately *not* `collect`: a branch's runs and images are left alone.
    /// Run text inside a choice is already rendered by whichever branch the
    /// layout picked, and popping a figure id here would desync the queue,
    /// which pairs the nth structure occurrence with the nth *drawn* placement
    /// and does not walk `mc:AlternateContent` at all. A picture inside one
    /// does not occur in practice, so this keeps the figure pairing unchanged.
    fn harvest_boxes(&mut self, content: &[Inline]) {
        for inline in content {
            match inline {
                Inline::Image(img) => {
                    if let Some(GraphicContent::WordProcessingShape(wsp)) = img.graphic.as_ref() {
                        self.harvest_text_box(&wsp.txbx_content);
                    }
                }
                Inline::Pict(pict) => {
                    for primitive in &pict.primitives {
                        if let Some(tb) = primitive.common().text_box.as_ref() {
                            self.harvest_text_box(&tb.content);
                        }
                    }
                }
                Inline::Hyperlink(h) => self.harvest_boxes(&h.content),
                Inline::Field(f) => self.harvest_boxes(&f.content),
                Inline::AlternateContent(ac) => {
                    let before = self.pending_boxes.len();
                    for choice in &ac.choices {
                        self.harvest_boxes(&choice.content);
                        if self.pending_boxes.len() > before {
                            break;
                        }
                    }
                    if self.pending_boxes.len() == before
                        && let Some(fallback) = ac.fallback.as_ref()
                    {
                        self.harvest_boxes(fallback);
                    }
                }
                _ => {}
            }
        }
    }

    /// Emit a text box's block content onto `pending_boxes`.
    ///
    /// The interior is a full block walk — a box holding a table emits a table
    /// — but it runs with figure emission **off**: figure ids come from the
    /// layout's draw order, which does not queue a box's interior images, so a
    /// pop here would shift every later image's id by one. A picture inside a
    /// text box therefore stays absent from markdown, as it is today.
    fn harvest_text_box(&mut self, content: &[DocxBlock]) {
        if content.is_empty() || self.box_depth >= MAX_TEXT_BOX_DEPTH {
            return;
        }
        self.box_depth += 1;
        let figures = self.figures.take();
        // The nested walk runs `paragraph`/`table`, which flush. Park the host
        // block's queues so the box's interior cannot drain them into itself.
        let outer_figures = std::mem::take(&mut self.pending_figures);
        let outer_boxes = std::mem::take(&mut self.pending_boxes);

        let mut inner = Vec::new();
        self.blocks(content, &mut inner);

        self.pending_figures = outer_figures;
        self.pending_boxes = outer_boxes;
        self.figures = figures;
        self.box_depth -= 1;
        self.pending_boxes.extend(inner);
    }

    fn paragraph(&mut self, para: &Paragraph, out: &mut Vec<Block>) {
        let props = self.para_props(para);
        let chunks = self.chunks(para, &para.content);
        if chunks.iter().all(|c| c.text.trim().is_empty()) {
            // An image in its own paragraph is the common case: no text, but
            // the walk collected figures that must still be emitted.
            self.flush_pending(out);
            return;
        }

        // Headings win over list formatting: a numbered heading is a heading.
        if let Some(lvl) = props.outline_level {
            // Deliberately plain — no `**` inside the `#`. The `#` *is* the
            // emphasis, and `# **Heading**` is redundant markup. Links are
            // flattened too: heading text feeds the outline and anchors.
            let text: String = chunks.iter().map(|c| c.text.as_str()).collect();
            out.push(Block::Heading {
                // The declared outline level, not a remapped one: a dense 1..N
                // remap discards the author's intended heading ranks.
                level: OutlineLevel::value(lvl).clamp(1, 6),
                text: self.escape(text.trim()),
            });
            self.flush_pending(out);
            return;
        }

        let (text, bold, italic) = render_chunks(&chunks, !self.in_cell);

        if let Some(nref) = props.numbering {
            let level = self
                .doc
                .numbering
                .get(&NumId::new(nref.num_id))
                .and_then(|lv| lv.get(nref.level as usize));
            let ordered = !matches!(level.map(|l| l.format), Some(NumberFormat::Bullet) | None);
            let marker = if ordered {
                let start = level.map(|l| l.start).unwrap_or(1);
                let key = (nref.num_id, nref.level);
                let n = *self
                    .counters
                    .entry(key)
                    .and_modify(|c| *c += 1)
                    .or_insert(start);
                // Deeper levels restart once a shallower one advances.
                self.counters
                    .retain(|(id, l), _| !(*id == nref.num_id && *l > nref.level));
                format!("{n}.")
            } else {
                // `render_blocks` prints `- ` for unordered items and ignores
                // this, but keep it meaningful rather than empty.
                "-".to_string()
            };
            out.push(Block::ListItem {
                ordered,
                marker,
                level: nref.level,
                text,
                bold,
                italic,
            });
            self.flush_pending(out);
            return;
        }

        out.push(Block::Paragraph { text, bold, italic });
        self.flush_pending(out);
    }

    /// Emit a table as [`Block::MergedTable`], which is the whole point of the
    /// native path: `gridSpan` and `vMerge` are explicit in the source, so the
    /// merge structure is read rather than inferred from cell coordinates.
    fn table(&mut self, table: &Table, out: &mut Vec<Block>) {
        // Grid column of each cell, so a vMerge run can be matched down a
        // column even when rows have different cell counts.
        let mut cols: Vec<Vec<u32>> = Vec::new();
        for row in &table.rows {
            let mut c = row.properties.grid_before;
            let mut rc = Vec::new();
            for cell in &row.cells {
                rc.push(c);
                c += cell.properties.grid_span.unwrap_or(1);
            }
            cols.push(rc);
        }

        // Row extent of each vMerge=Restart cell: how many following rows carry
        // a Continue cell in the same grid column.
        let mut rowspans: Vec<Vec<u16>> =
            table.rows.iter().map(|r| vec![1; r.cells.len()]).collect();
        for r in 0..table.rows.len() {
            for c in 0..table.rows[r].cells.len() {
                if table.rows[r].cells[c].properties.vertical_merge != Some(VerticalMerge::Restart)
                {
                    continue;
                }
                let col = cols[r][c];
                let mut n: u16 = 1;
                for r2 in r + 1..table.rows.len() {
                    let continues = table.rows[r2]
                        .cells
                        .iter()
                        .zip(&cols[r2])
                        .any(|(cell, cc)| {
                            *cc == col
                                && cell.properties.vertical_merge == Some(VerticalMerge::Continue)
                        });
                    if !continues {
                        break;
                    }
                    n += 1;
                }
                rowspans[r][c] = n;
            }
        }

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for (r, row) in table.rows.iter().enumerate() {
            let mut cells = Vec::new();
            for (c, cell) in row.cells.iter().enumerate() {
                // Continuation cells are absorbed into the restart cell's
                // rowspan and are absent from the grid, matching HTML.
                if cell.properties.vertical_merge == Some(VerticalMerge::Continue) {
                    continue;
                }
                cells.push(Cell::spanning(
                    self.cell_text(&cell.content),
                    cell.properties.grid_span.unwrap_or(1) as u16,
                    rowspans[r][c],
                ));
            }
            rows.push(cells);
        }

        // `header_rows` is a count of *leading* header rows. A `tblHeader` flag
        // deeper in the table marks a repeat-on-page-break row, which is not a
        // header in any markup we emit, so stop at the first non-header row.
        let header_rows = table
            .rows
            .iter()
            .take_while(|r| r.properties.is_header.unwrap_or(false))
            .count();

        if rows.iter().any(|r| !r.is_empty()) {
            out.push(Block::MergedTable { rows, header_rows });
        }
        // Figures found inside cells (bubbled up by `cell_text`) land after
        // the table: both table dialects are text-only per cell, so an image
        // reference inside one would be mangled by the cell escaping.
        self.flush_pending(out);
    }

    /// Flatten a cell's block content to a single line of inline markdown.
    ///
    /// A cell is a `Vec<Block>` in DOCX (it can hold paragraphs, lists, even
    /// nested tables) but both of our table renderings are single-line per
    /// cell, so the content is collapsed. Text is left *unescaped*: the
    /// renderer escapes per dialect (`|` for pipes, `&<>` for HTML), and
    /// escaping here would double up.
    fn cell_text(&mut self, content: &[DocxBlock]) -> String {
        let outer = std::mem::replace(&mut self.in_cell, true);
        let mut inner = Vec::new();
        self.blocks(content, &mut inner);
        self.in_cell = outer;
        let mut parts: Vec<String> = Vec::new();
        for b in inner {
            match b {
                Block::Heading { text, .. } | Block::Paragraph { text, .. } => parts.push(text),
                Block::ListItem { text, .. } => parts.push(text),
                // A figure inside a cell can't live in single-line cell text;
                // requeue it so the enclosing `table` emits it afterwards.
                Block::Figure { id, format } => self.pending_figures.push((id, format)),
                _ => {}
            }
        }
        parts
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn blocks(&mut self, blocks: &[DocxBlock], out: &mut Vec<Block>) {
        for b in blocks {
            match b {
                DocxBlock::Paragraph(p) => self.paragraph(p, out),
                DocxBlock::Table(t) => self.table(t, out),
                DocxBlock::SectionBreak(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(bold: bool, italic: bool, strike: bool) -> Fmt {
        Fmt {
            bold,
            italic,
            strike,
        }
    }

    fn chunks(v: &[(Fmt, &str)]) -> Vec<Chunk> {
        v.iter()
            .map(|(f, t)| Chunk {
                fmt: *f,
                link: None,
                text: t.to_string(),
            })
            .collect()
    }

    fn linked(f: Fmt, url: &str, t: &str) -> Chunk {
        Chunk {
            fmt: f,
            link: Some(url.to_string()),
            text: t.to_string(),
        }
    }

    #[test]
    fn uniform_emphasis_becomes_block_flags() {
        let (text, bold, italic) =
            render_chunks(&chunks(&[(fmt(true, false, false), "all bold")]), true);
        assert_eq!((text.as_str(), bold, italic), ("all bold", true, false));
    }

    #[test]
    fn mixed_emphasis_is_baked_inline_and_flags_cleared() {
        let (text, bold, italic) = render_chunks(
            &chunks(&[
                (fmt(false, false, false), "plain "),
                (fmt(true, false, false), "bold"),
            ]),
            true,
        );
        assert_eq!(
            (text.as_str(), bold, italic),
            ("plain **bold**", false, false)
        );
    }

    /// `Block` has no strike flag, so a uniformly-struck paragraph must take
    /// the inline path rather than silently losing the strikethrough.
    #[test]
    fn uniform_strike_falls_back_to_inline() {
        let (text, bold, italic) =
            render_chunks(&chunks(&[(fmt(false, false, true), "gone")]), true);
        assert_eq!((text.as_str(), bold, italic), ("~~gone~~", false, false));
    }

    /// Markers must hug non-space content or markdown won't parse them, so
    /// whitespace inside a run is hoisted outside the markers.
    #[test]
    fn markers_hug_content_across_run_whitespace() {
        let (text, ..) = render_chunks(
            &chunks(&[
                (fmt(true, false, false), "bold "),
                (fmt(false, false, false), "tail"),
            ]),
            true,
        );
        assert_eq!(text, "**bold** tail");
    }

    #[test]
    fn escaping_is_skipped_for_cell_content() {
        let c = chunks(&[(fmt(false, false, false), "a_b*c")]);
        assert_eq!(render_chunks(&c, true).0, "a\\_b\\*c");
        // Inside a cell the renderer escapes per dialect; escaping here would
        // double the backslash in a pipe table and surface it in HTML.
        assert_eq!(render_chunks(&c, false).0, "a_b*c");
    }

    #[test]
    fn adjacent_runs_sharing_format_do_not_split_markers() {
        let (text, ..) = render_chunks(
            &chunks(&[
                (fmt(true, false, false), "a"),
                (fmt(true, false, false), "b"),
                (fmt(false, false, false), "!"),
            ]),
            true,
        );
        assert_eq!(text, "**ab**!");
    }

    /// A link wraps exactly its anchor text, with emphasis inside the
    /// brackets — `[*anchor*](url)` — matching the PDF path's rendering.
    #[test]
    fn link_wraps_anchor_outside_emphasis() {
        let (text, bold, italic) = render_chunks(
            &[
                chunks(&[(fmt(false, false, false), "see ")]).remove(0),
                linked(fmt(false, true, false), "https://example.com", "the docs"),
            ],
            true,
        );
        assert_eq!(
            (text.as_str(), bold, italic),
            ("see [*the docs*](https://example.com)", false, false)
        );
    }

    /// A uniformly-formatted paragraph that is entirely one link must still
    /// take the inline path — block flags cannot carry a URL.
    #[test]
    fn uniform_link_paragraph_stays_inline() {
        let (text, bold, ..) = render_chunks(
            &[linked(
                fmt(false, false, false),
                "https://example.com",
                "home",
            )],
            true,
        );
        assert_eq!(text, "[home](https://example.com)");
        assert!(!bold);
    }

    /// Word splits runs inside one hyperlink too; adjacent chunks sharing
    /// format and link must coalesce into a single `[anchor](url)`.
    #[test]
    fn adjacent_runs_in_one_link_coalesce() {
        let (text, ..) = render_chunks(
            &[
                linked(fmt(false, false, false), "https://example.com", "run"),
                linked(fmt(false, false, false), "https://example.com", "llama"),
            ],
            true,
        );
        assert_eq!(text, "[runllama](https://example.com)");
    }

    /// URLs containing spaces or parentheses take the angle-bracket form so
    /// the destination cannot terminate early.
    #[test]
    fn link_url_with_space_uses_angle_brackets() {
        let (text, ..) = render_chunks(
            &[linked(fmt(false, false, false), "https://e.com/a b", "x")],
            true,
        );
        assert_eq!(text, "[x](<https://e.com/a b>)");
    }

    /// Provenance tagging must not change what is emitted; body indices must
    /// be non-decreasing (the block→page join walks them with a forward
    /// cursor); and note bodies must form a `Note`-tagged tail behind the
    /// rule, so the page split can send them to the last page.
    #[test]
    fn emit_with_sources_is_emit_plus_tags() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docx_files/legal/courts_3rd_circuit_ch4.docx"
        ))
        .expect("corpus fixture");
        let parsed = liteparse_ooxml::docx::parse(&data).expect("fixture parses");
        let doc = liteparse_ooxml::render::resolve::resolve(parsed);

        let tagged = emit_with_sources(
            &doc,
            EmitOptions {
                links: true,
                figures: None,
            },
        );
        let plain = emit(&doc, true);
        assert_eq!(
            format!("{plain:?}"),
            format!("{:?}", tagged.iter().map(|(b, _)| b).collect::<Vec<_>>()),
            "tagging must be a pure annotation"
        );

        let body: Vec<usize> = tagged
            .iter()
            .filter_map(|(_, s)| match s {
                BlockSource::Body(i) => Some(*i),
                BlockSource::Note => None,
            })
            .collect();
        assert!(!body.is_empty());
        assert!(
            body.windows(2).all(|w| w[0] <= w[1]),
            "body indices are non-decreasing"
        );

        let first_note = tagged
            .iter()
            .position(|(_, s)| *s == BlockSource::Note)
            .expect("fixture has footnotes");
        assert!(
            matches!(tagged[first_note].0, Block::HorizontalRule),
            "notes start with the separating rule"
        );
        assert!(
            tagged[first_note..]
                .iter()
                .all(|(_, s)| *s == BlockSource::Note),
            "notes are a contiguous tail"
        );
        assert!(
            tagged[first_note..].len() > 1,
            "note bodies follow the rule"
        );
    }
}
