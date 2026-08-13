import { RGBA } from "@opentui/core"

const color = (hex: string) => RGBA.fromHex(hex)

export const theme = {
  background: color("#0D1017"),
  panel: color("#141923"),
  elevated: color("#1A2130"),
  border: color("#2A3446"),
  borderActive: color("#7894FF"),
  text: color("#E9EDF7"),
  muted: color("#8993A8"),
  subtle: color("#5E687C"),
  primary: color("#7894FF"),
  onPrimary: color("#0D1017"),
  accent: color("#62D6B0"),
  warning: color("#F4C86B"),
  danger: color("#FF7A8A"),
  success: color("#62D6B0"),
  transparent: RGBA.fromInts(0, 0, 0, 0),
}

export type Theme = typeof theme
