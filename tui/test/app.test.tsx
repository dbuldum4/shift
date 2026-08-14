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

function assertReadableShell(frame: string) {
  expect(frame).toContain("SHIFT")
  expect(frame).toContain("Inputs")
  expect(frame).toContain("Conversion")
  expect(frame).toMatch(/Ready|input added|Converting|Cancelling|Done|Conversion exited/)
  expect(frame).not.toContain("Besideaeach")
  expect(frame).not.toMatch(/files\//)
  expect(frame).not.toMatch(/addafile/)
  expect(frame).not.toMatch(/enteradd/)
  expect(frame).not.toContain("Run convers/")
  expect(frame).not.toMatch(/to\/arURL/)
  const footer =
    frame
      .split("\n")
      .map((line) => line.trimEnd())
      .filter((line) => line.trim())
      .at(-1) ?? ""
  expect(footer).not.toContain("Run conversion")
  expect(footer).not.toContain("Overwrite")
}

test("renders a complete wide-screen application shell", async () => {
  const app = await testRender(() => <App />, { width: 120, height: 34 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    assertReadableShell(frame)
    expect(frame).toContain("Add files")
    expect(frame).toContain("enter add")
    expect(frame).toContain("Run conversion")
    expect(frame).toContain("Beside each source")
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
    assertReadableShell(frame)
    expect(frame).toContain("ctrl+k")
    expect(frame).toContain("enter add")
    expect(frame).toContain("Run conversion")
    expect(frame).toContain("Beside each source")
  } finally {
    app.renderer.destroy()
  }
})

test("keeps a short stacked terminal readable", async () => {
  const app = await testRender(() => <App />, { width: 80, height: 24 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    assertReadableShell(frame)
    expect(frame).toContain("What should Shift convert?")
    expect(frame).toContain("enter add")
    expect(frame).toContain("Run conversion")
    expect(frame).toContain("Beside each source")
    expect(frame).toContain("Expand folders")
  } finally {
    app.renderer.destroy()
  }
})

test("resizes from a wide shell to a stacked one without collisions", async () => {
  const app = await testRender(() => <App />, { width: 120, height: 34 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    app.resize(60, 28)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    assertReadableShell(frame)
    expect(frame).toContain("Add files")
    expect(frame).toContain("arrows move")
  } finally {
    app.renderer.destroy()
  }
})

test("keeps a 50x20 terminal readable without overlapping chrome", async () => {
  const app = await testRender(() => <App />, { width: 50, height: 20 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    const frame = app.captureCharFrame()
    assertReadableShell(frame)
    expect(frame).toContain("What should Shift convert?")
    expect(frame).toContain("Add files")
    expect(frame).toContain("Add folder")
    expect(frame).toContain("Add URL")
    expect(frame).toContain("Run conversion")
    expect(frame).toContain("arrows move")
    const footer =
      frame
        .split("\n")
        .map((line) => line.trimEnd())
        .filter((line) => line.trim())
        .at(-1) ?? ""
    expect(footer).toContain("arrows move")
    expect(footer).not.toMatch(/╭|╰|│/)
  } finally {
    app.renderer.destroy()
  }
})

test("keeps the command palette usable on a short terminal", async () => {
  const app = await testRender(() => <App />, { width: 50, height: 20 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    app.mockInput.pressKey("k", { ctrl: true })
    const frame = await app.waitForFrame((value) => value.includes("Command palette"))
    expect(frame).toContain("Add files")
    expect(frame).toContain("Type an action")
    expect(frame).not.toMatch(/Add filesBrowse/)
    expect(frame).not.toMatch(/Add filesAdd folder/)
    expect(frame).not.toMatch(/Choose destinationWrite/)
  } finally {
    app.renderer.destroy()
  }
})

test("keeps the help overlay readable on a short terminal", async () => {
  const app = await testRender(() => <App />, { width: 50, height: 20 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    app.mockInput.pressKey("?")
    const frame = await app.waitForFrame((value) => value.includes("Keyboard"))
    expect(frame).toContain("ctrl+p")
    expect(frame).toContain("Add input files")
    expect(frame).not.toMatch(/ctrl\+p \/ aAdd/)
  } finally {
    app.renderer.destroy()
  }
})

test("truncates a queued source instead of colliding on a narrow stack", async () => {
  const app = await testRender(() => <App />, { width: 50, height: 20 })
  try {
    await Bun.sleep(20)
    await app.renderOnce()
    app.mockInput.pressArrow("right")
    app.mockInput.pressArrow("right")
    app.mockInput.pressEnter()
    await app.waitForFrame((frame) => frame.includes("Public web page"))
    await Bun.sleep(20)
    await app.mockInput.typeText("https://shift.test/very/long/path/to/article")
    app.mockInput.pressEnter()
    const frame = await app.waitForFrame((value) => value.includes("1 queued") && value.includes("shift.test"))
    assertReadableShell(frame)
    expect(frame).toContain("Run conversion")
    expect(frame).not.toContain("https://shift.test/very/long/path/to/article")
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
