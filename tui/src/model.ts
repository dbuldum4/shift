import { basename, extname } from "node:path"

export type ItemState = "queued" | "running" | "succeeded" | "failed" | "cancelled"

export type QueueItem = {
  id: string
  source: string
  name: string
  kind: "file" | "url"
  state: ItemState
  detail?: string
  artifacts: string[]
}

export type FormatCapability = {
  id: string
  label: string
  extension: string
  mediaType: string
}

export type ModuleCapability = {
  id: string
  label: string
  inputs: string[]
  outputs: string[]
  supportsUrl: boolean
}

export type CapabilityResponse = {
  schemaVersion: 1
  cliVersion: string
  platform: { os: string; arch: string }
  inputCount: number
  suggestedFormat: string | null
  formats: FormatCapability[]
  modules: ModuleCapability[]
}

export type ConversionSettings = {
  formats: string[]
  outputDir?: string
  namingTemplate?: string
  force: boolean
  recursive: boolean
  preferredModule?: string
}

export function isUrl(value: string) {
  return /^https?:\/\//i.test(value.trim())
}

export function createQueueItem(source: string, id: string = crypto.randomUUID()): QueueItem {
  const trimmed = source.trim()
  const url = isUrl(trimmed)
  return {
    id,
    source: trimmed,
    name: url ? displayUrl(trimmed) : basename(trimmed) || trimmed,
    kind: url ? "url" : "file",
    state: "queued",
    artifacts: [],
  }
}

export function dedupeSources(sources: string[]) {
  const seen = new Set<string>()
  return sources.filter((source) => {
    const key = process.platform === "win32" ? source.toLocaleLowerCase() : source
    if (seen.has(key)) return false
    seen.add(key)
    return true
  })
}

export function buildConversionArgs(items: QueueItem[], settings: ConversionSettings) {
  if (items.length === 0) throw new Error("Select at least one file or URL")
  if (settings.formats.length === 0) throw new Error("Select at least one output format")

  const args = ["--headless"]
  if (items.length > 1) args.push("batch")
  args.push(...items.map((item) => item.source))
  args.push("--to", settings.formats[0])
  for (const format of settings.formats.slice(1)) args.push("--also-to", format)
  if (settings.outputDir) args.push("--output-dir", settings.outputDir)
  if (settings.namingTemplate) args.push("--name-template", settings.namingTemplate)
  if (settings.preferredModule) args.push("--module", settings.preferredModule)
  if (settings.force) args.push("--force")
  if (settings.recursive) args.push("--recursive")
  if (items.some((item) => item.kind === "url")) args.push("--yes")
  args.push("--progress", "--print-json")
  return args
}

export function parseCapabilities(raw: string): CapabilityResponse {
  const value: unknown = JSON.parse(raw)
  if (!value || typeof value !== "object") throw new Error("Shift returned an invalid capability response")
  const response = value as Partial<CapabilityResponse>
  if (response.schemaVersion !== 1 || !Array.isArray(response.formats) || !Array.isArray(response.modules)) {
    throw new Error("Shift returned an unsupported capability schema")
  }
  for (const format of response.formats) {
    if (!format || typeof format.id !== "string" || typeof format.label !== "string") {
      throw new Error("Shift returned an invalid format entry")
    }
  }
  return response as CapabilityResponse
}

export function formatCategory(format: FormatCapability) {
  if (/^(audio|video|image)\//.test(format.mediaType)) {
    return format.mediaType.startsWith("audio/") ? "Audio" : format.mediaType.startsWith("video/") ? "Video" : "Images"
  }
  if (["srt", "vtt"].includes(format.id)) return "Subtitles"
  if (["csv", "tsv", "xlsx"].includes(format.id)) return "Spreadsheets"
  if (["pdf-pages-zip", "png-sequence-zip"].includes(format.id)) return "Archives"
  return "Documents & publishing"
}

export function formatDescription(format: FormatCapability) {
  const extension = format.extension.toUpperCase()
  if (format.id === "transcript") return "Local speech transcription"
  if (format.id === "pdf-pages-zip") return "Split PDF pages as a ZIP"
  if (format.id === "png-sequence-zip") return "Video frames as a ZIP"
  return `${extension} · ${format.mediaType}`
}

export function sourceBadge(item: QueueItem) {
  if (item.kind === "url") return "URL"
  return extname(item.source).slice(1).toUpperCase() || "FILE"
}

export function statusGlyph(state: ItemState) {
  return {
    queued: "○",
    running: "◉",
    succeeded: "✓",
    failed: "×",
    cancelled: "–",
  }[state]
}

export function matchesQuery(value: string, query: string) {
  const needle = query.trim().toLocaleLowerCase()
  if (!needle) return true
  const haystack = value.toLocaleLowerCase()
  if (haystack.includes(needle)) return true
  let cursor = 0
  for (const character of haystack) {
    if (character === needle[cursor]) cursor += 1
    if (cursor === needle.length) return true
  }
  return false
}

function displayUrl(value: string) {
  try {
    const url = new URL(value)
    const path = url.pathname === "/" ? "" : url.pathname
    return `${url.hostname}${path}`
  } catch {
    return value
  }
}
