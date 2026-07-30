import { access, readdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
);
const docsRoot = path.join(websiteRoot, 'docs', 'v3');
const languages = ['zh', 'en'];
const tutorialComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'RuntimeTutorial.tsx'),
  'utf8',
);
const tutorialSteps = JSON.parse(
  await readFile(
    path.join(websiteRoot, 'theme', 'generated', 'runtime-tutorial.json'),
    'utf8',
  ),
);

const requiredPages = [
  'index.mdx',
  'guide/index.mdx',
  'guide/installation.mdx',
  'guide/quick-start.mdx',
  'guide/agent-skill.mdx',
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
const experienceFailures = [];
const programCounts = Object.fromEntries(
  languages.map((language) => [
    language,
    Object.fromEntries(
      Object.keys(completeProgramContracts).map((name) => [name, 0]),
    ),
  ]),
);

if (tutorialSteps.length !== 5) {
  experienceFailures.push(
    `runtime tutorial: expected 5 steps, found ${tutorialSteps.length}`,
  );
}

for (const step of tutorialSteps) {
  const focusAnnotation = step.highlighted?.annotations?.find(
    (annotation) => annotation.name === 'focus',
  );
  if (
    !step.id ||
    !step.code ||
    !Array.isArray(step.focus) ||
    step.focus.length !== 2 ||
    focusAnnotation?.fromLineNumber !== step.focus[0] ||
    focusAnnotation?.toLineNumber !== step.focus[1]
  ) {
    experienceFailures.push(
      `runtime tutorial: ${step.id || 'unknown step'} is missing matching line-focus data`,
    );
  }
}

for (const marker of [
  'rootMargin="-42% 0px -42% 0px"',
  "selectOn={['scroll']}",
  'className="box-tutorial-sticky"',
  'className="box-code-line is-focused"',
  'data-runtime-tutorial="true"',
]) {
  if (!tutorialComponent.includes(marker)) {
    experienceFailures.push(
      `RuntimeTutorial.tsx: missing scroll-and-focus contract ${marker}`,
    );
  }
}

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
  const homepage = await readFile(
    path.join(docsRoot, language, 'index.mdx'),
    'utf8',
  );
  const quickStart = await readFile(
    path.join(docsRoot, language, 'guide', 'quick-start.mdx'),
    'utf8',
  );
  const agentSkill = await readFile(
    path.join(docsRoot, language, 'guide', 'agent-skill.mdx'),
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

    if (!homepage.includes(`\`\`\`${languageName}`)) {
      programFailures.push(
        `${language}/index.mdx: missing visible homepage ${languageName} program`,
      );
    }
  }

  const asyncPythonContract = [
    'import asyncio',
    'from a3s_box import AsyncSandbox',
    'async def main() -> None:',
    'async with await AsyncSandbox.create',
    'result = await sandbox.commands.run',
    'asyncio.run(main())',
  ];
  for (const [filePath, source] of [
    [`${language}/index.mdx`, homepage],
    [`${language}/guide/quick-start.mdx`, quickStart],
  ]) {
    for (const marker of asyncPythonContract) {
      if (!source.includes(marker)) {
        programFailures.push(
          `${filePath}: incomplete asynchronous Python SDK program; missing ${marker}`,
        );
      }
    }
  }

  if (!/^```acl(?:\s|$)/m.test(networking)) {
    programFailures.push(
      `${language}/guide/networking-compose.mdx: ACL example must use an acl fence`,
    );
  }

  for (const marker of [
    'integrations/skills/install.sh',
    'sh -s -- --home a3s-code',
    'sh -s -- --home codex',
    'sh -s -- --home claude',
    'sh -s -- --home all',
    '/a3s-box',
    'allowed-tools',
  ]) {
    if (!agentSkill.includes(marker)) {
      experienceFailures.push(
        `${language}/guide/agent-skill.mdx: missing Skill integration marker ${marker}`,
      );
    }
  }

  const tabsContract = [
    '<Tabs groupId="box-sdk-language" className="box-sdk-tabs">',
    '<Tab label="Rust" value="rust">',
    '<Tab label="TypeScript" value="typescript">',
    '<Tab label="Python" value="python">',
    '<Tab label="Go" value="go">',
  ];
  for (const marker of tabsContract) {
    if (!quickStart.includes(marker)) {
      experienceFailures.push(
        `${language}/guide/quick-start.mdx: missing SDK Tabs marker ${marker}`,
      );
    }
  }

  for (const marker of [
    "import { RuntimeTutorial } from '../../../../theme/components/RuntimeTutorial';",
    `<RuntimeTutorial locale="${language}" />`,
  ]) {
    if (!quickStart.includes(marker)) {
      experienceFailures.push(
        `${language}/guide/quick-start.mdx: missing runtime tutorial marker ${marker}`,
      );
    }
  }

  for (const marker of [
    'groupId="box-sdk-language"',
    'className="box-sdk-tabs box-home-sdk-tabs"',
    "import { RuntimeTutorial } from '../../../theme/components/RuntimeTutorial';",
    `<RuntimeTutorial locale="${language}" />`,
  ]) {
    if (!homepage.includes(marker)) {
      experienceFailures.push(
        `${language}/index.mdx: missing visible homepage experience marker ${marker}`,
      );
    }
  }
}

if (programFailures.length > 0) {
  throw new Error(
    `Executable example contract failed:\n${programFailures
      .map((failure) => `  - ${failure}`)
      .join('\n')}`,
  );
}

if (experienceFailures.length > 0) {
  throw new Error(
    `Documentation experience contract failed:\n${experienceFailures
      .map((failure) => `  - ${failure}`)
      .join('\n')}`,
  );
}

console.log(
  `Documentation contract verified: ${requiredPages.length} routes × ${languages.length} languages, Agent Skill integration, complete Rust/TypeScript/Python/Go programs in Tabs, the five-step line-focus tutorial, and ACL fences.`,
);
