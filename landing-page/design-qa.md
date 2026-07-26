# Design QA

## Evidence

- Source direction: former study 06, Quiet
- Production capture: `/Users/denizbuldum/Documents/shift/landing-page/qa-production.png`
- Mobile capture: `/Users/denizbuldum/Documents/shift/landing-page/qa-mobile-production.png`
- Desktop viewport: 1600 × 900 CSS px
- Mobile viewport: 390 × 844 CSS px

## Final implementation

Direction 06 is now the sole landing page. The comparison switcher and the
other five study implementations have been removed. The requested product
statement is the dominant element at 50px on desktop and 27–34px on mobile,
while the original black, gray, and restrained dark-blue palette remains.

The page is deliberately sparse: wordmark, GitHub link, product statement,
download link, CLI install command, and a quiet engine index. It contains no
illustrations, screenshots, gradients, decorative cards, or icon set.

## Checks

- Exact lowercase statement preserved.
- Desktop and mobile document widths match their viewport widths.
- Copy interaction exposes both `copy` and `copied` accessible states.
- GitHub Pages build emits assets under `/shift/`.
- GitHub Pages workflow is manual-only and cannot deploy from a push.

## Findings

No actionable P0, P1, or P2 issues remain.

final result: passed
