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

export function App() {
  const renderer = useRenderer()
  const terminal = useTerminalDimensions()
  const [items, setItems] = createSignal<QueueItem[]>([])
  const [selected, setSelected] = createSignal(0)
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

  const wide = createMemo(() => terminal().width >= 96)
  const compact = createMemo(() => terminal().height < 24)
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
    const all = dedupeSources([...items().map((item) => item.source), ...sources])
    setItems(all.map((source) => items().find((item) => item.source === source) ?? createQueueItem(source)))
    if (folder) setSettings("recursive", true)
    setSelected(Math.max(0, all.length - sources.length))
    setModal(undefined)
    setStatus(`${sources.length} input${sources.length === 1 ? "" : "s"} added`)
  }

  function removeSelected() {
    if (busy() || items().length === 0) return
    const index = selected()
    setItems((current) => current.filter((_, itemIndex) => itemIndex !== index))
    setSelected((current) => Math.max(0, Math.min(current, items().length - 2)))
    setStatus("Input removed")
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
    if (event.name === "up" || event.name === "k") return moveSelection(-1)
    if (event.name === "down" || event.name === "j") return moveSelection(1)
    if (event.name === "return" && selectedItem()?.state === "succeeded" && selectedItem()?.artifacts[0]) {
      return openPath(selectedItem()!.artifacts[0])
    }
  })

  return (
    <box width="100%" height="100%" backgroundColor={theme.background} padding={1} gap={1}>
      <Header
        version={capabilities()?.cliVersion}
        busy={busy()}
        onCommands={() => setModal("commands")}
        onHelp={() => setModal("help")}
      />

      <box flexGrow={1} minHeight={0} flexDirection={wide() ? "row" : "column"} gap={1}>
        <QueuePanel
          items={items()}
          selected={selected()}
          compact={compact()}
          onSelect={setSelected}
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
      <Footer />

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
  const roomy = () => terminal().width >= 96
  return (
    <box
      height={3}
      flexShrink={0}
      flexDirection="row"
      alignItems="center"
      justifyContent="space-between"
      paddingLeft={1}
      paddingRight={1}
    >
      <box flexDirection="row" gap={2} alignItems="center">
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
      <box flexDirection="row" gap={2}>
        <Show when={props.busy}>
          <text fg={theme.accent}>● working</text>
        </Show>
        <text fg={theme.muted} onMouseUp={props.onCommands}>
          <span style={{ fg: theme.text }}>ctrl+k</span> commands
        </text>
        <Show when={terminal().width >= 70}>
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
  compact: boolean
  onSelect: (index: number) => void
  onAdd: () => void
  onAddFolder: () => void
  onAddUrl: () => void
  onRemove: () => void
  bind: (value: ScrollBoxRenderable) => void
}) {
  return (
    <box
      flexGrow={1}
      minWidth={0}
      minHeight={props.compact ? 8 : 12}
      border
      borderStyle="rounded"
      borderColor={theme.border}
      title=" Inputs "
      titleColor={theme.text}
    >
      <Show
        when={props.items.length > 0}
        fallback={
          <box flexGrow={1} alignItems="center" justifyContent="center" gap={1}>
            <text fg={theme.text} attributes={TextAttributes.BOLD}>
              What should Shift convert?
            </text>
            <text fg={theme.muted}>Pick files, a folder, or paste a public URL.</text>
            <box flexDirection="row" gap={1} paddingTop={1}>
              <Button label="Add files" shortcut="ctrl+p" primary onUse={props.onAdd} />
              <Button label="Add folder" onUse={props.onAddFolder} />
              <Button label="Add URL" shortcut="ctrl+l" onUse={props.onAddUrl} />
            </box>
          </box>
        }
      >
        <scrollbox ref={props.bind} flexGrow={1} scrollbarOptions={{ visible: true }}>
          <For each={props.items}>
            {(item, index) => {
              const active = () => props.selected === index()
              return (
                <box
                  minHeight={2}
                  flexDirection="row"
                  paddingLeft={1}
                  paddingRight={1}
                  backgroundColor={active() ? theme.elevated : theme.transparent}
                  border={active() ? ["left"] : undefined}
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
                  <box flexGrow={1} minWidth={0}>
                    <text
                      fg={active() ? theme.text : theme.muted}
                      attributes={active() ? TextAttributes.BOLD : undefined}
                      wrapMode="none"
                    >
                      {item.name}
                    </text>
                    <text fg={theme.subtle} wrapMode="none">
                      {item.detail ?? item.source}
                    </text>
                  </box>
                  <text width={8} fg={theme.accent}>
                    {sourceBadge(item)}
                  </text>
                  <Show when={active()}>
                    <text fg={theme.muted} onMouseUp={props.onRemove}>
                      remove
                    </text>
                  </Show>
                </box>
              )
            }}
          </For>
        </scrollbox>
        <box height={2} flexShrink={0} flexDirection="row" gap={2} paddingLeft={1} paddingRight={1} alignItems="center">
          <text fg={theme.accent} onMouseUp={props.onAdd}>
            + files
          </text>
          <text fg={theme.accent} onMouseUp={props.onAddFolder}>
            + folder
          </text>
          <text fg={theme.accent} onMouseUp={props.onAddUrl}>
            + URL
          </text>
          <text flexGrow={1} />
          <text fg={theme.muted}>{props.items.length} queued</text>
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
  return (
    <box
      width={props.wide ? 42 : "100%"}
      minHeight={props.wide ? 0 : 8}
      border
      borderStyle="rounded"
      borderColor={theme.border}
      title=" Conversion "
      titleColor={theme.text}
      paddingLeft={1}
      paddingRight={1}
    >
      <Setting
        label="Output"
        value={props.formats || "No compatible output"}
        shortcut="ctrl+o"
        onUse={props.onFormats}
      />
      <Setting label="Converter" value={props.module} shortcut="ctrl+m" onUse={props.onModule} />
      <Setting
        label="Destination"
        value={props.outputDir ?? "Beside each source"}
        shortcut="ctrl+d"
        onUse={props.onOutputDir}
        onClear={props.outputDir ? props.onClearOutput : undefined}
      />
      <Setting
        label="Naming"
        value={props.namingTemplate ?? "{stem}.{ext}"}
        onUse={props.onNaming}
        onClear={props.namingTemplate ? props.onClearNaming : undefined}
      />
      <Toggle label="Overwrite existing" enabled={props.force} onUse={props.onToggleForce} />
      <Toggle label="Expand folders recursively" enabled={props.recursive} onUse={props.onToggleRecursive} />
      <box flexGrow={1} minHeight={1} />
      <Button
        label={props.busy ? "Cancel conversion" : "Run conversion"}
        shortcut={props.busy ? "ctrl+c" : "ctrl+r"}
        primary={!props.busy}
        danger={props.busy}
        disabled={!props.busy && !props.canRun}
        onUse={props.busy ? props.onCancel : props.onRun}
      />
      <box height={1} />
    </box>
  )
}

function Setting(props: { label: string; value: string; shortcut?: string; onUse: () => void; onClear?: () => void }) {
  return (
    <box paddingTop={1} paddingBottom={1} onMouseUp={props.onUse}>
      <box flexDirection="row" justifyContent="space-between">
        <text fg={theme.muted}>{props.label}</text>
        <text fg={theme.subtle}>{props.shortcut}</text>
      </box>
      <box flexDirection="row">
        <text flexGrow={1} fg={theme.text} wrapMode="none">
          {props.value}
        </text>
        <Show when={props.onClear}>
          <text
            fg={theme.muted}
            onMouseUp={(event: { stopPropagation(): void }) => {
              event.stopPropagation()
              props.onClear?.()
            }}
          >
            clear
          </text>
        </Show>
      </box>
    </box>
  )
}

function Toggle(props: { label: string; enabled: boolean; onUse: () => void }) {
  return (
    <box height={2} flexDirection="row" alignItems="center" justifyContent="space-between" onMouseUp={props.onUse}>
      <text fg={theme.text}>{props.label}</text>
      <text fg={props.enabled ? theme.accent : theme.subtle}>{props.enabled ? "● on" : "○ off"}</text>
    </box>
  )
}

function Button(props: {
  label: string
  shortcut?: string
  primary?: boolean
  danger?: boolean
  disabled?: boolean
  onUse: () => void
}) {
  const background = () =>
    props.disabled ? theme.elevated : props.danger ? theme.danger : props.primary ? theme.primary : theme.elevated
  const foreground = () =>
    props.disabled ? theme.subtle : props.primary || props.danger ? theme.onPrimary : theme.text
  return (
    <box
      height={2}
      alignItems="center"
      justifyContent="center"
      flexDirection="row"
      gap={1}
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={background()}
      onMouseUp={() => {
        if (!props.disabled) props.onUse()
      }}
    >
      <text fg={foreground()} attributes={TextAttributes.BOLD}>
        {props.label}
      </text>
      <Show when={props.shortcut}>
        <text fg={foreground()}>{props.shortcut}</text>
      </Show>
    </box>
  )
}

function ActivityBar(props: { status: string; outputs: string[]; busy: boolean; onOpen: (path: string) => void }) {
  const latest = () => props.outputs.at(-1)
  return (
    <box
      height={2}
      flexShrink={0}
      flexDirection="row"
      alignItems="center"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={theme.panel}
    >
      <text width={3} fg={props.busy ? theme.warning : theme.accent}>
        {props.busy ? "◉" : "●"}
      </text>
      <text flexGrow={1} fg={theme.muted} wrapMode="none">
        {props.status}
      </text>
      <Show when={latest()}>
        <text fg={theme.accent} onMouseUp={() => props.onOpen(latest()!)}>
          open latest ↗
        </text>
      </Show>
    </box>
  )
}

function Footer() {
  return (
    <box height={1} flexShrink={0} flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
      <box flexDirection="row" gap={2}>
        <KeyHint key="a" label="add" />
        <KeyHint key="↑↓" label="navigate" />
        <KeyHint key="ctrl+r" label="run" />
      </box>
      <text fg={theme.subtle}>{process.cwd()}</text>
    </box>
  )
}

function Doctor(props: { lines: string[]; onClose: () => void }) {
  useKeyboard((event) => {
    if (event.name === "escape" || event.name === "return") props.onClose()
  })
  return (
    <Modal title="Converter health" onClose={props.onClose} width={82}>
      <scrollbox padding={2} maxHeight={24} scrollbarOptions={{ visible: true }}>
        <For each={props.lines}>
          {(line) => <text fg={line.includes("missing") ? theme.warning : theme.text}>{line}</text>}
        </For>
      </scrollbox>
    </Modal>
  )
}
