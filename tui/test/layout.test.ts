import { expect, test } from "bun:test"
import {
  activityHeight,
  chromeHeight,
  fitHints,
  footerContents,
  helpKeyWidth,
  homePath,
  MIN_QUEUE_HEIGHT,
  paletteLabelWidth,
  STACK_GAP,
  stackedSettingsHeight,
  truncateEnd,
  truncateStart,
} from "../src/layout"

const emptyHints = [
  { key: "arrows", label: "move" },
  { key: "enter", label: "add" },
  { key: "a", label: "files" },
]

test("shortens a home-relative working directory", () => {
  expect(homePath("/Users/ada/Documents/shift", "/Users/ada")).toBe("~/Documents/shift")
  expect(homePath("/Users/ada", "/Users/ada")).toBe("~")
  expect(homePath("C:\\Users\\ada\\src", "C:\\Users\\ada")).toBe("~\\src")
  expect(homePath("/tmp/work", "/Users/ada")).toBe("/tmp/work")
})

test("truncates long strings from either end", () => {
  expect(truncateStart("/Users/ada/Documents/shift/tui", 14)).toBe("…nts/shift/tui")
  expect(truncateEnd("Beside each source", 10)).toBe("Beside ea…")
  expect(truncateStart("short", 10)).toBe("short")
  expect(truncateEnd("", 4)).toBe("")
})

test("keeps only the footer hints that fit next to a shortened path", () => {
  const wide = footerContents(emptyHints, "/Users/ada/Documents/shift/tui", 80, "/Users/ada")
  expect(wide.hints).toEqual(emptyHints)
  expect(wide.path).toBe("~/Documents/shift/tui")

  const tight = footerContents(emptyHints, "/Users/ada/Documents/shift/tui", 42, "/Users/ada")
  expect(tight.hints).toEqual(emptyHints)
  expect(tight.path.startsWith("…")).toBe(true)
  expect(tight.path.endsWith("tui")).toBe(true)

  const mid = footerContents(emptyHints, "/Users/ada/Documents/shift/tui", 28, "/Users/ada")
  expect(mid.hints.map((hint) => hint.label)).toEqual(["move", "add"])

  const cramped = footerContents(emptyHints, "/Users/ada/Documents/shift/tui", 20, "/Users/ada")
  expect(cramped.hints.map((hint) => hint.label)).toEqual(["move"])
})

test("drops overflowing hints instead of painting them on top of each other", () => {
  expect(fitHints(emptyHints, 11).map((hint) => hint.key)).toEqual(["arrows"])
  expect(fitHints(emptyHints, 22).map((hint) => hint.key)).toEqual(["arrows", "enter"])
  expect(fitHints(emptyHints, 4)).toEqual([])
})

test("leaves room for a usable queue when the conversion pane stacks", () => {
  expect(activityHeight(24)).toBe(1)
  expect(activityHeight(34)).toBe(2)
  expect(stackedSettingsHeight(34)).toBe(9)
  expect(stackedSettingsHeight(24)).toBe(9)
  expect(stackedSettingsHeight(20)).toBe(5)
  expect(chromeHeight(24) + STACK_GAP + stackedSettingsHeight(24) + MIN_QUEUE_HEIGHT).toBeLessThanOrEqual(24)
  expect(chromeHeight(20) + STACK_GAP + stackedSettingsHeight(20) + MIN_QUEUE_HEIGHT).toBeLessThanOrEqual(20)
  expect(chromeHeight(18) + STACK_GAP + stackedSettingsHeight(18) + 4).toBeLessThanOrEqual(18)
})

test("narrows palette and help columns before they collide", () => {
  expect(paletteLabelWidth(120)).toBe(25)
  expect(paletteLabelWidth(50)).toBe(18)
  expect(paletteLabelWidth(40)).toBe(14)
  expect(helpKeyWidth(80)).toBe(22)
  expect(helpKeyWidth(50)).toBe(14)
  expect(helpKeyWidth(36)).toBe(10)
})
