import { createMemo, createSignal, For, onMount, Show } from "solid-js"
import { TextAttributes, type InputRenderable } from "@opentui/core"
import { useKeyboard } from "@opentui/solid"
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
        <box paddingLeft={2} paddingRight={2} paddingBottom={1} flexDirection="row" gap={2}>
          <KeyHint key="↑↓" label="navigate" />
          <KeyHint key="enter" label="run" />
          <KeyHint key="esc" label="close" />
        </box>
      }
    >
      <box padding={2} gap={1}>
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
          <For each={visible()}>
            {(action, index) => {
              const selected = () => active() === index()
              return (
                <box
                  flexDirection="row"
                  paddingLeft={1}
                  paddingRight={1}
                  backgroundColor={selected() ? theme.primary : theme.transparent}
                  onMouseOver={() => setActive(index())}
                  onMouseUp={() => submit(action)}
                >
                  <text
                    width={25}
                    fg={action.disabled ? theme.subtle : selected() ? theme.onPrimary : theme.text}
                    attributes={selected() ? TextAttributes.BOLD : undefined}
                  >
                    {action.label}
                  </text>
                  <text flexGrow={1} fg={selected() ? theme.onPrimary : theme.muted}>
                    {action.description}
                  </text>
                  <text fg={selected() ? theme.onPrimary : theme.accent}>{action.shortcut ?? ""}</text>
                </box>
              )
            }}
          </For>
        </Show>
      </box>
    </Modal>
  )
}
