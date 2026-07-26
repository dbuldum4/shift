# Prototype Instructions

Run the local server yourself and open the preview in the browser available to this environment. Do not give the user server-start instructions when you can run it.

Before making substantial visual changes, use the Product Design plugin's `get-context` skill when the visual source is unclear or no longer matches the current goal. When the user gives durable prototype-specific design feedback, preferences, or decisions, record them in `AGENTS.md`.

When implementing from a selected generated mock, treat that image as the source of truth for layout, component anatomy, density, spacing, color, typography, visible content, and hierarchy.

Build app UI in `src/`. Keep `.openai/hosting.json`, `worker/index.js`, `scripts/prepare-sites-build.mjs`, and `tests/sites-worker.test.mjs` intact so the same local prototype can be handed to Sites. Before a Sites handoff, run `npm run build` and `npm run test:sites`; the build must leave `dist/client/index.html`, `dist/server/index.js`, and `dist/.openai/hosting.json`.

## Prototype direction

- Present 5–10 genuinely distinct landing-page directions behind a persistent top switcher so the user can compare and choose one.
- Use Geist Mono and keep each direction grounded in Shift's native file-conversion product and the sparse, typographic spirit of the GPUI landing page.
- Keep every direction minimalist and monochrome like the GPUI reference. Variation should come from layout and information architecture, not loud art direction.
- Permit only a restrained touch of dark blue for code, output, or interaction emphasis.
- Remove the original directions. Replace them with six new directions numbered 01–06, derived from original direction 01's structure and original direction 06's cold noir palette.
- New directions must be extremely minimalist, single-column, and contain no product mockups, diagrams, illustrations, cards-as-visuals, or other decorative visuals.
- Use the exact lowercase positioning line: “shift is a blazingly fast, native, opinionated, robust, and resilient file converter built for macOS”.
- Take strong structural cues from the GPUI reference: sparse navigation, small lowercase wordmark, large negative space, modest monospace sizing, understated links, and documentation-like vertical flow.
