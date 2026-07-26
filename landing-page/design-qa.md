# Design QA

## Evidence

- Source: `/var/folders/tt/rd0pp0d57rsfk0x2dcslgv3w0000gn/T/codex-clipboard-44c03df0-48e2-46d0-9a05-108985d99247.png` (2820 × 1554)
- Primary direction: `/Users/denizbuldum/Documents/shift/landing-page/qa-01-statement.png` (1600 × 900)
- Normalized comparison: `/Users/denizbuldum/Documents/shift/landing-page/qa-comparison.png` (1600 × 450)
- Other desktop directions: `qa-02-manual.png` through `qa-06-quiet.png` (1600 × 900 each)
- Mobile direction 01: `/Users/denizbuldum/Documents/shift/landing-page/qa-mobile-statement.png` (390 × 1847)

## Fidelity

The replacements inherit GPUI’s black canvas, monospace voice, low-contrast rules,
quiet navigation, broad negative space, lowercase copy, and restrained link
treatment. Shift’s cold dark-blue selection/CTA treatment is the only material
color addition. Direction 01 is the closest structural heir; direction 03 explores
GPUI-scale typography while remaining text-only.

All six are single-column page compositions. They contain no illustrations,
screenshots, diagrams, decorative cards, gradients, or iconography. Every
direction uses the exact requested product statement:

> shift is a blazingly fast, native, opinionated, robust, and resilient file converter built for macOS

## Responsive and interaction checks

- Switched through all six directions at 1600 × 900 and 390 × 844.
- Every mobile state reported `scrollWidth: 390` and `clientWidth: 390`; no
  horizontal overflow was found.
- Direction 01’s full mobile page was visually inspected for type wrapping,
  row collapse, borders, and command-line containment.
- The top switcher remains sticky; mobile labels reduce to 01–06.
- Press feedback, keyboard focus, pointer-gated hover, and reduced-motion behavior
  are present.
- Vite production build and Sites compatibility tests pass.

## Comparison history

1. The original six broad visual explorations were removed after the brief
   narrowed to old 01’s structure and old 06’s cold-noir palette.
2. Six new studies were built from that one system: Statement, Manual, Entrance,
   Roster, Rule, and Quiet.
3. Desktop captures confirmed consistent monochrome treatment and one-column
   composition. Mobile measurements confirmed all six fit the viewport.
4. The final set keeps only restrained, functional motion.

## Findings

No actionable P0, P1, or P2 issues remain.

## Follow-up polish

- P3: after a direction is selected, remove the comparison switcher and tune the
  chosen page’s final content depth.

final result: passed
