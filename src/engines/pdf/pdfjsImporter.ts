import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

export async function importPdfJs() {
  const pdfUrl = new URL("../../vendor/pdfjs/pdf.mjs", import.meta.url);
  const pdfjs = await import(pdfUrl.href);

  const dirPath = dirname(fileURLToPath(pdfUrl));
  return {
    fn: pdfjs.getDocument,
    dir: dirPath,
  };
}
