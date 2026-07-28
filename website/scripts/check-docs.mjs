import { access, readdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
);
const docsRoot = path.join(websiteRoot, 'docs', 'v3');
const languages = ['zh', 'en'];

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
  'sdk/rust.mdx',
  'sdk/typescript.mdx',
  'sdk/python.mdx',
  'sdk/go.mdx',
  'reference/index.mdx',
  'reference/platforms.mdx',
  'reference/troubleshooting.mdx',
];

const completeProgramContracts = {
  rust: [
    {
      pattern: /#\[tokio::main\]/,
      message: 'a Tokio entry-point attribute',
    },
    {
      pattern: /\basync\s+fn\s+main\s*\(/,
      message: 'an async main function',
    },
  ],
  typescript: [
    {
      pattern: /\basync\s+function\s+main\s*\(/,
      message: 'an async main function',
    },
    {
      pattern: /\bmain\(\)\.catch\s*\(/,
      message: 'top-level error handling',
    },
  ],
  python: [
    {
      pattern: /\b(?:async\s+)?def\s+main\s*\(/,
      message: 'a main function',
    },
    {
      pattern: /if\s+__name__\s*==\s*["']__main__["']\s*:/,
      message: 'a __main__ entry point',
    },
  ],
  go: [
    {
      pattern: /^\s*package\s+main\s*$/m,
      message: 'package main',
    },
    {
      pattern: /\bfunc\s+main\s*\(\s*\)/,
      message: 'a main function',
    },
  ],
};

async function collectFiles(directory, matcher) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await collectFiles(absolutePath, matcher)));
    } else if (matcher(entry.name)) {
      files.push(absolutePath);
    }
  }

  return files;
}

for (const language of languages) {
  for (const page of requiredPages) {
    await access(path.join(docsRoot, language, page));
  }
}

const pageSets = await Promise.all(
  languages.map(async (language) => {
    const languageRoot = path.join(docsRoot, language);
    const pages = await collectFiles(languageRoot, (name) =>
      /\.(md|mdx)$/.test(name),
    );
    return new Set(
      pages.map((file) =>
        path.relative(languageRoot, file).replaceAll('\\', '/'),
      ),
    );
  }),
);

const parityFailures = [];
for (const page of new Set([...pageSets[0], ...pageSets[1]])) {
  for (let index = 0; index < languages.length; index += 1) {
    if (!pageSets[index].has(page)) {
      parityFailures.push(`${languages[index]}/${page}`);
    }
  }
}

if (parityFailures.length > 0) {
  throw new Error(
    `Documentation language parity failed; missing:\n${parityFailures
      .map((page) => `  - ${page}`)
      .join('\n')}`,
  );
}

const placeholderFailures = [];
const translationFailures = [];
const programFailures = [];
const programCounts = Object.fromEntries(
  languages.map((language) => [
    language,
    Object.fromEntries(
      Object.keys(completeProgramContracts).map((name) => [name, 0]),
    ),
  ]),
);

for (const language of languages) {
  const languageRoot = path.join(docsRoot, language);

  for (const file of await collectFiles(languageRoot, (name) =>
    /\.(md|mdx)$/.test(name),
  )) {
    const contents = await readFile(file, 'utf8');
    const relativePath = `${language}/${path
      .relative(languageRoot, file)
      .replaceAll('\\', '/')}`;

    if (/\b(TODO|TBD|Lorem ipsum)\b/i.test(contents)) {
      placeholderFailures.push(relativePath);
    }
    if (
      language === 'zh' &&
      (contents.match(/\p{Script=Han}/gu) ?? []).length < 20
    ) {
      translationFailures.push(relativePath);
    }

    const fencePattern = /^```([A-Za-z0-9_-]+)[^\n]*\r?\n([\s\S]*?)^```\s*$/gm;
    for (const match of contents.matchAll(fencePattern)) {
      const languageName = match[1].toLowerCase();
      const contract = completeProgramContracts[languageName];
      if (!contract) {
        continue;
      }

      programCounts[language][languageName] += 1;
      for (const requirement of contract) {
        if (!requirement.pattern.test(match[2])) {
          programFailures.push(
            `${relativePath}: ${languageName} block is missing ${requirement.message}`,
          );
        }
      }
    }
  }
}

if (placeholderFailures.length > 0) {
  throw new Error(
    `Documentation contains placeholder text:\n${placeholderFailures
      .map((file) => `  - ${file}`)
      .join('\n')}`,
  );
}

if (translationFailures.length > 0) {
  throw new Error(
    `Chinese documentation is missing substantive Chinese content:\n${translationFailures
      .map((file) => `  - ${file}`)
      .join('\n')}`,
  );
}

for (const language of languages) {
  const quickStart = await readFile(
    path.join(docsRoot, language, 'guide', 'quick-start.mdx'),
    'utf8',
  );
  const networking = await readFile(
    path.join(docsRoot, language, 'guide', 'networking-compose.mdx'),
    'utf8',
  );

  for (const languageName of Object.keys(completeProgramContracts)) {
    if (!quickStart.includes(`\`\`\`${languageName}`)) {
      programFailures.push(
        `${language}/guide/quick-start.mdx: missing ${languageName} program`,
      );
    }

    if (programCounts[language][languageName] === 0) {
      programFailures.push(
        `${language}: no complete ${languageName} programs were found`,
      );
    }
  }

  if (!/^```acl(?:\s|$)/m.test(networking)) {
    programFailures.push(
      `${language}/guide/networking-compose.mdx: ACL example must use an acl fence`,
    );
  }
}

if (programFailures.length > 0) {
  throw new Error(
    `Executable example contract failed:\n${programFailures
      .map((failure) => `  - ${failure}`)
      .join('\n')}`,
  );
}

console.log(
  `Documentation contract verified: ${requiredPages.length} routes × ${languages.length} languages, complete Rust/TypeScript/Python/Go programs, and ACL fences.`,
);
