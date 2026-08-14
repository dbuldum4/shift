export type Hint = {
  key: string
  label: string
}

export const WIDE_BREAKPOINT = 96
export const HELP_BREAKPOINT = 70
export const COMMANDS_BREAKPOINT = 54
export const STACK_ACTIONS_BREAKPOINT = 48
export const QUEUE_URL_BREAKPOINT = 48
export const QUEUE_BADGE_BREAKPOINT = 64
export const QUEUE_REMOVE_BREAKPOINT = 72
export const SHORTCUT_BREAKPOINT = 42
export const EMPTY_SHORTCUT_BREAKPOINT = 80

/** Padding, header, activity, footer, and the three gaps around those chrome rows. */
const PADDING_Y = 2
const HEADER_HEIGHT = 1
const FOOTER_HEIGHT = 1
const MAIN_GAPS = 3

/** Gap between the stacked Inputs and Conversion panes. */
export const STACK_GAP = 1
export const MIN_QUEUE_HEIGHT = 6
export const MIN_STACKED_SETTINGS_HEIGHT = 5
export const STACKED_SETTINGS_HEIGHT = 9

export function hintText(hint: Hint) {
  return `${hint.key} ${hint.label}`
}

export function hintsWidth(hints: Hint[], gap = 2) {
  if (hints.length === 0) return 0
  return hints.reduce((sum, hint) => sum + hintText(hint).length, 0) + gap * (hints.length - 1)
}

export function fitHints(hints: Hint[], width: number, gap = 2) {
  const fitted: Hint[] = []
  let used = 0
  for (const hint of hints) {
    const size = hintText(hint).length
    const next = used + (fitted.length > 0 ? gap : 0) + size
    if (next > width) break
    fitted.push(hint)
    used = next
  }
  return fitted
}

export function homePath(path: string, home = process.env.HOME) {
  if (!home) return path
  if (path === home) return "~"
  const separator = path.includes("\\") && !path.includes("/") ? "\\" : "/"
  const prefix = home.endsWith("/") || home.endsWith("\\") ? home : `${home}${separator}`
  if (path.startsWith(prefix)) return `~${separator}${path.slice(prefix.length)}`
  return path
}

export function truncateStart(value: string, width: number) {
  if (width <= 0) return ""
  if (value.length <= width) return value
  if (width === 1) return "…"
  return `…${value.slice(value.length - (width - 1))}`
}

export function truncateEnd(value: string, width: number) {
  if (width <= 0) return ""
  if (value.length <= width) return value
  if (width === 1) return "…"
  return `${value.slice(0, width - 1)}…`
}

export function activityHeight(terminalHeight: number) {
  return terminalHeight >= 28 ? 2 : 1
}

export function chromeHeight(terminalHeight: number) {
  return PADDING_Y + HEADER_HEIGHT + MAIN_GAPS + activityHeight(terminalHeight) + FOOTER_HEIGHT
}

export function stackedSettingsHeight(terminalHeight: number) {
  const leftover = terminalHeight - chromeHeight(terminalHeight) - STACK_GAP
  const preferred = leftover - MIN_QUEUE_HEIGHT
  if (preferred >= MIN_STACKED_SETTINGS_HEIGHT) return Math.min(STACKED_SETTINGS_HEIGHT, preferred)
  return Math.max(4, leftover - 4)
}

export function paletteLabelWidth(terminalWidth: number) {
  if (terminalWidth < 48) return 14
  if (terminalWidth < 64) return 18
  return 25
}

export function helpKeyWidth(terminalWidth: number) {
  if (terminalWidth < 40) return 10
  if (terminalWidth < 56) return 14
  return 22
}

export function footerContents(
  hints: Hint[],
  cwd: string,
  terminalWidth: number,
  home = process.env.HOME,
): { hints: Hint[]; path: string } {
  const width = Math.max(0, terminalWidth - 4)
  const path = homePath(cwd, home)
  const fitted = fitHints(hints, width)
  const remaining = Math.max(0, width - hintsWidth(fitted) - (fitted.length > 0 && path ? 2 : 0))
  return { hints: fitted, path: truncateStart(path, remaining) }
}
