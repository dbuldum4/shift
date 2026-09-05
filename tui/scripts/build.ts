import { mkdir, rm } from "node:fs/promises"
import { join } from "node:path"
import { createSolidTransformPlugin } from "@opentui/solid/bun-plugin"

type Target = { os: "linux" | "darwin" | "windows"; arch: "x64" | "arm64"; baseline?: boolean; musl?: boolean }

const targets: Target[] = [
  { os: "linux", arch: "x64" },
  { os: "linux", arch: "x64", baseline: true },
  { os: "linux", arch: "arm64" },
  { os: "linux", arch: "x64", musl: true },
  { os: "linux", arch: "arm64", musl: true },
  { os: "darwin", arch: "x64" },
  { os: "darwin", arch: "arm64" },
  { os: "windows", arch: "x64" },
  { os: "windows", arch: "x64", baseline: true },
]

const single = process.argv.includes("--single")
if (!single) {
  const install = Bun.spawnSync(
    [process.execPath, "install", "--os=*", "--cpu=*", "--no-save", "@opentui/core@0.4.5"],
    { cwd: join(import.meta.dirname, ".."), stdout: "inherit", stderr: "inherit" },
  )
  if (!install.success) throw new Error("Could not install OpenTUI native packages for every target")
}

const hostOs = process.platform === "win32" ? "windows" : process.platform
const selected = single
  ? targets.filter((target) => target.os === hostOs && target.arch === process.arch && !target.baseline && !target.musl)
  : targets

if (selected.length === 0) throw new Error(`No OpenTUI build target for ${process.platform}/${process.arch}`)

const root = join(import.meta.dirname, "..")
const output = join(root, "dist")
await rm(output, { recursive: true, force: true })
await mkdir(output, { recursive: true })
const plugin = createSolidTransformPlugin()

for (const target of selected) {
  const parts = [
    "bun",
    target.os,
    target.arch,
    target.baseline ? "baseline" : undefined,
    target.musl ? "musl" : undefined,
  ].filter(Boolean)
  const compileTarget = parts.join("-") as Bun.Build.CompileTarget
  const suffix = [target.os, target.arch, target.baseline ? "baseline" : undefined, target.musl ? "musl" : undefined]
    .filter(Boolean)
    .join("-")
  const outfile = join(output, `shift-tui-${suffix}${target.os === "windows" ? ".exe" : ""}`)
  console.log(`Building ${compileTarget}`)
  const result = await Bun.build({
    entrypoints: [join(root, "src/index.tsx")],
    tsconfig: join(root, "tsconfig.json"),
    plugins: [plugin],
    format: "esm",
    minify: true,
    sourcemap: "none",
    compile: {
      target: compileTarget,
      outfile,
      autoloadBunfig: false,
      autoloadDotenv: false,
      autoloadTsconfig: true,
      autoloadPackageJson: true,
      windows: {},
    },
    define: {
      "process.env.OPENTUI_LIBC": JSON.stringify(target.musl ? "musl" : "glibc"),
    },
  })
  if (!result.success) {
    for (const log of result.logs) console.error(log)
    process.exit(1)
  }
}
