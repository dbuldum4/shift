# Design QA

## Evidence

- Source visual truth: `/var/folders/tt/rd0pp0d57rsfk0x2dcslgv3w0000gn/T/codex-clipboard-44c03df0-48e2-46d0-9a05-108985d99247.png`
- Primary implementation: `/Users/denizbuldum/Documents/shift/landing-page/qa-terminal.png`
- Full comparison: `/Users/denizbuldum/Documents/shift/landing-page/qa-comparison.png`
- Additional desktop directions:
  - `/Users/denizbuldum/Documents/shift/landing-page/qa-workbench.png`
  - `/Users/denizbuldum/Documents/shift/landing-page/qa-signal.png`
  - `/Users/denizbuldum/Documents/shift/landing-page/qa-receipt.png`
  - `/Users/denizbuldum/Documents/shift/landing-page/qa-blueprint.png`
  - `/Users/denizbuldum/Documents/shift/landing-page/qa-noir.png`
- Mobile implementation: `/Users/denizbuldum/Documents/shift/landing-page/qa-mobile-terminal.png`
- Source pixels: 2820 × 1554
- Desktop implementation: 1600 × 880 CSS px, 1600 × 880 captured px, device scale factor 1
- Mobile implementation: 390 × 844 CSS px, 390 × 844 captured px, device scale factor 1
- Comparison normalization: both desktop images were aspect-fit and centered into 800 × 440 frames, then placed side-by-side in a 1600 × 440 comparison
- State: initial/hero state, direction 01 selected

## Full-view comparison

The implementation carries forward the reference’s defining visual system: black canvas, Geist-like monospace typography, generous negative space, quiet gray navigation, thin low-contrast borders, left-led product statement, and a technical example surface on the right. Shift-specific copy and conversion UI replace GPUI-specific content rather than duplicating it.

The six directions now read as one monochrome family. Their differences come from content density, hero proportions, and technical storytelling rather than unrelated color systems.

## Focused-region comparison

A separate crop was not needed. The 1600px-wide side-by-side comparison keeps the hero typography, navigation density, primary CTA, and technical example card legible. The full-resolution individual captures were also inspected for code/output details, small labels, and border contrast.

## Required fidelity surfaces

- Fonts and typography: Geist Mono Variable loads locally through the project. Display text uses tighter tracking and leading; small navigation and labels stay lighter and more open. Hierarchy and wrapping remain stable at 1600px and 390px.
- Spacing and layout rhythm: the desktop studies preserve the reference’s broad two-column rhythm and large quiet fields. Mobile layouts collapse to one column. All six mobile states report a 390px document width with no horizontal page overflow.
- Colors and visual tokens: final palette is near-black, soft white, gray, and a restrained dark-blue/cyan accent used for selected state, output, or pipeline emphasis. Loud yellow, red, cream, and orange directions were removed.
- Image quality and asset fidelity: the reference did not require a hero illustration or product photography. Product examples are accessible HTML interface surfaces and use Phosphor icons; no placeholder imagery is present.
- Copy and content: every direction uses concrete Shift product language, supported format families, actual conversion engines, source-preservation behavior, and working GitHub/release destinations.

## Interaction and browser checks

- Direction switcher: all six buttons tested at 1600 × 880 and 390 × 844.
- Copy command: tested; successful state changes the accessible name from “Copy…” to “Copied…”.
- Navigation/CTA targets: inspected for valid in-page anchors and GitHub/release URLs.
- Console: no warnings or errors in the final desktop state.
- Reduced motion, reduced transparency, keyboard focus, pointer-gated hover, and press feedback are implemented.

## Comparison history

1. Initial implementation introduced saturated yellow, warm paper, orange, and editorial directions. This was a major mismatch with the user’s clarified “minimalist and monochrome” GPUI goal.
2. All six directions were rebuilt into a black/gray family with dark blue reserved for technical emphasis. New desktop captures confirmed the correction.
3. Mobile QA found the terminal hero’s grid item extending to x=402 in a 390px viewport. `min-width: 0` was added to the hero children; the final right edge is x=366 and document width remains 390px.
4. Motion review removed the looping marquee and shortened/reduced comparison animations.

## Findings

No actionable P0, P1, or P2 issues remain.

## Follow-up polish

- P3: once a direction is chosen, remove the comparison bar and tighten that direction’s below-the-fold narrative into the final production page.

final result: passed
