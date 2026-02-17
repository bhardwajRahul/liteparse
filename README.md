# LiteParse

Open-source PDF parsing with spatial text extraction, no LLMs and cloud dependencies.

## Overview

LiteParse is a standalone OSS PDF parsing tool focused exclusively on **fast and light** parsing. It provides high-quality spatial text extraction with bounding boxes, without proprietary LLM features or cloud dependencies. Everything runs locally on your machine. 

### Features

- **Fast Text Extraction**: Spatial text extraction using PDF.js
- **Flexible OCR System**:
  - **Built-in**: Tesseract.js (zero setup, works out of the box!)
  - **HTTP Servers**: Plug in any OCR server (EasyOCR, PaddleOCR, custom)
  - **Standard API**: Simple, well-defined OCR API specification
- **Screenshot Generation**: Generate high-quality page screenshots for LLM agents
- **Multiple Output Formats**: JSON and Text
- **Bounding Boxes**: Precise text positioning information
- **Table Detection**: Heuristic table detection (outlined tables)
- **Standalone Binary**: No cloud dependencies, runs entirely locally
- **Multi-platform**: Linux, macOS (Intel/ARM), Windows

## Installation

### CLI Tool

#### Option 1: Global Install (Recommended)

Install globally via npm to use the `liteparse` command anywhere:

```bash
npm i -g liteparse
```

Then use it:

```bash
liteparse parse document.pdf
liteparse screenshot document.pdf
```

#### Option 2: Use with npx

Run directly without installing:

```bash
npx liteparse parse document.pdf
npx liteparse screenshot document.pdf
```

#### Option 3: Homebrew (macOS/Linux)

Coming soon! Once published, you'll be able to install via Homebrew:

```bash
brew tap yourusername/liteparse
brew install liteparse
```

#### Option 4: Instal from Source

You can clone the repo and install the CLI globally from source:

```
git clone https://github.com/run-llama/liteparse.git
cd liteparse
npm run build
npm pack
npm install -g ./liteparse-*.tgz
```

## Usage

### Parse PDF Files

You can run tests without building the binary using the CLI:

```bash
# Basic parsing
liteparse parse document.pdf

# Parse with specific format
liteparse parse document.pdf --format json -o output.md

# Parse specific pages
liteparse parse document.pdf --target-pages "1-5,10,15-20"

# Parse without OCR
liteparse parse document.pdf --no-ocr
```

### Batch Parsing

You can also parse an entire directory of documents:

```bash
liteparse batch-parse ./input-directory ./output-directory
```

### Generate Screenshots

Screenshots are essential for LLM agents to extract visual information that text alone cannot capture.

```bash
# Screenshot all pages
liteparse screenshot document.pdf -o ./screenshots

# Screenshot specific pages
liteparse screenshot document.pdf --pages "1,3,5" -o ./screenshots

# Custom output directory and DPI
liteparse screenshot document.pdf -o ./images --dpi 300 -o ./screenshots

# Screenshot page range
liteparse screenshot document.pdf --pages "1-10" -o ./screenshots
```

### Library Usage

Install as a dependency in your project:

```bash
npm install liteparse
# or
pnpm add liteparse
```

```typescript
import { LiteParse } from 'liteparse';

const parser = new LiteParse({ ocrEnabled: true });
const result = await parser.parse('document.pdf');
console.log(result.text);
```

### CLI Options

#### Parse Command

```
$ liteparse parse <file> [options]

Options:
  -o, --output <file>              Output file path
  --format <format>                Output format: json|text (default: "text")
  --ocr-server-url <url>           HTTP OCR server URL (uses Tesseract if not provided)
  --no-ocr                         Disable OCR
  --ocr-language <lang>            OCR language(s) (default: "en")
  --max-pages <n>                  Max pages to parse (default: "1000")
  --target-pages <pages>           Target pages (e.g., "1-5,10,15-20")
  --pdf-engine <engine>            PDF engine: pdfjs|pdfium (default: "pdfjs")
  --dpi <dpi>                      DPI for rendering (default: "150")
  --no-tables                      Disable table detection
  --no-precise-bbox                Disable precise bounding boxes
  --skip-diagonal-text             Skip diagonal text
  --preserve-small-text            Preserve very small text
  --config <file>                  Config file (JSON)
```

#### Batch Parse Command

```
$ liteparse batch-parse --help

Parse multiple documents in batch mode (reuses PDF engine for efficiency)

Options:
  --format <format>       Output format: json|text (default: "text")
  --ocr-server-url <url>  HTTP OCR server URL (uses Tesseract if not provided)
  --no-ocr                Disable OCR
  --ocr-language <lang>   OCR language(s) (default: "en")
  --max-pages <n>         Max pages to parse per file (default: "1000")
  --dpi <dpi>             DPI for rendering (default: "150")
  --no-tables             Disable table detection
  --no-precise-bbox       Disable precise bounding boxes
  --recursive             Recursively search input directory
  --extension <ext>       Only process files with this extension (e.g., ".pdf")
  --config <file>         Config file (JSON)
  -q, --quiet             Suppress progress output
  -h, --help              display help for command
```

#### Screenshot Command

```
$ liteparse screenshot <file> [options]

Options:
  -o, --output-dir <dir>           Output directory for screenshots (default: "./screenshots")
  --pages <pages>                  Page numbers to screenshot (e.g., "1,3,5" or "1-5")
  --dpi <dpi>                      DPI for rendering (default: "150")
  --format <format>                Image format: png|jpg (default: "png")
  --config <file>                  Config file (JSON)
```

## OCR Setup

### Default: Tesseract.js

```bash
# Tesseract is enabled by default
pnpm parse document.pdf

# Specify language
pnpm parse document.pdf --ocr-language fra

# Disable OCR
pnpm parse document.pdf --no-ocr
```

### Optional: HTTP OCR Servers

For higher accuracy or better performance, you can use an HTTP OCR server. We provide ready-to-use example wrappers for popular OCR engines:

- [EasyOCR](ocr/easyocr/README.md)
- [PaddleOCR](ocr/paddleocr/README.md)

You can integrate any OCR service by implementing the simple LiteParse OCR API specification (see [`OCR_API_SPEC.md`](OCR_API_SPEC.md)).

The API requires:
- POST `/ocr` endpoint
- Accepts `file` and `language` parameters
- Returns JSON: `{ results: [{ text, bbox: [x1,y1,x2,y2], confidence }] }`

See the example servers in `ocr/easyocr/` and `ocr/paddleocr/` as templates.

For the complete OCR API specification, see [`OCR_API_SPEC.md`](OCR_API_SPEC.md).

## Multi-Format Input Support

LiteParse supports **automatic conversion** of various document formats to PDF before parsing. This makes it unique compared to other PDF-only parsing tools!

### Supported Input Formats

#### Office Documents (via LibreOffice)
- **Word**: `.doc`, `.docx`, `.docm`, `.odt`, `.rtf`
- **PowerPoint**: `.ppt`, `.pptx`, `.pptm`, `.odp`
- **Spreadsheets**: `.xls`, `.xlsx`, `.xlsm`, `.ods`, `.csv`, `.tsv`

Just install the dependency and LiteParse will automatically convert these formats to PDF for parsing:

```bash
# macOS
brew install --cask libreoffice

# Ubuntu/Debian
apt-get install libreoffice
```

#### Images (via ImageMagick)
- **Formats**: `.jpg`, `.jpeg`, `.png`, `.gif`, `.bmp`, `.tiff`, `.webp`, `.svg`

Just install ImageMagick and LiteParse will convert images to PDF for parsing (with OCR):

```bash
# macOS
brew install imagemagick

# Ubuntu/Debian
apt-get install imagemagick
```

## Configuration

You can configure parsing options via CLI flags or a JSON config file. The config file allows you to set sensible defaults and override as needed.

### Config File Example

Create a `liteparse.config.json` file:

```json
{
  "ocrLanguage": "en",
  "ocrEnabled": true,
  "maxPages": 1000,
  "dpi": 150,
  "outputFormat": "json",
  "includeImages": true,
  "includeCharts": true,
  "tableDetection": true,
  "preciseBoundingBox": true,
  "skipDiagonalText": false,
  "preserveVerySmallText": false
}
```

For HTTP OCR servers, just add `ocrServerUrl`:

```json
{
  "ocrServerUrl": "http://localhost:8828/ocr",
  "ocrLanguage": "en",
  "outputFormat": "json"
}
```

Use with:

```bash
pnpm parse document.pdf --config liteparse.config.json
```

## Development

We provide a fairly rich `AGENTS.md`/`CLAUDE.md` that we recommend using to help with development + coding agents.

```bash
# Install dependencies
npm install

# Build TypeScript
npm run build

# Watch mode
npm run dev

# Test parsing
npm test
```

## License

Apache 2.0

## Credits

Built on top of:

- [PDF.js](https://github.com/mozilla/pdf.js) - PDF parsing engine
- [Tesseract.js](https://github.com/naptha/tesseract.js) - In-process OCR engine
- [EasyOCR](https://github.com/JaidedAI/EasyOCR) - HTTP OCR server (optional)
- [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) - HTTP OCR server (optional)
- [Sharp](https://github.com/lovell/sharp) - Image processing
