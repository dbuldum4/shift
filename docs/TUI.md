# Shift terminal application

Shift's terminal UI is a standalone OpenTUI/Solid client. It deliberately owns
presentation only: the Rust `shift-cli` binary remains the authoritative engine
for capability discovery, argument validation, recipes, URL policy, output
naming, queue ordering, conversion, cancellation, and artifact writes.

## Process boundary

- `shift-cli` with no arguments and an attached input/output terminal launches
  the sibling `shift-tui` executable.
- `shift-cli --headless …` always stays in the Rust process and preserves the
  existing CLI contract.
- `shift-tui` resolves its engine from `SHIFT_CLI_ENGINE`, then a sibling
  `shift-cli`, then `PATH`.
- `shift-cli formats --json --input …` returns schema version 1 capability
  data. Repeated inputs return the supported-format intersection, so the TUI
  cannot offer an output the shared registry would reject.
- Conversion commands are passed as an argv array—never through a shell. Output
  records are JSON strings on stdout; progress and diagnostics remain stderr.

## Interaction contract

The main view consists of an input queue, conversion settings, an activity bar,
and a shortcut footer. At 96 columns it uses a two-panel layout; below that it
stacks vertically. Every action in the command palette has a keyboard path and
all highlighted rows, buttons, toggles, picker entries, and output links accept
mouse input. File and output pickers support wheel scrolling and filter input.

OpenTUI owns terminal lifecycle, alternate-screen behavior, selection, mouse
tracking, and Windows/Linux/macOS rendering. Shift sets `exitOnCtrlC: false` so
it can cancel an active conversion before exiting and restores the renderer on
normal exit, `SIGHUP`, or `SIGTERM`.

## Building

Dependencies are pinned in `tui/package.json` and `package-lock.json`.

```sh
cd tui
npm ci --ignore-scripts
npm run typecheck
npm run test:node
bun test
bun run build
```

`bun run build:all` emits OpenTUI clients for glibc and musl Linux (x64/arm64),
macOS (x64/arm64), and Windows (x64). Baseline x64 variants avoid an AVX2
requirement. A complete cross-platform Shift distribution must place the native
Rust `shift-cli` engine beside the matching `shift-tui` client.

The macOS release workflow builds the current-architecture client and packages
both executables under `Shift.app/Contents/Resources/bin`. Package verification
executes the TUI's headless bridge to prove sibling engine discovery works.
