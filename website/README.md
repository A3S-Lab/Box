# A3S Box website

This directory contains the Rspress website published at
`https://a3s-lab.github.io/Box/`.

```bash
npm ci
npm run lint
npm run build
npm run check:site
```

Use `npm run dev` for local authoring. The site is versioned under `docs/v3`.
Chinese lives in `docs/v3/zh` and is served without a language prefix. The
complete English mirror lives in `docs/v3/en` and is served below `/en/`.
Language parity, complete SDK programs, the shared language Tabs, scroll-driven
line-focus tutorials, Agent Skill install routes, and the custom ACL grammar are
checked during every build. Run `npm run generate:tutorial` after changing the
TypeScript tutorial source; `npm run build` does this automatically. The
homepage uses a Canvas UI-inspired 2D grid with reduced-motion behavior;
attribution lives in `THIRD_PARTY_NOTICES.md`. All internal links must remain
valid when the site is hosted below the `/Box/` GitHub Pages base path.
