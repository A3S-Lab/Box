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
  'guide/agent-skill.html',
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

if (!rootHomepage.includes('给每个任务')) {
  throw new Error('The default homepage is not rendered in Chinese.');
}
if (!rootHomepage.includes(`${base}en/`)) {
  throw new Error('The Chinese homepage does not expose the English locale.');
}
if (!englishHomepage.includes('Give every task')) {
  throw new Error('The /en/ homepage is not rendered in English.');
}
for (const [homepagePath, html] of [
  ['index.html', rootHomepage],
  ['en/index.html', englishHomepage],
]) {
  for (const marker of [
    'id="agent-skill"',
    'box-global-grid-canvas',
    'a3s-box · skill installer',
    'integrations/skills/install.sh',
    'id="sdk-code-tour"',
    'id="homepage-code-hike"',
    'box-home-sdk-tabs',
    'data-runtime-tutorial="true"',
    'class="box-tutorial-sticky"',
    'data-tutorial-step="create"',
    'data-tutorial-step="cleanup"',
    'data-focus="true"',
    'id="runtime-features"',
    'id="kernel-boundary"',
    'id="copy-on-write"',
    'id="warm-pool"',
    'id="confidential-computing"',
    'id="platform-support"',
    'id="runtime-capabilities"',
    'id="native-sdks"',
    'id="home-cta"',
    'box-install-platforms',
    'id="box-install-tab-unix"',
    'id="box-install-tab-homebrew"',
    'data-terminal-scenario="microvm"',
    'data-terminal-scenario="sandbox"',
    'data-terminal-scenario="cow"',
    'data-terminal-scenario="pool"',
    'data-terminal-scenario="tee"',
    'box-kernel-lane--shared',
    'box-kernel-lane--microvm',
    'box-shared-kernel',
    'box-vm-boundary',
    'box-cow-scene',
    'box-pool-scene',
    'box-tee-scene',
    'box-tee-report-packet',
    'box-tee-secret-packet',
    'SEV-SNP',
    'RA-TLS',
    '--snapshot-fork',
  ]) {
    if (!html.includes(marker)) {
      throw new Error(
        `${homepagePath} does not visibly render its homepage SDK tour marker: ${marker}`,
      );
    }
  }

  const narrativeSequence = [
    'id="runtime-features"',
    'id="platform-support"',
    'id="runtime-capabilities"',
    'id="native-sdks"',
    'id="sdk-code-tour"',
    'id="agent-skill"',
    'id="home-cta"',
  ];
  let previousNarrativeIndex = -1;
  for (const marker of narrativeSequence) {
    const markerIndex = html.indexOf(marker);
    if (markerIndex <= previousNarrativeIndex) {
      throw new Error(
        `${homepagePath} has a missing or out-of-order homepage section at ${marker}.`,
      );
    }
    previousNarrativeIndex = markerIndex;
  }
  if (html.includes('box-principles')) {
    throw new Error(
      `${homepagePath} renders the duplicated isolation principles section.`,
    );
  }
  for (const language of ['Rust', 'TypeScript', 'Python', 'Go']) {
    if (!html.includes(`>${language}<`)) {
      throw new Error(
        `${homepagePath} does not render the ${language} homepage SDK tab.`,
      );
    }
  }
}
for (const route of [
  `${base}en/guide/agent-skill.html`,
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
  const agentSkillPath = `${localePrefix}guide/agent-skill.html`;
  const agentSkillHtml = await readFile(
    path.join(outputRoot, agentSkillPath),
    'utf8',
  );
  const agentSkillText = agentSkillHtml
    .replace(/<[^>]+>/g, '')
    .replaceAll('&lt;', '<')
    .replaceAll('&gt;', '>')
    .replaceAll('&amp;', '&')
    .replace(/\s+/g, ' ');

  for (const marker of [
    'integrations/skills/install.sh',
    'sh -s -- --home a3s-code',
    'sh -s -- --home codex',
    'sh -s -- --home claude',
    'sh -s -- --home all',
    '/a3s-box',
    'allowed-tools',
  ]) {
    if (!agentSkillText.includes(marker)) {
      throw new Error(
        `${agentSkillPath} is missing its Skill integration marker: ${marker}`,
      );
    }
  }

  const quickStartPath = `${localePrefix}guide/quick-start.html`;
  const quickStartHtml = await readFile(
    path.join(outputRoot, quickStartPath),
    'utf8',
  );

  for (const marker of [
    'box-sdk-tabs',
    'data-runtime-tutorial="true"',
    'class="box-tutorial-sticky"',
    'data-tutorial-step="create"',
    'data-tutorial-step="cleanup"',
    'data-focus="true"',
  ]) {
    if (!quickStartHtml.includes(marker)) {
      throw new Error(
        `${quickStartPath} is missing its SDK Tabs or line-focus tutorial marker: ${marker}`,
      );
    }
  }
  for (const language of ['Rust', 'TypeScript', 'Python', 'Go']) {
    if (!quickStartHtml.includes(`>${language}<`)) {
      throw new Error(
        `${quickStartPath} does not render the ${language} SDK tab.`,
      );
    }
  }

  const tutorialStepCount = (
    quickStartHtml.match(/data-tutorial-step="[^"]+"/g) ?? []
  ).length;
  if (tutorialStepCount !== 5) {
    throw new Error(
      `${quickStartPath} should server-render 5 tutorial steps, found ${tutorialStepCount}.`,
    );
  }

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
  `Bilingual runtime features, Agent Skill, Tabs, line-focus tutorials, references, and ACL highlighting verified across ${htmlFiles.length} HTML pages.`,
);
