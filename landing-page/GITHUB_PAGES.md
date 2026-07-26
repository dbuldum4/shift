# GitHub Pages

The landing page is prepared for the project URL:

`https://dbuldum4.github.io/shift/`

The workflow is intentionally manual and will not deploy on push.

## First deployment

1. In the repository settings, open **Pages** and select **GitHub Actions** as
   the source.
2. Open **Actions**, select **Deploy landing page to GitHub Pages**, and choose
   **Run workflow**.

The workflow installs with Bun, builds from `landing-page`, and publishes
`landing-page/dist/client`. Vite applies the `/shift/` asset base only inside
GitHub Actions, so local development continues to use `/`.
