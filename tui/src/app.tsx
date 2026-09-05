import { createEffect, createMemo, createSignal, For, Match, Show, Switch } from "solid-js"
import { createStore } from "solid-js/store"
import { TextAttributes, type ScrollBoxRenderable } from "@opentui/core"
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/solid"
import { ActionPalette, type PaletteAction } from "./components/action-palette"
import { FilePicker } from "./components/file-picker"
import { FormatPicker } from "./components/format-picker"
import { Help } from "./components/help"
import { KeyHint, Modal } from "./components/modal"
import { TextPrompt } from "./components/text-prompt"
import { openPath, queryCapabilities, queryDoctor, runConversion } from "./engine"
import {
  activityHeight,
  COMMANDS_BREAKPOINT,
  EMPTY_SHORTCUT_BREAKPOINT,
  footerContents,
  HELP_BREAKPOINT,
  QUEUE_BADGE_BREAKPOINT,
  QUEUE_REMOVE_BREAKPOINT,
  QUEUE_URL_BREAKPOINT,
  SHORTCUT_BREAKPOINT,
  STACK_ACTIONS_BREAKPOINT,
  stackedSettingsHeight,
  truncateEnd,
  WIDE_BREAKPOINT,
  type Hint,
} from "./layout"
import {
  createQueueItem,
  dedupeSources,
  sourceBadge,
  statusGlyph,
  isUrl,
  type CapabilityResponse,
  type ConversionSettings,
  type QueueItem,
} from "./model"
import { theme } from "./theme"

const emptyFooterHints: Hint[] = [
  { key: "arrows", label: "move" },
  { key: "enter", label: "add" },
  { key: "a", label: "files" },
]

const queueFooterHints: Hint[] = [
  { key: "arrows", label: "move" },
  { key: "a", label: "add" },
  { key: "ctrl+r", label: "run" },
]

const settingsFooterHints: Hint[] = [
  { key: "arrows", label: "move" },
  { key: "enter", label: "use" },
  { key: "ctrl+r", label: "run" },
]

const SETTINGS = {
  formats: 0,
  module: 1,
  destination: 2,
  naming: 3,
  force: 4,
  recursive: 5,
  run: 6,
} as const

const SETTING_COUNT = 7

type ModalName =
  | "files"
  | "inputFolder"
  | "outputDir"
  | "formats"
  | "modules"
  | "naming"
  | "url"
  | "help"
  | "commands"
  | "doctor"

const emptySourceActions = [
  { id: "files", label: "Add files", shortcut: "ctrl+p", modal: "files" },
  { id: "folder", label: "Add folder", shortcut: undefined, modal: "inputFolder" },
  { id: "url", label: "Add URL", shortcut: "ctrl+l", modal: "url" },
] as const

export function App() {
  const renderer = useRenderer()
  const terminal = useTerminalDimensions()
  const [items, setItems] = createSignal<QueueItem[]>([])
  const [selected, setSelected] = createSignal(0)
  const [emptyFocus, setEmptyFocus] = createSignal(0)
  const [focusPane, setFocusPane] = createSignal<"queue" | "settings">("queue")
  const [settingsFocus, setSettingsFocus] = createSignal<number>(SETTINGS.formats)
  const [modal, setModal] = createSignal<ModalName>()
  const [capabilities, setCapabilities] = createSignal<CapabilityResponse>()
  const [settings, setSettings] = createStore<ConversionSettings>({
    formats: ["markdown"],
    force: false,
    recursive: false,
  })
  const [busy, setBusy] = createSignal(false)
  const [status, setStatus] = createSignal("Ready")
  const [outputs, setOutputs] = createSignal<string[]>([])
  const [doctor, setDoctor] = createSignal<string[]>([])
  const [capabilityError, setCapabilityError] = createSignal<string>()
  let abort: AbortController | undefined
  let list: ScrollBoxRenderable | undefined
  let capabilityGeneration = 0

  const wide = createMemo(() => terminal().width >= WIDE_BREAKPOINT)
  const compact = createMemo(() => terminal().height <= 24)
  const selectedItem = createMemo(() => items()[selected()])
  const availableFormats = createMemo(() => capabilities()?.formats ?? [])
  const selectedFormatLabels = createMemo(() =>
    settings.formats.map((id) => availableFormats().find((format) => format.id === id)?.label ?? id).join(" + "),
  )
  const selectedModuleLabel = createMemo(
    () => capabilities()?.modules.find((module) => module.id === settings.preferredModule)?.label ?? "Automatic",
  )
  const canRun = createMemo(() => items().length > 0 && settings.formats.length > 0 && !busy())

  createEffect(() => {
    const sources = items().map((item) => item.source)
    const generation = ++capabilityGeneration
    queueMicrotask(() => {
      try {
        const response = queryCapabilities(sources)
        if (generation !== capabilityGeneration) return
        setCapabilities(response)
        setCapabilityError(undefined)
        const ids = new Set(response.formats.map((format) => format.id))
        const retained = settings.formats.filter((format) => ids.has(format))
        if (retained.length > 0) setSettings("formats", retained)
        else if (response.suggestedFormat) setSettings("formats", [response.suggestedFormat])
        else if (response.formats[0]) setSettings("formats", [response.formats[0].id])
        else if (sources.length > 0) setSettings("formats", [])
      } catch (cause) {
        if (generation !== capabilityGeneration) return
        setCapabilityError(cause instanceof Error ? cause.message : String(cause))
      }
    })
  })

  function addSources(sources: string[], folder = false) {
    const wasEmpty = items().length === 0
    const all = dedupeSources([...items().map((item) => item.source), ...sources])
    setItems(all.map((source) => items().find((item) => item.source === source) ?? createQueueItem(source)))
    if (folder) setSettings("recursive", true)
    setSelected(Math.max(0, all.length - sources.length))
    setModal(undefined)
    setStatus(`${sources.length} input${sources.length === 1 ? "" : "s"} added`)
    if (wasEmpty) {
      setFocusPane("settings")
      setSettingsFocus(SETTINGS.formats)
    }
  }

  function removeSelected() {
    if (busy() || items().length === 0) return
    const index = selected()
    const remaining = items().length - 1
    setItems((current) => current.filter((_, itemIndex) => itemIndex !== index))
    setSelected((current) => Math.max(0, Math.min(current, items().length - 2)))
    setStatus("Input removed")
    if (remaining === 0) {
      setFocusPane("queue")
      setEmptyFocus(0)
    }
  }

  function moveSelection(delta: number) {
    if (!items().length) return
    setSelected((current) => (current + delta + items().length) % items().length)
    const target = list?.getChildren()[selected()]
    if (!target || !list) return
    const y = target.y - list.y
    if (y >= list.height) list.scrollBy(y - list.height + 1)
    if (y < 0) list.scrollBy(y)
  }

  function moveEmptyFocus(delta: number) {
    setEmptyFocus((current) => (current + delta + emptySourceActions.length) % emptySourceActions.length)
  }

  function activateEmptyFocus() {
    const action = emptySourceActions[emptyFocus()]
    if (action) setModal(action.modal)
  }

  function activateQueueItem() {
    const item = selectedItem()
    if (item?.state === "succeeded" && item.artifacts[0]) return openPath(item.artifacts[0])
  }

  function moveQueue(delta: number) {
    if (delta > 0 && selected() === items().length - 1) {
      setFocusPane("settings")
      setSettingsFocus(SETTINGS.formats)
      return
    }
    if (delta < 0 && selected() === 0) return
    moveSelection(delta)
  }

  function moveSettings(delta: number) {
    const next = settingsFocus() + delta
    if (next < 0) {
      setFocusPane("queue")
      return
    }
    if (next >= SETTING_COUNT) return
    setSettingsFocus(next)
  }

  function activateSetting() {
    switch (settingsFocus()) {
      case SETTINGS.formats:
        if (availableFormats().length) setModal("formats")
        return
      case SETTINGS.module:
        return setModal("modules")
      case SETTINGS.destination:
        return setModal("outputDir")
      case SETTINGS.naming:
        return setModal("naming")
      case SETTINGS.force:
        return setSettings("force", !settings.force)
      case SETTINGS.recursive:
        return setSettings("recursive", !settings.recursive)
      case SETTINGS.run:
        if (busy()) return cancel()
        return void start()
    }
  }

  async function start() {
    if (!canRun()) return
    abort = new AbortController()
    setBusy(true)
    setOutputs([])
    setItems((current) =>
      current.map((item) => ({ ...item, state: "running", detail: "Queued for conversion", artifacts: [] })),
    )
    setStatus(`Converting ${items().length} input${items().length === 1 ? "" : "s"}…`)
    try {
      const result = await runConversion(
        items(),
        settings,
        {
          onProgress(line) {
            if (!line) return
            setStatus(line)
            setItems((current) => current.map((item) => (item.state === "running" ? { ...item, detail: line } : item)))
          },
          onOutput(path) {
            setOutputs((current) => [...current, path])
          },
        },
        abort.signal,
      )
      const state = result.exitCode === 0 ? "succeeded" : "failed"
      setItems((current) =>
        current.map((item) => ({ ...item, state, detail: result.exitCode === 0 ? "Complete" : "Conversion failed" })),
      )
      if (items().length === 1 && result.outputs.length > 0) {
        setItems((current) => current.map((item) => ({ ...item, artifacts: result.outputs })))
      }
      setStatus(
        result.exitCode === 0
          ? `Done · ${result.outputs.length} artifact${result.outputs.length === 1 ? "" : "s"} created`
          : `Conversion exited with code ${result.exitCode}`,
      )
    } catch (cause) {
      const cancelled = abort.signal.aborted
      setItems((current) =>
        current.map((item) => ({
          ...item,
          state: cancelled ? "cancelled" : "failed",
          detail: cancelled ? "Cancelled" : cause instanceof Error ? cause.message : String(cause),
        })),
      )
      setStatus(cancelled ? "Cancelled" : cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusy(false)
      abort = undefined
    }
  }

  function cancel() {
    abort?.abort()
    setStatus("Cancelling…")
  }

  function inspectDoctor() {
    try {
      const result = queryDoctor()
      const entries = Object.entries(result.values).map(([key, value]) => `${key}=${value}`)
      setDoctor(entries.length ? entries : [result.raw || `Doctor exited with code ${result.exitCode}`])
    } catch (cause) {
      setDoctor([cause instanceof Error ? cause.message : String(cause)])
    }
    setModal("doctor")
  }

  const actions = createMemo<PaletteAction[]>(() => [
    {
      id: "files",
      label: "Add files",
      description: "Browse and multi-select local files",
      shortcut: "ctrl+p",
      run: () => setModal("files"),
    },
    {
      id: "folder",
      label: "Add folder",
      description: "Queue a folder recursively",
      run: () => setModal("inputFolder"),
    },
    {
      id: "url",
      label: "Add URL",
      description: "Convert a public web page or remote file",
      shortcut: "ctrl+l",
      run: () => setModal("url"),
    },
    {
      id: "formats",
      label: "Choose outputs",
      description: "Select a primary format and fan-out outputs",
      shortcut: "ctrl+o",
      disabled: !availableFormats().length,
      run: () => setModal("formats"),
    },
    {
      id: "module",
      label: "Choose converter",
      description: "Prefer a conversion engine or use automatic routing",
      shortcut: "ctrl+m",
      run: () => setModal("modules"),
    },
    {
      id: "destination",
      label: "Choose destination",
      description: "Write outputs into a folder",
      shortcut: "ctrl+d",
      run: () => setModal("outputDir"),
    },
    {
      id: "naming",
      label: "Set naming template",
      description: "Customize output names with {stem}, {parent}, {format}, and {ext}",
      run: () => setModal("naming"),
    },
    {
      id: "beside",
      label: "Save beside sources",
      description: "Clear the output folder",
      disabled: !settings.outputDir,
      run: () => setSettings("outputDir", undefined),
    },
    {
      id: "force",
      label: settings.force ? "Disable overwrite" : "Enable overwrite",
      description: "Toggle replacement of existing outputs",
      run: () => setSettings("force", !settings.force),
    },
    {
      id: "recursive",
      label: settings.recursive ? "Disable recursive folders" : "Enable recursive folders",
      description: "Toggle folder expansion",
      run: () => setSettings("recursive", !settings.recursive),
    },
    {
      id: "run",
      label: "Run queue",
      description: "Start conversion with the current settings",
      shortcut: "ctrl+r",
      disabled: !canRun(),
      run: () => void start(),
    },
    {
      id: "remove",
      label: "Remove selected input",
      description: "Remove one item from the queue",
      shortcut: "ctrl+x",
      disabled: !selectedItem() || busy(),
      run: removeSelected,
    },
    {
      id: "doctor",
      label: "Check converter health",
      description: "Inspect installed conversion engines",
      run: inspectDoctor,
    },
    {
      id: "help",
      label: "Keyboard & mouse help",
      description: "Show every shortcut",
      shortcut: "?",
      run: () => setModal("help"),
    },
  ])

  useKeyboard((event) => {
    if (modal()) return
    if (event.ctrl && event.name === "c") {
      if (busy()) cancel()
      else renderer.destroy()
      return
    }
    if ((event.ctrl && event.name === "p") || event.name === "a") return setModal("files")
    if (event.ctrl && event.name === "l") return setModal("url")
    if (event.ctrl && event.name === "o" && availableFormats().length) return setModal("formats")
    if (event.ctrl && event.name === "m") return setModal("modules")
    if (event.ctrl && event.name === "d") return setModal("outputDir")
    if (event.ctrl && event.name === "k") return setModal("commands")
    if (event.ctrl && event.name === "r") return void start()
    if ((event.ctrl && event.name === "x") || event.name === "delete") return removeSelected()
    if (event.name === "?" || (event.shift && event.name === "/")) return setModal("help")
    if (!items().length) {
      if (event.name === "left" || event.name === "up" || event.name === "k") return moveEmptyFocus(-1)
      if (event.name === "right" || event.name === "down" || event.name === "j") return moveEmptyFocus(1)
      if (event.name === "return") return activateEmptyFocus()
      return
    }
    if (event.name === "tab") {
      setFocusPane((pane) => (pane === "settings" ? "queue" : "settings"))
      return
    }
    if (focusPane() === "settings") {
      if (event.name === "up" || event.name === "k") return moveSettings(-1)
      if (event.name === "down" || event.name === "j") return moveSettings(1)
      if (event.name === "left" || event.name === "h") return setFocusPane("queue")
      if (event.name === "return") return activateSetting()
      if (event.name === "space" && (settingsFocus() === SETTINGS.force || settingsFocus() === SETTINGS.recursive)) {
        return activateSetting()
      }
      return
    }
    if (event.name === "up" || event.name === "k") return moveQueue(-1)
    if (event.name === "down" || event.name === "j") return moveQueue(1)
    if (event.name === "right" || event.name === "l") {
      setFocusPane("settings")
      return
    }
    if (event.name === "return") return activateQueueItem()
  })

  return (
    <box width="100%" height="100%" backgroundColor={theme.background} padding={1} gap={1} overflow="hidden">
      <Header
        version={capabilities()?.cliVersion}
        busy={busy()}
        onCommands={() => setModal("commands")}
        onHelp={() => setModal("help")}
      />

      <box flexGrow={1} minWidth={0} minHeight={0} overflow="hidden" flexDirection={wide() ? "row" : "column"} gap={1}>
        <QueuePanel
          items={items()}
          selected={selected()}
          emptyFocus={emptyFocus()}
          compact={compact()}
          paneActive={items().length === 0 || focusPane() === "queue"}
          onSelect={(index) => {
            setSelected(index)
            setFocusPane("queue")
          }}
          onEmptyFocus={setEmptyFocus}
          onAdd={() => setModal("files")}
          onAddFolder={() => setModal("inputFolder")}
          onAddUrl={() => setModal("url")}
          onRemove={removeSelected}
          bind={(value) => (list = value)}
        />
        <SettingsPanel
          wide={wide()}
          formats={selectedFormatLabels()}
          module={selectedModuleLabel()}
          outputDir={settings.outputDir}
          namingTemplate={settings.namingTemplate}
          force={settings.force}
          recursive={settings.recursive}
          busy={busy()}
          canRun={canRun()}
          focusedIndex={items().length > 0 && focusPane() === "settings" ? settingsFocus() : undefined}
          onFocus={(index) => {
            setFocusPane("settings")
            setSettingsFocus(index)
          }}
          onFormats={() => setModal("formats")}
          onModule={() => setModal("modules")}
          onOutputDir={() => setModal("outputDir")}
          onNaming={() => setModal("naming")}
          onClearNaming={() => setSettings("namingTemplate", undefined)}
          onClearOutput={() => setSettings("outputDir", undefined)}
          onToggleForce={() => setSettings("force", !settings.force)}
          onToggleRecursive={() => setSettings("recursive", !settings.recursive)}
          onRun={() => void start()}
          onCancel={cancel}
        />
      </box>

      <ActivityBar status={capabilityError() ?? status()} outputs={outputs()} busy={busy()} onOpen={openPath} />
      <Footer empty={items().length === 0} settings={items().length > 0 && focusPane() === "settings"} />

      <Switch>
        <Match when={modal() === "files"}>
          <FilePicker mode="files" onCancel={() => setModal(undefined)} onSelect={(paths) => addSources(paths)} />
        </Match>
        <Match when={modal() === "inputFolder"}>
          <FilePicker
            mode="directory"
            title="Choose input folder"
            onCancel={() => setModal(undefined)}
            onSelect={(paths) => addSources(paths, true)}
          />
        </Match>
        <Match when={modal() === "outputDir"}>
          <FilePicker
            mode="directory"
            title="Choose output folder"
            initialPath={settings.outputDir}
            onCancel={() => setModal(undefined)}
            onSelect={([path]) => {
              setSettings("outputDir", path)
              setModal(undefined)
            }}
          />
        </Match>
        <Match when={modal() === "formats"}>
          <FormatPicker
            formats={availableFormats()}
            selected={settings.formats}
            suggested={capabilities()?.suggestedFormat}
            onCancel={() => setModal(undefined)}
            onSelect={(formats) => {
              setSettings("formats", formats)
              setModal(undefined)
            }}
          />
        </Match>
        <Match when={modal() === "modules"}>
          <ActionPalette
            actions={[
              {
                id: "auto",
                label: "Automatic",
                description: "Use Shift's configured route priority",
                run: () => {
                  setSettings("preferredModule", undefined)
                  setModal(undefined)
                },
              },
              ...(capabilities()?.modules ?? []).map((module) => ({
                id: module.id,
                label: module.label,
                description: `${module.inputs.slice(0, 5).join(", ")}${module.inputs.length > 5 ? "…" : ""} → ${module.outputs.slice(0, 4).join(", ")}`,
                run: () => {
                  setSettings("preferredModule", module.id)
                  setModal(undefined)
                },
              })),
            ]}
            onCancel={() => setModal(undefined)}
          />
        </Match>
        <Match when={modal() === "naming"}>
          <TextPrompt
            title="Output naming template"
            label="File name only. Available: {stem}, {parent}, {format}, {ext}"
            placeholder="{stem}-converted.{ext}"
            initialValue={settings.namingTemplate}
            action="Use template"
            onCancel={() => setModal(undefined)}
            onSubmit={(value) => {
              setSettings("namingTemplate", value)
              setModal(undefined)
            }}
          />
        </Match>
        <Match when={modal() === "url"}>
          <TextPrompt
            title="Add URL"
            label="Public web page or direct file URL"
            placeholder="https://example.com/article"
            action="Add URL"
            onCancel={() => setModal(undefined)}
            onSubmit={(value) => {
              if (!isUrl(value)) {
                setStatus("Enter a complete public http:// or https:// URL")
                return
              }
              addSources([value])
            }}
          />
        </Match>
        <Match when={modal() === "commands"}>
          <ActionPalette actions={actions()} onCancel={() => setModal(undefined)} />
        </Match>
        <Match when={modal() === "help"}>
          <Help onClose={() => setModal(undefined)} />
        </Match>
        <Match when={modal() === "doctor"}>
          <Doctor lines={doctor()} onClose={() => setModal(undefined)} />
        </Match>
      </Switch>
    </box>
  )
}

function Header(props: { version?: string; busy: boolean; onCommands: () => void; onHelp: () => void }) {
  const terminal = useTerminalDimensions()
  const roomy = () => terminal().width >= WIDE_BREAKPOINT
  return (
    <box
      height={1}
      flexShrink={0}
      overflow="hidden"
      flexDirection="row"
      alignItems="center"
      justifyContent="space-between"
      paddingLeft={1}
      paddingRight={1}
    >
      <box flexDirection="row" gap={2} alignItems="center" minWidth={0} flexShrink={1} overflow="hidden">
        <text fg={theme.primary} attributes={TextAttributes.BOLD}>
          SHIFT
        </text>
        <Show when={roomy()}>
          <text fg={theme.muted}>Convert anything, without leaving the terminal</text>
          <Show when={props.version}>
            <text fg={theme.subtle}>v{props.version}</text>
          </Show>
        </Show>
      </box>
      <box flexDirection="row" gap={2} flexShrink={0}>
        <Show when={props.busy}>
          <text fg={theme.accent}>● working</text>
        </Show>
        <text fg={theme.muted} onMouseUp={props.onCommands}>
          <span style={{ fg: theme.text }}>ctrl+k</span>
          <Show when={terminal().width >= COMMANDS_BREAKPOINT}> commands</Show>
        </text>
        <Show when={terminal().width >= HELP_BREAKPOINT}>
          <text fg={theme.muted} onMouseUp={props.onHelp}>
            <span style={{ fg: theme.text }}>?</span> help
          </text>
        </Show>
      </box>
    </box>
  )
}

function QueuePanel(props: {
  items: QueueItem[]
  selected: number
  emptyFocus: number
  compact: boolean
  paneActive: boolean
  onSelect: (index: number) => void
  onEmptyFocus: (index: number) => void
  onAdd: () => void
  onAddFolder: () => void
  onAddUrl: () => void
  onRemove: () => void
  bind: (value: ScrollBoxRenderable) => void
}) {
  const terminal = useTerminalDimensions()
  const stackActions = () => terminal().width < STACK_ACTIONS_BREAKPOINT
  const openEmpty = {
    files: props.onAdd,
    folder: props.onAddFolder,
    url: props.onAddUrl,
  }
  return (
    <box
      flexGrow={1}
      flexShrink={1}
      minWidth={0}
      minHeight={4}
      overflow="hidden"
      border
      borderStyle="rounded"
      borderColor={props.paneActive ? theme.borderActive : theme.border}
      title=" Inputs "
      titleColor={theme.text}
    >
      <Show
        when={props.items.length > 0}
        fallback={
          <box flexGrow={1} minHeight={0} overflow="hidden" alignItems="center" justifyContent="center" gap={1}>
            <text fg={theme.text} attributes={TextAttributes.BOLD}>
              What should Shift convert?
            </text>
            <Show when={!props.compact}>
              <text fg={theme.muted}>Pick files, a folder, or paste a public URL.</text>
            </Show>
            <box
              flexDirection={stackActions() ? "column" : "row"}
              gap={stackActions() ? 0 : 1}
              paddingTop={props.compact || stackActions() ? 0 : 1}
              alignItems="center"
              flexShrink={0}
            >
              <For each={emptySourceActions}>
                {(action, index) => (
                  <Button
                    label={action.label}
                    shortcut={terminal().width >= EMPTY_SHORTCUT_BREAKPOINT ? action.shortcut : undefined}
                    compact={stackActions() || props.compact}
                    focused={props.emptyFocus === index()}
                    onFocus={() => props.onEmptyFocus(index())}
                    onUse={openEmpty[action.id]}
                  />
                )}
              </For>
            </box>
          </box>
        }
      >
        <scrollbox
          ref={props.bind}
          flexGrow={1}
          scrollX={false}
          horizontalScrollbarOptions={{ visible: false }}
          verticalScrollbarOptions={{ visible: true }}
        >
          <For each={props.items}>
            {(item, index) => {
              const active = () => props.selected === index()
              const showBadge = () => terminal().width >= QUEUE_BADGE_BREAKPOINT
              const showRemove = () => active() && terminal().width >= QUEUE_REMOVE_BREAKPOINT
              const nameWidth = () => {
                const panel = terminal().width >= WIDE_BREAKPOINT ? terminal().width - 52 : terminal().width - 12
                return Math.max(8, panel - 3 - (showBadge() ? 8 : 0) - (showRemove() ? 7 : 0))
              }
              return (
                <box
                  minHeight={2}
                  flexDirection="row"
                  paddingLeft={1}
                  paddingRight={1}
                  overflow="hidden"
                  backgroundColor={active() ? (props.paneActive ? theme.elevated : theme.panel) : theme.transparent}
                  border={active() && props.paneActive ? ["left"] : undefined}
                  borderColor={theme.primary}
                  onMouseOver={() => props.onSelect(index())}
                  onMouseDown={() => props.onSelect(index())}
                >
                  <text
                    width={3}
                    fg={
                      item.state === "failed"
                        ? theme.danger
                        : item.state === "succeeded"
                          ? theme.success
                          : item.state === "running"
                            ? theme.warning
                            : theme.muted
                    }
                  >
                    {statusGlyph(item.state)}
                  </text>
                  <box flexGrow={1} minWidth={0} overflow="hidden">
                    <text
                      fg={active() ? theme.text : theme.muted}
                      attributes={active() ? TextAttributes.BOLD : undefined}
                      wrapMode="none"
                    >
                      {truncateEnd(item.name, nameWidth())}
                    </text>
                    <text fg={theme.subtle} wrapMode="none">
                      {truncateEnd(item.detail ?? item.source, nameWidth())}
                    </text>
                  </box>
                  <Show when={showBadge()}>
                    <text width={8} fg={theme.accent}>
                      {sourceBadge(item)}
                    </text>
                  </Show>
                  <Show when={showRemove()}>
                    <text fg={theme.muted} onMouseUp={props.onRemove}>
                      remove
                    </text>
                  </Show>
                </box>
              )
            }}
          </For>
        </scrollbox>
        <box
          height={1}
          flexShrink={0}
          overflow="hidden"
          flexDirection="row"
          gap={2}
          paddingLeft={1}
          paddingRight={1}
          alignItems="center"
        >
          <text fg={theme.accent} onMouseUp={props.onAdd}>
            + files
          </text>
          <text fg={theme.accent} onMouseUp={props.onAddFolder}>
            + folder
          </text>
          <Show when={terminal().width >= QUEUE_URL_BREAKPOINT}>
            <text fg={theme.accent} onMouseUp={props.onAddUrl}>
              + URL
            </text>
          </Show>
          <text flexGrow={1} minWidth={0} />
          <Show when={terminal().width >= SHORTCUT_BREAKPOINT}>
            <text fg={theme.muted}>{props.items.length} queued</text>
          </Show>
        </box>
      </Show>
    </box>
  )
}

function SettingsPanel(props: {
  wide: boolean
  formats: string
  module: string
  outputDir?: string
  namingTemplate?: string
  force: boolean
  recursive: boolean
  busy: boolean
  canRun: boolean
  focusedIndex?: number
  onFocus: (index: number) => void
  onFormats: () => void
  onModule: () => void
  onOutputDir: () => void
  onNaming: () => void
  onClearNaming: () => void
  onClearOutput: () => void
  onToggleForce: () => void
  onToggleRecursive: () => void
  onRun: () => void
  onCancel: () => void
}) {
  const terminal = useTerminalDimensions()
  const dense = () => !props.wide || terminal().height < 28
  const showShortcuts = () => terminal().width >= SHORTCUT_BREAKPOINT
  const rowWidth = () => (props.wide ? 36 : terminal().width - 6)
  const paneHeight = () => (props.wide ? undefined : stackedSettingsHeight(terminal().height))
  const paneActive = () => props.focusedIndex !== undefined
  let settingsList: ScrollBoxRenderable | undefined

  createEffect(() => {
    const index = props.focusedIndex
    if (index === undefined || index >= SETTINGS.run || !settingsList) return
    const target = settingsList.getChildren()[index]
    if (!target) return
    const y = target.y - settingsList.y
    if (y >= settingsList.height) settingsList.scrollBy(y - settingsList.height + 1)
    if (y < 0) settingsList.scrollBy(y)
  })

  return (
    <box
      width={props.wide ? 42 : "100%"}
      height={paneHeight()}
      flexGrow={props.wide ? 1 : 0}
      flexShrink={0}
      minWidth={0}
      minHeight={props.wide ? 0 : 4}
      overflow="hidden"
      border
      borderStyle="rounded"
      borderColor={paneActive() ? theme.borderActive : theme.border}
      title=" Conversion "
      titleColor={theme.text}
      paddingLeft={1}
      paddingRight={1}
    >
      <scrollbox
        ref={(value: ScrollBoxRenderable) => (settingsList = value)}
        flexGrow={1}
        minHeight={0}
        scrollX={false}
        horizontalScrollbarOptions={{ visible: false }}
      >
        <Setting
          dense={dense()}
          rowWidth={rowWidth()}
          label="Output"
          value={props.formats || "No compatible output"}
          shortcut={showShortcuts() ? "ctrl+o" : undefined}
          focused={props.focusedIndex === SETTINGS.formats}
          onFocus={() => props.onFocus(SETTINGS.formats)}
          onUse={props.onFormats}
        />
        <Setting
          dense={dense()}
          rowWidth={rowWidth()}
          label="Converter"
          value={props.module}
          shortcut={showShortcuts() ? "ctrl+m" : undefined}
          focused={props.focusedIndex === SETTINGS.module}
          onFocus={() => props.onFocus(SETTINGS.module)}
          onUse={props.onModule}
        />
        <Setting
          dense={dense()}
          rowWidth={rowWidth()}
          label="Destination"
          value={props.outputDir ?? "Beside each source"}
          shortcut={showShortcuts() ? "ctrl+d" : undefined}
          focused={props.focusedIndex === SETTINGS.destination}
          onFocus={() => props.onFocus(SETTINGS.destination)}
          onUse={props.onOutputDir}
          onClear={props.outputDir ? props.onClearOutput : undefined}
        />
        <Setting
          dense={dense()}
          rowWidth={rowWidth()}
          label="Naming"
          value={props.namingTemplate ?? "{stem}.{ext}"}
          focused={props.focusedIndex === SETTINGS.naming}
          onFocus={() => props.onFocus(SETTINGS.naming)}
          onUse={props.onNaming}
          onClear={props.namingTemplate ? props.onClearNaming : undefined}
        />
        <Toggle
          dense={dense()}
          label="Overwrite existing"
          enabled={props.force}
          focused={props.focusedIndex === SETTINGS.force}
          onFocus={() => props.onFocus(SETTINGS.force)}
          onUse={props.onToggleForce}
        />
        <Toggle
          dense={dense()}
          label={dense() ? "Expand folders" : "Expand folders recursively"}
          enabled={props.recursive}
          focused={props.focusedIndex === SETTINGS.recursive}
          onFocus={() => props.onFocus(SETTINGS.recursive)}
          onUse={props.onToggleRecursive}
        />
      </scrollbox>
      <Button
        label={props.busy ? "Cancel conversion" : "Run conversion"}
        shortcut={showShortcuts() ? (props.busy ? "ctrl+c" : "ctrl+r") : undefined}
        compact={dense()}
        focused={props.focusedIndex === SETTINGS.run}
        danger={props.busy}
        disabled={!props.busy && !props.canRun}
        onFocus={() => props.onFocus(SETTINGS.run)}
        onUse={props.busy ? props.onCancel : props.onRun}
      />
      <Show when={!dense()}>
        <box height={1} flexShrink={0} />
      </Show>
    </box>
  )
}

function Setting(props: {
  label: string
  value: string
  shortcut?: string
  dense?: boolean
  focused?: boolean
  rowWidth: number
  onFocus?: () => void
  onUse: () => void
  onClear?: () => void
}) {
  const valueWidth = () => {
    const row = Math.max(16, props.rowWidth)
    if (props.dense) {
      return Math.max(4, row - props.label.length - (props.shortcut?.length ?? 0) - (props.onClear ? 8 : 2) - 2)
    }
    return Math.max(4, row - (props.onClear ? 6 : 0))
  }
  const labelFg = () => (props.focused ? theme.onPrimary : theme.muted)
  const valueFg = () => (props.focused ? theme.onPrimary : theme.text)
  const hintFg = () => (props.focused ? theme.onPrimary : theme.subtle)
  return (
    <box
      height={props.dense ? 1 : undefined}
      paddingTop={props.dense ? 0 : 1}
      paddingBottom={props.dense ? 0 : 1}
      paddingLeft={props.focused ? 1 : 0}
      paddingRight={props.focused ? 1 : 0}
      overflow="hidden"
      backgroundColor={props.focused ? theme.primary : theme.transparent}
      onMouseOver={props.onFocus}
      onMouseDown={props.onFocus}
      onMouseUp={props.onUse}
    >
      <Show
        when={props.dense}
        fallback={
          <>
            <box flexDirection="row" justifyContent="space-between" overflow="hidden">
              <text fg={labelFg()}>{props.label}</text>
              <text fg={hintFg()}>{props.shortcut}</text>
            </box>
            <box flexDirection="row" overflow="hidden" minWidth={0}>
              <text flexGrow={1} minWidth={0} fg={valueFg()} wrapMode="none">
                {truncateEnd(props.value, valueWidth())}
              </text>
              <Show when={props.onClear}>
                <text
                  fg={hintFg()}
                  onMouseUp={(event: { stopPropagation(): void }) => {
                    event.stopPropagation()
                    props.onClear?.()
                  }}
                >
                  clear
                </text>
              </Show>
            </box>
          </>
        }
      >
        <box flexDirection="row" alignItems="center" overflow="hidden" minWidth={0} gap={1}>
          <text width={Math.min(11, props.label.length)} fg={labelFg()}>
            {props.label}
          </text>
          <text flexGrow={1} minWidth={0} fg={valueFg()} wrapMode="none">
            {truncateEnd(props.value, valueWidth())}
          </text>
          <Show when={props.onClear}>
            <text
              fg={hintFg()}
              onMouseUp={(event: { stopPropagation(): void }) => {
                event.stopPropagation()
                props.onClear?.()
              }}
            >
              clear
            </text>
          </Show>
          <Show when={props.shortcut}>
            <text fg={hintFg()}>{props.shortcut}</text>
          </Show>
        </box>
      </Show>
    </box>
  )
}

function Toggle(props: {
  label: string
  enabled: boolean
  dense?: boolean
  focused?: boolean
  onFocus?: () => void
  onUse: () => void
}) {
  return (
    <box
      height={props.dense ? 1 : 2}
      flexDirection="row"
      alignItems="center"
      justifyContent="space-between"
      paddingLeft={props.focused ? 1 : 0}
      paddingRight={props.focused ? 1 : 0}
      overflow="hidden"
      backgroundColor={props.focused ? theme.primary : theme.transparent}
      onMouseOver={props.onFocus}
      onMouseDown={props.onFocus}
      onMouseUp={props.onUse}
    >
      <text fg={props.focused ? theme.onPrimary : theme.text} wrapMode="none">
        {props.label}
      </text>
      <text fg={props.focused ? theme.onPrimary : props.enabled ? theme.accent : theme.subtle}>
        {props.enabled ? "● on" : "○ off"}
      </text>
    </box>
  )
}

function Button(props: {
  label: string
  shortcut?: string
  primary?: boolean
  focused?: boolean
  danger?: boolean
  disabled?: boolean
  compact?: boolean
  onFocus?: () => void
  onUse: () => void
}) {
  const highlighted = () => !props.disabled && (props.focused || props.primary)
  const background = () =>
    props.disabled ? theme.elevated : props.danger ? theme.danger : highlighted() ? theme.primary : theme.elevated
  const foreground = () =>
    props.disabled ? theme.subtle : highlighted() || props.danger ? theme.onPrimary : theme.text
  return (
    <box
      height={props.compact ? 1 : 3}
      flexShrink={0}
      alignItems="center"
      justifyContent="center"
      flexDirection={props.compact ? "row" : "column"}
      gap={props.compact ? 1 : 0}
      paddingLeft={1}
      paddingRight={1}
      overflow="hidden"
      backgroundColor={background()}
      onMouseOver={props.onFocus}
      onMouseDown={props.onFocus}
      onMouseUp={() => {
        if (!props.disabled) props.onUse()
      }}
    >
      <Show when={!props.compact}>
        <box height={1} />
      </Show>
      <box height={1} flexDirection="row" alignItems="center" justifyContent="center" gap={1}>
        <text fg={foreground()} attributes={TextAttributes.BOLD}>
          {props.label}
        </text>
        <Show when={props.shortcut}>
          <text fg={foreground()}>{props.shortcut}</text>
        </Show>
      </box>
    </box>
  )
}

function ActivityBar(props: { status: string; outputs: string[]; busy: boolean; onOpen: (path: string) => void }) {
  const terminal = useTerminalDimensions()
  const latest = () => props.outputs.at(-1)
  const statusWidth = () => Math.max(8, terminal().width - (latest() && terminal().width >= 48 ? 24 : 8))
  return (
    <box
      height={activityHeight(terminal().height)}
      flexShrink={0}
      overflow="hidden"
      flexDirection="row"
      alignItems="center"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={theme.panel}
    >
      <box
        flexGrow={1}
        minWidth={0}
        height={1}
        overflow="hidden"
        flexDirection="row"
        alignItems="center"
        justifyContent="center"
        gap={1}
      >
        <text fg={props.busy ? theme.warning : theme.accent}>{props.busy ? "◉" : "●"}</text>
        <text fg={theme.muted} wrapMode="none">
          {truncateEnd(props.status, statusWidth())}
        </text>
      </box>
      <Show when={latest() && terminal().width >= 48}>
        <text fg={theme.accent} onMouseUp={() => props.onOpen(latest()!)}>
          open latest ↗
        </text>
      </Show>
    </box>
  )
}

function Footer(props: { empty: boolean; settings?: boolean }) {
  const terminal = useTerminalDimensions()
  const contents = createMemo(() =>
    footerContents(
      props.empty ? emptyFooterHints : props.settings ? settingsFooterHints : queueFooterHints,
      process.cwd(),
      terminal().width,
    ),
  )
  return (
    <box
      height={1}
      flexShrink={0}
      overflow="hidden"
      flexDirection="row"
      justifyContent="space-between"
      paddingLeft={1}
      paddingRight={1}
      gap={2}
    >
      <box flexDirection="row" gap={2} flexShrink={0} overflow="hidden">
        <For each={contents().hints}>{(hint) => <KeyHint key={hint.key} label={hint.label} />}</For>
      </box>
      <Show when={contents().path}>
        <text fg={theme.subtle} wrapMode="none">
          {contents().path}
        </text>
      </Show>
    </box>
  )
}

function Doctor(props: { lines: string[]; onClose: () => void }) {
  useKeyboard((event) => {
    if (event.name === "escape" || event.name === "return") props.onClose()
  })
  return (
    <Modal title="Converter health" onClose={props.onClose} width={82}>
      <scrollbox padding={2} maxHeight={24} scrollX={false} horizontalScrollbarOptions={{ visible: false }}>
        <For each={props.lines}>
          {(line) => <text fg={line.includes("missing") ? theme.warning : theme.text}>{line}</text>}
        </For>
      </scrollbox>
    </Modal>
  )
}
