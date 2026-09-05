import { dirname, join } from "node:path"
import { existsSync } from "node:fs"
import type { QueueItem, ConversionSettings, CapabilityResponse } from "./model.ts"
import { buildConversionArgs, parseCapabilities } from "./model.ts"

export type ConversionCallbacks = {
  onOutput?: (path: string) => void
  onProgress?: (line: string) => void
}

export function resolveEngine(environment = process.env, executable = process.execPath) {
  if (environment.SHIFT_CLI_ENGINE) return environment.SHIFT_CLI_ENGINE
  const sibling = join(dirname(executable), process.platform === "win32" ? "shift-cli.exe" : "shift-cli")
  if (existsSync(sibling)) return sibling
  return process.platform === "win32" ? "shift-cli.exe" : "shift-cli"
}

export function capabilityArgs(inputs: string[]) {
  return ["--headless", "formats", "--json", ...inputs.flatMap((input) => ["--input", input])]
}

export function queryCapabilities(inputs: string[], engine = resolveEngine()): CapabilityResponse {
  const processResult = Bun.spawnSync([engine, ...capabilityArgs(inputs)], {
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  })
  if (processResult.exitCode !== 0) {
    const detail = decoder.decode(processResult.stderr).trim()
    throw new Error(detail || `Could not read Shift capabilities (exit ${processResult.exitCode})`)
  }
  return parseCapabilities(decoder.decode(processResult.stdout))
}

export function queryDoctor(engine = resolveEngine()) {
  const processResult = Bun.spawnSync([engine, "--headless", "doctor", "--script"], {
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  })
  const text = decoder.decode(processResult.stdout)
  const values = Object.fromEntries(
    text
      .split(/\r?\n/)
      .map((line) => line.split("=", 2))
      .filter((parts): parts is [string, string] => parts.length === 2 && Boolean(parts[0])),
  )
  return { exitCode: processResult.exitCode, values, raw: text }
}

export async function runConversion(
  items: QueueItem[],
  settings: ConversionSettings,
  callbacks: ConversionCallbacks = {},
  signal?: AbortSignal,
  engine = resolveEngine(),
) {
  const args = buildConversionArgs(items, settings)
  const child = Bun.spawn([engine, ...args], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, SHIFT_HEADLESS: "1" },
    signal,
  })

  const outputs: string[] = []
  const stdout = consumeLines(child.stdout, (line) => {
    const output = parseOutputLine(line)
    if (!output) return
    outputs.push(output)
    callbacks.onOutput?.(output)
  })
  const stderr = consumeLines(child.stderr, (line) => callbacks.onProgress?.(cleanProgressLine(line)))
  const exitCode = await child.exited
  await Promise.all([stdout, stderr])
  return { exitCode, outputs, args }
}

export function parseOutputLine(line: string) {
  const trimmed = line.trim()
  if (!trimmed) return undefined
  try {
    const value: unknown = JSON.parse(trimmed)
    return typeof value === "string" ? value : undefined
  } catch {
    return undefined
  }
}

export function cleanProgressLine(line: string) {
  return line
    .replace(/\x1B\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/^shift-cli:\s*/, "")
    .trim()
}

export function openPath(path: string) {
  const command =
    process.platform === "darwin"
      ? ["open", path]
      : process.platform === "win32"
        ? ["explorer.exe", path]
        : ["xdg-open", path]
  const child = Bun.spawn(command, { stdin: "ignore", stdout: "ignore", stderr: "ignore" })
  child.unref()
}

async function consumeLines(stream: ReadableStream<Uint8Array>, onLine: (line: string) => void) {
  const reader = stream.getReader()
  const streamDecoder = new TextDecoder()
  let pending = ""
  while (true) {
    const { done, value } = await reader.read()
    if (done) break
    pending += streamDecoder.decode(value, { stream: true })
    const lines = pending.split(/[\r\n]+/)
    pending = lines.pop() ?? ""
    for (const line of lines) if (line) onLine(line)
  }
  pending += streamDecoder.decode()
  if (pending) onLine(pending)
}

const decoder = new TextDecoder()
