# Motion review

| Before | After | Why |
| --- | --- | --- |
| Direction changes used a `420ms` keyframed entrance | Direction changes use a `220ms` opacity/8px translate entrance with the strong project ease-out | The switcher is used repeatedly while comparing studies; the shorter duration keeps it responsive and remains below the 300ms UI budget |
| The supported-format strip ran as a perpetual `26s` marquee | The strip is static | Ambient looping motion did not explain state or improve orientation, and it worked against the quiet GPUI-like personality |
| Noir stack cards moved up to 8px over `240ms` on hover | Stack cards move no more than 4px over `160ms` | The smaller, faster response preserves depth without turning a frequently explored surface into a decorative animation |

## Verdict

### Origin, physicality & cohesion

All pressable elements respond immediately with `scale(0.97)`. The remaining motion is restrained, uses transform/opacity, and matches the minimalist, technical character of the site.

### Accessibility

Hover motion is gated behind `@media (hover: hover) and (pointer: fine)`. `prefers-reduced-motion` replaces the direction entrance with a short opacity-only transition and removes transform transitions.

**Approve** — no feel-breaking regressions, no unjustified looping motion, UI durations are within bounds, animated properties are GPU-friendly, and reduced-motion handling is present.
