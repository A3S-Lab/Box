import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { highlight } from 'codehike/code';
import { format } from 'prettier';

const here = path.dirname(fileURLToPath(import.meta.url));
const websiteRoot = path.join(here, '..');
const outputPath = path.join(
  websiteRoot,
  'theme',
  'generated',
  'runtime-tutorial.json',
);
const theme = JSON.parse(
  await readFile(path.join(websiteRoot, 'codehike-theme.json'), 'utf8'),
);

function focusRange(code, firstLine, lastLine = firstLine) {
  const lines = code.split('\n');
  const from = lines.findIndex((line) => line.includes(firstLine));
  const to = lines.findIndex(
    (line, index) => index >= from && line.includes(lastLine),
  );

  if (from < 0 || to < 0) {
    throw new Error(`Could not find focus range: ${firstLine} → ${lastLine}`);
  }

  return [from + 1, to + 1];
}

const steps = [
  {
    id: 'create',
    layer: '01 / CREATE',
    filename: 'sandbox.ts',
    language: 'TypeScript',
    title: {
      zh: '创建 MicroVM',
      en: 'Create a MicroVM',
    },
    body: {
      zh: '传入 OCI 镜像即可创建 Sandbox。没有指定 isolation 时，Box 默认启动 MicroVM。',
      en: 'Pass an OCI image to create a Sandbox. Box starts a MicroVM when isolation is not specified.',
    },
    note: {
      zh: '本地 SDK 直接连接本机运行时，不需要 endpoint 或 API key。',
      en: 'The local SDK connects to the runtime on this machine. It needs no endpoint or API key.',
    },
    tags: ['OCI image', 'MicroVM', 'Sandbox.create'],
    focusText: ["const sandbox = await Sandbox.create('alpine:3.20');"],
    code: `import { Sandbox } from '@a3s-lab/box';

async function main(): Promise<void> {
  const sandbox = await Sandbox.create('alpine:3.20');
  await sandbox.kill();
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});`,
  },
  {
    id: 'resources',
    layer: '02 / RESOURCES',
    filename: 'sandbox.ts',
    language: 'TypeScript',
    title: {
      zh: '设置资源和网络',
      en: 'Set resources and networking',
    },
    body: {
      zh: 'CPU、内存和网络模式都是创建参数。这里关闭网络，适合不需要下载依赖的测试。',
      en: 'CPU, memory, and network mode are creation options. This example disables networking for an offline test.',
    },
    note: {
      zh: '需要联网时可使用默认 TSI，或在支持的平台上连接命名桥接网络。',
      en: 'Keep the default TSI mode for network access, or use a named bridge network on supported hosts.',
    },
    tags: ['cpus', 'memoryMb', 'network'],
    focusText: ["const sandbox = await Sandbox.create('alpine:3.20', {", '});'],
    code: `import { Sandbox } from '@a3s-lab/box';

async function main(): Promise<void> {
  const sandbox = await Sandbox.create('alpine:3.20', {
    cpus: 2,
    memoryMb: 1024,
    network: { mode: 'none' },
  });

  await sandbox.kill();
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});`,
  },
  {
    id: 'files',
    layer: '03 / FILES',
    filename: 'sandbox.ts',
    language: 'TypeScript',
    title: {
      zh: '写入工作文件',
      en: 'Write a workspace file',
    },
    body: {
      zh: '通过 files API 写入和读取来宾文件。调用返回前，写入结果已经由运行时确认。',
      en: 'Use the files API to write and read guest files. The runtime confirms the write before the call returns.',
    },
    note: {
      zh: '命名卷、绑定挂载和 tmpfs 在创建 Sandbox 时配置。',
      en: 'Configure named volumes, bind mounts, and tmpfs when creating the Sandbox.',
    },
    tags: ['files.write', 'files.read', '/workspace'],
    focusText: [
      "await sandbox.files.write('/workspace/status.txt', 'ready\\n');",
      "console.log(await sandbox.files.read('/workspace/status.txt'));",
    ],
    code: `import { Sandbox } from '@a3s-lab/box';

async function main(): Promise<void> {
  const sandbox = await Sandbox.create('alpine:3.20', {
    cpus: 2,
    memoryMb: 1024,
    network: { mode: 'none' },
  });

  try {
    await sandbox.files.write('/workspace/status.txt', 'ready\\n');
    console.log(await sandbox.files.read('/workspace/status.txt'));
  } finally {
    await sandbox.kill();
  }
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});`,
  },
  {
    id: 'command',
    layer: '04 / COMMAND',
    filename: 'sandbox.ts',
    language: 'TypeScript',
    title: {
      zh: '运行命令并检查结果',
      en: 'Run a command and check the result',
    },
    body: {
      zh: 'commands.run 返回 stdout、stderr、退出码和截断状态。非零退出码由调用方决定如何处理。',
      en: 'commands.run returns stdout, stderr, exit code, and truncation state. The caller decides how to handle a non-zero exit.',
    },
    note: {
      zh: '也可使用 commands.runScript 通过 stdin 执行脚本，并设置解释器、目录和超时。',
      en: 'Use commands.runScript to send a script over stdin and set its interpreter, working directory, and timeout.',
    },
    tags: ['commands.run', 'exitCode', 'stderr'],
    focusText: [
      'const result = await sandbox.commands.run',
      'console.log(result.stdout);',
    ],
    code: `import { Sandbox } from '@a3s-lab/box';

async function main(): Promise<void> {
  const sandbox = await Sandbox.create('alpine:3.20', {
    cpus: 2,
    memoryMb: 1024,
    network: { mode: 'none' },
  });

  try {
    await sandbox.files.write('/workspace/status.txt', 'ready\\n');

    const result = await sandbox.commands.run(
      ['test', '-s', '/workspace/status.txt'],
      { timeoutMs: 30_000 },
    );
    if (result.exitCode !== 0) {
      throw new Error(result.stderr);
    }
    console.log(result.stdout);
  } finally {
    await sandbox.kill();
  }
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});`,
  },
  {
    id: 'cleanup',
    layer: '05 / CLEANUP',
    filename: 'sandbox.ts',
    language: 'TypeScript',
    title: {
      zh: '始终清理 Sandbox',
      en: 'Always clean up the Sandbox',
    },
    body: {
      zh: '把 kill 放在 finally 中。测试成功、命令失败或代码抛错时，运行时资源都会被释放。',
      en: 'Put kill in finally. Runtime resources are released after success, command failure, or an exception.',
    },
    note: {
      zh: '需要保留状态时，将 persistent 设为 true，并使用 stop、snapshot 或 remove 管理后续生命周期。',
      en: 'To keep state, set persistent to true and manage the later lifecycle with stop, snapshot, or remove.',
    },
    tags: ['finally', 'kill', 'cleanup'],
    focusText: ['} finally {', 'await sandbox.kill();'],
    code: `import { Sandbox } from '@a3s-lab/box';

async function main(): Promise<void> {
  const sandbox = await Sandbox.create('alpine:3.20', {
    cpus: 2,
    memoryMb: 1024,
    network: { mode: 'none' },
  });

  try {
    await sandbox.files.write('/workspace/status.txt', 'ready\\n');

    const result = await sandbox.commands.run(
      ['test', '-s', '/workspace/status.txt'],
      { timeoutMs: 30_000 },
    );
    if (result.exitCode !== 0) {
      throw new Error(result.stderr);
    }
    console.log(result.stdout);
  } finally {
    await sandbox.kill();
  }
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});`,
  },
];

const result = [];
for (const step of steps) {
  const focus = focusRange(step.code, ...step.focusText);
  const highlighted = await highlight(
    {
      value: step.code,
      lang: 'typescript',
      meta: step.filename,
    },
    theme,
  );
  highlighted.annotations = [
    {
      name: 'focus',
      query: step.id,
      fromLineNumber: focus[0],
      toLineNumber: focus[1],
    },
  ];
  const { focusText: _focusText, ...publicStep } = step;
  result.push({
    ...publicStep,
    focus,
    highlighted,
  });
}

await mkdir(path.dirname(outputPath), { recursive: true });
const output = await format(JSON.stringify(result), { parser: 'json' });
await writeFile(outputPath, output, 'utf8');
