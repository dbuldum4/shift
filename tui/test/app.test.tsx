import { afterAll, beforeAll, expect, test } from "bun:test"
import { chmod, mkdtemp, rm } from "node:fs/promises"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { testRender } from "@opentui/solid"
import { App } from "../src/app"

let directory = ""
let engine = ""
let previous: string | undefined

beforeAll(async () => {
  directory = await mkdtemp(join(tmpdir(), "shift-tui-test-"))
  engine = join(directory, "shift-cli")
  const capabilities = JSON.stringify({
    schemaVersion: 1,
    cliVersion: "1.2.0-test",
    platform: { os: "linux", arch: "x64" },
    inputCount: 0,
    suggestedFormat: "markdown",
    formats: [
      { id: "markdown", label: "Markdown", extension: "md", mediaType: "text/markdown" },
      { id: "pdf", label: "PDF", extension: "pdf", mediaType: "application/pdf" },
    ],
    modules: [{ id: "pandoc", label: "Pandoc", inputs: ["md"], outputs: ["pdf"], supportsUrl: false }],
  })
  await Bun.write(engine, `#!/bin/sh\nprintf '%s\\n' '${capabilities}'\n`)
  await chmod(engine, 0o755)
  previous = process.env.SHIFT_CLI_ENGINE
  process.env.SHIFT_CLI_ENGINE = engine
})

afterAll(async () => {
  if (previous === undefined) delete process.env.SHIFT_CLI_ENGINE
  else process.env.SHIFT_CLI_ENGINE = previous
  await rm(directory, { recursive: true, force: true })
})

test("renders a complete wide-screen application shell", async () => {
  const app = await testRender(() => <App />, { width: 120, height: 34 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    expect(frame).toContain("SHIFT")
    expect(frame).toContain("Inputs")
    expect(frame).toContain("Conversion")
    expect(frame).toContain("Add files")
    expect(frame).toContain("Run conversion")
  } finally {
    app.renderer.destroy()
  }
})

test("reflows into a usable narrow terminal", async () => {
  const app = await testRender(() => <App />, { width: 74, height: 30 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    expect(frame).toContain("SHIFT")
    expect(frame).toContain("Inputs")
    expect(frame).toContain("Conversion")
    expect(frame).toContain("ctrl+k")
  } finally {
    app.renderer.destroy()
  }
})
