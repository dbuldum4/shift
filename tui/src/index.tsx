import { createCliRenderer } from "@opentui/core"
import { render } from "@opentui/solid"
import { App } from "./app"
import { resolveEngine } from "./engine"

if (process.argv.length > 2 || !process.stdin.isTTY || !process.stdout.isTTY) {
  await runHeadless()
} else {
  await runTui()
}

async function runTui() {
  process.title = "Shift"
  const renderer = await createCliRenderer({
    targetFps: 60,
    exitOnCtrlC: false,
    useMouse: process.env.SHIFT_DISABLE_MOUSE !== "1",
    autoFocus: false,
    openConsoleOnError: false,
    externalOutputMode: "passthrough",
  })

  const shutdown = () => {
    if (!renderer.isDestroyed) renderer.destroy()
  }
  process.once("SIGHUP", shutdown)
  process.once("SIGTERM", shutdown)
  renderer.once("destroy", () => {
    process.off("SIGHUP", shutdown)
    process.off("SIGTERM", shutdown)
  })
  const destroyed = new Promise<void>((resolve) => renderer.once("destroy", resolve))

  try {
    await render(() => <App />, renderer)
    await destroyed
  } finally {
    shutdown()
  }
}

async function runHeadless() {
  const engine = resolveEngine()
  const args = process.argv.slice(2)
  if (!args.includes("--headless")) args.unshift("--headless")
  const child = Bun.spawn([engine, ...args], {
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
    env: { ...process.env, SHIFT_HEADLESS: "1" },
  })
  process.exitCode = await child.exited
}
