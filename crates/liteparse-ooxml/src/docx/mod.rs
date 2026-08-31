//! OOXML DOCX parser.
//!
//! Parses a `.docx` file (ZIP of XML) into a single, fully-resolved [`model::Document`]
//! struct. All style inheritance is resolved, all relationships are dereferenced,
//! and all types are ADTs — no unparsed strings, no invalid states.

pub mod dimension {
    pub use crate::model::dimension::*;
}
pub mod geometry {
    pub use crate::model::geometry::*;
}
pub mod model {
    pub use crate::model::*;
}

pub mod error;
pub mod parse;
pub mod relationships;
pub(crate) mod whitespace_workaround;

/// Debug-only surface for the `ws_span_probe` example.
#[doc(hidden)]
pub fn debug_whitespace_spans(xml: &[u8]) -> Vec<(usize, usize)> {
    whitespace_workaround::debug_spans(xml)
}
pub mod zip;

/// Parse a DOCX file from raw bytes into a fully resolved `Document`.
///
/// This is the main entry point. It:
/// 1. Extracts the ZIP archive
/// 2. Parses and resolves styles, numbering, theme, and settings
/// 3. Parses the document body, headers, footers, footnotes, and endnotes
/// 4. Assembles everything into a single `Document` struct
pub fn parse(data: &[u8]) -> error::Result<model::Document> {
    parse::parse(data)
}
