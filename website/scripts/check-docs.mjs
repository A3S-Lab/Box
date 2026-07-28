import { access, readdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
);
const docsRoot = path.join(websiteRoot, 'docs', 'v3');

const requiredPages = [
  'index.mdx',
  'guide/index.mdx',
  'guide/installation.mdx',
  'guide/quick-start.mdx',
  'guide/architecture.mdx',
  'guide/images-builds.mdx',
  'guide/networking-compose.mdx',
  'guide/storage-snapshots.mdx',
  'guide/windows.mdx',
  'sdk/index.mdx',
  'sdk/go.mdx',
  'sdk/rust.mdx',
  'sdk/python.mdx',
  'sdk/typescript.mdx',
  'reference/index.mdx',
  'reference/platforms.mdx',
  'reference/troubleshooting.mdx',
];

for (const page of requiredPages) {
  await access(path.join(docsRoot, page));
}

async function collectMarkdownFiles(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await collectMarkdownFiles(absolutePath)));
    } else if (/\.(md|mdx)$/.test(entry.name)) {
      files.push(absolutePath);
    }
  }

  return files;
}

const invalid = [];
for (const file of await collectMarkdownFiles(docsRoot)) {
  const contents = await readFile(file, 'utf8');
  if (/\b(TODO|TBD|Lorem ipsum)\b/i.test(contents)) {
    invalid.push(path.relative(docsRoot, file));
  }
}

if (invalid.length > 0) {
  throw new Error(
    `Documentation contains placeholder text:\n${invalid
      .map((file) => `  - ${file}`)
      .join('\n')}`,
  );
}

console.log(`Documentation contract verified: ${requiredPages.length} routes.`);
