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
    expect(frame).toContain("enter add")
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

async function renderEmptyApp() {
  const app = await testRender(() => <App />, { width: 120, height: 34 })
  await app.waitForFrame((frame) => frame.includes("What should Shift convert?"))
  return app
}

test("moves the empty-state add cursor and activates with enter", async () => {
  const files = await renderEmptyApp()
  try {
    files.mockInput.pressEnter()
    await files.waitForFrame((frame) => frame.includes("Choose input files"))
  } finally {
    files.renderer.destroy()
  }

  const folder = await renderEmptyApp()
  try {
    folder.mockInput.pressArrow("right")
    folder.mockInput.pressEnter()
    await folder.waitForFrame((frame) => frame.includes("Choose input folder"))
  } finally {
    folder.renderer.destroy()
  }

  const url = await renderEmptyApp()
  try {
    url.mockInput.pressArrow("right")
    url.mockInput.pressArrow("right")
    url.mockInput.pressEnter()
    await url.waitForFrame((frame) => frame.includes("Public web page or direct file URL"))
  } finally {
    url.renderer.destroy()
  }

  const wrap = await renderEmptyApp()
  try {
    wrap.mockInput.pressArrow("left")
    wrap.mockInput.pressEnter()
    await wrap.waitForFrame((frame) => frame.includes("Public web page or direct file URL"))
  } finally {
    wrap.renderer.destroy()
  }
})
