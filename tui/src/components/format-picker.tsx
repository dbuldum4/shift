import { createMemo, createSignal, For, onMount, Show } from "solid-js"
import { TextAttributes, type InputRenderable, type ScrollBoxRenderable } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import type { FormatCapability } from "../model"
import { formatCategory, formatDescription, matchesQuery } from "../model"
import { theme } from "../theme"
import { KeyHint, Modal } from "./modal"

export function FormatPicker(props: {
  formats: FormatCapability[]
  selected: string[]
  suggested?: string | null
  onCancel: () => void
  onSelect: (formats: string[]) => void
}) {
  const dimensions = useTerminalDimensions()
  const [query, setQuery] = createSignal("")
  const [active, setActive] = createSignal(
    Math.max(
      0,
      props.formats.findIndex((format) => format.id === props.selected[0]),
    ),
  )
  const [selected, setSelected] = createSignal(new Set(props.selected))
  let search: InputRenderable | undefined
  let scroll: ScrollBoxRenderable | undefined

  const visible = createMemo(() =>
    props.formats.filter((format) =>
      matchesQuery(`${format.label} ${format.id} ${formatCategory(format)} ${formatDescription(format)}`, query()),
    ),
  )

  onMount(() => setTimeout(() => search?.focus(), 1))

  function move(delta: number) {
    if (visible().length === 0) return
    setActive((current) => (current + delta + visible().length) % visible().length)
    const target = scroll?.getChildren()[active()]
    if (!target || !scroll) return
    const y = target.y - scroll.y
    if (y >= scroll.height) scroll.scrollBy(y - scroll.height + 1)
    if (y < 0) scroll.scrollBy(y)
  }

  function toggle(format = visible()[active()]) {
    if (!format) return
    const next = new Set(selected())
    if (next.has(format.id) && next.size > 1) next.delete(format.id)
    else next.add(format.id)
    setSelected(next)
  }

  function makePrimary(format = visible()[active()]) {
    if (!format) return
    const next = [format.id, ...[...selected()].filter((id) => id !== format.id)]
    props.onSelect(next)
  }

  function submit() {
    const ids = [...selected()]
    const primary = props.selected.find((id) => ids.includes(id)) ?? ids[0]
    if (!primary) return
    props.onSelect([primary, ...ids.filter((id) => id !== primary)])
  }

  useKeyboard((event) => {
    if (event.name === "escape") return props.onCancel()
    if (event.name === "up" || (event.ctrl && event.name === "k")) return move(-1)
    if (event.name === "down" || (event.ctrl && event.name === "j")) return move(1)
    if (event.name === "pageup") return move(-10)
    if (event.name === "pagedown") return move(10)
    if (event.name === "space") return toggle()
    if (event.ctrl && event.name === "return") return submit()
    if (event.name === "return") return makePrimary()
  })

  return (
    <Modal
      title="Choose output formats"
      onClose={props.onCancel}
      width={88}
      footer={
        <box paddingLeft={2} paddingRight={2} paddingBottom={1} flexDirection="row" justifyContent="space-between">
          <box flexDirection="row" gap={2}>
            <KeyHint key="space" label="add output" />
            <KeyHint key="enter" label="make primary" />
          </box>
          <box backgroundColor={theme.primary} paddingLeft={1} paddingRight={1} onMouseUp={submit}>
            <text fg={theme.onPrimary} attributes={TextAttributes.BOLD}>
              Use {selected().size} output{selected().size === 1 ? "" : "s"} ctrl+enter
            </text>
          </box>
        </box>
      }
    >
      <box paddingLeft={2} paddingRight={2} paddingTop={1} gap={1} flexGrow={1} minHeight={0}>
        <input
          ref={(value: InputRenderable) => (search = value)}
          placeholder="Search formats (PDF, audio, publishing…)"
          placeholderColor={theme.subtle}
          textColor={theme.text}
          focusedTextColor={theme.text}
          backgroundColor={theme.elevated}
          focusedBackgroundColor={theme.elevated}
          cursorColor={theme.primary}
          onInput={(value) => {
            setQuery(value)
            setActive(0)
          }}
        />
        <Show
          when={visible().length > 0}
          fallback={<text fg={theme.muted}>No compatible formats match that search.</text>}
        >
          <scrollbox
            ref={(value: ScrollBoxRenderable) => (scroll = value)}
            flexGrow={1}
            minHeight={Math.min(10, dimensions().height - 9)}
            scrollbarOptions={{ visible: true }}
          >
            <For each={visible()}>
              {(format, index) => {
                const isActive = () => active() === index()
                const isSelected = () => selected().has(format.id)
                const primary = () => props.selected[0] === format.id
                return (
                  <box
                    flexDirection="row"
                    paddingLeft={1}
                    paddingRight={1}
                    backgroundColor={isActive() ? theme.primary : theme.transparent}
                    onMouseOver={() => setActive(index())}
                    onMouseDown={() => setActive(index())}
                    onMouseUp={() => toggle(format)}
                  >
                    <text width={3} fg={isActive() ? theme.onPrimary : theme.accent}>
                      {isSelected() ? "●" : "○"}
                    </text>
                    <text
                      width={22}
                      fg={isActive() ? theme.onPrimary : theme.text}
                      attributes={primary() ? TextAttributes.BOLD : undefined}
                    >
                      {format.label}
                    </text>
                    <text flexGrow={1} fg={isActive() ? theme.onPrimary : theme.muted} wrapMode="none">
                      {formatDescription(format)}
                    </text>
                    <text width={24} fg={isActive() ? theme.onPrimary : theme.subtle}>
                      {formatCategory(format)}
                    </text>
                    <Show when={format.id === props.suggested}>
                      <text fg={isActive() ? theme.onPrimary : theme.accent}> suggested</text>
                    </Show>
                  </box>
                )
              }}
            </For>
          </scrollbox>
        </Show>
      </box>
    </Modal>
  )
}
