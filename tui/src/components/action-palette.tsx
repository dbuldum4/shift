import { createMemo, createSignal, For, onMount, Show } from "solid-js"
import { TextAttributes, type InputRenderable } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { paletteLabelWidth, truncateEnd } from "../layout"
import { matchesQuery } from "../model"
import { theme } from "../theme"
import { KeyHint, Modal } from "./modal"

export type PaletteAction = {
  id: string
  label: string
  description: string
  shortcut?: string
  disabled?: boolean
  run: () => void
}

export function ActionPalette(props: { actions: PaletteAction[]; onCancel: () => void }) {
  const terminal = useTerminalDimensions()
  const [query, setQuery] = createSignal("")
  const [active, setActive] = createSignal(0)
  let input: InputRenderable | undefined
  const visible = createMemo(() =>
    props.actions.filter((action) => matchesQuery(`${action.label} ${action.description}`, query())),
  )
  onMount(() => setTimeout(() => input?.focus(), 1))

  function move(delta: number) {
    if (visible().length) setActive((current) => (current + delta + visible().length) % visible().length)
  }

  function submit(action = visible()[active()]) {
    if (!action || action.disabled) return
    props.onCancel()
    action.run()
  }

  useKeyboard((event) => {
    if (event.name === "escape") return props.onCancel()
    if (event.name === "up") return move(-1)
    if (event.name === "down") return move(1)
    if (event.name === "return") return submit()
  })

  return (
    <Modal
      title="Command palette"
      onClose={props.onCancel}
      width={76}
      footer={
        <box
          paddingLeft={2}
          paddingRight={2}
          paddingBottom={1}
          flexDirection="row"
          flexWrap="wrap"
          gap={2}
          overflow="hidden"
        >
          <KeyHint key="j/k" label="navigate" />
          <Show when={terminal().width >= 40}>
            <KeyHint key="enter" label="run" />
          </Show>
          <KeyHint key="esc" label="close" />
        </box>
      }
    >
      <box padding={2} gap={1} flexGrow={1} minHeight={0} overflow="hidden">
        <input
          ref={(next: InputRenderable) => (input = next)}
          placeholder="Type an action"
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
        <Show when={visible().length} fallback={<text fg={theme.muted}>No matching actions.</text>}>
          <scrollbox flexGrow={1} minHeight={0} scrollX={false} horizontalScrollbarOptions={{ visible: false }}>
            <For each={visible()}>
              {(action, index) => {
                const selected = () => active() === index()
                const labelWidth = () => paletteLabelWidth(terminal().width)
                return (
                  <box
                    flexDirection="row"
                    paddingLeft={1}
                    paddingRight={1}
                    gap={1}
                    minWidth={0}
                    overflow="hidden"
                    backgroundColor={selected() ? theme.primary : theme.transparent}
                    onMouseOver={() => setActive(index())}
                    onMouseUp={() => submit(action)}
                  >
                    <text
                      width={labelWidth()}
                      fg={action.disabled ? theme.subtle : selected() ? theme.onPrimary : theme.text}
                      attributes={selected() ? TextAttributes.BOLD : undefined}
                      wrapMode="none"
                    >
                      {truncateEnd(action.label, labelWidth())}
                    </text>
                    <text flexGrow={1} minWidth={0} fg={selected() ? theme.onPrimary : theme.muted} wrapMode="none">
                      {action.description}
                    </text>
                    <Show when={action.shortcut && terminal().width >= 56}>
                      <text fg={selected() ? theme.onPrimary : theme.accent}>{action.shortcut}</text>
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
