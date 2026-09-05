import { createSignal } from "solid-js"
import { TextAttributes, type InputRenderable } from "@opentui/core"
import { useKeyboard } from "@opentui/solid"
import { theme } from "../theme"
import { focusInputWhenReady } from "./focus-input"
import { KeyHint, Modal } from "./modal"

export function TextPrompt(props: {
  title: string
  label: string
  placeholder: string
  initialValue?: string
  action: string
  onCancel: () => void
  onSubmit: (value: string) => void
}) {
  const [value, setValue] = createSignal(props.initialValue ?? "")
  let input: InputRenderable | undefined
  focusInputWhenReady(() => input)

  function submit() {
    const next = value().trim()
    if (next) props.onSubmit(next)
  }

  useKeyboard((event) => {
    if (event.name === "escape") props.onCancel()
    if (event.name === "return") submit()
  })

  return (
    <Modal
      title={props.title}
      onClose={props.onCancel}
      width={72}
      footer={
        <box
          paddingLeft={2}
          paddingRight={2}
          paddingBottom={1}
          flexDirection="row"
          justifyContent="space-between"
          overflow="hidden"
          gap={1}
        >
          <KeyHint key="esc" label="cancel" />
          <box backgroundColor={theme.primary} paddingLeft={1} paddingRight={1} onMouseUp={submit}>
            <text fg={theme.onPrimary} attributes={TextAttributes.BOLD}>
              {props.action} enter
            </text>
          </box>
        </box>
      }
    >
      <box padding={2} gap={1}>
        <text fg={theme.muted}>{props.label}</text>
        <input
          ref={(next: InputRenderable) => (input = next)}
          value={props.initialValue}
          placeholder={props.placeholder}
          placeholderColor={theme.subtle}
          textColor={theme.text}
          focusedTextColor={theme.text}
          backgroundColor={theme.elevated}
          focusedBackgroundColor={theme.elevated}
          cursorColor={theme.primary}
          onInput={setValue}
        />
      </box>
    </Modal>
  )
}
