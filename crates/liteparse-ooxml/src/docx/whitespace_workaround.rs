//! Workaround for quick-xml's silent whitespace stripping.
//!
//! # The problem
//!
//! quick-xml's serde [`Deserializer`](quick_xml::de::Deserializer) wraps every
//! input reader in a private trimmer that drops `Text` events containing only
//! whitespace. Even when the source XML carries `xml:space="preserve"`, the
//! trimmer eats the content before any visitor sees it. Custom `Deserialize`
//! impls cannot intercept — the relevant constructors and trim flag are private,
//! and `XmlRead` cannot be plugged in via any public API in 0.39.
//!
//! In Word documents this typically appears as `<w:r>` runs that contain a
//! single whitespace-only `<w:t xml:space="preserve"> </w:t>` between two
//! other runs (e.g. a label followed by a value with different formatting).
//! Without this workaround, the space disappears and rendered text reads
//! `Label:Value` instead of `Label: Value`.
//!
//! # The hack
//!
//! Before handing each XML part to quick-xml we resolve WordprocessingML text
//! elements, then substitute each byte in a whitespace-only text node with a
//! Private Use Area codepoint. quick-xml then sees a non-whitespace text node
//! and preserves it. Resolving namespaces makes the workaround independent of
//! the arbitrary prefix chosen by the DOCX producer. The body parser
//! ([`crate::docx::parse::body`]) reverses the substitution when emitting
//! [`RunElement::Text`].
//!
//! Sentinel mapping (BYTE → CHAR):
//!
//! | source byte | sentinel    |
//! |-------------|-------------|
//! | `0x20` SP   | `U+E020`    |
//! | `0x09` HT   | `U+E009`    |
//! | `0x0A` LF   | `U+E00A`    |
//! | `0x0D` CR   | `U+E00D`    |
//!
//! The pattern `0xE000 + <ASCII byte>` keeps the reversal trivial and ensures
//! sentinels never collide with anything OOXML legitimately emits — Word never
//! writes Private Use Area codepoints.
//!
//! TODO(quick-xml): drop this module once one of the following lands upstream
//! and we adopt it:
//!     * a public `Deserializer` constructor that accepts a custom `XmlRead`,
//!     * a public knob to disable `StartTrimmer`, or
//!     * `xml:space="preserve"` honored for whitespace-only `Text` events.
//!
//! Tracked upstream at the quick-xml repository
//! (<https://github.com/tafia/quick-xml>); this module is the local
//! stopgap until one of the above is available.

/// Sentinel for ASCII space (`0x20`).
pub(crate) const WS_SENTINEL_SPACE: char = '\u{E020}';
/// Sentinel for ASCII tab (`0x09`).
pub(crate) const WS_SENTINEL_TAB: char = '\u{E009}';
/// Sentinel for ASCII line feed (`0x0A`).
pub(crate) const WS_SENTINEL_LF: char = '\u{E00A}';
/// Sentinel for ASCII carriage return (`0x0D`).
pub(crate) const WS_SENTINEL_CR: char = '\u{E00D}';

const TRANSITIONAL_WORDPROCESSINGML_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORDPROCESSINGML_NAMESPACE: &[u8] =
    b"http://purl.oclc.org/ooxml/wordprocessingml/main";

#[inline]
fn is_wordprocessingml_namespace(uri: &[u8]) -> bool {
    matches!(
        uri,
        TRANSITIONAL_WORDPROCESSINGML_NAMESPACE | STRICT_WORDPROCESSINGML_NAMESPACE
    )
}

/// Pre-process an XML byte buffer so whitespace-only WordprocessingML text
/// content survives quick-xml's trimmer. Returns the original buffer unchanged
/// when no substitution is needed.
///
/// See module-level docs for why this is necessary.
pub(crate) fn substitute_whitespace_only_runs(xml: &[u8]) -> Vec<u8> {
    // quick-xml consumes a leading UTF-8 BOM without counting it in
    // `buffer_position()`, so every span it reports on a BOM-prefixed part is
    // shifted 3 bytes left of the source buffer. Scan the BOM-less tail and
    // slice against that same tail: positions agree by construction.
    let bom = if xml.starts_with(UTF8_BOM) {
        UTF8_BOM.len()
    } else {
        0
    };
    let body = &xml[bom..];
    let spans = whitespace_only_wordprocessingml_text_spans(body);
    if spans.is_empty() {
        return xml.to_vec();
    }

    let mut out: Vec<u8> = Vec::with_capacity(xml.len());
    out.extend_from_slice(&xml[..bom]);
    let mut cursor = 0;
    for (start, end) in spans {
        out.extend_from_slice(&body[cursor..start]);
        if body[start..end].iter().all(is_xml_whitespace_byte) {
            substitute_whitespace(&body[start..end], &mut out);
        } else {
            // Position bookkeeping drifted (as the unaccounted BOM once made
            // it). A missed substitution only loses an inter-run space —
            // never corrupt the part or panic mid-parse over one.
            debug_assert!(
                false,
                "whitespace span {start}..{end} does not cover whitespace"
            );
            out.extend_from_slice(&body[start..end]);
        }
        cursor = end;
    }
    out.extend_from_slice(&body[cursor..]);
    out
}

/// UTF-8 byte-order mark; legal before the XML declaration and emitted by
/// several DOCX producers.
const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

/// Reverse the sentinel substitution. Returns the original string when no
/// sentinels are present (zero-allocation common path).
pub(crate) fn restore_whitespace_sentinels(s: &str) -> String {
    if !s.chars().any(is_ws_sentinel) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        out.push(match ch {
            WS_SENTINEL_SPACE => ' ',
            WS_SENTINEL_TAB => '\t',
            WS_SENTINEL_LF => '\n',
            WS_SENTINEL_CR => '\r',
            other => other,
        });
    }
    out
}

/// Debug hook for the `ws_span_probe` example: the span scan exactly as
/// production applies it (BOM-adjusted), returned as absolute offsets into
/// the raw buffer so the probe can validate them against the source bytes.
pub(crate) fn debug_spans(xml: &[u8]) -> Vec<(usize, usize)> {
    let bom = if xml.starts_with(UTF8_BOM) {
        UTF8_BOM.len()
    } else {
        0
    };
    whitespace_only_wordprocessingml_text_spans(&xml[bom..])
        .into_iter()
        .map(|(s, e)| (s + bom, e + bom))
        .collect()
}

#[inline]
fn is_xml_whitespace_byte(b: &u8) -> bool {
    matches!(*b, b' ' | b'\t' | b'\n' | b'\r')
}

#[inline]
fn is_ws_sentinel(c: char) -> bool {
    matches!(
        c,
        WS_SENTINEL_SPACE | WS_SENTINEL_TAB | WS_SENTINEL_LF | WS_SENTINEL_CR
    )
}

/// Return source spans for whitespace-only text nodes directly inside
/// WordprocessingML `<*:t>` elements. Namespace prefixes are resolved by
/// quick-xml, so DrawingML and other XML vocabularies are not mutated.
/// Malformed XML is left untouched.
fn whitespace_only_wordprocessingml_text_spans(xml: &[u8]) -> Vec<(usize, usize)> {
    use quick_xml::events::Event;
    use quick_xml::name::ResolveResult;
    use quick_xml::reader::NsReader;

    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut spans = Vec::new();
    let mut depth: usize = 0;
    let mut text_depth = None;

    loop {
        let (namespace, event) = match reader.read_resolved_event() {
            Ok(event) => event,
            Err(_) => return Vec::new(),
        };

        match event {
            Event::Start(start) => {
                depth += 1;
                if matches!(namespace, ResolveResult::Bound(uri) if is_wordprocessingml_namespace(uri.as_ref()))
                    && start.local_name().as_ref() == b"t"
                {
                    text_depth = Some(depth);
                }
            }
            Event::Text(text) if text_depth.is_some() => {
                let content = text.as_ref();
                if !content.is_empty() && content.iter().all(is_xml_whitespace_byte) {
                    let end = reader.buffer_position() as usize;
                    let Some(start) = end.checked_sub(content.len()) else {
                        return Vec::new();
                    };
                    spans.push((start, end));
                }
            }
            Event::End(_) => {
                if text_depth == Some(depth) {
                    text_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => return if depth == 0 { spans } else { Vec::new() },
            _ => {}
        }
    }
}

fn substitute_whitespace(content: &[u8], out: &mut Vec<u8>) {
    for &b in content {
        let sentinel = match b {
            b' ' => WS_SENTINEL_SPACE,
            b'\t' => WS_SENTINEL_TAB,
            b'\n' => WS_SENTINEL_LF,
            b'\r' => WS_SENTINEL_CR,
            _ => unreachable!("caller only passes XML whitespace"),
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(sentinel.encode_utf8(&mut buf).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    fn substitute_wml_fragment(fragment: &str) -> String {
        let open =
            r#"<w:root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#;
        let xml = format!("{open}{fragment}</w:root>");
        let out = s(substitute_whitespace_only_runs(xml.as_bytes()));
        out.strip_prefix(open)
            .and_then(|out| out.strip_suffix("</w:root>"))
            .unwrap()
            .to_string()
    }

    // ── substitute_whitespace_only_runs ──────────────────────────────────────

    #[test]
    fn single_space_preserve_is_substituted() {
        let xml = br#"<w:r><w:t xml:space="preserve"> </w:t></w:r>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(
            out,
            format!(
                r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#,
                WS_SENTINEL_SPACE
            )
        );
    }

    #[test]
    fn three_spaces_preserve_each_substituted() {
        let xml = br#"<w:t xml:space="preserve">   </w:t>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        let expected = format!(
            r#"<w:t xml:space="preserve">{0}{0}{0}</w:t>"#,
            WS_SENTINEL_SPACE
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn whitespace_only_without_preserve_is_also_substituted() {
        // Many third-party DOCX writers emit whitespace-only <w:t> without the
        // xml:space attribute. quick-xml strips them all the same.
        let xml = br#"<w:t> </w:t>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(out, format!("<w:t>{}</w:t>", WS_SENTINEL_SPACE));
    }

    #[test]
    fn tab_only_is_substituted() {
        let xml = b"<w:t xml:space=\"preserve\">\t</w:t>";
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(
            out,
            format!("<w:t xml:space=\"preserve\">{}</w:t>", WS_SENTINEL_TAB)
        );
    }

    #[test]
    fn lf_and_cr_substituted() {
        let xml = b"<w:t xml:space=\"preserve\">\r\n</w:t>";
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(
            out,
            format!(
                "<w:t xml:space=\"preserve\">{}{}</w:t>",
                WS_SENTINEL_CR, WS_SENTINEL_LF
            )
        );
    }

    #[test]
    fn mixed_content_is_left_untouched() {
        // Content has a non-whitespace char → quick-xml preserves it as-is,
        // we must not modify.
        let xml = br#"<w:t xml:space="preserve">hello world </w:t>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(out, std::str::from_utf8(xml).unwrap());
    }

    #[test]
    fn empty_w_t_is_left_untouched() {
        let xml = br#"<w:t></w:t>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(out, std::str::from_utf8(xml).unwrap());
    }

    #[test]
    fn self_closing_w_t_is_left_untouched() {
        let xml = br#"<w:t xml:space="preserve"/>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(out, std::str::from_utf8(xml).unwrap());
    }

    #[test]
    fn longer_element_names_are_not_matched() {
        // `<w:tbl>`, `<w:tab/>`, `<w:tc>` all start with `<w:t` but should not
        // trigger substitution.
        let xml = br#"<w:tbl><w:tr><w:tc><w:tab/></w:tc></w:tr></w:tbl>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(out, std::str::from_utf8(xml).unwrap());
    }

    #[test]
    fn multiple_runs_in_one_buffer() {
        let xml = br#"<w:p><w:r><w:t>A</w:t></w:r><w:r><w:t xml:space="preserve"> </w:t></w:r><w:r><w:t>B</w:t></w:r></w:p>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        let expected = format!(
            r#"<w:p><w:r><w:t>A</w:t></w:r><w:r><w:t xml:space="preserve">{}</w:t></w:r><w:r><w:t>B</w:t></w:r></w:p>"#,
            WS_SENTINEL_SPACE
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn non_wordprocessingml_text_is_unchanged() {
        let xml = br#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:t> </a:t></a:theme>"#;
        let out = substitute_whitespace_only_runs(xml);
        assert_eq!(&out[..], &xml[..]);
    }

    #[test]
    fn malformed_xml_is_left_untouched() {
        let xml = br#"<w:root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t> </w:t>"#;
        let out = substitute_whitespace_only_runs(xml);
        assert_eq!(&out[..], &xml[..]);
    }

    #[test]
    fn empty_buffer_is_handled() {
        let xml: &[u8] = b"";
        let out = substitute_whitespace_only_runs(xml);
        assert!(out.is_empty());
    }

    #[test]
    fn attribute_with_quoted_gt_is_handled() {
        // XML technically allows `>` in attribute values without escaping.
        // The scanner must respect quoting so the inner `>` doesn't end the
        // start tag prematurely.
        let xml = br#"<w:t weird="a>b" xml:space="preserve"> </w:t>"#;
        let out = substitute_wml_fragment(std::str::from_utf8(xml).unwrap());
        assert_eq!(
            out,
            format!(
                r#"<w:t weird="a>b" xml:space="preserve">{}</w:t>"#,
                WS_SENTINEL_SPACE
            )
        );
    }

    // ── restore_whitespace_sentinels ─────────────────────────────────────────

    #[test]
    fn restore_replaces_each_sentinel_with_original_byte() {
        let input = format!(
            "{}{}{}{}",
            WS_SENTINEL_SPACE, WS_SENTINEL_TAB, WS_SENTINEL_LF, WS_SENTINEL_CR
        );
        assert_eq!(restore_whitespace_sentinels(&input), " \t\n\r");
    }

    #[test]
    fn restore_passes_normal_text_through() {
        let input = "hello world";
        assert_eq!(restore_whitespace_sentinels(input), "hello world");
    }

    #[test]
    fn restore_handles_mixed_text_and_sentinels() {
        let input = format!("a{}b", WS_SENTINEL_SPACE);
        assert_eq!(restore_whitespace_sentinels(&input), "a b");
    }

    // ── round-trip: substitute then parse via quick-xml then restore ────────

    #[test]
    fn round_trip_through_quick_xml_recovers_original_whitespace() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct TextXml {
            #[serde(rename = "$text", default)]
            content: String,
        }
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "t")]
            t: TextXml,
        }

        let original = br#"<r xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t xml:space="preserve"> </w:t></r>"#;
        let preprocessed = substitute_whitespace_only_runs(original);
        let parsed: R = quick_xml::de::from_str(std::str::from_utf8(&preprocessed).unwrap())
            .expect("quick-xml parse");
        let restored = restore_whitespace_sentinels(&parsed.t.content);
        assert_eq!(restored, " ", "expected single literal space to survive");
    }

    /// The "Label: Value" class this module exists to fix isn't only
    /// whitespace-*only* nodes — a mixed run with a significant *trailing*
    /// space (`<w:t xml:space="preserve">Label: </w:t>`) must also survive the
    /// full substitute → quick-xml → restore round-trip. This node is not
    /// all-whitespace, so `substitute_whitespace_only_runs` leaves it untouched;
    /// the test asserts quick-xml itself preserves the trailing space.
    #[test]
    fn round_trip_preserves_trailing_space_in_mixed_content() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct TextXml {
            #[serde(rename = "$text", default)]
            content: String,
        }
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "t")]
            t: TextXml,
        }

        let original = br#"<r xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t xml:space="preserve">Label: </w:t></r>"#;
        let preprocessed = substitute_whitespace_only_runs(original);
        let parsed: R = quick_xml::de::from_str(std::str::from_utf8(&preprocessed).unwrap())
            .expect("quick-xml parse");
        let restored = restore_whitespace_sentinels(&parsed.t.content);
        assert_eq!(
            restored, "Label: ",
            "trailing space in mixed content must survive quick-xml"
        );
    }
}
