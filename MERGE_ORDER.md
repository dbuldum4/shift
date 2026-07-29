# Open PR merge order

Recommended review and merge order for the open Shift PRs targeting `main`
(as of 2026-07-29). All remaining PRs currently merge cleanly alone once
rebased, but they thrash shared files (`conversion/mod.rs`, `app.rs`,
`shift-cli.rs`, `main.rs`, `session_settings.rs`), so order matters for
dependency correctness and rebase cost.

## Recommended merge order

| Order | PR | Title | Why here | Status |
|------:|----|--------|----------|--------|
| **1** | **#12** | Fix temporary directory test race | Tiny, pure test fix. Already duplicated inside **#8** and **#9**. Land once, then rebase those two and drop the duplicate commit. Stabilizes CI before bigger reviews. | **Merged** (Wave A) |
| **2** | **#13** | Add bounded binary artifact inspection | Mostly additive (`inspection.rs` + result card). Low product coupling; small surface vs other features. Good early confidence win. | **Merged** (Wave A) |
| **3** | **#15** | Add qpdf PDF toolkit | New conversion module + PDF options. Extends the existing `pdf_slice` story without depending on batch/recipes. | **Merged** (Wave B) |
| **4** | **#11** | Add Docling transcription and expanded formats | Extends an existing engine (Docling). New formats/options should land **before** recipes snapshot them. | **Merged** (Wave B) |
| **5** | **#8** | Add fit-to-size conversion goals | Cross-cutting `ConversionOptions` + FFmpeg/sips behavior. Core capability; recipes should be able to capture `target_size`. | **Merged** (Wave C, 2026-07-29) |
| **6** | **#10** | Complete batch folder workflows | Completes shared batch/folder orchestration (hierarchy, naming templates, per-item formats). Foundation for multi-file UX and for anything that applies settings across a queue. | **Merged** (Wave D, 2026-07-29) |
| **7** | **#9** | Add reusable conversion recipes | Snapshots options + wires CLI/app/batch. **Should come after** the option surface (#15, #11, #8) and batch plumbing (#10) stabilize, or recipes will immediately need a follow-up for missing knobs. | Open — **next** |
| **8** | **#14** | Add macOS workflow integrations | Finder open-with + `shift-cli watch`. Ingestion/automation layer; benefits from solid batch/path handling first. Least entangled with conversion engines. | Open |
| **9** | **#16** | Prepare Shift 1.0 release hardening | Version bump, packaging, release docs/preflight. **Last** so 1.0 metadata and notes match what actually shipped. | Open |

## Review waves

Parallel review is fine within a wave; merge is serial after the prior wave
lands and later PRs rebase.

| Wave | Review together | Merge after prior wave |
|------|-----------------|------------------------|
| **A** | #12, #13 | **Done** (merged 2026-07-29) |
| **B** | #15, #11 | **Done** (merged 2026-07-29; #15 then #11) |
| **C** | #8 | **Done** (merged 2026-07-29; review fixes for hop-1 target-size + PreferCopy coverage) |
| **D** | #10 | **Done** (merged 2026-07-29; rebased onto Waves A–C, hierarchy/templates/fan-out) |
| **E** | #9 | Merge E ← **next** |
| **F** | #14 | Merge F |
| **G** | #16 | Merge G → cut 1.0 |

## Notes

- After **#12** merges, rebase **#8** and **#9** and drop their duplicate
  “Fix temporary directory test race” commit. (**#8** done; still applies to **#9**.)
- Do not merge **#9** before **#8** / **#11** / **#15** / **#10** unless a
  follow-up is planned so recipes learn the new knobs and batch plumbing.
  (**#8**, **#11**, **#15**, and **#10** are in.)
- Do not merge **#16** early — release metadata should reflect the final feature
  set.
- Optional: slide **#13** to just before **#14** if binary inspection is treated
  as polish rather than 1.0-critical. Engine / options / batch / recipes order
  should stay as above. (**#13** already merged in Wave A.)
- Rebase **#9** (and later open PRs) onto current `main` before review so they
  pick up Waves A–D (fit-to-size, batch hierarchy/templates/fan-out, and
  intermediate-hop clearing of `target_size_bytes`).
