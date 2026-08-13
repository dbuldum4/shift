import assert from "node:assert/strict"
import { describe, test } from "node:test"
import { capabilityArgs, cleanProgressLine, parseOutputLine, resolveEngine } from "../src/engine.ts"
import {
  buildConversionArgs,
  createQueueItem,
  dedupeSources,
  formatCategory,
  matchesQuery,
  parseCapabilities,
  type FormatCapability,
} from "../src/model.ts"

describe("Shift TUI pure contracts under the system Node runtime", () => {
  test("builds a safe argv array for paths with spaces and URL punctuation", () => {
    const items = [createQueueItem("A report.pdf", "1"), createQueueItem("https://example.com/a?b=c", "2")]
    const args = buildConversionArgs(items, {
      formats: ["markdown", "html"],
      outputDir: "Output folder",
      force: true,
      recursive: false,
    })
    assert.deepEqual(args.slice(0, 5), ["--headless", "batch", "A report.pdf", "https://example.com/a?b=c", "--to"])
    assert.ok(args.includes("--yes"))
    assert.ok(args.includes("--also-to"))
    assert.ok(args.includes("Output folder"))
  })

  test("keeps the explicit engine injection cross-platform", () => {
    assert.equal(
      resolveEngine({ SHIFT_CLI_ENGINE: "C:\\Shift\\shift-cli.exe" }, "/ignored/shift-tui"),
      "C:\\Shift\\shift-cli.exe",
    )
  })

  test("serializes repeated capability inputs without a shell", () => {
    assert.deepEqual(capabilityArgs(["a.pdf", "b.docx"]), [
      "--headless",
      "formats",
      "--json",
      "--input",
      "a.pdf",
      "--input",
      "b.docx",
    ])
  })

  test("parses the versioned capability schema", () => {
    const response = parseCapabilities(
      JSON.stringify({
        schemaVersion: 1,
        cliVersion: "1.2.0",
        platform: { os: "linux", arch: "x64" },
        inputCount: 1,
        suggestedFormat: "markdown",
        formats: [{ id: "markdown", label: "Markdown", extension: "md", mediaType: "text/markdown" }],
        modules: [],
      }),
    )
    assert.equal(response.formats[0].id, "markdown")
    assert.throws(() => parseCapabilities("{}"), /unsupported capability schema/)
  })

  test("normalizes output and progress records", () => {
    assert.equal(parseOutputLine('"/tmp/a.md"'), "/tmp/a.md")
    assert.equal(parseOutputLine("not json"), undefined)
    assert.equal(cleanProgressLine("\u001b[32mshift-cli: Writing output\u001b[0m"), "Writing output")
  })

  test("deduplicates and classifies sources", () => {
    assert.deepEqual(dedupeSources(["a", "b", "a"]), ["a", "b"])
    assert.equal(createQueueItem("https://example.com", "1").kind, "url")
    assert.equal(createQueueItem("report.docx", "2").kind, "file")
  })

  test("classifies format families and fuzzy matches labels", () => {
    const format = (id: string, mediaType: string) => ({ id, label: id, extension: id, mediaType }) as FormatCapability
    assert.equal(formatCategory(format("mp3", "audio/mpeg")), "Audio")
    assert.equal(formatCategory(format("xlsx", "application/vnd.ms-excel")), "Spreadsheets")
    assert.equal(matchesQuery("Portable Document Format", "pdf"), true)
    assert.equal(matchesQuery("Portable Document Format", "xyz"), false)
  })

  test("rejects empty queue and output selections", () => {
    assert.throws(() => buildConversionArgs([], { formats: ["pdf"], force: false, recursive: false }), /at least one/)
    assert.throws(
      () => buildConversionArgs([createQueueItem("a.md", "1")], { formats: [], force: false, recursive: false }),
      /output format/,
    )
  })
})
