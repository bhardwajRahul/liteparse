import { describe, expect, it } from "vitest";
import { searchItems } from "./searchItems";
import { JsonTextItem } from "../core/types";

function item(
  text: string,
  x: number,
  y: number,
  width: number,
  height = 12,
  fontSize = 12
): JsonTextItem {
  return { text, x, y, width, height, fontSize };
}

describe("searchItems", () => {
  it("matches a phrase within a single item", () => {
    const items = [item("hello world", 10, 20, 100)];
    const results = searchItems(items, { phrase: "hello world" });
    expect(results).toHaveLength(1);
    expect(results[0].text).toBe("hello world");
    expect(results[0].x).toBe(10);
    expect(results[0].width).toBe(100);
  });

  it("matches a phrase spanning multiple items", () => {
    const items = [item("0°C", 10, 50, 30), item("to", 45, 50, 15), item("70°C", 65, 50, 35)];
    const results = searchItems(items, { phrase: "0°C to 70°C" });
    expect(results).toHaveLength(1);
    expect(results[0].text).toBe("0°C to 70°C");
    expect(results[0].x).toBe(10);
    expect(results[0].width).toBe(90); // 65 + 35 - 10
  });

  it("narrows match and does not include unrelated leading items", () => {
    const items = [item("Operating", 10, 50, 70), item("0°C to 70°C", 85, 50, 90)];
    const results = searchItems(items, { phrase: "0°C to 70°C" });
    expect(results).toHaveLength(1);
    expect(results[0].x).toBe(85);
    expect(results[0].width).toBe(90);
  });

  it("is case-insensitive by default", () => {
    const items = [item("Revenue Grew", 10, 20, 100)];
    const results = searchItems(items, { phrase: "revenue grew" });
    expect(results).toHaveLength(1);
    expect(results[0].text).toBe("revenue grew");
  });

  it("respects caseSensitive option", () => {
    const items = [item("pH Level", 10, 20, 80)];
    expect(searchItems(items, { phrase: "pH", caseSensitive: true })).toHaveLength(1);
    expect(searchItems(items, { phrase: "ph", caseSensitive: true })).toHaveLength(0);
    expect(searchItems(items, { phrase: "PH", caseSensitive: true })).toHaveLength(0);
  });

  it("returns empty array when no match", () => {
    const items = [item("hello", 10, 20, 50)];
    const results = searchItems(items, { phrase: "goodbye" });
    expect(results).toHaveLength(0);
  });

  it("matches spatially adjacent items without inserting a space", () => {
    // "29-CA-" and "261755" are adjacent (gap=0), so joined as "29-CA-261755"
    // "Case No." and "29-CA-" have a word gap (gap=5 > tolerance), so space inserted
    const items = [
      item("Case No.", 10, 50, 60),
      item("29-CA-", 75, 50, 50),
      item("261755", 125, 50, 50),
    ];
    const results = searchItems(items, { phrase: "Case No. 29-CA-261755" });
    expect(results).toHaveLength(1);
    expect(results[0].text).toBe("Case No. 29-CA-261755");
  });

  it("matches adjacent items with en-dash", () => {
    // "pages 10–" and "20" are adjacent (gap=0)
    const items = [item("pages 10\u2013", 10, 50, 60), item("20", 70, 50, 20)];
    const results = searchItems(items, { phrase: "pages 10\u201320" });
    expect(results).toHaveLength(1);
  });

  it("narrows correctly past adjacent items", () => {
    // "prefix" has word gap to "29-CA-", "29-CA-" is adjacent to "261755"
    const items = [
      item("prefix", 10, 50, 40),
      item("29-CA-", 55, 50, 50),
      item("261755", 105, 50, 50),
    ];
    const results = searchItems(items, { phrase: "29-CA-261755" });
    expect(results).toHaveLength(1);
    expect(results[0].x).toBe(55);
  });

  it("merges bounding boxes vertically for wrapped phrases", () => {
    const items = [item("temperature", 10, 50, 80, 12), item("range", 10, 65, 40, 12)];
    const results = searchItems(items, { phrase: "temperature range" });
    expect(results).toHaveLength(1);
    expect(results[0].y).toBe(50);
    expect(results[0].height).toBe(27); // 65 + 12 - 50
  });
});
