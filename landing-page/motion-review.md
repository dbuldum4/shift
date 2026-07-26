# Motion review

| Surface | Current behavior | Review |
| --- | --- | --- |
| Page entrance | 240ms opacity + 6px vertical entrance | Quiet acknowledgement of load without delaying access |
| Buttons and links | 130ms color/border response; 0.97 press scale | Immediate feedback without decorative motion |
| Hover | Applied only to fine pointers | Avoids sticky or accidental touch states |
| Reduced motion | 160ms opacity-only entrance; transforms removed | Preserves state feedback without spatial movement |

No looping animation, stagger, blur, spring overshoot, or layout-property
animation remains. Motion uses transform and opacity only for the initial page
entrance.

**Approve** — no feel-breaking regressions or accessibility issues found.
