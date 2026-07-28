import { access, readdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
);
const outputRoot = path.join(websiteRoot, 'doc_build');
const base = '/Box/';

const routeFiles = [
  'index.html',
  'guide/index.html',
  'guide/installation.html',
  'guide/quick-start.html',
  'guide/architecture.html',
  'guide/images-builds.html',
  'guide/networking-compose.html',
  'guide/storage-snapshots.html',
  'guide/windows.html',
  'sdk/index.html',
  'sdk/rust.html',
  'sdk/typescript.html',
  'sdk/python.html',
  'sdk/go.html',
  'reference/index.html',
  'reference/platforms.html',
  'reference/troubleshooting.html',
];

const requiredFiles = [
  ...routeFiles,
  ...routeFiles.map((file) => `en/${file}`),
  'llms.txt',
  'llms-full.txt',
  'a3s-box-mark.svg',
  'social-card.svg',
];

async function collectHtmlFiles(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await collectHtmlFiles(absolutePath)));
    } else if (entry.name.endsWith('.html')) {
      files.push(absolutePath);
    }
  }

  return files;
}

for (const file of requiredFiles) {
  await access(path.join(outputRoot, file));
}

const rootHomepage = await readFile(
  path.join(outputRoot, 'index.html'),
  'utf8',
);
const englishHomepage = await readFile(
  path.join(outputRoot, 'en', 'index.html'),
  'utf8',
);

if (!rootHomepage.includes('让 Agent 任务')) {
  throw new Error('The default homepage is not rendered in Chinese.');
}
if (!rootHomepage.includes(`${base}en/`)) {
  throw new Error('The Chinese homepage does not expose the English locale.');
}
if (!englishHomepage.includes('Run agent workloads')) {
  throw new Error('The /en/ homepage is not rendered in English.');
}
for (const route of [
  `${base}en/guide/quick-start.html`,
  `${base}en/sdk/rust.html`,
  `${base}en/sdk/typescript.html`,
  `${base}en/sdk/python.html`,
  `${base}en/sdk/go.html`,
]) {
  if (!englishHomepage.includes(route)) {
    throw new Error(
      `The English homepage is missing its localized ${route} link.`,
    );
  }
}

for (const localePrefix of ['', 'en/']) {
  const relativePath = `${localePrefix}guide/networking-compose.html`;
  const html = await readFile(path.join(outputRoot, relativePath), 'utf8');
  const aclStart = html.indexOf('class="rp-codeblock language-acl"');
  const aclEnd = html.indexOf('</pre>', aclStart);
  const aclMarkup =
    aclStart >= 0 && aclEnd > aclStart ? html.slice(aclStart, aclEnd) : '';
  const tokenKinds = new Set(
    [...aclMarkup.matchAll(/var\(--shiki-token-([a-z-]+)\)/g)].map(
      (match) => match[1],
    ),
  );

  if (!aclMarkup.includes('data-lang="acl"')) {
    throw new Error(
      `${relativePath} does not contain an ACL-highlighted block.`,
    );
  }
  if (tokenKinds.size < 3) {
    throw new Error(
      `${relativePath} did not receive token-level ACL syntax highlighting.`,
    );
  }
}

const brokenReferences = [];
const htmlFiles = await collectHtmlFiles(outputRoot);
const referencePattern = /(?:href|src)="([^"]+)"/g;

for (const htmlFile of htmlFiles) {
  const html = await readFile(htmlFile, 'utf8');

  for (const [, rawReference] of html.matchAll(referencePattern)) {
    if (
      rawReference.startsWith('#') ||
      rawReference.startsWith('data:') ||
      rawReference.startsWith('mailto:') ||
      /^[a-z]+:\/\//i.test(rawReference)
    ) {
      continue;
    }

    if (rawReference.startsWith('/') && !rawReference.startsWith(base)) {
      brokenReferences.push(
        `${path.relative(outputRoot, htmlFile)} -> ${rawReference} (outside ${base})`,
      );
      continue;
    }

    if (!rawReference.startsWith(base)) {
      continue;
    }

    const withoutBase = rawReference
      .slice(base.length)
      .split(/[?#]/, 1)[0]
      .replace(/\/+/g, '/');
    const outputPath =
      withoutBase === '' || withoutBase.endsWith('/')
        ? path.join(outputRoot, withoutBase, 'index.html')
        : path.join(outputRoot, withoutBase);

    try {
      await access(outputPath);
    } catch {
      brokenReferences.push(
        `${path.relative(outputRoot, htmlFile)} -> ${rawReference}`,
      );
    }
  }
}

if (brokenReferences.length > 0) {
  throw new Error(
    `Built-site reference check failed:\n${brokenReferences
      .map((reference) => `  - ${reference}`)
      .join('\n')}`,
  );
}

console.log(
  `Bilingual built-site references and ACL highlighting verified across ${htmlFiles.length} HTML pages.`,
);
