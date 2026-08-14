import { For, Show } from "solid-js"
import { TextAttributes } from "@opentui/core"
import { useKeyboard, useTerminalDimensions } from "@opentui/solid"
import { helpKeyWidth, truncateEnd } from "../layout"
import { theme } from "../theme"
import { Modal } from "./modal"

const shortcuts = [
  ["ctrl+p / a", "Add input files"],
  ["ctrl+l", "Add a public URL"],
  ["ctrl+o", "Pick one or more output formats"],
  ["ctrl+m", "Choose a preferred converter"],
  ["ctrl+d", "Choose or clear the output folder"],
  ["ctrl+r", "Start the queue / retry failed items"],
  ["ctrl+x / delete", "Remove the selected input"],
  ["←→↑↓ or j / k", "Move the add cursor or queued inputs"],
  ["enter", "Activate the focused add action"],
  ["space", "Toggle the focused setting"],
  ["ctrl+k", "Open the command palette"],
  ["ctrl+c", "Cancel active work; press again to exit"],
  ["?", "Open this shortcut guide"],
] as const

export function Help(props: { onClose: () => void }) {
  const terminal = useTerminalDimensions()
  useKeyboard((event) => {
    if (event.name === "escape" || event.name === "return" || event.name === "?") props.onClose()
  })
  const keyWidth = () => helpKeyWidth(terminal().width)
  return (
    <Modal title="Keyboard & mouse" onClose={props.onClose} width={74}>
      <scrollbox
        padding={2}
        gap={1}
        flexGrow={1}
        minHeight={0}
        scrollX={false}
        horizontalScrollbarOptions={{ visible: false }}
      >
        <Show when={terminal().height >= 22}>
          <box gap={1} flexShrink={0}>
            <text fg={theme.text} attributes={TextAttributes.BOLD}>
              Shift is fully usable without leaving the keyboard.
            </text>
            <text fg={theme.muted}>
              Every highlighted row and button is also clickable. Scroll lists and pickers with the mouse wheel.
            </text>
            <box height={1} />
          </box>
        </Show>
        <For each={shortcuts}>
          {([key, description]) => (
            <box flexDirection="row" overflow="hidden" minWidth={0}>
              <text width={keyWidth()} fg={theme.accent} wrapMode="none">
                {truncateEnd(key, keyWidth())}
              </text>
              <text flexGrow={1} minWidth={0} fg={theme.text} wrapMode="none">
                {truncateEnd(description, Math.max(8, terminal().width - keyWidth() - 10))}
              </text>
            </box>
          )}
        </For>
        <Show when={terminal().height >= 22}>
          <box gap={1} flexShrink={0}>
            <box height={1} />
            <text fg={theme.subtle}>
              Esc closes any dialog. Terminal text selection and native copy remain available.
            </text>
          </box>
        </Show>
      </scrollbox>
    </Modal>
  )
}
