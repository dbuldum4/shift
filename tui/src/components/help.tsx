import { For } from "solid-js"
import { TextAttributes } from "@opentui/core"
import { useKeyboard } from "@opentui/solid"
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
  useKeyboard((event) => {
    if (event.name === "escape" || event.name === "return" || event.name === "?") props.onClose()
  })
  return (
    <Modal title="Keyboard & mouse" onClose={props.onClose} width={74}>
      <box padding={2} gap={1}>
        <text fg={theme.text} attributes={TextAttributes.BOLD}>
          Shift is fully usable without leaving the keyboard.
        </text>
        <text fg={theme.muted}>
          Every highlighted row and button is also clickable. Scroll lists and pickers with the mouse wheel.
        </text>
        <box height={1} />
        <For each={shortcuts}>
          {([key, description]) => (
            <box flexDirection="row">
              <text width={22} fg={theme.accent}>
                {key}
              </text>
              <text fg={theme.text}>{description}</text>
            </box>
          )}
        </For>
        <box height={1} />
        <text fg={theme.subtle}>Esc closes any dialog. Terminal text selection and native copy remain available.</text>
      </box>
    </Modal>
  )
}
