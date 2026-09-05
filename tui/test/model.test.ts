import { describe, expect, test } from "bun:test"
import {
  buildConversionArgs,
  createQueueItem,
  dedupeSources,
  formatCategory,
  matchesQuery,
  parseCapabilities,
  type FormatCapability,
} from "../src/model"

describe("queue model", () => {
  test("classifies local paths and URLs without touching the filesystem", () => {
    expect(createQueueItem("/tmp/report.docx", "1")).toMatchObject({ kind: "file", name: "report.docx" })
    expect(createQueueItem("https://example.com/a/b", "2")).toMatchObject({ kind: "url", name: "example.com/a/b" })
  })

  test("deduplicates sources while preserving first-seen order", () => {
    expect(dedupeSources(["a.pdf", "b.pdf", "a.pdf"])).toEqual(["a.pdf", "b.pdf"])
  })

  test("builds the single-input headless contract", () => {
    const args = buildConversionArgs([createQueueItem("report.docx", "1")], {
      formats: ["markdown"],
      force: false,
      recursive: false,
    })
    expect(args).toEqual(["--headless", "report.docx", "--to", "markdown", "--progress", "--print-json"])
  })

  test("builds batch fan-out and destination flags", () => {
    const args = buildConversionArgs([createQueueItem("a.pdf", "1"), createQueueItem("b.docx", "2")], {
      formats: ["markdown", "html", "pdf"],
      outputDir: "/tmp/out",
      namingTemplate: "{stem}-converted.{ext}",
      preferredModule: "pandoc",
      force: true,
      recursive: true,
    })
    expect(args.slice(0, 5)).toEqual(["--headless", "batch", "a.pdf", "b.docx", "--to"])
    expect(args).toContain("--also-to")
    expect(args).toContain("--output-dir")
    expect(args).toContain("--name-template")
    expect(args).toContain("--module")
    expect(args).toContain("--force")
    expect(args).toContain("--recursive")
  })

  test("confirms public URLs for non-interactive engine execution", () => {
    const args = buildConversionArgs([createQueueItem("https://example.com", "1")], {
      formats: ["html"],
      force: false,
      recursive: false,
    })
    expect(args).toContain("--yes")
  })

  test("rejects incomplete conversion requests", () => {
    expect(() => buildConversionArgs([], { formats: ["pdf"], force: false, recursive: false })).toThrow(
      "Select at least one",
    )
    expect(() =>
      buildConversionArgs([createQueueItem("a.md", "1")], { formats: [], force: false, recursive: false }),
    ).toThrow("output format")
  })
})

describe("capability schema", () => {
  const payload = {
    schemaVersion: 1,
    cliVersion: "1.2.0",
    platform: { os: "linux", arch: "x64" },
    inputCount: 0,
    suggestedFormat: "markdown",
    formats: [{ id: "markdown", label: "Markdown", extension: "md", mediaType: "text/markdown" }],
    modules: [],
  }

  test("accepts the stable versioned bridge", () => {
    expect(parseCapabilities(JSON.stringify(payload)).formats[0].id).toBe("markdown")
  })

  test("rejects incompatible or malformed bridges", () => {
    expect(() => parseCapabilities("{}")).toThrow("unsupported capability schema")
    expect(() => parseCapabilities(JSON.stringify({ ...payload, formats: [{ label: "Missing id" }] }))).toThrow(
      "invalid format",
    )
  })

  test("categorizes output families", () => {
    const format = (id: string, mediaType: string) => ({ id, label: id, extension: id, mediaType }) as FormatCapability
    expect(formatCategory(format("mp3", "audio/mpeg"))).toBe("Audio")
    expect(formatCategory(format("mp4", "video/mp4"))).toBe("Video")
    expect(formatCategory(format("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"))).toBe(
      "Spreadsheets",
    )
  })

  test("fuzzy search supports substrings and ordered initials", () => {
    expect(matchesQuery("Documents & publishing", "publish")).toBe(true)
    expect(matchesQuery("Portable Document Format", "pdf")).toBe(true)
    expect(matchesQuery("Portable Document Format", "xyz")).toBe(false)
  })
})
