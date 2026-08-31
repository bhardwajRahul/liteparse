//! Parser for document body content: blocks (paragraphs, tables, section breaks)
//! and inline content (text runs, images, hyperlinks, fields, etc.).
//!
//! Single-pass serde over the full document. Drawings and VML picts are
//! serde-parsed inline via `DrawingXml` / `PictXml`; they produce their
//! model values (`Image` / `Pict`) during the `convert_container` walk via
//! the `ConvertCtx`.
//!
//! No style resolution or property merging — output is raw parsed data.

use crate::docx::error::Result;
use crate::docx::model::*;
use crate::docx::parse::body_schema::*;
use crate::docx::parse::serde_xml::from_xml;
use crate::docx::whitespace_workaround::restore_whitespace_sentinels;

/// Parse `w:document > w:body`, returning blocks and final section properties.
pub fn parse_body(data: &[u8]) -> Result<(Vec<Block>, SectionProperties)> {
    if data.is_empty() {
        return Ok((Vec::new(), SectionProperties::default()));
    }
    let doc: DocXml = from_xml(data)?;
    let mut ctx = ConvertCtx::new();
    let (blocks, final_section) = convert_container(doc.body.children, &mut ctx);
    Ok((blocks, final_section.unwrap_or_default()))
}

/// Parse a body-level XML part (header, footer, footnote body, etc.) into blocks.
pub fn parse_blocks(data: &[u8]) -> Result<Vec<Block>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let container: BlockContainerXml = from_xml(data)?;
    let mut ctx = ConvertCtx::new();
    let (blocks, _) = convert_container(container.children, &mut ctx);
    Ok(blocks)
}

// ── Top-level document schema wrapper ────────────────────────────────────

use serde::Deserialize;

/// Thin wrapper for `<w:document>` — just extracts `<w:body>`.
#[derive(Deserialize)]
struct DocXml {
    body: BlockContainerXml,
}

// ── Conversion ────────────────────────────────────────────────────────────

/// Conversion context, threaded through the body walk. Currently empty —
/// drawings and picts are serde-parsed inline — but kept as a type so a
/// later phase can add cross-node state without changing every call site.
pub(crate) struct ConvertCtx {
    _private: (),
}

impl ConvertCtx {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

/// Convert a list of block-level children into `(Vec<Block>, Option<SectionProperties>)`.
/// The section properties, if returned, are for a trailing `<w:sectPr>` at
/// this level — the final section for `<w:body>`, or one that appears inside
/// a table cell (§17.6.17).
pub(crate) fn convert_container(
    children: Vec<BlockChildXml>,
    ctx: &mut ConvertCtx,
) -> (Vec<Block>, Option<SectionProperties>) {
    let mut blocks = Vec::new();
    let mut final_section = None;
    for child in children {
        match child {
            BlockChildXml::Paragraph(p) => {
                let (para, sect_after) = convert_paragraph(*p, ctx);
                blocks.push(Block::Paragraph(Box::new(para)));
                if let Some(sp) = sect_after {
                    blocks.push(Block::SectionBreak(Box::new(sp)));
                }
            }
            BlockChildXml::Table(t) => {
                blocks.push(Block::Table(Box::new(convert_table(*t, ctx))));
            }
            BlockChildXml::SectPr(sp) => {
                final_section = Some(SectionProperties::from(*sp));
            }
            BlockChildXml::Sdt(sdt) => {
                // Flatten SDT wrapper — treat its content as block-level.
                if let Some(content) = sdt.content {
                    let (nested_blocks, nested_sect) = convert_container(content.children, ctx);
                    blocks.extend(nested_blocks);
                    if nested_sect.is_some() {
                        final_section = nested_sect;
                    }
                }
            }
            // Block-level markers and ignored elements — renderer doesn't use them.
            BlockChildXml::BookmarkStart(_)
            | BlockChildXml::BookmarkEnd(_)
            | BlockChildXml::CommentRangeStart(_)
            | BlockChildXml::CommentRangeEnd(_)
            | BlockChildXml::ProofErr(_)
            | BlockChildXml::Other => {}
        }
    }
    (blocks, final_section)
}

fn convert_paragraph(p: ParaXml, ctx: &mut ConvertCtx) -> (Paragraph, Option<SectionProperties>) {
    let rsids = ParagraphRevisionIds {
        r: hex_rsid(p.rsid_r.as_deref()),
        r_default: hex_rsid(p.rsid_r_default.as_deref()),
        p: hex_rsid(p.rsid_p.as_deref()),
        r_pr: hex_rsid(p.rsid_r_pr.as_deref()),
        del: hex_rsid(p.rsid_del.as_deref()),
    };

    // pPr may appear as either the dedicated field OR inside $value (serde
    // collects all matching children; since `pPr` is named on the struct
    // *and* in the enum, serde prefers the dedicated field — but just in
    // case, we merge from both sources).
    let p_pr = p.p_pr.or_else(|| {
        p.content.iter().find_map(|c| {
            if let ParaChildXml::PPr(pp) = c {
                Some((**pp).clone())
            } else {
                None
            }
        })
    });

    let parsed_p_pr = p_pr.map(|pp| pp.split());
    let (style_id, properties, mark_run_properties, section_properties) = match parsed_p_pr {
        Some(pp) => (
            pp.style_id,
            pp.properties,
            pp.run_properties,
            pp.section_properties,
        ),
        None => (None, ParagraphProperties::default(), None, None),
    };

    let content = convert_para_children(p.content, ctx);

    (
        Paragraph {
            style_id,
            properties,
            mark_run_properties,
            content,
            rsids,
        },
        section_properties,
    )
}

/// Flatten a `RunXml`'s children into zero-or-more `Inline`s and append to
/// the parent content. Text / tab / br / cr / lastRenderedPageBreak are
/// accumulated into one `Inline::TextRun`; sibling inlines flush the accumulator
/// and append independently.
fn extend_from_run(r: RunXml, out: &mut Vec<Inline>, ctx: &mut ConvertCtx) {
    let rsids = RevisionIds {
        r: hex_rsid(r.rsid_r.as_deref()),
        r_pr: hex_rsid(r.rsid_r_pr.as_deref()),
        del: hex_rsid(r.rsid_del.as_deref()),
    };
    let (props, style_id) = r.r_pr.map(|rp| rp.split()).unwrap_or_default();

    let mut acc: Vec<RunElement> = Vec::new();
    let flush = |acc: &mut Vec<RunElement>, out: &mut Vec<Inline>| {
        if !acc.is_empty() {
            out.push(Inline::TextRun(Box::new(TextRun {
                style_id: style_id.clone(),
                properties: props.clone(),
                content: std::mem::take(acc),
                rsids,
            })));
        }
    };

    for child in r.content {
        match child {
            // Unmodelled run-level element (e.g. commentReference) — skipped.
            RunChildXml::Other => {}
            RunChildXml::Text(t) => {
                acc.push(RunElement::Text(restore_whitespace_sentinels(&t.content)))
            }
            RunChildXml::DelText(t) => {
                acc.push(RunElement::Text(restore_whitespace_sentinels(&t.content)))
            }
            RunChildXml::Tab => acc.push(RunElement::Tab),
            RunChildXml::PTab(p) => acc.push(RunElement::PositionTab(p.into())),
            RunChildXml::Br(br) => acc.push(run_break(br)),
            RunChildXml::Cr => acc.push(RunElement::LineBreak(BreakKind::TextWrapping)),
            RunChildXml::SoftHyphen => {} // optional hyphen — only visible if line breaks here; we don't hyphenate
            RunChildXml::NoBreakHyphen => acc.push(RunElement::Text("\u{2011}".to_string())),
            RunChildXml::LastRenderedPageBreak => acc.push(RunElement::LastRenderedPageBreak),
            RunChildXml::Drawing(d) => {
                flush(&mut acc, out);
                if let Some(img) = drawing_to_image(d, ctx) {
                    out.push(Inline::Image(Box::new(img)));
                }
            }
            RunChildXml::Pict(p) => {
                flush(&mut acc, out);
                out.push(Inline::Pict(p.into_model(ctx)));
            }
            RunChildXml::Sym(s) => {
                flush(&mut acc, out);
                let char_code = u16::from_str_radix(&s.char, 16).unwrap_or_else(|_| {
                    log::warn!("sym: invalid hex char code {:?}; using 0", s.char);
                    0
                });
                out.push(Inline::Symbol(Symbol {
                    font: s.font,
                    char_code,
                }));
            }
            RunChildXml::InstrText(t) => {
                flush(&mut acc, out);
                out.push(Inline::InstrText(restore_whitespace_sentinels(&t.content)));
            }
            RunChildXml::FldChar(fc) => {
                // An unrecognized fldCharType carries no usable meaning (it
                // delimits begin/separate/end), so drop the marker entirely.
                let Some(ty) = fc.fld_char_type else { continue };
                flush(&mut acc, out);
                // §17.16.7: `<w:checked>` reflects a user toggle; absent, the
                // authored `<w:default>` is the rendered state.
                let form_checkbox = fc
                    .ff_data
                    .and_then(|ff| ff.check_box)
                    .map(|cb| cb.checked.or(cb.default).map(|v| v.0).unwrap_or(false));
                out.push(Inline::FieldChar(FieldChar {
                    field_char_type: FieldCharType::from(ty),
                    dirty: fc.dirty.map(|b| b.0),
                    fld_lock: fc.fld_lock.map(|b| b.0),
                    form_checkbox,
                }));
            }
            RunChildXml::FootnoteRef(n) => {
                // A reference with no usable id resolves to no note.
                let Some(id) = n.id else { continue };
                flush(&mut acc, out);
                out.push(Inline::FootnoteRef(NoteId::new(id)));
            }
            RunChildXml::EndnoteRef(n) => {
                let Some(id) = n.id else { continue };
                flush(&mut acc, out);
                out.push(Inline::EndnoteRef(NoteId::new(id)));
            }
            RunChildXml::FootnoteRefMark => {
                flush(&mut acc, out);
                out.push(Inline::FootnoteRefMark);
            }
            RunChildXml::EndnoteRefMark => {
                flush(&mut acc, out);
                out.push(Inline::EndnoteRefMark);
            }
            RunChildXml::Separator => {
                flush(&mut acc, out);
                out.push(Inline::Separator);
            }
            RunChildXml::ContinuationSeparator => {
                flush(&mut acc, out);
                out.push(Inline::ContinuationSeparator);
            }
            RunChildXml::AlternateContent(ac) => {
                flush(&mut acc, out);
                out.push(Inline::AlternateContent(convert_alt_content(ac, ctx)));
            }
            RunChildXml::RPr(_) => {} // already captured via r.r_pr
        }
    }
    flush(&mut acc, out);
}

fn run_break(br: BrXml) -> RunElement {
    use crate::docx::parse::body_schema::StBrType;
    match br.ty {
        Some(StBrType::Page) => RunElement::PageBreak,
        Some(StBrType::Column) => RunElement::ColumnBreak,
        _ => {
            let clear = br.clear.map(BreakClear::from).unwrap_or(BreakClear::None);
            if clear != BreakClear::None {
                RunElement::LineBreak(BreakKind::Clear(clear))
            } else {
                RunElement::LineBreak(BreakKind::TextWrapping)
            }
        }
    }
}

/// Convert a sequence of paragraph-level children (`EG_PContent`) into inline
/// content. Shared by `<w:p>`, `<w:hyperlink>`, and `<w:fldSimple>`, and used
/// recursively to flatten revision/structural wrappers.
///
/// Revision handling is the "accept all changes" (final) view: insert-side
/// wrappers (`<w:ins>`, `<w:moveTo>`) and structural wrappers (`<w:smartTag>`,
/// `<w:customXml>`) are flattened in place and rendered; delete-side wrappers
/// (`<w:del>`, `<w:moveFrom>`) are dropped along with their content. Nested
/// wrappers re-apply the same rules — e.g. a `<w:del>` inside a `<w:ins>` is
/// still dropped.
fn convert_para_children(children: Vec<ParaChildXml>, ctx: &mut ConvertCtx) -> Vec<Inline> {
    let mut content = Vec::new();
    append_para_children(children, &mut content, ctx);
    content
}

fn append_para_children(
    children: Vec<ParaChildXml>,
    content: &mut Vec<Inline>,
    ctx: &mut ConvertCtx,
) {
    for child in children {
        match child {
            ParaChildXml::Run(r) => extend_from_run(r, content, ctx),
            ParaChildXml::Hyperlink(h) => {
                content.push(Inline::Hyperlink(convert_hyperlink(h, ctx)));
            }
            ParaChildXml::FldSimple(f) => {
                content.push(Inline::Field(convert_fld_simple(f, ctx)));
            }
            ParaChildXml::BookmarkStart(b) => content.push(Inline::BookmarkStart {
                id: BookmarkId::new(b.id),
                name: b.name,
            }),
            ParaChildXml::BookmarkEnd(b) => {
                content.push(Inline::BookmarkEnd(BookmarkId::new(b.id)));
            }
            // Insert-side revision + structural wrappers: flatten and render.
            ParaChildXml::Ins(w)
            | ParaChildXml::MoveTo(w)
            | ParaChildXml::SmartTag(w)
            | ParaChildXml::CustomXml(w) => append_para_children(w.content, content, ctx),
            // §17.5.2.31: run-level SDT (checkbox/dropdown/date/text content
            // controls) — render the display content in place.
            ParaChildXml::Sdt(s) => {
                if let Some(inner) = s.content {
                    append_para_children(inner.content, content, ctx);
                }
            }
            // Delete-side revision wrappers: content is deleted — drop it.
            ParaChildXml::Del(_) | ParaChildXml::MoveFrom(_) => {}
            ParaChildXml::PPr(_) => {} // already captured on the parent
            ParaChildXml::Other => {}
        }
    }
}

fn convert_hyperlink(h: HyperlinkXml, ctx: &mut ConvertCtx) -> Hyperlink {
    let target = if let Some(id) = h.r_id {
        HyperlinkTarget::ExternalRel(RelId::new(id))
    } else {
        HyperlinkTarget::Internal {
            anchor: h.anchor.unwrap_or_default(),
        }
    };
    let content = convert_para_children(h.content, ctx);
    Hyperlink { target, content }
}

fn convert_fld_simple(f: FldSimpleXml, ctx: &mut ConvertCtx) -> Field {
    let instruction = match crate::field::parse(&f.instr) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("failed to parse field instruction {:?}: {}", f.instr, e);
            crate::field::FieldInstruction::Unknown {
                field_type: String::new(),
                raw: f.instr.clone(),
            }
        }
    };
    let content = convert_para_children(f.content, ctx);
    Field {
        instruction,
        content,
    }
}

fn convert_alt_content(a: AltContentXml, ctx: &mut ConvertCtx) -> AlternateContent {
    let choices = a
        .choices
        .into_iter()
        .filter_map(|c| {
            // §M.2.2: @Requires is a space-separated list of namespace prefixes;
            // the choice is usable only if we understand *all* of them. A single
            // unknown token drops the whole choice (falling through to the next
            // choice or the fallback).
            let requires: Vec<McRequires> = c
                .requires
                .split_whitespace()
                .map(mc_requires)
                .collect::<Option<_>>()?;
            let content = convert_mc_content(c.content, ctx);
            Some(McChoice { requires, content })
        })
        .collect();
    let fallback = a.fallback.map(|f| convert_mc_content(f.content, ctx));
    AlternateContent { choices, fallback }
}

fn mc_requires(s: &str) -> Option<McRequires> {
    match s {
        "wps" => Some(McRequires::Wps),
        "wpg" => Some(McRequires::Wpg),
        "wpc" => Some(McRequires::Wpc),
        "wpi" => Some(McRequires::Wpi),
        "m" => Some(McRequires::Math),
        "a14" => Some(McRequires::A14),
        "w14" => Some(McRequires::W14),
        "w15" => Some(McRequires::W15),
        "w16" => Some(McRequires::W16),
        other => {
            log::warn!("mc:Choice: unsupported Requires {:?}", other);
            None
        }
    }
}

fn convert_mc_content(items: Vec<McContentXml>, ctx: &mut ConvertCtx) -> Vec<Inline> {
    let mut out = Vec::new();
    for i in items {
        match i {
            McContentXml::Other => {}
            McContentXml::Drawing(d) => {
                if let Some(img) = drawing_to_image(d, ctx) {
                    out.push(Inline::Image(Box::new(img)));
                }
            }
            McContentXml::Pict(p) => {
                out.push(Inline::Pict(p.into_model(ctx)));
            }
        }
    }
    out
}

/// Convert a serde-parsed `<w:drawing>` into the model's `Image`. Returns
/// `None` when neither `<wp:inline>` nor `<wp:anchor>` is present.
fn drawing_to_image(
    d: crate::docx::parse::body_schema::DrawingXml,
    ctx: &mut ConvertCtx,
) -> Option<Image> {
    if let Some(inline) = d.inline {
        return Some(inline.into_image(ctx));
    }
    d.anchor.map(|a| a.into_image(ctx))
}

fn convert_table(t: TableXml, ctx: &mut ConvertCtx) -> Table {
    let (properties, _style_id) = t.tbl_pr.map(|tp| tp.split()).unwrap_or_default();
    let grid = t
        .tbl_grid
        .map(|g| {
            g.cols
                .into_iter()
                .map(|c| GridColumn {
                    width: c.w.unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = collect_table_rows(t.children)
        .into_iter()
        .map(|r| convert_table_row(r, ctx))
        .collect();
    Table {
        properties,
        grid,
        rows,
    }
}

/// Walk `<w:tbl>`'s direct children and flatten out every `<w:tr>`,
/// recursing through revision-tracking, custom-XML, and SDT row wrappers.
/// Range markers, proofreading errors, permission ranges, and the
/// `tblPr`/`tblGrid` duplicates produced by `$value` are dropped — they
/// have no rendered effect at table level. Document order is preserved.
fn collect_table_rows(children: Vec<TableChildXml>) -> Vec<TableRowXml> {
    let mut rows = Vec::with_capacity(children.len());
    for child in children {
        match child {
            TableChildXml::Row(r) => rows.push(*r),
            TableChildXml::Sdt(s) => {
                if let Some(content) = s.content {
                    rows.extend(collect_table_rows(content.children));
                }
            }
            TableChildXml::Ins(rt)
            | TableChildXml::Del(rt)
            | TableChildXml::MoveFrom(rt)
            | TableChildXml::MoveTo(rt) => rows.extend(rt.rows),
            TableChildXml::CustomXml(cx) => {
                rows.extend(collect_table_rows(cx.children));
            }
            TableChildXml::BookmarkStart(_)
            | TableChildXml::BookmarkEnd(_)
            | TableChildXml::CommentRangeStart(_)
            | TableChildXml::CommentRangeEnd(_)
            | TableChildXml::ProofErr(_)
            | TableChildXml::PermStart(_)
            | TableChildXml::PermEnd(_)
            | TableChildXml::TblPr(_)
            | TableChildXml::TblGrid(_)
            | TableChildXml::Other => {}
        }
    }
    rows
}

fn convert_table_row(r: TableRowXml, ctx: &mut ConvertCtx) -> TableRow {
    let rsids = TableRowRevisionIds {
        r: hex_rsid(r.rsid_r.as_deref()),
        r_pr: hex_rsid(r.rsid_r_pr.as_deref()),
        del: hex_rsid(r.rsid_del.as_deref()),
        tr: hex_rsid(r.rsid_tr.as_deref()),
    };
    let properties = r.tr_pr.map(TableRowProperties::from).unwrap_or_default();
    let property_exceptions = r.tbl_pr_ex.map(Into::into);
    let cells = collect_row_cells(r.children)
        .into_iter()
        .map(|c| convert_table_cell(c, ctx))
        .collect();
    TableRow {
        properties,
        cells,
        rsids,
        property_exceptions,
    }
}

/// Walk a `<w:tr>`'s direct children and flatten out every `<w:tc>`,
/// recursing through cell-level SDT (`<w:sdt>`) and custom-XML wrappers.
/// Range markers, proofreading errors, permission ranges, and the
/// `tblPrEx`/`trPr` duplicates produced by `$value` are dropped — they have
/// no rendered effect at cell level. Document order is preserved. Mirrors
/// `collect_table_rows` one level down.
fn collect_row_cells(children: Vec<RowChildXml>) -> Vec<TableCellXml> {
    let mut cells = Vec::with_capacity(children.len());
    for child in children {
        match child {
            RowChildXml::Cell(c) => cells.push(*c),
            RowChildXml::Sdt(s) => {
                if let Some(content) = s.content {
                    cells.extend(collect_row_cells(content.children));
                }
            }
            RowChildXml::CustomXml(cx) => {
                cells.extend(collect_row_cells(cx.children));
            }
            RowChildXml::BookmarkStart(_)
            | RowChildXml::BookmarkEnd(_)
            | RowChildXml::CommentRangeStart(_)
            | RowChildXml::CommentRangeEnd(_)
            | RowChildXml::ProofErr(_)
            | RowChildXml::PermStart(_)
            | RowChildXml::PermEnd(_)
            | RowChildXml::TblPrEx(_)
            | RowChildXml::TrPr(_)
            | RowChildXml::Other => {}
        }
    }
    cells
}

fn convert_table_cell(c: TableCellXml, ctx: &mut ConvertCtx) -> TableCell {
    let properties = c.tc_pr.map(TableCellProperties::from).unwrap_or_default();
    let (content, _final_sect) = convert_container(c.content, ctx);
    TableCell {
        properties,
        content,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn hex_rsid(s: Option<&str>) -> Option<RevisionSaveId> {
    s.and_then(RevisionSaveId::from_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect all rendered text from a converted inline sequence, recursing
    /// into hyperlinks and fields.
    fn collect_text(inlines: &[Inline]) -> String {
        let mut s = String::new();
        for inl in inlines {
            match inl {
                Inline::TextRun(r) => {
                    for el in &r.content {
                        if let RunElement::Text(t) = el {
                            s.push_str(t);
                        }
                    }
                }
                Inline::Hyperlink(h) => s.push_str(&collect_text(&h.content)),
                Inline::Field(f) => s.push_str(&collect_text(&f.content)),
                _ => {}
            }
        }
        s
    }

    /// Parse a `<w:p>` and return the concatenated rendered text.
    fn para_text(xml: &str) -> String {
        let p: ParaXml = quick_xml::de::from_str(xml).unwrap();
        let mut ctx = ConvertCtx::new();
        let (para, _) = convert_paragraph(p, &mut ctx);
        collect_text(&para.content)
    }

    /// §17.16.7: a FORMCHECKBOX begin marker carries its rendered state in
    /// `<w:ffData><w:checkBox>` — `<w:checked>` wins over `<w:default>`,
    /// and an ffData without a checkBox member stays `None`.
    #[test]
    fn fld_char_ffdata_checkbox_state() {
        let checkbox_state = |xml: &str| -> Option<bool> {
            let p: ParaXml = quick_xml::de::from_str(xml).unwrap();
            let mut ctx = ConvertCtx::new();
            let (para, _) = convert_paragraph(p, &mut ctx);
            para.content.iter().find_map(|i| match i {
                Inline::FieldChar(fc) => Some(fc.form_checkbox),
                _ => None,
            })?
        };
        let field = |ff_data: &str| {
            format!(
                r#"<w:p xmlns:w="x"><w:r>
                     <w:fldChar w:fldCharType="begin">{ff_data}</w:fldChar>
                   </w:r></w:p>"#
            )
        };
        assert_eq!(
            checkbox_state(&field(
                r#"<w:ffData><w:name w:val="C1"/><w:checkBox><w:sizeAuto/><w:default w:val="0"/></w:checkBox></w:ffData>"#
            )),
            Some(false)
        );
        assert_eq!(
            checkbox_state(&field(
                r#"<w:ffData><w:checkBox><w:default w:val="0"/><w:checked w:val="1"/></w:checkBox></w:ffData>"#
            )),
            Some(true)
        );
        // FORMTEXT-style ffData (no checkBox): not a checkbox.
        assert_eq!(
            checkbox_state(&field(r#"<w:ffData><w:name w:val="T1"/></w:ffData>"#)),
            None
        );
        assert_eq!(checkbox_state(&field("")), None);
    }

    /// Run-level `<w:sdt>` (CT_SdtRun) — the display run inside
    /// `<w:sdtContent>` must render in place, not vanish into the `Other`
    /// catch-all. This is how `w14:checkbox` content controls carry their
    /// ☐/☒ glyph, so dropping it silently loses form-checkbox state.
    #[test]
    fn run_level_sdt_content_renders_in_place() {
        let text = para_text(
            r#"<w:p xmlns:w="x">
              <w:r><w:t>Answer: </w:t></w:r>
              <w:sdt>
                <w:sdtPr><w:id w:val="1"/></w:sdtPr>
                <w:sdtContent><w:r><w:t>&#x2610;</w:t></w:r></w:sdtContent>
              </w:sdt>
              <w:r><w:t> Yes</w:t></w:r>
            </w:p>"#,
        );
        assert_eq!(text, "Answer: ☐ Yes");
    }

    /// A `<w:sdt>` nested inside another `<w:sdt>`'s content (CT_SdtContentRun
    /// is EG_PContent) flattens recursively.
    #[test]
    fn nested_run_level_sdt_flattens() {
        let text = para_text(
            r#"<w:p xmlns:w="x">
              <w:sdt><w:sdtContent>
                <w:r><w:t>a</w:t></w:r>
                <w:sdt><w:sdtContent><w:r><w:t>b</w:t></w:r></w:sdtContent></w:sdt>
              </w:sdtContent></w:sdt>
            </w:p>"#,
        );
        assert_eq!(text, "ab");
    }

    /// Parse a `<w:p>` and return the run-elements of its first `TextRun`.
    fn first_run_elements(xml: &str) -> Vec<RunElement> {
        let p: ParaXml = quick_xml::de::from_str(xml).unwrap();
        let mut ctx = ConvertCtx::new();
        let (para, _) = convert_paragraph(p, &mut ctx);
        para.content
            .into_iter()
            .find_map(|i| match i {
                Inline::TextRun(r) => Some(r.content),
                _ => None,
            })
            .unwrap_or_default()
    }

    // ── Revision & structural wrappers (accept-all-changes / final view) ──

    #[test]
    fn ins_content_is_rendered() {
        // Tracked insertions are part of the final document — their runs must
        // survive, not fall into the `Other` catch-all.
        assert_eq!(
            para_text(r#"<w:p xmlns:w="x"><w:ins><w:r><w:t>kept</w:t></w:r></w:ins></w:p>"#),
            "kept"
        );
    }

    #[test]
    fn del_content_is_dropped() {
        // Tracked deletions are not in the final document — drop them.
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>"#
            ),
            ""
        );
    }

    #[test]
    fn del_nested_in_ins_is_still_dropped() {
        // Nested wrappers re-apply the rules: text inserted then deleted is
        // deleted; the surrounding insertion is kept.
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:ins>
                     <w:r><w:t>A</w:t></w:r>
                     <w:del><w:r><w:delText>B</w:delText></w:r></w:del>
                     <w:r><w:t>C</w:t></w:r>
                   </w:ins></w:p>"#
            ),
            "AC"
        );
    }

    #[test]
    fn move_to_rendered_move_from_dropped() {
        assert_eq!(
            para_text(r#"<w:p xmlns:w="x"><w:moveTo><w:r><w:t>here</w:t></w:r></w:moveTo></w:p>"#),
            "here"
        );
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:moveFrom><w:r><w:t>away</w:t></w:r></w:moveFrom></w:p>"#
            ),
            ""
        );
    }

    #[test]
    fn smart_tag_and_inline_custom_xml_are_flattened() {
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:smartTag><w:r><w:t>date</w:t></w:r></w:smartTag></w:p>"#
            ),
            "date"
        );
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:customXml><w:r><w:t>tagged</w:t></w:r></w:customXml></w:p>"#
            ),
            "tagged"
        );
    }

    #[test]
    fn ins_wrapping_a_hyperlink_keeps_the_link_text() {
        assert_eq!(
            para_text(
                r#"<w:p xmlns:w="x"><w:ins>
                     <w:hyperlink w:anchor="a"><w:r><w:t>link</w:t></w:r></w:hyperlink>
                   </w:ins></w:p>"#
            ),
            "link"
        );
    }

    // ── run_break ────────────────────────────────────────────────────────

    #[test]
    fn br_type_page_and_column() {
        assert!(matches!(
            first_run_elements(r#"<w:p xmlns:w="x"><w:r><w:br type="page"/></w:r></w:p>"#)
                .as_slice(),
            [RunElement::PageBreak]
        ));
        assert!(matches!(
            first_run_elements(r#"<w:p xmlns:w="x"><w:r><w:br type="column"/></w:r></w:p>"#)
                .as_slice(),
            [RunElement::ColumnBreak]
        ));
    }

    #[test]
    fn br_plain_is_text_wrapping_line_break() {
        assert!(matches!(
            first_run_elements(r#"<w:p xmlns:w="x"><w:r><w:br/></w:r></w:p>"#).as_slice(),
            [RunElement::LineBreak(BreakKind::TextWrapping)]
        ));
    }

    #[test]
    fn br_clear_all_is_clearing_line_break() {
        assert!(matches!(
            first_run_elements(r#"<w:p xmlns:w="x"><w:r><w:br clear="all"/></w:r></w:p>"#)
                .as_slice(),
            [RunElement::LineBreak(BreakKind::Clear(BreakClear::All))]
        ));
    }

    // ── mc:Choice Requires ───────────────────────────────────────────────

    #[test]
    fn mc_requires_known_and_unknown() {
        assert_eq!(mc_requires("wps"), Some(McRequires::Wps));
        assert_eq!(mc_requires("w14"), Some(McRequires::W14));
        // Unknown / unsupported token → None (choice is dropped, fallback used).
        assert_eq!(mc_requires("nope"), None);
        // `mc_requires` maps a *single* token; a space-separated list is split by
        // `convert_alt_content` (see `alt_content_requires_*` below), so a raw
        // multi-token string is not a valid single token.
        assert_eq!(mc_requires("wps w14"), None);
    }

    #[test]
    fn alt_content_multi_token_requires_is_kept_when_all_known() {
        // §M.2.2: a space-separated Requires with every token understood keeps
        // the choice (previously the whole choice was dropped).
        let xml = r#"<w:r xmlns:w="x" xmlns:mc="m">
              <mc:AlternateContent>
                <mc:Choice Requires="wps w14"><w:drawing/></mc:Choice>
                <mc:Fallback/>
              </mc:AlternateContent>
            </w:r>"#;
        let r: RunXml = quick_xml::de::from_str(xml).unwrap();
        let mut out = Vec::new();
        let mut ctx = ConvertCtx::new();
        extend_from_run(r, &mut out, &mut ctx);
        let Some(Inline::AlternateContent(ac)) = out.into_iter().next() else {
            panic!("expected AlternateContent");
        };
        assert_eq!(ac.choices.len(), 1, "multi-token choice kept");
        assert_eq!(
            ac.choices[0].requires,
            vec![McRequires::Wps, McRequires::W14]
        );
    }

    #[test]
    fn alt_content_choice_with_unknown_token_is_dropped() {
        let xml = r#"<w:r xmlns:w="x" xmlns:mc="m">
              <mc:AlternateContent>
                <mc:Choice Requires="wps nope"><w:drawing/></mc:Choice>
                <mc:Fallback/>
              </mc:AlternateContent>
            </w:r>"#;
        let r: RunXml = quick_xml::de::from_str(xml).unwrap();
        let mut out = Vec::new();
        let mut ctx = ConvertCtx::new();
        extend_from_run(r, &mut out, &mut ctx);
        let Some(Inline::AlternateContent(ac)) = out.into_iter().next() else {
            panic!("expected AlternateContent");
        };
        assert!(
            ac.choices.is_empty(),
            "choice with an unknown token dropped"
        );
    }

    /// A `<w:tr>` whose `<w:tc>` cells are interleaved with cell-level
    /// `<w:sdt>` (CT_SdtCell) wrappers must (a) parse — a plain
    /// `Vec<TableCellXml>` field would report a duplicate `tc` when cells
    /// are non-contiguous — and (b) recover every cell, including the ones
    /// nested inside `<w:sdtContent>`.
    #[test]
    fn row_with_interspersed_sdt_cells_recovers_all_cells() {
        let xml = r#"
            <w:tr xmlns:w="x">
              <w:trPr/>
              <w:tc><w:p/></w:tc>
              <w:tc><w:p/></w:tc>
              <w:sdt>
                <w:sdtPr/>
                <w:sdtContent><w:tc><w:p/></w:tc></w:sdtContent>
              </w:sdt>
              <w:sdt>
                <w:sdtContent><w:tc><w:p/></w:tc></w:sdtContent>
              </w:sdt>
              <w:tc><w:p/></w:tc>
            </w:tr>
        "#;
        let row: TableRowXml = quick_xml::de::from_str(xml).expect("row must parse");
        assert!(row.tr_pr.is_some(), "trPr still captured on the parent");
        let cells = collect_row_cells(row.children);
        assert_eq!(cells.len(), 5, "3 bare + 2 sdt-wrapped cells recovered");
    }

    /// Nested `<w:customXml>` cell wrapper (CT_CustomXmlCell) also flattens.
    #[test]
    fn row_with_custom_xml_cell_wrapper_recovers_cells() {
        let xml = r#"
            <w:tr xmlns:w="x">
              <w:tc><w:p/></w:tc>
              <w:customXml>
                <w:tc><w:p/></w:tc>
                <w:tc><w:p/></w:tc>
              </w:customXml>
            </w:tr>
        "#;
        let row: TableRowXml = quick_xml::de::from_str(xml).expect("row must parse");
        assert_eq!(collect_row_cells(row.children).len(), 3);
    }

    /// A `<w:customXml>` row wrapper (CT_CustomXmlRow) inside `<w:tbl>` must
    /// have its nested `<w:tr>` rows recovered — the element name is
    /// `customXml`, not `customXmlIns`/etc.
    #[test]
    fn table_with_custom_xml_row_wrapper_recovers_rows() {
        let xml = r#"
            <w:tbl xmlns:w="x">
              <w:tblPr/>
              <w:tblGrid><w:gridCol w:w="100"/></w:tblGrid>
              <w:tr><w:tc><w:p/></w:tc></w:tr>
              <w:customXml>
                <w:tr><w:tc><w:p/></w:tc></w:tr>
              </w:customXml>
            </w:tbl>
        "#;
        let tbl: TableXml = quick_xml::de::from_str(xml).expect("table must parse");
        assert_eq!(collect_table_rows(tbl.children).len(), 2);
    }
}
