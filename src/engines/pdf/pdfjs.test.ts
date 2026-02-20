import { dirname, join } from "path";
import { fileURLToPath } from "url";
import { vi, describe, it, expect } from "vitest";

vi.mock("node:fs/promises", async () => {
  const actual = await vi.importActual<typeof import("node:fs/promises")>("node:fs/promises");
  return {
    default: {
      ...actual,
      readFile: vi.fn(async () => {
        return Buffer.from("hello world");
      }),
    },
  };
});

vi.mock("./pdfjsImporter.js", async () => {
  const actual = await vi.importActual<typeof import("./pdfjsImporter.js")>("./pdfjsImporter.js");
  return {
    ...actual,
    importPdfJs: vi.fn().mockImplementation(async () => {
      const __filename = fileURLToPath(import.meta.url);
      const __dirname = dirname(__filename);
      // From dist/src/engines/pdf/ we need to go up to dist/src/vendor/pdfjs
      const PDFJS_DIR = join(__dirname, "../../vendor/pdfjs");

      const mockDocument = {
        getMetadata: vi
          .fn()
          .mockResolvedValue({ numPages: 10, size: 1024 * 20, encoding: "utf-8" }),
        numPages: 10,
      };

      const mockGetDocument = vi.fn().mockReturnValue({
        promise: Promise.resolve(mockDocument),
      });

      return { fn: mockGetDocument, dir: PDFJS_DIR };
    }),
  };
});

vi.mock("./pdfium-renderer.js", async () => {
  const actual = await vi.importActual<typeof import("./pdfjsImporter.js")>("./pdfjsImporter.js");
  return {
    ...actual,
    PdfiumRenderer: vi.fn(
      class {
        constructor() {}

        renderPageToBuffer = vi.fn().mockResolvedValue(Buffer.from("this is a page"));
      }
    ),
  };
});

import { PdfJsEngine } from "./pdfjs.js";

function getTestData() {
  const mockViewport = {
    width: 612,
    height: 792,
    transform: [1, 0, 0, 1, 0, 0],
  };

  const mockTextContent = {
    items: [
      {
        str: "Hello, World!",
        transform: [12, 0, 0, 12, 50, 700],
        width: 100,
        height: 12,
        fontName: "Helvetica",
      },
      {
        str: "Second line of text",
        transform: [10, 0, 0, 10, 50, 680],
        width: 150,
        height: 10,
        fontName: "Times-Roman",
      },
    ],
  };
  const mockPage = {
    getViewport: vi.fn().mockReturnValue(mockViewport),
    getTextContent: vi.fn().mockResolvedValue(mockTextContent),
    cleanup: vi.fn(async () => {}),
  };
  const mockDocument = {
    getPage: vi.fn().mockResolvedValue(mockPage),
    numPages: 10,
  };

  const doc = {
    numPages: 10,
    data: new Uint8Array(Buffer.from("hello world")),
    metadata: { numPages: 10, size: 1024 * 20, encoding: "utf-8" },
    _pdfDocument: mockDocument,
  };

  return doc;
}

function getExpectedResults() {
  return [
    {
      str: "Hello, World!",
      x: 50,
      y: 700,
      width: 100,
      height: 12,
      w: 100,
      h: 12,
      r: 0,
      fontName: "Helvetica",
      fontSize: 12,
    },
    {
      str: "Second line of text",
      x: 50,
      y: 680,
      width: 150,
      height: 10,
      w: 150,
      h: 10,
      r: 0,
      fontName: "Times-Roman",
      fontSize: 10,
    },
  ];
}

describe("test PdfJS methods", () => {
  it("test loadDocument", async () => {
    const engine = new PdfJsEngine();
    const result = await engine.loadDocument("test.pdf");
    expect(result.data).toStrictEqual(new Uint8Array(Buffer.from("hello world")));
    expect(result.metadata).toBeDefined();
    expect("numPages" in (result.metadata as object)).toBeTruthy();
    expect("size" in (result.metadata as object)).toBeTruthy();
    expect("encoding" in (result.metadata as object)).toBeTruthy();
    expect(result.numPages).toBe(10);
  });

  it("test extractPage", async () => {
    const doc = getTestData();
    const expectedTextItems = getExpectedResults();

    const engine = new PdfJsEngine();
    const result = await engine.extractPage(doc, 1);
    expect(result.pageNum).toBe(1);
    expect(result.width).toBe(612);
    expect(result.height).toBe(792);
    expect(result.images.length).toBe(0);
    expect(result.annotations?.length).toBe(0);
    expect(result.textItems).toStrictEqual(expectedTextItems);
    expect(result.garbledTextRegions).toBeUndefined();
  });

  it("test extractAllPages (all pages)", async () => {
    const doc = getTestData();
    const expectedTextItems = getExpectedResults();

    const engine = new PdfJsEngine();
    const results = await engine.extractAllPages(doc);
    expect(results.length).toBe(10);
    let counter = 1;
    for (const result of results) {
      expect(result.pageNum).toBe(counter);
      expect(result.width).toBe(612);
      expect(result.height).toBe(792);
      expect(result.images.length).toBe(0);
      expect(result.annotations?.length).toBe(0);
      expect(result.textItems).toStrictEqual(expectedTextItems);
      expect(result.garbledTextRegions).toBeUndefined();
      counter++;
    }
  });

  it("test extractAllPages (with maxPages)", async () => {
    const doc = getTestData();
    const expectedTextItems = getExpectedResults();

    const engine = new PdfJsEngine();
    const results = await engine.extractAllPages(doc, 5);
    expect(results.length).toBe(5);
    let counter = 1;
    for (const result of results) {
      expect(result.pageNum).toBe(counter);
      expect(result.width).toBe(612);
      expect(result.height).toBe(792);
      expect(result.images.length).toBe(0);
      expect(result.annotations?.length).toBe(0);
      expect(result.textItems).toStrictEqual(expectedTextItems);
      expect(result.garbledTextRegions).toBeUndefined();
      counter++;
    }
  });

  it("test extractAllPages (with targetPages)", async () => {
    const doc = getTestData();
    const expectedTextItems = getExpectedResults();
    const expectedPages = [1, 2, 3, 5];

    const engine = new PdfJsEngine();
    const results = await engine.extractAllPages(doc, undefined, "1,2,3,5");
    expect(results.length).toBe(4);
    for (let i = 0; i < results.length; i++) {
      expect(results[i].pageNum).toBe(expectedPages[i]);
      expect(results[i].width).toBe(612);
      expect(results[i].height).toBe(792);
      expect(results[i].images.length).toBe(0);
      expect(results[i].annotations?.length).toBe(0);
      expect(results[i].textItems).toStrictEqual(expectedTextItems);
      expect(results[i].garbledTextRegions).toBeUndefined();
    }
  });

  it("test extractAllPages (with targetPages and maxPages)", async () => {
    const doc = getTestData();
    const expectedTextItems = getExpectedResults();
    const expectedPages = [1, 2, 3, 5];

    const engine = new PdfJsEngine();
    const results = await engine.extractAllPages(doc, 4, "1,2,3,5,8");
    expect(results.length).toBe(4);
    for (let i = 0; i < results.length; i++) {
      expect(results[i].pageNum).toBe(expectedPages[i]);
      expect(results[i].width).toBe(612);
      expect(results[i].height).toBe(792);
      expect(results[i].images.length).toBe(0);
      expect(results[i].annotations?.length).toBe(0);
      expect(results[i].textItems).toStrictEqual(expectedTextItems);
      expect(results[i].garbledTextRegions).toBeUndefined();
    }
  });

  it("test renderPageImage", async () => {
    const doc = getTestData();
    const engine = new PdfJsEngine();
    // ensure current PDF path is set
    await engine.loadDocument("test.pdf");

    const buf = await engine.renderPageImage(doc, 1, 20);
    expect(buf).toStrictEqual(Buffer.from("this is a page"));
  });
});
