# A3S Box website

This directory contains the Rspress website published at
`https://a3s-lab.github.io/Box/`.

```bash
npm ci
npm run lint
npm run build
npm run check:site
```

Use `npm run dev` for local authoring. The site is versioned under `docs/v3`;
the default version is served without a version prefix. All internal links must
remain valid when the site is hosted below the `/Box/` GitHub Pages base path.
