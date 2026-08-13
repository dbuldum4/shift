import { describe, expect, test } from "bun:test"
import { capabilityArgs, cleanProgressLine, parseOutputLine, resolveEngine } from "../src/engine"

describe("engine adapter", () => {
  test("uses an explicitly injected engine before sibling discovery", () => {
    expect(resolveEngine({ SHIFT_CLI_ENGINE: "/opt/shift/engine" }, "/opt/shift/shift-tui")).toBe("/opt/shift/engine")
  })

  test("builds repeated input capability arguments without shell quoting", () => {
    expect(capabilityArgs(["A report.pdf", "https://example.com/a?b=c"])).toEqual([
      "--headless",
      "formats",
      "--json",
      "--input",
      "A report.pdf",
      "--input",
      "https://example.com/a?b=c",
    ])
  })

  test("parses only JSON string output records", () => {
    expect(parseOutputLine('"/tmp/report.md"')).toBe("/tmp/report.md")
    expect(parseOutputLine('{"path":"/tmp/report.md"}')).toBeUndefined()
    expect(parseOutputLine("diagnostic noise")).toBeUndefined()
  })

  test("normalizes carriage-return progress and ANSI styling", () => {
    expect(cleanProgressLine("\u001b[32mshift-cli: Converting (50%)\u001b[0m")).toBe("Converting (50%)")
  })
})
