import { onCleanup, onMount } from "solid-js"
import type { InputRenderable } from "@opentui/core"

/** Focus a modal input after mount, and ignore the callback if the renderer is already gone. */
export function focusInputWhenReady(getInput: () => InputRenderable | undefined) {
  onMount(() => {
    const timer = setTimeout(() => {
      const input = getInput()
      if (!input || input.isDestroyed) return
      try {
        input.focus()
      } catch {
        // Tests and fast closes can destroy the EditBuffer before this timeout.
      }
    }, 1)
    onCleanup(() => clearTimeout(timer))
  })
}
