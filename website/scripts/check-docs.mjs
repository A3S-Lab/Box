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
const homeComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'HomeLayout.tsx'),
  'utf8',
);
const homeContent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'home-content.ts'),
  'utf8',
);
const featureComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'RuntimeFeatureShowcase.tsx'),
  'utf8',
);
const performanceComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'PerformanceMetrics.tsx'),
  'utf8',
);
const installComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'BoxInstallSwitcher.tsx'),
  'utf8',
);
const terminalComponent = await readFile(
  path.join(websiteRoot, 'theme', 'components', 'RuntimeTerminalShowcase.tsx'),
  'utf8',
);
const heroStyles = (
  await Promise.all(
    ['hero-install.css', 'hero-terminal.css'].map((file) =>
      readFile(path.join(websiteRoot, 'theme', file), 'utf8'),
    ),
  )
).join('\n');
const featureStyles = (
  await Promise.all(
    [
      'runtime-features.css',
      'runtime-isolation.css',
      'runtime-tee.css',
      'runtime-features-responsive.css',
    ].map((file) => readFile(path.join(websiteRoot, 'theme', file), 'utf8')),
  )
).join('\n');
const performanceStyles = await readFile(
  path.join(websiteRoot, 'theme', 'performance-metrics.css'),
  'utf8',
);
const buttonStyles = await readFile(
  path.join(websiteRoot, 'theme', 'button-orbit.css'),
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
  'reference/performance.mdx',
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

function recordMarkerOrder(source, markers, label, failures) {
  let previousIndex = -1;
  for (const marker of markers) {
    const markerIndex = source.indexOf(marker);
    if (markerIndex <= previousIndex) {
      failures.push(`${label}: missing or out of order at ${marker}`);
      return;
    }
    previousIndex = markerIndex;
  }
}

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

const homepageSequence = [
  '<RuntimeFeatureShowcase',
  '<PerformanceMetrics',
  'id="platform-support"',
  'id="runtime-capabilities"',
  'id="native-sdks"',
  'id="sdk-code-tour"',
  '<AgentSkillSection',
  'id="home-cta"',
];
let previousHomepageIndex = -1;
for (const marker of homepageSequence) {
  const markerIndex = homeComponent.indexOf(marker);
  if (markerIndex <= previousHomepageIndex) {
    experienceFailures.push(
      `HomeLayout.tsx: homepage narrative sequence is missing or out of order at ${marker}`,
    );
  }
  previousHomepageIndex = markerIndex;
}
if (homeComponent.includes('box-principles')) {
  experienceFailures.push(
    'HomeLayout.tsx: duplicated isolation principles must not return after the core mechanisms',
  );
}

for (const marker of [
  "from 'simple-icons'",
  "id: 'unix'",
  "id: 'windows'",
  "id: 'homebrew'",
  "id: 'rust'",
  "id: 'go'",
  "id: 'python'",
  "id: 'typescript'",
  'role="tablist"',
  "event.key === 'ArrowRight'",
  "event.key === 'Home'",
  'navigator.clipboard.writeText',
]) {
  if (!installComponent.includes(marker)) {
    experienceFailures.push(
      `BoxInstallSwitcher.tsx: missing install-switcher contract ${marker}`,
    );
  }
}

for (const marker of [
  'data-terminal-phase={phase}',
  'data-terminal-scenario={scenario.id}',
  'IntersectionObserver',
  "document.addEventListener('visibilitychange'",
  "window.matchMedia('(prefers-reduced-motion: reduce)')",
  "type TerminalPhase = 'typing' | 'output' | 'complete'",
  'aria-pressed={index === activeIndex}',
  'onClick={restart}',
]) {
  if (!terminalComponent.includes(marker)) {
    experienceFailures.push(
      `RuntimeTerminalShowcase.tsx: missing animated terminal contract ${marker}`,
    );
  }
}

for (const marker of [
  '.box-install-target-icons',
  '.box-terminal-scenarios',
  '@keyframes box-terminal-cursor',
  '@media (prefers-reduced-motion: reduce)',
]) {
  if (!heroStyles.includes(marker)) {
    experienceFailures.push(
      `hero styles: missing A3S Code-aligned hero contract ${marker}`,
    );
  }
}

for (const marker of [
  "import { BoxInstallSwitcher } from './BoxInstallSwitcher'",
  "import { PerformanceMetrics } from './PerformanceMetrics'",
  "import { RuntimeTerminalShowcase } from './RuntimeTerminalShowcase'",
  '<AnimatedButtonBorder />',
  '<BoxInstallSwitcher',
  '<PerformanceMetrics',
  '<RuntimeTerminalShowcase',
]) {
  if (!homeComponent.includes(marker)) {
    experienceFailures.push(
      `HomeLayout.tsx: missing A3S Code-aligned hero contract ${marker}`,
    );
  }
}

for (const marker of [
  'id="performance-benchmarks"',
  'data-performance-metric={metric.id}',
  'data-metric-value={value}',
  'className="box-performance-grid"',
  'className="box-performance-context"',
  'className="box-performance-footer"',
  'new IntersectionObserver(',
  'window.requestAnimationFrame(renderFrame)',
  "element.dataset.animationState = 'complete'",
  "'(prefers-reduced-motion: reduce)'",
  'href={reportHref}',
]) {
  if (!performanceComponent.includes(marker)) {
    experienceFailures.push(
      `PerformanceMetrics.tsx: missing real-host metric contract ${marker}`,
    );
  }
}

for (const marker of [
  "id: 'cached-lifecycle'",
  "value: '2.219'",
  "id: 'warm-pool-fill'",
  "value: '1.325'",
  "id: 'persistent-exec'",
  "value: '113.943'",
  "id: 'tmpfs-write'",
  "value: '1,194.372'",
  "id: 'cow-write'",
  "value: '357.750'",
  'cached Alpine 3.22',
  '4 台 / 3.020 秒 p50',
  '4 VMs / 3.020 s p50',
  '不是跨平台保证',
  'not a cross-platform guarantee',
]) {
  if (!homeContent.includes(marker)) {
    experienceFailures.push(
      `home-content.ts: missing measured-performance context ${marker}`,
    );
  }
}

for (const marker of [
  '.box-performance-grid',
  '.box-performance-metric',
  '.box-performance-value-number',
  '.box-performance-value-animated',
  '.box-performance-context',
  '.box-performance-footer',
  '@media (max-width: 640px)',
  '@media (prefers-reduced-motion: reduce)',
]) {
  if (!performanceStyles.includes(marker)) {
    experienceFailures.push(
      `performance-metrics.css: missing responsive metric contract ${marker}`,
    );
  }
}

for (const marker of [
  '.box-button-orbit',
  '.box-button-orbit-gradient',
  'conic-gradient(',
  '@keyframes box-button-full-border-orbit',
  '.box-button--primary:hover .box-button-orbit-gradient',
  '.box-button--primary:focus-visible .box-button-orbit-gradient',
  '#62d78b',
  '#5be5c2',
  '@media (prefers-reduced-motion: reduce)',
]) {
  if (!buttonStyles.includes(marker)) {
    experienceFailures.push(
      `button-orbit.css: missing animated border contract ${marker}`,
    );
  }
}

for (const color of ['#f5b95f', '#8b6cff', '#45ddff']) {
  if (buttonStyles.includes(color)) {
    experienceFailures.push(
      `button-orbit.css: off-palette color remains in green primary button ${color}`,
    );
  }
}

for (const marker of [
  'id="runtime-features"',
  'className="box-kernel-lane box-kernel-lane--shared"',
  'className="box-kernel-lane box-kernel-lane--microvm"',
  'className="box-shared-kernel"',
  'className="box-vm-boundary"',
  'namespace + seccomp',
  'shared host kernel',
  'guest Linux kernel',
  'hardware VM boundary',
  'higher startup and memory cost',
  'className="box-cow-scene"',
  'className="box-pool-scene"',
  'className="box-tee-scene"',
  'className="box-tee-report-packet"',
  'className="box-tee-secret-packet"',
  'SEV-SNP',
  'RA-TLS',
  'No hardware security claim',
  'MAP_PRIVATE',
  '--snapshot-fork',
  'Linux/KVM',
  'not a universal performance guarantee',
]) {
  if (!featureComponent.includes(marker)) {
    experienceFailures.push(
      `RuntimeFeatureShowcase.tsx: missing runtime feature contract ${marker}`,
    );
  }
}

for (const marker of [
  '@keyframes box-shared-startup',
  '@keyframes box-microvm-startup',
  '@keyframes box-shared-risk-path',
  '@keyframes box-microvm-risk-path',
  '@keyframes box-dirty-page',
  '@keyframes box-pool-request',
  '@keyframes box-tee-report',
  '@keyframes box-tee-secret',
  '@media (prefers-reduced-motion: reduce)',
]) {
  if (!featureStyles.includes(marker)) {
    experienceFailures.push(
      `runtime-features.css: missing animation or accessibility contract ${marker}`,
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
  const guideOverview = await readFile(
    path.join(docsRoot, language, 'guide', 'index.mdx'),
    'utf8',
  );
  const sdkOverview = await readFile(
    path.join(docsRoot, language, 'sdk', 'index.mdx'),
    'utf8',
  );
  const imageGuide = await readFile(
    path.join(docsRoot, language, 'guide', 'images-builds.mdx'),
    'utf8',
  );
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
  const guideMeta = JSON.parse(
    await readFile(
      path.join(docsRoot, language, 'guide', '_meta.json'),
      'utf8',
    ),
  );
  const sdkMeta = JSON.parse(
    await readFile(path.join(docsRoot, language, 'sdk', '_meta.json'), 'utf8'),
  );
  const navigation = JSON.parse(
    await readFile(path.join(docsRoot, language, '_nav.json'), 'utf8'),
  );

  const expectedGuideOrder = [
    'index',
    'installation',
    'quick-start',
    'architecture',
    'images-builds',
    'storage-snapshots',
    'networking-compose',
    'windows',
    'agent-skill',
  ];
  const expectedSdkOrder = ['index', 'rust', 'go', 'python', 'typescript'];
  const expectedNavigation =
    language === 'zh'
      ? ['指南', 'SDK', '参考', 'Agent Skill', '资源']
      : ['Guides', 'SDKs', 'Reference', 'Agent Skill', 'Resources'];
  for (const [label, actual, expected] of [
    ['guide sidebar', guideMeta, expectedGuideOrder],
    ['SDK sidebar', sdkMeta, expectedSdkOrder],
    ['top navigation', navigation.map((item) => item.text), expectedNavigation],
  ]) {
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      experienceFailures.push(
        `${language}: ${label} does not follow the product narrative order`,
      );
    }
  }

  recordMarkerOrder(
    guideOverview,
    [
      '](./installation/)',
      '](./quick-start/)',
      '](./architecture/)',
      '](./images-builds/)',
      '](./storage-snapshots/)',
      '](./networking-compose/)',
      '](./windows/)',
      '](./agent-skill/)',
    ],
    `${language}/guide/index.mdx: guide journey`,
    experienceFailures,
  );
  recordMarkerOrder(
    sdkOverview,
    ['](./rust/)', '](./go/)', '](./python/)', '](./typescript/)'],
    `${language}/sdk/index.mdx: SDK journey`,
    experienceFailures,
  );
  recordMarkerOrder(
    imageGuide,
    ['](/sdk/rust)', '](/sdk/go)', '](/sdk/python)', '](/sdk/typescript)'],
    `${language}/guide/images-builds.mdx: SDK links`,
    experienceFailures,
  );

  const nextStepContracts = [
    ['installation.mdx', '](./quick-start/)'],
    ['quick-start.mdx', '](./architecture/)'],
    ['architecture.mdx', '](./images-builds/)'],
    ['images-builds.mdx', '](./storage-snapshots/)'],
    ['storage-snapshots.mdx', '](./networking-compose/)'],
    ['networking-compose.mdx', '](/reference/platforms)'],
    ['windows.mdx', '](./agent-skill/)'],
    ['agent-skill.mdx', '](/sdk/)'],
  ];
  for (const [fileName, nextLink] of nextStepContracts) {
    const source = await readFile(
      path.join(docsRoot, language, 'guide', fileName),
      'utf8',
    );
    const nextHeading = language === 'zh' ? '## 下一步' : '## Next step';
    if (!source.includes(nextHeading) || !source.includes(nextLink)) {
      experienceFailures.push(
        `${language}/guide/${fileName}: missing its contextual next step ${nextLink}`,
      );
    }
  }

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
    '<Tab label="Go" value="go">',
    '<Tab label="Python" value="python">',
    '<Tab label="TypeScript" value="typescript">',
  ];
  for (const marker of tabsContract) {
    if (!quickStart.includes(marker)) {
      experienceFailures.push(
        `${language}/guide/quick-start.mdx: missing SDK Tabs marker ${marker}`,
      );
    }
  }

  const sdkTabSequence = tabsContract.slice(1);
  for (const [filePath, source] of [
    [`${language}/index.mdx`, homepage],
    [`${language}/guide/quick-start.mdx`, quickStart],
  ]) {
    let previousTabIndex = -1;
    for (const marker of sdkTabSequence) {
      const markerIndex = source.indexOf(marker);
      if (markerIndex <= previousTabIndex) {
        experienceFailures.push(
          `${filePath}: SDK tabs are out of order at ${marker}`,
        );
      }
      previousTabIndex = markerIndex;
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
  `Documentation contract verified: ${requiredPages.length} routes × ${languages.length} languages, ordered navigation and guide handoffs, runtime feature animations, Agent Skill integration, complete Rust/Go/Python/TypeScript programs in Tabs, the five-step line-focus tutorial, and ACL fences.`,
);
