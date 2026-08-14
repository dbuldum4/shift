import { readdir } from "node:fs/promises"
import { basename, dirname, join, normalize } from "node:path"
import { createEffect, createMemo, createSignal, For, onMount, Show } from "solid-js"
import type { InputRenderable, ScrollBoxRenderable } from "@opentui/core"
import { TextAttributes } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { truncateStart } from "../layout"
import { matchesQuery } from "../model"
import { theme } from "../theme"
import { KeyHint, Modal } from "./modal"

type PickerEntry = {
  name: string
  path: string
  directory: boolean
}

export function FilePicker(props: {
  mode: "files" | "directory"
  title?: string
  initialPath?: string
  onCancel: () => void
  onSelect: (paths: string[]) => void
}) {
  const dimensions = useTerminalDimensions()
  const [directory, setDirectory] = createSignal(normalize(props.initialPath || process.cwd()))
  const [entries, setEntries] = createSignal<PickerEntry[]>([])
  const [query, setQuery] = createSignal("")
  const [active, setActive] = createSignal(0)
  const [selected, setSelected] = createSignal(new Set<string>())
  const [showHidden, setShowHidden] = createSignal(false)
  const [loading, setLoading] = createSignal(true)
  const [error, setError] = createSignal<string>()
  let search: InputRenderable | undefined
  let scroll: ScrollBoxRenderable | undefined

  const visible = createMemo(() => {
    const filtered = entries().filter((entry) => matchesQuery(entry.name, query()))
    return [{ name: "..", path: dirname(directory()), directory: true }, ...filtered]
  })

  createEffect(() => {
    directory()
    showHidden()
    void load()
  })

  createEffect(() => {
    query()
    setActive(0)
  })

  onMount(() => setTimeout(() => search?.focus(), 1))

  async function load() {
    setLoading(true)
    setError(undefined)
    try {
      const result = await readdir(directory(), { withFileTypes: true })
      setEntries(
        result
          .filter((entry) => showHidden() || !entry.name.startsWith("."))
          .filter((entry) => entry.isDirectory() || props.mode === "files")
          .map((entry) => ({
            name: entry.name,
            path: join(directory(), entry.name),
            directory: entry.isDirectory(),
          }))
          .sort((left, right) =>
            left.directory === right.directory
              ? left.name.localeCompare(right.name, undefined, { numeric: true })
              : left.directory
                ? -1
                : 1,
          ),
      )
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
      setEntries([])
    } finally {
      setLoading(false)
    }
  }

  function move(delta: number) {
    if (visible().length === 0) return
    setActive((current) => (current + delta + visible().length) % visible().length)
    const target = scroll?.getChildren()[active()]
    if (!target || !scroll) return
    const y = target.y - scroll.y
    if (y >= scroll.height) scroll.scrollBy(y - scroll.height + 1)
    if (y < 0) scroll.scrollBy(y)
  }

  function activate(entry = visible()[active()]) {
    if (!entry) return
    if (entry.directory) {
      setDirectory(entry.path)
      setQuery("")
      setActive(0)
      return
    }
    toggle(entry.path)
  }

  function toggle(path: string) {
    const next = new Set(selected())
    if (next.has(path)) next.delete(path)
    else next.add(path)
    setSelected(next)
  }

  function submit() {
    if (props.mode === "directory") return props.onSelect([directory()])
    const paths = [...selected()]
    if (paths.length === 0) {
      const entry = visible()[active()]
      if (entry && !entry.directory) paths.push(entry.path)
    }
    if (paths.length > 0) props.onSelect(paths)
  }

  useKeyboard((event) => {
    if (event.name === "escape") return props.onCancel()
    if (event.name === "up" || (event.name === "k" && event.ctrl)) return move(-1)
    if (event.name === "down" || (event.name === "j" && event.ctrl)) return move(1)
    if (event.name === "pageup") return move(-10)
    if (event.name === "pagedown") return move(10)
    if (event.name === "backspace" && !query()) return setDirectory(dirname(directory()))
    if (event.ctrl && event.name === "h") return setShowHidden((value) => !value)
    if (event.ctrl && event.name === "return") return submit()
    if (event.name === "space") {
      const entry = visible()[active()]
      if (props.mode === "files" && entry && !entry.directory) toggle(entry.path)
      return
    }
    if (event.name === "return") return activate()
  })

  return (
    <Modal
      title={props.title ?? (props.mode === "files" ? "Choose input files" : "Choose folder")}
      onClose={props.onCancel}
      width={86}
      footer={
        <box
          paddingLeft={2}
          paddingRight={2}
          paddingBottom={1}
          flexDirection={dimensions().width < 64 ? "column" : "row"}
          justifyContent="space-between"
          gap={1}
          overflow="hidden"
        >
          <box flexDirection="row" gap={2} flexWrap="wrap" overflow="hidden">
            <KeyHint key="j/k" label="navigate" />
            <KeyHint
              key={props.mode === "files" ? "space" : "enter"}
              label={props.mode === "files" ? "mark" : "open"}
            />
            <Show when={dimensions().width >= 56}>
              <KeyHint key="ctrl+h" label={showHidden() ? "hide hidden" : "show hidden"} />
            </Show>
          </box>
          <box backgroundColor={theme.primary} paddingLeft={1} paddingRight={1} onMouseUp={() => submit()}>
            <text fg={theme.onPrimary} attributes={TextAttributes.BOLD}>
              {props.mode === "files" ? `Add ${selected().size || 1}` : "Choose folder"} ctrl+enter
            </text>
          </box>
        </box>
      }
    >
      <box paddingLeft={2} paddingRight={2} paddingTop={1} gap={1} flexGrow={1} minHeight={0} overflow="hidden">
        <text fg={theme.accent} wrapMode="none">
          {truncateStart(directory(), Math.max(8, dimensions().width - 8))}
        </text>
        <input
          ref={(value: InputRenderable) => (search = value)}
          placeholder="Filter this folder"
          placeholderColor={theme.subtle}
          textColor={theme.text}
          focusedTextColor={theme.text}
          backgroundColor={theme.elevated}
          focusedBackgroundColor={theme.elevated}
          cursorColor={theme.primary}
          onInput={(value) => setQuery(value)}
        />
        <Show when={!loading()} fallback={<text fg={theme.muted}>Reading folder…</text>}>
          <Show when={!error()} fallback={<text fg={theme.danger}>{error()}</text>}>
            <scrollbox
              ref={(value: ScrollBoxRenderable) => (scroll = value)}
              flexGrow={1}
              minHeight={0}
              scrollX={false}
              horizontalScrollbarOptions={{ visible: false }}
            >
              <For each={visible()}>
                {(entry, index) => {
                  const isActive = () => active() === index()
                  const isSelected = () => selected().has(entry.path)
                  return (
                    <box
                      height={1}
                      flexDirection="row"
                      paddingLeft={1}
                      paddingRight={1}
                      minWidth={0}
                      overflow="hidden"
                      backgroundColor={isActive() ? theme.primary : theme.transparent}
                      onMouseOver={() => setActive(index())}
                      onMouseDown={() => setActive(index())}
                      onMouseUp={() => activate(entry)}
                    >
                      <text width={3} fg={isActive() ? theme.onPrimary : theme.accent}>
                        {entry.directory ? "▸" : isSelected() ? "●" : "○"}
                      </text>
                      <text flexGrow={1} minWidth={0} fg={isActive() ? theme.onPrimary : theme.text} wrapMode="none">
                        {entry.name}
                      </text>
                    </box>
                  )
                }}
              </For>
            </scrollbox>
          </Show>
        </Show>
      </box>
    </Modal>
  )
}
