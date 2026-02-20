import fs from "node:fs/promises";
import { PdfEngine, PdfDocument, PageData, Image, Annotation, BoundingBox } from "./interface.js";
import { TextItem } from "../../core/types.js";
import { PdfiumRenderer } from "./pdfium-renderer.js";
import { importPdfJs } from "./pdfjsImporter.js";

/** PDF.js internal document type - opaque to our code */
interface PdfJsDocument {
  numPages: number;
  getPage(pageNum: number): Promise<PdfJsPage>;
  getMetadata(): Promise<unknown>;
  destroy(): Promise<void>;
}

/** PDF.js internal page type */
interface PdfJsPage {
  getViewport(params: { scale: number }): PdfJsViewport;
  getTextContent(): Promise<PdfJsTextContent>;
  cleanup(): Promise<void>;
}

/** PDF.js viewport type */
interface PdfJsViewport {
  width: number;
  height: number;
  transform: number[];
}

/** PDF.js text content type */
interface PdfJsTextContent {
  items: PdfJsTextItem[];
}

/** PDF.js text item type */
interface PdfJsTextItem {
  str: string;
  transform: number[];
  width: number;
  height: number;
  fontName?: string;
}

/** Extended PdfDocument with internal PDF.js document reference */
interface PdfJsExtendedDocument extends PdfDocument {
  _pdfDocument: PdfJsDocument;
}

// Dynamic import of PDF.js
const { fn: getDocument, dir: PDFJS_DIR } = await importPdfJs();

const CMAP_URL = `${PDFJS_DIR}/cmaps/`;
const STANDARD_FONT_DATA_URL = `${PDFJS_DIR}/standard_fonts/`;
const CMAP_PACKED = true;

/**
 * Extract rotation angle in degrees from PDF transformation matrix
 * Matrix format: [a, b, c, d, e, f] where rotation is atan2(b, a)
 */
function getRotation(transform: number[]): number {
  return Math.atan2(transform[1], transform[0]) * (180 / Math.PI);
}

/**
 * Multiply two transformation matrices
 */
function multiplyMatrices(m1: number[], m2: number[]): number[] {
  return [
    m1[0] * m2[0] + m1[2] * m2[1],
    m1[1] * m2[0] + m1[3] * m2[1],
    m1[0] * m2[2] + m1[2] * m2[3],
    m1[1] * m2[2] + m1[3] * m2[3],
    m1[0] * m2[4] + m1[2] * m2[5] + m1[4],
    m1[1] * m2[4] + m1[3] * m2[5] + m1[5],
  ];
}

/**
 * Apply transformation matrix to a point
 */
function applyTransformation(
  point: { x: number; y: number },
  transform: number[]
): { x: number; y: number } {
  return {
    x: point.x * transform[0] + point.y * transform[2] + transform[4],
    y: point.x * transform[1] + point.y * transform[3] + transform[5],
  };
}

/**
 * Decompose transformation matrix to get scale factors using SVD
 * This computes the singular values of the 2D transformation matrix
 */
function singularValueDecompose2dScale(m: number[]): { x: number; y: number } {
  // Create transpose of the 2x2 part of the matrix
  const transpose = [m[0], m[2], m[1], m[3]];

  // Multiply matrix m with its transpose to get eigenvalues
  const a = m[0] * transpose[0] + m[1] * transpose[2];
  const b = m[0] * transpose[1] + m[1] * transpose[3];
  const c = m[2] * transpose[0] + m[3] * transpose[2];
  const d = m[2] * transpose[1] + m[3] * transpose[3];

  // Solve the second degree polynomial to get roots (eigenvalues)
  const first = (a + d) / 2;
  const second = Math.sqrt((a + d) ** 2 - 4 * (a * d - c * b)) / 2;
  const sx = first + second || 1;
  const sy = first - second || 1;

  // Scale values are the square roots of the eigenvalues
  return { x: Math.sqrt(sx), y: Math.sqrt(sy) };
}

// Pre-compiled regex patterns for string decoding
const BUGGY_FONT_MARKER_REGEX = /:->\|>_(\d+)_\d+_<\|<-:/g;
const BUGGY_FONT_MARKER_CHECK = ":->|>";
const PIPE_PATTERN_REGEX = /\s*\|([^|])\|\s*/g;

/**
 * Detect garbled text from fonts with corrupted ToUnicode mappings.
 *
 * When PDF fonts lack proper ToUnicode maps, PDF.js may output characters
 * mapped to unexpected Unicode code points. Common patterns include:
 *
 * 1. Private Use Area (PUA) characters - fonts often map glyphs here
 * 2. Mix of unrelated scripts (Arabic + Latin Extended in English text)
 * 3. Rare/obscure Unicode blocks appearing in normal text
 * 4. Control characters or specials
 *
 * Returns true if the string appears to be garbled font output.
 */
function isGarbledFontOutput(str: string): boolean {
  if (str.length < 3) return false;

  let privateUseCount = 0;
  let arabicCount = 0;
  let latinExtendedCount = 0;
  let basicLatinLetterCount = 0;
  let suspiciousCount = 0; // Other suspicious Unicode ranges

  for (const char of str) {
    const code = char.charCodeAt(0);

    // Private Use Area (U+E000-U+F8FF) - almost always garbled
    if (code >= 0xe000 && code <= 0xf8ff) {
      privateUseCount++;
    }
    // Arabic block (0x600-0x6FF) and Arabic Extended (0x750-0x77F, 0x8A0-0x8FF)
    else if (
      (code >= 0x600 && code <= 0x6ff) ||
      (code >= 0x750 && code <= 0x77f) ||
      (code >= 0x8a0 && code <= 0x8ff)
    ) {
      arabicCount++;
    }
    // Latin Extended-A (0x100-0x17F), Latin Extended-B (0x180-0x24F),
    // Latin Extended Additional (0x1E00-0x1EFF)
    else if ((code >= 0x100 && code <= 0x24f) || (code >= 0x1e00 && code <= 0x1eff)) {
      latinExtendedCount++;
    }
    // Basic Latin letters (a-z, A-Z)
    else if ((code >= 0x41 && code <= 0x5a) || (code >= 0x61 && code <= 0x7a)) {
      basicLatinLetterCount++;
    }
    // Suspicious ranges that rarely appear in normal text:
    // - Syriac (0x700-0x74F)
    // - Thaana (0x780-0x7BF)
    // - NKo (0x7C0-0x7FF)
    // - Samaritan (0x800-0x83F)
    // - Specials (0xFFF0-0xFFFF)
    // - Geometric Shapes (0x25A0-0x25FF) in running text
    // - Box Drawing (0x2500-0x257F) in running text
    // - Combining Diacritical Marks alone (0x0300-0x036F)
    else if (
      (code >= 0x700 && code <= 0x7ff) || // Syriac, Thaana, NKo
      (code >= 0x800 && code <= 0x83f) || // Samaritan
      (code >= 0xfff0 && code <= 0xffff) || // Specials
      (code >= 0x2500 && code <= 0x25ff) || // Box drawing, geometric shapes
      (code >= 0x0300 && code <= 0x036f) // Combining marks (suspicious if frequent)
    ) {
      suspiciousCount++;
    }
  }

  const totalChars = str.length;

  // Private Use Area characters are almost always garbled fonts
  if (privateUseCount >= 2) {
    return true;
  }

  // Mix of Arabic AND Latin Extended is extremely rare in legitimate text
  if (arabicCount >= 2 && latinExtendedCount >= 2) {
    return true;
  }

  // High concentration of suspicious characters
  if (suspiciousCount >= 3 || suspiciousCount > totalChars * 0.2) {
    return true;
  }

  // Text predominantly Latin Extended with very few basic Latin letters
  // (legitimate Latin-script text would have mostly basic Latin)
  if (latinExtendedCount > totalChars * 0.3 && basicLatinLetterCount < totalChars * 0.2) {
    return true;
  }

  // Mix of Arabic/suspicious with Latin Extended (script mixing)
  if ((arabicCount >= 1 || suspiciousCount >= 1) && latinExtendedCount >= 3) {
    return true;
  }

  return false;
}

export class PdfJsEngine implements PdfEngine {
  name = "pdfjs";
  private pdfiumRenderer: PdfiumRenderer | null = null;
  private currentPdfPath: string | null = null;

  async loadDocument(filePath: string): Promise<PdfDocument> {
    const data = new Uint8Array(await fs.readFile(filePath));

    // Store path for PDFium rendering
    this.currentPdfPath = filePath;

    const loadingTask = getDocument({
      data,
      cMapUrl: CMAP_URL,
      cMapPacked: CMAP_PACKED,
      standardFontDataUrl: STANDARD_FONT_DATA_URL,
    });

    const pdfDocument = await loadingTask.promise;
    const metadata = await pdfDocument.getMetadata();

    return {
      numPages: pdfDocument.numPages,
      data,
      metadata,
      _pdfDocument: pdfDocument,
    } as PdfJsExtendedDocument;
  }

  async extractPage(doc: PdfDocument, pageNum: number): Promise<PageData> {
    const pdfDocument = (doc as PdfJsExtendedDocument)._pdfDocument;
    const page = await pdfDocument.getPage(pageNum);

    // Get viewport
    const viewport = page.getViewport({ scale: 1.0 });

    // Extract text content
    const textContent = await page.getTextContent();
    const viewportWidth = viewport.width;
    const viewportHeight = viewport.height;
    const viewportTransform = viewport.transform;

    const textItems: TextItem[] = [];
    const garbledTextRegions: BoundingBox[] = [];
    for (const item of textContent.items) {
      // Skip items with zero dimensions
      if (item.height === 0 || item.width === 0) continue;

      // Apply viewport transformation to convert PDF coordinates to screen coordinates
      // This properly handles Y-axis flip (PDF is bottom-up, screen is top-down)
      const cm = multiplyMatrices(viewportTransform, item.transform);

      // Get lower-left corner (text space origin)
      const ll = applyTransformation({ x: 0, y: 0 }, cm);

      // Get scale factors to properly size the bounding box
      const scale = singularValueDecompose2dScale(item.transform);

      // Get upper-right corner
      const ur = applyTransformation({ x: item.width / scale.x, y: item.height / scale.y }, cm);

      // Calculate final bounding box in viewport space
      const left = Math.min(ll.x, ur.x);
      const right = Math.max(ll.x, ur.x);
      const top = Math.min(ll.y, ur.y);
      const bottom = Math.max(ll.y, ur.y);

      // Skip items that are off-page (negative coordinates or beyond page bounds)
      if (top < 0 || left < 0 || top > viewportHeight || left > viewportWidth) continue;

      const width = right - left;
      const height = bottom - top;

      // Calculate rotation from combined transformation matrix
      let rotation = getRotation(cm);
      // Normalize to 0-360 range
      if (rotation < 0) {
        rotation += 360;
      }

      // Decode buggy font markers from PDF.js (only if marker is present)
      // Format: :->|>_<charCode>_<fontChar>_<|<-:
      let decodedStr = item.str;
      if (decodedStr.includes(BUGGY_FONT_MARKER_CHECK)) {
        BUGGY_FONT_MARKER_REGEX.lastIndex = 0; // Reset regex state
        decodedStr = decodedStr.replace(BUGGY_FONT_MARKER_REGEX, (_: string, charCode: string) =>
          String.fromCharCode(parseInt(charCode))
        );
      }

      // Handle pipe-separated characters: " |a|  |r|  |X| " -> "arX"
      // Some PDFs encode text with characters separated by pipes and spaces
      if (decodedStr.includes("|")) {
        PIPE_PATTERN_REGEX.lastIndex = 0; // Reset regex state
        const matches = [...decodedStr.matchAll(PIPE_PATTERN_REGEX)];
        if (matches.length > 0) {
          decodedStr = matches.map((m) => m[1]).join("");
        }
      }

      // Skip garbled text from fonts with corrupted ToUnicode mappings
      // Save the bounding box so OCR can fill in these specific regions
      if (isGarbledFontOutput(decodedStr)) {
        garbledTextRegions.push({ x: left, y: top, width, height });
        continue;
      }

      textItems.push({
        str: decodedStr,
        x: left,
        y: top,
        width,
        height,
        w: width,
        h: height,
        r: rotation,
        fontName: item.fontName,
        fontSize: Math.sqrt(
          item.transform[0] * item.transform[0] + item.transform[1] * item.transform[1]
        ),
      });
    }

    const images: Image[] = [];

    // Skip annotation extraction - not currently used in processing pipeline
    // Can be re-enabled if needed for link extraction, etc.
    const annotations: Annotation[] = [];

    await page.cleanup();

    return {
      pageNum,
      width: viewport.width,
      height: viewport.height,
      textItems,
      images,
      annotations,
      garbledTextRegions: garbledTextRegions.length > 0 ? garbledTextRegions : undefined,
    };
  }

  async extractAllPages(
    doc: PdfDocument,
    maxPages?: number,
    targetPages?: string
  ): Promise<PageData[]> {
    const numPages = Math.min(doc.numPages, maxPages || doc.numPages);

    const pages: PageData[] = [];

    // Parse target pages if specified
    let pageNumbers: number[];
    if (targetPages) {
      pageNumbers = this.parseTargetPages(targetPages, doc.numPages);
    } else {
      pageNumbers = Array.from({ length: numPages }, (_, i) => i + 1);
    }

    for (const pageNum of pageNumbers) {
      if (maxPages && pages.length >= maxPages) {
        break;
      }
      const pageData = await this.extractPage(doc, pageNum);
      pages.push(pageData);
    }

    return pages;
  }

  async renderPageImage(_doc: PdfDocument, pageNum: number, dpi: number): Promise<Buffer> {
    // Use PDFium for rendering (more robust with inline images)
    if (!this.currentPdfPath) {
      throw new Error("PDF path not available for rendering");
    }

    if (!this.pdfiumRenderer) {
      this.pdfiumRenderer = new PdfiumRenderer();
    }

    return await this.pdfiumRenderer.renderPageToBuffer(this.currentPdfPath, pageNum, dpi);
  }

  async close(doc: PdfDocument): Promise<void> {
    const pdfDocument = (doc as PdfJsExtendedDocument)._pdfDocument;
    if (pdfDocument && pdfDocument.destroy) {
      await pdfDocument.destroy();
    }

    // Clean up PDFium renderer (only if it was initialized)
    if (this.pdfiumRenderer) {
      await this.pdfiumRenderer.close();
      this.pdfiumRenderer = null;
    }
    this.currentPdfPath = null;
  }

  private parseTargetPages(targetPages: string, maxPages: number): number[] {
    const pages: number[] = [];
    const parts = targetPages.split(",");

    for (const part of parts) {
      const trimmed = part.trim();
      if (trimmed.includes("-")) {
        // Range: "1-5"
        const [start, end] = trimmed.split("-").map((n) => parseInt(n.trim()));
        for (let i = start; i <= Math.min(end, maxPages); i++) {
          if (i >= 1) {
            pages.push(i);
          }
        }
      } else {
        // Single page: "10"
        const pageNum = parseInt(trimmed);
        if (pageNum >= 1 && pageNum <= maxPages) {
          pages.push(pageNum);
        }
      }
    }

    return [...new Set(pages)].sort((a, b) => a - b);
  }
}
