import * as path from 'node:path';
import { defineConfig } from '@rspress/core';

const base = process.env.DOCS_BASE ?? '/Box/';
const siteOrigin = process.env.DOCS_ORIGIN ?? 'https://a3s-lab.github.io';

export default defineConfig({
  root: path.join(__dirname, 'docs'),
  base,
  siteOrigin,
  title: 'A3S Box',
  description:
    'A local OCI workload runtime with dedicated-kernel MicroVM isolation, an explicit shared-kernel Sandbox, and native SDKs for Rust, Go, Python, and TypeScript.',
  lang: 'en',
  icon: '/favicon.svg',
  logo: '/a3s-box-mark.svg',
  logoText: 'A3S Box',
  outDir: 'doc_build',
  llms: true,
  multiVersion: {
    default: 'v3',
    versions: ['v3'],
  },
  head: [
    ['meta', { name: 'theme-color', content: '#090c0b' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:site_name', content: 'A3S Box' }],
    [
      'meta',
      {
        property: 'og:image',
        content: `${siteOrigin}${base}social-card.svg`,
      },
    ],
    ['meta', { name: 'twitter:card', content: 'summary_large_image' }],
    (route) => [
      'link',
      {
        rel: 'canonical',
        href: `${siteOrigin}${base.replace(/\/$/, '')}${route.routePath}`,
      },
    ],
  ],
  themeConfig: {
    darkMode: 'force-dark',
    search: true,
    enableContentAnimation: true,
    editLink: {
      docRepoBaseUrl: 'https://github.com/A3S-Lab/Box/tree/main/website/docs',
    },
    lastUpdated: true,
    llmsUI: {
      placement: 'outline',
      viewOptions: ['markdownLink', 'chatgpt', 'claude'],
    },
    socialLinks: [
      {
        icon: 'github',
        mode: 'link',
        content: 'https://github.com/A3S-Lab/Box',
      },
    ],
  },
});
