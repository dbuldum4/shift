# Shift security & reliability hardening plan

**Date:** 2026-07-30
**Branch base:** `main` (`7649762`)
**Scope:** Findings 1–64 from the security/reliability audit.

## Goals

Close P1 SSRF, release-pipeline, and related P2/P3 reliability gaps without
changing the product conversion contract (same modules, same capability lists,
same CLI/app surface semantics unless a finding requires safer CLI input).

## Non-goals

- New conversion engines or format pairs
- Gatekeeper notarization
- Full multi-process distributed locking beyond history/settings safety

## Architecture principles

1. **Fail closed** on network policy, DNS errors, and size limits.
2. **Validate every hop** (redirects, DNS, connection) for public URL fetches.
3. **Private-by-default** filesystem permissions (0700/0600) for secrets and temps.
4. **Bounded resources** for paste, expand, history, cache, batch workers, engines.
5. **Atomic exclusive writes** for destinations and settings.
6. **Sanitize credentials** before argv, stderr, history, and UI.

---

## PR Plan

### PR 1: SSRF and network hardening

- **Description:** Fix public-URL validation to revalidate redirects, pin
  resolved addresses or validate each connection, fail closed on DNS errors,
  bound DNS preflight with cancellation, trust absolute curl (or native client),
  keep credentials out of child argv/errors, and sanitize Defuddle stderr before
  history. Covers findings **1, 2, 14, 17, 23, 27, 29**.
- **Files/components affected:** `src/conversion/defuddle.rs`,
  `src/conversion/magic_paste.rs`, `src/conversion/process.rs` (shared helpers
  if needed), unit tests under those modules
- **Dependencies:** None
- **Remediation notes:**
  - Extract shared `url_policy` helpers if duplication is high.
  - Prefer connecting with pinned `SocketAddr` after policy check, or
    re-resolve and re-check before every connect/redirect hop.
  - DNS: fail closed on lookup error when private blocking is on; use a
    `recv_timeout`/`thread::spawn` deadline and honor cancel.
  - Resolve `/usr/bin/curl` (macOS) / known path, not bare `curl`.
  - Cap redirect header capture size; unique header temp names.
  - Redact credentials in all error strings and history.

### PR 2: Release pipeline security and multi-arch

- **Description:** Multi-arch or universal macOS builds; fix checksum path
  basenames; validate workflow_dispatch `tag` before shell use (env + semver
  allow-list); pin action SHAs and tool versions; align Python version docs with
  launchers; Finder UTIs parity where practical. Covers **3, 4, 18, 20, 22, 57**.
- **Files/components affected:** `.github/workflows/release.yml`,
  `.github/workflows/ci.yml`, `scripts/package-macos.sh`,
  `scripts/verify-macos-package.sh`, `README.md`, optional registry parity test
- **Dependencies:** None
- **Remediation notes:**
  - Matrix `macos-14`/`macos-13` (arm64/x86_64) or lipo universal binary.
  - Write checksums with `basename` (cd into dist first).
  - `RELEASE_TAG` env var; `^v?[0-9]+\.[0-9]+\.[0-9]+([.-].*)?$` validation.
  - Pin `actions/checkout`, `setup-node`, `setup-uv` to full SHAs.

### PR 3: Process, temp, operand, and password-file safety

- **Description:** Create password files with mode 0600 before write; exclusive
  no-replace destination opens; pass operands after `--` / absolute paths;
  bound atomic temp name length; private temp dirs (0700) and sensitive files
  (0600); monitor output size during write; harden cancel/timeout against
  orphan descendants. Covers **5, 6, 7, 8, 24, 25, 26**.
- **Files/components affected:** `src/conversion/process.rs`,
  `src/conversion/mod.rs`, `src/conversion/batch.rs`, `src/conversion/qpdf.rs`,
  `src/conversion/pdf_slice.rs`, conversion modules that shell out (ffmpeg,
  pandoc, markitdown, docling, defuddle, sips, qpdf)
- **Dependencies:** None
- **Remediation notes:**
  - Unix: `OpenOptionsExt::mode(0o600)` then write password.
  - Destination: `create_new(true)` or exclusive rename; document force path.
  - Temp name: short slug + hash, reserve suffix budget under NAME_MAX.
  - Output size: poll file size in wait loop or engine-level caps.

### PR 4: Expansion, batch, and watch hardening

- **Description:** Canonical-root containment for recursive expand; global file
  budget + dedupe across roots; stream/cap expansion; off-UI-thread expansion
  in app; worker cap; collision HashSet + case-aware keys; watch state after
  outcome; continue on transient NotFound; global queue admission; cancel exit
  codes. Covers **15, 16, 33, 34, 35, 36 (recipes partial), 37, 38, 39, 41, 59**.
- **Files/components affected:** `src/conversion/sources.rs`,
  `src/conversion/batch.rs`, `src/conversion/watch.rs`,
  `src/conversion/recipes.rs` or `src/recipes.rs`, `src/app.rs`,
  `src/bin/shift-cli.rs`
- **Dependencies:** PR 3 (destination exclusive create helpers if shared)
- **Remediation notes:**
  - `canonicalize` root; reject symlink targets outside root.
  - Shared `ExpandBudget` / dedupe set for multi-root CLI.
  - Default worker max (e.g. min(cpus, 4)) + env override.
  - Cancel → exit 130 consistently.

### PR 5: Magic-paste bounds, staging cleanup, CLI secrets

- **Description:** Token/byte/clipboard limits; track and delete staged downloads;
  durable URL basename for outputs; PDF password via stdin/fd/prompt; path vs
  command ambiguity; safe path printing; reject invalid media options on
  non-media routes. Covers **13, 28, 32, 40, 60, 61, 63**.
- **Files/components affected:** `src/conversion/magic_paste.rs`,
  `src/bin/shift-cli.rs`, `src/app.rs`, `src/conversion/mod.rs`
- **Dependencies:** PR 1 (shared URL/redaction helpers)
- **Remediation notes:**
  - `--pdf-password -` reads stdin; or `--pdf-password-file` with 0600 check.
  - Prefer existing path over subcommand name when first arg exists as file.
  - NUL-delimited optional flag or JSON path emission for machine use.

### PR 6: Artifact cache integrity and leases

- **Description:** Copy exports instead of hard-link mutation; cryptographic
  digest + length verification/manifest; purge after writes and periodically;
  purge shares staging mutex / leases; background validation for large reuse.
  Covers **11, 12, 48, 49, 56** (+ cache half of **64**).
- **Files/components affected:** `src/artifact_cache.rs`, `src/app.rs`
- **Dependencies:** None (merge carefully with PR 4/7 on `app.rs`)
- **Remediation notes:**
  - Prefer BLAKE3 or SHA-256 sidecars over FNV for verification.
  - Staging leases set prevents purge races.
  - Keep FNV only as optional fast path if still needed for naming, never as sole trust.

### PR 7: History durability and multi-process safety

- **Description:** Bound rows and lazy-load BLOBs; transactional migration with
  count verification; surface corruption without resetting IDs; save backoff;
  flush on quit; SQLite-generated IDs; VACUUM/secure retention policy; private
  file modes; lossless path storage; intern `qpdf`. Covers **10, 42, 43, 44, 45,
  46, 47, 55, 62** (+ history half of **64**).
- **Files/components affected:** `src/history.rs`, `src/app.rs`
- **Dependencies:** None (merge carefully with PR 4/6 on `app.rs`)
- **Remediation notes:**
  - Load metadata without artifact blobs; fetch blob on demand.
  - Lower hard cap or enforce total byte budget.
  - `INTEGER PRIMARY KEY AUTOINCREMENT` for ids.

### PR 8: Settings, UI secrets, docs, engine resource limits

- **Description:** Unique settings temps + locking; preserve unknown fields /
  refuse downgrade; quarantine bad settings; bounded JSON read; atomic
  preferences write; masked PDF password UI with clear-on-source-change;
  README accuracy; spreadsheet byte limits pre-materialization; qpdf page ZIP
  budgets; Docling frame-rate bounds. Covers **9, 21, 30, 31, 50, 51, 52, 53,
  54, 58**.
- **Files/components affected:** `src/session_settings.rs`, `src/preferences.rs`,
  `src/main.rs`, `src/text_input.rs`, `src/app.rs`, `README.md`,
  `src/conversion/spreadsheet.rs`, `src/conversion/qpdf.rs`,
  `src/conversion/docling.rs`
- **Dependencies:** PR 3 (password-file / private perms helpers if shared);
  PR 4 for app expansion docs consistency
- **Remediation notes:**
  - Spreadsheet: file size + decompressed estimate + per-cell byte caps.
  - qpdf: max pages, max intermediate files/bytes.
  - Docling: min interval, max scene rate, duration-derived frame cap.

---

## Linearized stack order

```
main
 └─ PR1 ssrf-network
     └─ PR2 release-ci
         └─ PR3 process-fs-safety
             └─ PR4 expansion-batch-watch
                 └─ PR5 paste-cli-secrets
                     └─ PR6 artifact-cache
                         └─ PR7 history
                             └─ PR8 settings-ui-engines
```

Independent pairs for **parallel implementation** (worktrees off `main`):
- Level 0: PR1, PR2, PR3, PR6 (mostly disjoint files)
- Level 1: PR4 (after PR3), PR5 (after PR1), PR7 (after PR6 if app conflicts)
- Level 2: PR8 (after PR3 + PR4)

Stack assembly cherry-picks in linearized order and resolves `app.rs` /
`shift-cli.rs` / `magic_paste.rs` / `qpdf.rs` conflicts.

## Testing requirements (every PR)

```sh
cargo fmt --check
cargo lint   # if available; else cargo clippy --all-targets -- -D warnings where project uses it
cargo test --all-targets
```

Prefer fake executables over network/binary deps. Add unit tests for each closed finding class.

## Stack tooling

Use GitHub native **gh-stack** (`gh stack init` / `gh stack submit`) so PRs form a
reviewable stack with correct base branches.
