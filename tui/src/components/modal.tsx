import { useTerminalDimensions } from "@opentui/solid"
import { RGBA } from "@opentui/core"
import type { JSX, ParentProps } from "solid-js"
import { theme } from "../theme"

export function Modal(
  props: ParentProps<{
    title: string
    onClose: () => void
    width?: number
    footer?: JSX.Element
  }>,
) {
  const terminal = useTerminalDimensions()
  return (
    <box
      position="absolute"
      zIndex={1000}
      width={terminal().width}
      height={terminal().height}
      alignItems="center"
      justifyContent="center"
      backgroundColor={RGBA.fromInts(0, 0, 0, 165)}
      onMouseUp={() => props.onClose()}
    >
      <box
        width={Math.min(props.width ?? 78, terminal().width - 2)}
        maxHeight={terminal().height - 2}
        backgroundColor={theme.panel}
        border
        borderStyle="rounded"
        borderColor={theme.borderActive}
        title={` ${props.title} `}
        titleColor={theme.text}
        onMouseUp={(event: { stopPropagation(): void }) => event.stopPropagation()}
      >
        <box flexGrow={1} minHeight={0}>
          {props.children}
        </box>
        {props.footer}
      </box>
    </box>
  )
}

export function KeyHint(props: { key: string; label: string }) {
  return (
    <text fg={theme.muted}>
      <span style={{ fg: theme.text }}>{props.key}</span> {props.label}
    </text>
  )
}
