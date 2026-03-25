---
title: Visual Citations with Bounding Boxes
description: Use bounding boxes and screenshots to show exactly where information was found in a document.
sidebar:
  order: 4
---

When building agents or RAG workflows, it is often not enough to parse text and call it done. Frequently, users and applications will require you to show _where_ that text came from. 

LiteParse gives you spatial coordinates for every text item, plus page screenshots, so you can highlight exact regions on the rendered page.

## How bounding boxes work

When you parse a document with JSON output, each page includes a key data source for visual citations: **`textItems`**. Every extracted text element with its position (`x`, `y`, `width`, `height`) and content.

```json
$ lit parse document.pdf --format json
{
  "pages": [{
    "page": 1,
    "width": 612,
    "height": 792,
    "text": "...",
    "textItems": [
      { "text": "Revenue grew 15%", "x": 72, "y": 200, "width": 150, "height": 12, ... }
    ],
  }]
}
```

Coordinates are in **PDF points** (1 point = 1/72 inch). Origin is the top-left corner of the page, with X increasing right and Y increasing down.

## Library usage

The library lets you do both in a single script, parse for bboxes and generate screenshots. For example, you might be looking for specific information like "Revenue" and want to show exactly where it appears on the page:

```typescript
import { LiteParse } from "@llamaindex/liteparse";

const parser = new LiteParse({ outputFormat: "json", dpi: 150 });

const result = await parser.parse("report.pdf");
const screenshots = await parser.screenshot("report.pdf");

// Find a text item by its content
for (const page of result.json?.pages || []) {
  for (const item of page.textItems) {
    if (item.text.includes("Revenue")) {
      console.log(`Found on page ${page.page}: (${item.x}, ${item.y}) ${item.width}×${item.height}`);
    }
  }
}
```

## Converting coordinates to image pixels

Text item coordinates are in PDF points, but screenshots are in pixels. To draw highlights on a screenshot, you need to scale the coordinates:

```typescript
const scaleFactor = dpi / 72; // PDF points → pixels at your chosen DPI

function itemToPixels(item, dpi = 150) {
  const scale = dpi / 72;
  return {
    x: item.x * scale,
    y: item.y * scale,
    width: item.width * scale,
    height: item.height * scale,
  };
}
```

For example, at the default 150 DPI the scale factor is `150 / 72 ≈ 2.08`, so a text item at `(72, 200)` maps to pixel `(150, 416)`.

## Full example: highlighting citations with sharp

Here's a complete workflow that parses a PDF, searches for matching text, and draws yellow highlight boxes on the page screenshot:

```typescript
import { LiteParse } from "@llamaindex/liteparse";
import sharp from "sharp";

const DPI = 150;
const SCALE = DPI / 72;

async function main() {
  const parser = new LiteParse({ outputFormat: "json", dpi: DPI });

  const result = await parser.parse("report.pdf");
  const screenshots = await parser.screenshot("report.pdf");

  // Search for text items containing a query, grouped by page
  const query = "revenue";
  const hitsByPage = new Map<number, Array<{ x: number; y: number; width: number; height: number }>>();

  for (const page of result.json?.pages || []) {
    for (const item of page.textItems) {
      if (item.text.toLowerCase().includes(query)) {
        if (!hitsByPage.has(page.page)) hitsByPage.set(page.page, []);
        hitsByPage.get(page.page)!.push(item);
      }
    }
  }

  // Draw all highlights per page into a single image
  for (const [pageNum, items] of hitsByPage) {
    const shot = screenshots.find((s) => s.pageNum === pageNum);
    if (!shot) continue;

    const composites = await Promise.all(
      items.map(async (item) => {
        const rect = {
          left: Math.round(item.x * SCALE),
          top: Math.round(item.y * SCALE),
          width: Math.round(item.width * SCALE),
          height: Math.round(item.height * SCALE),
        };

        const overlay = await sharp({
          create: {
            width: rect.width,
            height: rect.height,
            channels: 4,
            background: { r: 255, g: 255, b: 0, alpha: 0.3 },
          },
        })
          .png()
          .toBuffer();

        return { input: overlay, left: rect.left, top: rect.top };
      })
    );

    const highlighted = await sharp(shot.imageBuffer)
      .composite(composites)
      .png()
      .toBuffer();

    await sharp(highlighted).toFile(`citation_page${pageNum}.png`);
    console.log(`Saved citation_page${pageNum}.png (${items.length} highlights)`);
  }
}

main().catch(console.error);
```

## CLI usage

Parse to JSON to get bounding boxes:

```bash
lit parse document.pdf --format json -o result.json
```

Generate page screenshots alongside:

```bash
lit screenshot document.pdf -o ./screenshots
```

From there, you (or an agent) can process the resulting JSON and screenshots as needed using any tools available.

## Deprecated: `boundingBoxes`

The `boundingBoxes` array in JSON output is **deprecated** and will be removed in **v2.0**. It is a redundant representation of the same spatial data already available on each text item (`x`, `y`, `width`, `height`). Use `textItems` directly instead — it has the same coordinates plus text content, font metadata, and consistent indexing.

## Tips

- Use the same `dpi` value for both `parse()` and `screenshot()`. The default is `150` for both.
- Page `width` and `height` in the JSON are in PDF points, matching the coordinate space. Use these if you need to normalize coordinates to percentages.
