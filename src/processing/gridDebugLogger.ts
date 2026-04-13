import { mkdirSync, writeFileSync } from "fs";
import { mkdir, writeFile } from "fs/promises";
import { dirname } from "path";
import { ProjectionTextBox } from "../core/types.js";

/**
 * Configuration for grid projection debug logging.
 *
 * When enabled, logs detailed information about how text elements are
 * snapped, anchored, and projected during grid layout. Use filters to
 * narrow output to specific elements you're investigating.
 */
export interface GridDebugConfig {
  /**
   * Enable debug logging for grid projection.
   * @defaultValue `false`
   */
  enabled: boolean;

  /**
   * Only log elements whose text contains one of these substrings (case-insensitive).
   * If empty, all elements are logged.
   */
  textFilter?: string[];

  /**
   * Only log elements on these line indices (0-based within the page).
   */
  lineFilter?: number[];

  /**
   * Only log elements on this page number (1-indexed).
   */
  pageFilter?: number;

  /**
   * Only log elements within this bounding region (PDF coordinates).
   */
  regionFilter?: { x1: number; y1: number; x2: number; y2: number };

  /**
   * Write log output to a file path instead of stderr. If not set, logs to stderr.
   */
  outputPath?: string;

  /**
   * Generate PNG visualizations of the grid projection showing text boxes
   * color-coded by snap type (left/right/center/floating/flowing) with
   * anchor lines overlaid.
   * @defaultValue `false`
   */
  visualize?: boolean;

  /**
   * Directory to save visualization PNGs. Each page produces a file
   * named `page-{N}-grid.png`.
   * @defaultValue `"./debug-output"`
   */
  visualizePath?: string;

  /**
   * Enable trace mode for detailed render decision logging.
   * When enabled, each render logs the full decision chain: initial targetX,
   * lineMax computation, forward anchor checks, and which factor was the
   * binding constraint. Forward anchor mutations are also traced with their
   * triggering item. Respects textFilter/lineFilter/pageFilter.
   * @defaultValue `false`
   */
  trace?: boolean;
}

export const DEFAULT_DEBUG_CONFIG: GridDebugConfig = {
  enabled: false,
};

type LogEntry = {
  phase: string;
  lineIndex?: number;
  boxIndex?: number;
  text?: string;
  message: string;
  data?: Record<string, unknown>;
};

/** A segment of rendered text placed into the output grid. */
export interface RenderedSegment {
  lineIndex: number;
  /** Character column where this segment starts in the output line. */
  gridCol: number;
  text: string;
  snap: "left" | "right" | "center" | "floating" | "flowing";
}

/** Context for a render decision trace — captures every factor that influenced targetX. */
export interface RenderTraceContext {
  snapType: "left" | "right" | "center" | "floating";

  // Initial calculation
  initialTargetX: number;
  medianWidth: number;

  // lineMax computation
  lineMax: number;
  lastSnapLeft?: number;
  lastSnapLeftKey?: number;
  rawLineTrimLength: number;
  shouldSpace: number;

  // For right snap: per-item lineMax candidates
  lineMaxCandidates?: Array<{
    text: string;
    lineIndex: number;
    lineItemCount: number;
    filtered: boolean;
    value: number;
  }>;

  // Forward anchor check
  forwardAnchorValue?: number;

  // prevAnchors check
  prevAnchorValue?: number;

  // Floating-specific
  pdfFallbackUsed?: boolean;
  pdfFallbackTargetX?: number;
  floatingAnchorBump?: number;

  // Sparse anchor (left snap)
  isSparseAnchor?: boolean;

  // Final
  finalTargetX: number;
  bindingConstraint: string;
}

/** Captured data for a single page, used by the grid visualizer. */
export interface VisualizerPageData {
  pageNum: number;
  /** Rendered text segments with snap type for color coding. */
  segments: RenderedSegment[];
  /** Final projected text lines (the actual output). */
  rawLines: string[];
}

/**
 * Debug logger for grid projection. Provides targeted logging to trace
 * exactly why specific text elements are projected the way they are.
 *
 * Usage from LiteParse config:
 * ```typescript
 * const parser = new LiteParse({
 *   debug: {
 *     enabled: true,
 *     textFilter: ["Total", "Revenue"],  // only log elements containing these strings
 *     pageFilter: 2,                      // only page 2
 *     visualize: true,                    // generate PNG overlays
 *   }
 * });
 * ```
 */
export class GridDebugLogger {
  private config: GridDebugConfig;
  private entries: LogEntry[] = [];
  private currentPage: number = 0;
  private writeOutput: (msg: string) => void;

  // Trace mode: snap positions that matched-filter items belong to.
  // Forward anchor mutations affecting these positions are always logged.
  private trackedSnapPositions = new Set<number>();

  // Visualization data collection
  private vizPages: VisualizerPageData[] = [];
  private currentVizPage?: VisualizerPageData;

  constructor(config: GridDebugConfig) {
    this.config = config;
    if (config.outputPath) {
      // Defer file writing to flush()
      this.writeOutput = () => {};
    } else {
      this.writeOutput = (msg: string) => process.stderr.write(msg + "\n");
    }
  }

  get enabled(): boolean {
    return this.config.enabled;
  }

  get shouldVisualize(): boolean {
    return this.config.visualize ?? false;
  }

  get visualizerPages(): VisualizerPageData[] {
    return this.vizPages;
  }

  get debugConfig(): GridDebugConfig {
    return this.config;
  }

  setPage(pageNum: number, _width: number, _height: number): void {
    this.currentPage = pageNum;
    if (this.config.pageFilter && pageNum !== this.config.pageFilter) {
      return;
    }
    this.emit({ phase: "page", message: `=== Page ${pageNum} ===` });

    if (this.shouldVisualize) {
      this.currentVizPage = {
        pageNum,
        segments: [],
        rawLines: [],
      };
      this.vizPages.push(this.currentVizPage);
    }
  }

  private isPageFiltered(): boolean {
    return !!this.config.pageFilter && this.currentPage !== this.config.pageFilter;
  }

  /** Check if a bbox passes the configured filters */
  matchesBbox(bbox: ProjectionTextBox, lineIndex?: number): boolean {
    if (!this.config.enabled) return false;
    if (this.isPageFiltered()) return false;

    if (this.config.textFilter?.length) {
      const text = bbox.str.toLowerCase();
      if (!this.config.textFilter.some((f) => text.includes(f.toLowerCase()))) {
        return false;
      }
    }

    if (this.config.lineFilter?.length && lineIndex !== undefined) {
      if (!this.config.lineFilter.includes(lineIndex)) {
        return false;
      }
    }

    if (this.config.regionFilter) {
      const r = this.config.regionFilter;
      if (bbox.x + bbox.w < r.x1 || bbox.x > r.x2 || bbox.y + bbox.h < r.y1 || bbox.y > r.y2) {
        return false;
      }
    }

    return true;
  }

  /** Log which block a line range was assigned to */
  logBlock(blockIndex: number, start: number, end: number): void {
    if (!this.config.enabled || this.isPageFiltered()) return;
    this.emit({
      phase: "blocks",
      message: `Block ${blockIndex}: lines ${start}-${end - 1}`,
    });
  }

  /** Log flowing text classification */
  logFlowingBlock(start: number, end: number): void {
    if (!this.config.enabled || this.isPageFiltered()) return;
    this.emit({
      phase: "flowing",
      message: `Block lines ${start}-${end - 1} classified as flowing text`,
    });
  }

  /** Log non-flowing block (no-op, only used for symmetry with logFlowingBlock) */
  logStructuredBlock(_start: number, _end: number): void {}

  /** Log flowing line detection */
  logFlowingLine(lineIndex: number, reason: string): void {
    if (!this.config.enabled || this.isPageFiltered()) return;
    if (this.config.lineFilter?.length && !this.config.lineFilter.includes(lineIndex)) return;
    this.emit({
      phase: "flowing",
      lineIndex,
      message: `Line ${lineIndex} marked flowing: ${reason}`,
    });
  }

  /** Log anchor extraction results for a block */
  logAnchors(
    anchorLeft: Record<number, ProjectionTextBox[]>,
    anchorRight: Record<number, ProjectionTextBox[]>,
    anchorCenter: Record<number, ProjectionTextBox[]>
  ): void {
    if (!this.config.enabled || this.isPageFiltered()) return;

    const leftKeys = Object.keys(anchorLeft).map(Number);
    const rightKeys = Object.keys(anchorRight).map(Number);
    const centerKeys = Object.keys(anchorCenter).map(Number);

    this.emit({
      phase: "anchors",
      message: `Anchors: left=[${leftKeys.join(", ")}] right=[${rightKeys.join(", ")}] center=[${centerKeys.join(", ")}]`,
    });

    // Log which elements are in each anchor, but only if they match filters
    for (const key of leftKeys) {
      const items = anchorLeft[key];
      const matchingItems = this.config.textFilter?.length
        ? items.filter((b) => this.matchesBbox(b))
        : items;
      if (matchingItems.length > 0) {
        this.emit({
          phase: "anchors",
          message: `  left@${key}: ${matchingItems.map((b) => `"${b.str.substring(0, 30)}" (x=${b.x.toFixed(1)}, y=${b.y.toFixed(1)})`).join(", ")}`,
        });
      }
    }
    for (const key of rightKeys) {
      const items = anchorRight[key];
      const matchingItems = this.config.textFilter?.length
        ? items.filter((b) => this.matchesBbox(b))
        : items;
      if (matchingItems.length > 0) {
        this.emit({
          phase: "anchors",
          message: `  right@${key}: ${matchingItems.map((b) => `"${b.str.substring(0, 30)}" (x=${b.x.toFixed(1)}, y=${b.y.toFixed(1)})`).join(", ")}`,
        });
      }
    }
    for (const key of centerKeys) {
      const items = anchorCenter[key];
      const matchingItems = this.config.textFilter?.length
        ? items.filter((b) => this.matchesBbox(b))
        : items;
      if (matchingItems.length > 0) {
        this.emit({
          phase: "anchors",
          message: `  center@${key}: ${matchingItems.map((b) => `"${b.str.substring(0, 30)}" (x=${b.x.toFixed(1)}, y=${b.y.toFixed(1)})`).join(", ")}`,
        });
      }
    }
  }

  /** Log snap assignment for a bbox */
  logSnapAssignment(bbox: ProjectionTextBox, lineIndex: number, boxIndex: number): void {
    if (!this.matchesBbox(bbox, lineIndex)) return;

    // Track snap positions for matched items so we can trace forward anchor
    // mutations that affect them even when the triggering item doesn't match filters
    if (this.config.trace) {
      if (bbox.leftAnchor) this.trackedSnapPositions.add(parseFloat(bbox.leftAnchor));
      if (bbox.rightAnchor) this.trackedSnapPositions.add(parseFloat(bbox.rightAnchor));
      if (bbox.centerAnchor) this.trackedSnapPositions.add(parseFloat(bbox.centerAnchor));
    }

    this.emit({
      phase: "snap",
      lineIndex,
      boxIndex,
      text: bbox.str,
      message: `"${bbox.str.substring(0, 40)}" snap=${bbox.snap ?? "none"} leftAnchor=${bbox.leftAnchor ?? "-"} rightAnchor=${bbox.rightAnchor ?? "-"} centerAnchor=${bbox.centerAnchor ?? "-"} forceUnsnapped=${bbox.forceUnsnapped ?? false}`,
      data: {
        x: round2(bbox.x),
        y: round2(bbox.y),
        w: round2(bbox.w),
        h: round2(bbox.h),
        shouldSpace: bbox.shouldSpace,
      },
    });
  }

  /** Capture a rendered text segment for visualization. */
  captureRender(
    lineIndex: number,
    gridCol: number,
    text: string,
    snap: "left" | "right" | "center" | "floating" | "flowing"
  ): void {
    if (!this.currentVizPage) return;
    this.currentVizPage.segments.push({ lineIndex, gridCol, text, snap });
  }

  /** Capture the final raw lines after projection completes for this page. */
  captureRawLines(rawLines: string[]): void {
    if (!this.currentVizPage) return;
    this.currentVizPage.rawLines = Array.from(
      { length: rawLines.length },
      (_, i) => rawLines[i] ?? ""
    );
  }

  /** Log the rendering of a bbox to a target column position */
  logRender(bbox: ProjectionTextBox, lineIndex: number, targetX: number, reason: string): void {
    if (!this.matchesBbox(bbox, lineIndex)) return;
    this.emit({
      phase: "render",
      lineIndex,
      text: bbox.str,
      message: `"${bbox.str.substring(0, 40)}" → col ${targetX} (${reason})`,
      data: {
        pdfX: round2(bbox.x),
        snap: bbox.snap ?? "none",
        shouldSpace: bbox.shouldSpace,
      },
    });
  }

  /** Log forward anchor updates */
  logForwardAnchor(
    type: "left" | "right" | "center" | "floating",
    pdfX: number,
    gridCol: number
  ): void {
    if (!this.config.enabled || this.isPageFiltered()) return;
    // Only log if no text filter (forward anchors aren't bbox-specific) or if we've been logging
    if (this.config.textFilter?.length) return;
    this.emit({
      phase: "forward-anchor",
      message: `${type}@${round2(pdfX)} → col ${gridCol}`,
    });
  }

  /** Log duplicate resolution for multi-anchor elements */
  logDuplicateResolution(bbox: ProjectionTextBox, resolvedTo: string): void {
    if (!this.matchesBbox(bbox)) return;
    this.emit({
      phase: "dedup",
      text: bbox.str,
      message: `"${bbox.str.substring(0, 40)}" multi-anchor resolved to ${resolvedTo} (left=${bbox.leftAnchor ?? "-"} right=${bbox.rightAnchor ?? "-"} center=${bbox.centerAnchor ?? "-"})`,
    });
  }

  /** Log block-level context (medianWidth, sparse anchors, snap maps) */
  logBlockContext(
    blockStart: number,
    blockEnd: number,
    medianWidth: number,
    sparseLeftAnchors: Set<number>
  ): void {
    if (!this.config.trace || !this.config.enabled || this.isPageFiltered()) return;
    let msg = `Block ${blockStart}-${blockEnd - 1}: medianWidth=${round2(medianWidth)}`;
    if (sparseLeftAnchors.size > 0) {
      msg += `, sparseLeft=[${[...sparseLeftAnchors].map(round2).join(", ")}]`;
    }
    this.emit({ phase: "trace-block", message: msg });
  }

  /** Log full render decision trace for a single item */
  logRenderTrace(bbox: ProjectionTextBox, lineIndex: number, ctx: RenderTraceContext): void {
    if (!this.config.trace || !this.matchesBbox(bbox, lineIndex)) return;

    const lines: string[] = [];
    lines.push(
      `"${bbox.str.substring(0, 40)}" → col ${ctx.finalTargetX} (${ctx.snapType}) [binding: ${ctx.bindingConstraint}]`
    );
    lines.push(
      `  initial: round(${round2(bbox.x)} / ${round2(ctx.medianWidth)}) = ${ctx.initialTargetX}, COLUMN_SPACES cap → ${Math.min(ctx.initialTargetX, 4)}`
    );

    // lineMax details
    if (ctx.lineMaxCandidates) {
      // Right snap: show per-item candidates
      lines.push(`  lineMax: ${ctx.lineMax} from ${ctx.lineMaxCandidates.length} candidates:`);
      for (const c of ctx.lineMaxCandidates) {
        const mark = c.filtered ? " [FILTERED: single-item line]" : "";
        lines.push(
          `    L${c.lineIndex} "${c.text}" → ${c.value} (lineItems=${c.lineItemCount})${mark}`
        );
      }
    } else {
      const parts: string[] = [];
      if (ctx.lastSnapLeft !== undefined && ctx.lastSnapLeft > 0) {
        parts.push(
          `lastSnapLeft=${ctx.lastSnapLeft}${ctx.lastSnapLeftKey !== undefined ? ` (from left@${round2(ctx.lastSnapLeftKey)})` : ""}`
        );
      }
      parts.push(`rawLine.trimEnd=${ctx.rawLineTrimLength}`);
      if (ctx.shouldSpace > 0) parts.push(`shouldSpace=${ctx.shouldSpace}`);
      lines.push(`  lineMax: ${ctx.lineMax} (${parts.join(", ")})`);
    }

    // Forward anchor
    if (ctx.forwardAnchorValue !== undefined) {
      lines.push(`  forwardAnchor: ${ctx.forwardAnchorValue}`);
    }
    // prevAnchors
    if (ctx.prevAnchorValue !== undefined) {
      lines.push(`  prevAnchor: ${ctx.prevAnchorValue}`);
    }
    // Floating-specific
    if (ctx.pdfFallbackUsed) {
      lines.push(`  pdfFallback: targetX raised to ${ctx.pdfFallbackTargetX} (empty line)`);
    }
    if (ctx.floatingAnchorBump !== undefined) {
      lines.push(`  floatingAnchor bump: +${ctx.floatingAnchorBump}`);
    }
    if (ctx.isSparseAnchor) {
      lines.push(`  sparse anchor: skipping forward anchor propagation and block padding`);
    }

    this.emit({
      phase: "trace-render",
      lineIndex,
      text: bbox.str,
      message: lines.join("\n"),
    });
  }

  /** Log a forward anchor mutation with the triggering item */
  logForwardAnchorMutation(
    triggerText: string,
    triggerLineIndex: number,
    snapType: string,
    snapPosition: number,
    oldValue: number | undefined,
    newValue: number,
    rightBound: number
  ): void {
    if (!this.config.trace || !this.config.enabled || this.isPageFiltered()) return;
    // Log if: no text filter, or trigger matches filter, or snap position is tracked
    if (this.config.textFilter?.length) {
      const text = triggerText.toLowerCase();
      const matchesTrigger = this.config.textFilter.some((f) => text.includes(f.toLowerCase()));
      const matchesPosition = this.trackedSnapPositions.has(snapPosition);
      if (!matchesTrigger && !matchesPosition) return;
    }
    const oldStr = oldValue !== undefined ? `was ${oldValue}` : "unset";
    this.emit({
      phase: "trace-fwd-mutation",
      lineIndex: triggerLineIndex,
      message: `${snapType}@${round2(snapPosition)}: ${oldStr} → ${newValue} (triggered by "${triggerText.substring(0, 40)}" L${triggerLineIndex}, rightBound=${round2(rightBound)})`,
    });
  }

  /** Log line composition (the full set of bboxes on a line) */
  logLineComposition(lineIndex: number, line: ProjectionTextBox[]): void {
    if (!this.config.enabled || this.isPageFiltered()) return;
    if (this.config.lineFilter?.length && !this.config.lineFilter.includes(lineIndex)) return;
    // If text filter is set, only log lines that contain a matching bbox
    if (this.config.textFilter?.length) {
      if (!line.some((b) => this.matchesBbox(b, lineIndex))) return;
    }
    const items = line.map((b) => `"${b.str.substring(0, 20)}"@(${round2(b.x)},${round2(b.y)})`);
    this.emit({
      phase: "lines",
      lineIndex,
      message: `Line ${lineIndex} [${line.length} items]: ${items.join("  ")}`,
    });
  }

  private emit(entry: LogEntry): void {
    this.entries.push(entry);
    if (!this.config.outputPath) {
      const prefix = `[grid-debug][${entry.phase}]`;
      const loc =
        entry.lineIndex !== undefined
          ? ` L${entry.lineIndex}${entry.boxIndex !== undefined ? `:${entry.boxIndex}` : ""}`
          : "";
      let line = `${prefix}${loc} ${entry.message}`;
      if (entry.data) {
        line += ` ${JSON.stringify(entry.data)}`;
      }
      this.writeOutput(line);
    }
  }

  private formatEntries(): string {
    return (
      this.entries
        .map((entry) => {
          const prefix = `[${entry.phase}]`;
          const loc =
            entry.lineIndex !== undefined
              ? ` L${entry.lineIndex}${entry.boxIndex !== undefined ? `:${entry.boxIndex}` : ""}`
              : "";
          let line = `${prefix}${loc} ${entry.message}`;
          if (entry.data) {
            line += ` ${JSON.stringify(entry.data)}`;
          }
          return line;
        })
        .join("\n") + "\n"
    );
  }

  /** Synchronous flush of log entries to file. */
  flushSync(): void {
    if (!this.config.outputPath || this.entries.length === 0) return;
    mkdirSync(dirname(this.config.outputPath), { recursive: true });
    writeFileSync(this.config.outputPath, this.formatEntries(), "utf-8");
    this.entries = [];
  }

  /** Flush entries to file if outputPath is configured */
  async flush(): Promise<void> {
    if (!this.config.outputPath || this.entries.length === 0) return;
    await mkdir(dirname(this.config.outputPath), { recursive: true });
    await writeFile(this.config.outputPath, this.formatEntries(), "utf-8");
    this.entries = [];
  }
}

/** No-op logger that skips all checks. Use when debug is disabled. */
class NoopGridDebugLogger extends GridDebugLogger {
  constructor() {
    super({ enabled: false });
  }
  override get enabled(): boolean {
    return false;
  }
  override get shouldVisualize(): boolean {
    return false;
  }
  override matchesBbox(): boolean {
    return false;
  }
  override setPage(): void {}
  override logBlock(): void {}
  override logFlowingBlock(): void {}
  override logStructuredBlock(): void {}
  override logFlowingLine(): void {}
  override logAnchors(): void {}
  override logSnapAssignment(): void {}
  override captureRender(): void {}
  override captureRawLines(): void {}
  override logRender(): void {}
  override logForwardAnchor(): void {}
  override logDuplicateResolution(): void {}
  override logBlockContext(): void {}
  override logRenderTrace(): void {}
  override logForwardAnchorMutation(): void {}
  override logLineComposition(): void {}
  override flushSync(): void {}
  override async flush(): Promise<void> {}
}

/** Singleton no-op instance for zero-overhead when debug is off */
export const NOOP_LOGGER = new NoopGridDebugLogger();

export function createGridDebugLogger(config?: GridDebugConfig): GridDebugLogger {
  if (!config?.enabled) return NOOP_LOGGER;
  return new GridDebugLogger(config);
}

function round2(n: number): number {
  return Math.round(n * 100) / 100;
}
