import * as path from 'node:path';
import { defineConfig } from '@rspress/core';

const base = process.env.DOCS_BASE ?? '/Box/';
const siteOrigin = process.env.DOCS_ORIGIN ?? 'https://a3s-lab.github.io';

const aclLanguage = {
  name: 'acl',
  scopeName: 'source.acl',
  repository: {},
  patterns: [
    {
      begin: '/\\*',
      end: '\\*/',
      name: 'comment.block.acl',
    },
    {
      match: '(?://|#).*$',
      name: 'comment.line.acl',
    },
    {
      begin: '"',
      end: '"',
      name: 'string.quoted.double.acl',
      patterns: [
        {
          match: '\\\\(?:["\\\\/bfnrt]|u[0-9a-fA-F]{4})',
          name: 'constant.character.escape.acl',
        },
        {
          begin: '\\$\\{',
          end: '\\}',
          name: 'meta.interpolation.acl',
          patterns: [
            {
              match: '[A-Za-z_][\\w-]*',
              name: 'variable.other.interpolation.acl',
            },
          ],
        },
      ],
    },
    {
      match: '\\b[A-Za-z_][\\w-]*(?=\\s+(?:"(?:[^"\\\\]|\\\\.)*"\\s*)?\\{)',
      name: 'entity.name.type.block.acl',
    },
    {
      match: '\\b[A-Za-z_][\\w-]*(?=\\s*\\()',
      name: 'entity.name.function.acl',
    },
    {
      match: '\\b[A-Za-z_][\\w-]*(?=\\s*=)',
      name: 'variable.other.assignment.acl',
    },
    {
      match: '\\b(?:true|false|null)\\b',
      name: 'constant.language.acl',
    },
    {
      match: '\\b(?:0[xX][0-9a-fA-F]+|\\d+(?:\\.\\d+)?(?:[eE][+-]?\\d+)?)\\b',
      name: 'constant.numeric.acl',
    },
    {
      match: '==|!=|<=|>=|&&|\\|\\||[=+*/%<>!-]',
      name: 'keyword.operator.acl',
    },
    {
      match: '[{}\\[\\](),.:]',
      name: 'punctuation.separator.acl',
    },
  ],
};

export default defineConfig({
  root: path.join(__dirname, 'docs'),
  base,
  siteOrigin,
  title: 'A3S Box',
  description:
    '本地 OCI 工作负载运行时，默认使用独立内核 MicroVM 隔离，并提供 Rust、Go、Python 和 TypeScript 原生 SDK。',
  lang: 'zh',
  locales: [
    {
      lang: 'zh',
      label: '简体中文',
      title: 'A3S Box',
      description:
        '本地 OCI 工作负载运行时，默认使用独立内核 MicroVM 隔离，并提供 Rust、Go、Python 和 TypeScript 原生 SDK。',
    },
    {
      lang: 'en',
      label: 'English',
      title: 'A3S Box',
      description:
        'A local OCI workload runtime with dedicated-kernel MicroVM isolation, an explicit shared-kernel Sandbox, and native SDKs for Rust, TypeScript, Python, and Go.',
    },
  ],
  icon: '/favicon.svg',
  logo: '/a3s-box-mark.svg',
  logoText: 'A3S Box',
  outDir: 'doc_build',
  llms: true,
  markdown: {
    shiki: {
      langs: [
        'bash',
        'dockerfile',
        'go',
        'powershell',
        'python',
        'rust',
        'typescript',
        aclLanguage,
      ],
    },
  },
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
    localeRedirect: 'never',
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
