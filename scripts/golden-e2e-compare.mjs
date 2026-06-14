#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { spawn, spawnSync } from 'node:child_process';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, '..');
const defaultDb = '/home/chaizhenhua/.reverts/.reverts.db';
const defaultWorkDir = path.join(tmpdir(), 'reverts-golden-e2e');
const dbPath = process.env.REVERTS_DB ?? defaultDb;
const workDir = process.env.REVERTS_E2E_DIR ?? defaultWorkDir;
const cliPath = process.env.REVERTS_CLI ?? path.join(repoRoot, 'target/release/reverts-cli');
const skipBuild = process.env.REVERTS_E2E_SKIP_BUILD === '1';
const keepWorkDir = process.env.REVERTS_E2E_KEEP === '1';
const cliTimeoutMs = Number(process.env.REVERTS_E2E_CLI_TIMEOUT_MS ?? '30000');
const commandTimeoutMs = process.env.REVERTS_E2E_COMMAND_TIMEOUT_MS === undefined
  ? undefined
  : Number(process.env.REVERTS_E2E_COMMAND_TIMEOUT_MS);

const projects = {
  big: {
    id: '13495',
    out: path.join(workDir, 'project-13495'),
    originalCli: '/home/chaizhenhua/.reverts/claude-code-2.1.89.8db5ce8ff8968f86.formatted.js',
    generatedCli: path.join(workDir, 'project-13495/dist/cli.js'),
  },
  small: {
    id: '14105',
    out: path.join(workDir, 'project-14105'),
    originals: {
      offscreen: '/home/chaizhenhua/.reverts/offscreendocument_main.fb5ea31ab3d8a3fd.formatted.js',
      pageEmbed: '/home/chaizhenhua/.reverts/page_embed_script.d067c8b567d0bcf2.formatted.js',
      serviceWorker: '/home/chaizhenhua/.reverts/service_worker_bin_prod.dc5cf2fa974a3fad.formatted.js',
    },
    generated: {
      offscreen: path.join(workDir, 'project-14105/dist/modules/232122-offscreendocument_main.js'),
      pageEmbed: path.join(workDir, 'project-14105/dist/modules/232123-page_embed_script.js'),
      serviceWorker: path.join(workDir, 'project-14105/dist/modules/232124-service_worker_bin_prod.js'),
    },
  },
};

let failures = 0;
const summaries = [];

function section(title) {
  console.log(`\n== ${title} ==`);
}

function note(message) {
  console.log(`• ${message}`);
}

function fail(label, message) {
  failures += 1;
  console.error(`\n[FAIL] ${label}: ${message}`);
}

function run(cmd, args, options = {}) {
  const printable = [cmd, ...args].join(' ');
  note(printable);
  const spawnOptions = {
    cwd: options.cwd ?? repoRoot,
    env: { ...process.env, ...(options.env ?? {}) },
    encoding: 'utf8',
    input: options.input,
    maxBuffer: 128 * 1024 * 1024,
  };
  const timeout = options.timeoutMs ?? commandTimeoutMs;
  if (timeout !== undefined && timeout > 0) {
    spawnOptions.timeout = timeout;
  }
  const result = spawnSync(cmd, args, spawnOptions);
  if (result.error) {
    throw new Error(`${printable}: ${result.error.message}`);
  }
  if (options.expectSuccess !== false && result.status !== 0) {
    throw new Error(`${printable} exited ${result.status}\nSTDOUT:\n${result.stdout}\nSTDERR:\n${result.stderr}`);
  }
  return normalizeRunResult(result);
}

function normalizeRunResult(result) {
  return {
    status: result.status,
    signal: result.signal,
    stdout: result.stdout ?? '',
    stderr: result.stderr ?? '',
  };
}

function prepareProjects() {
  section('build generator and regenerate real projects');
  rmSync(workDir, { recursive: true, force: true });
  if (!skipBuild) {
    run('cargo', ['build', '--release', '-p', 'reverts-cli', '--locked']);
  }
  run(cliPath, ['generate-project-v2', '--input', dbPath, '--project-id', projects.big.id, '--output', projects.big.out]);
  run(cliPath, ['generate-project-v2', '--input', dbPath, '--project-id', projects.small.id, '--output', projects.small.out]);

  for (const project of [projects.big, projects.small]) {
    run('npm', ['install', '--no-audit', '--fund=false'], { cwd: project.out });
    run('npm', ['run', 'build'], { cwd: project.out });
    run('npm', ['run', 'check'], { cwd: project.out });
  }
  summaries.push('真实 DB 生成大/小项目，npm install/build/check 全部通过');
}

function stripAnsi(value) {
  return value.replace(/\x1B\[[0-?]*[ -/]*[@-~]/g, '');
}

function normalizeCliText(value) {
  return stripAnsi(value)
    .replaceAll(repoRoot, '<REPO>')
    .replaceAll(workDir, '<WORK>')
    .replaceAll(projects.big.originalCli, '<ORIGINAL_CLI>')
    .replaceAll(projects.big.generatedCli, '<GENERATED_CLI>')
    .replace(/\/tmp\/reverts-golden-e2e-[^\s:)]+/g, '<TMP>')
    .replace(/[ \t]+\n/g, '\n');
}

function compareValues(label, original, generated) {
  const left = stableStringify(original);
  const right = stableStringify(generated);
  if (left !== right) {
    fail(label, diffPreview(left, right));
    return false;
  }
  note(`${label}: OK`);
  return true;
}

function compareCliRun(label, args, extraEnv = {}) {
  const baseEnv = isolatedCliEnv(label, extraEnv);
  const original = runNodeCli(projects.big.originalCli, args, baseEnv.original, label);
  const generated = runNodeCli(projects.big.generatedCli, args, baseEnv.generated, label);
  return compareValues(label, normalizeCliResult(original), normalizeCliResult(generated));
}

function runNodeCli(script, args, env, label) {
  const result = spawnSync(process.execPath, [script, ...args], {
    cwd: repoRoot,
    env: { ...process.env, ...env },
    encoding: 'utf8',
    timeout: cliTimeoutMs,
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) {
    return { status: null, signal: null, stdout: '', stderr: result.error.message, error: result.error.name };
  }
  return normalizeRunResult(result);
}

function runNodeCliAsync(script, args, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [script, ...args], {
      cwd: repoRoot,
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      child.kill('SIGTERM');
      setTimeout(() => {
        if (!settled) child.kill('SIGKILL');
      }, 1000).unref();
    }, cliTimeoutMs);
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.on('error', (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve({
        status: null,
        signal: null,
        stdout,
        stderr: `${stderr}${error.message}`,
        error: error.name,
      });
    });
    child.on('close', (status, signal) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve({ status, signal, stdout, stderr });
    });
  });
}

function normalizeCliResult(result) {
  return {
    status: result.status,
    signal: result.signal,
    stdout: normalizeCliText(result.stdout),
    stderr: normalizeCliText(result.stderr),
  };
}

function isolatedCliEnv(label, extraEnv = {}) {
  const root = mkdtempSync(path.join(tmpdir(), `reverts-golden-e2e-${safeName(label)}-`));
  const originalHome = path.join(root, 'original-home');
  const generatedHome = path.join(root, 'generated-home');
  const common = {
    CI: '1',
    NO_COLOR: '1',
    TERM: 'dumb',
    TZ: 'UTC',
    DISABLE_TELEMETRY: '1',
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
    ...extraEnv,
  };
  return {
    original: isolatedHomeEnv(originalHome, common),
    generated: isolatedHomeEnv(generatedHome, common),
  };
}

function isolatedHomeEnv(home, common) {
  return {
    ...common,
    HOME: home,
    USERPROFILE: home,
    XDG_CONFIG_HOME: path.join(home, '.config'),
    XDG_CACHE_HOME: path.join(home, '.cache'),
    XDG_DATA_HOME: path.join(home, '.local/share'),
  };
}

function safeName(label) {
  return label.replace(/[^a-z0-9_-]+/gi, '-').slice(0, 48);
}

function compareCliGoldenMatrix() {
  section('golden compare: Claude CLI command surface');
  const cases = [
    ['version-long', ['--version']],
    ['version-short', ['-v']],
    ['help-long', ['--help']],
    ['help-command', ['help']],
    ['help-agents', ['help', 'agents']],
    ['help-auth', ['help', 'auth']],
    ['help-auto-mode', ['help', 'auto-mode']],
    ['help-config', ['help', 'config']],
    ['help-doctor', ['help', 'doctor']],
    ['help-install', ['help', 'install']],
    ['help-mcp', ['help', 'mcp']],
    ['help-plugin', ['help', 'plugin']],
    ['help-pr-comments', ['help', 'pr-comments']],
    ['help-setup-token', ['help', 'setup-token']],
    ['help-update', ['help', 'update']],
    ['agents-help', ['agents', '--help']],
    ['auth-help', ['auth', '--help']],
    ['auto-mode-help', ['auto-mode', '--help']],
    ['config-help', ['config', '--help']],
    ['doctor-help', ['doctor', '--help']],
    ['install-help', ['install', '--help']],
    ['mcp-help', ['mcp', '--help']],
    ['plugin-help', ['plugin', '--help']],
    ['pr-comments-help', ['pr-comments', '--help']],
    ['setup-token-help', ['setup-token', '--help']],
    ['update-help', ['update', '--help']],
    ['invalid-command', ['unknown-command']],
    ['invalid-flag', ['--definitely-not-real']],
  ];
  let passed = 0;
  for (const [label, args] of cases) {
    if (compareCliRun(`cli:${label}`, args)) passed += 1;
  }
  summaries.push(`Claude CLI 命令面 golden: ${passed}/${cases.length} 组完全一致`);
}

async function compareMockApiPrint() {
  section('golden compare: Claude CLI mocked Anthropic API --print streaming path');
  const server = createMockAnthropicServer();
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  const baseUrl = `http://127.0.0.1:${port}`;
  try {
    const args = ['--bare', '--print', '--model', 'sonnet', '--dangerously-skip-permissions', '--output-format', 'text', 'say mock ok'];
    const envs = isolatedCliEnv('mock-api-print', {
      ANTHROPIC_API_KEY: 'test',
      ANTHROPIC_BASE_URL: baseUrl,
      CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK: '1',
    });

    server.resetLogs('original');
    const original = await runNodeCliAsync(projects.big.originalCli, args, envs.original);
    const originalLogs = server.takeLogs();

    server.resetLogs('generated');
    const generated = await runNodeCliAsync(projects.big.generatedCli, args, envs.generated);
    const generatedLogs = server.takeLogs();

    const normalizedOriginalLogs = normalizeRequestLogs(originalLogs);
    const normalizedGeneratedLogs = normalizeRequestLogs(generatedLogs);
    const outputOk = compareValues('mock-api:process-output', normalizeCliResult(original), normalizeCliResult(generated));
    const shapeOk = validateMockApiShape('mock-api:original-shape', normalizedOriginalLogs)
      && validateMockApiShape('mock-api:generated-shape', normalizedGeneratedLogs);
    const requestsOk = compareValues('mock-api:requests', normalizedOriginalLogs, normalizedGeneratedLogs);
    if (outputOk && shapeOk && requestsOk) {
      const bodyHash = normalizedOriginalLogs.find((entry) => entry.method === 'POST')?.bodyHash;
      summaries.push(`mock Anthropic API --print streaming 完全一致；无 non-streaming fallback；POST body hash=${bodyHash}`);
    }
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

function createMockAnthropicServer() {
  let logs = [];
  let label = 'unlabeled';
  const server = http.createServer((request, response) => {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      const rawBody = Buffer.concat(chunks).toString('utf8');
      logs.push({
        label,
        method: request.method,
        url: request.url,
        headers: request.headers,
        rawBody,
      });
      if (request.method === 'HEAD') {
        response.writeHead(200, { 'x-mock': 'ok' });
        response.end();
        return;
      }
      if (request.method === 'POST' && request.url?.startsWith('/v1/messages')) {
        let body = {};
        try { body = JSON.parse(rawBody); } catch {}
        if (body.stream) {
          response.writeHead(200, {
            'content-type': 'text/event-stream; charset=utf-8',
            'cache-control': 'no-cache',
          });
          for (const event of streamingEvents()) {
            response.write(`event: ${event.event}\n`);
            response.write(`data: ${JSON.stringify(event.data)}\n\n`);
          }
          response.end();
        } else {
          response.writeHead(200, { 'content-type': 'application/json' });
          response.end(JSON.stringify(nonStreamingMessage()));
        }
        return;
      }
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: 'not found' } }));
    });
  });
  server.resetLogs = (nextLabel) => { logs = []; label = nextLabel; };
  server.takeLogs = () => logs.slice();
  return server;
}

function streamingEvents() {
  return [
    { event: 'message_start', data: { type: 'message_start', message: { id: 'msg_mock', type: 'message', role: 'assistant', model: 'claude-sonnet-4-20250514', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 1, output_tokens: 0 } } } },
    { event: 'content_block_start', data: { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } } },
    { event: 'content_block_delta', data: { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'mock ok' } } },
    { event: 'content_block_stop', data: { type: 'content_block_stop', index: 0 } },
    { event: 'message_delta', data: { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 2 } } },
    { event: 'message_stop', data: { type: 'message_stop' } },
  ];
}

function nonStreamingMessage() {
  return {
    id: 'msg_mock',
    type: 'message',
    role: 'assistant',
    model: 'claude-sonnet-4-20250514',
    content: [{ type: 'text', text: 'mock ok' }],
    stop_reason: 'end_turn',
    stop_sequence: null,
    usage: { input_tokens: 1, output_tokens: 2 },
  };
}

function normalizeRequestLogs(logs) {
  return logs.map((entry) => {
    let body = null;
    if (entry.rawBody.trim()) {
      body = JSON.parse(entry.rawBody);
    }
    const normalizedBody = body ? normalizeAnthropicBody(body) : null;
    return {
      method: entry.method,
      url: entry.url,
      rawBodyLength: entry.rawBody.length,
      stream: body?.stream ?? null,
      bodyHash: normalizedBody ? sha256(stableStringify(normalizedBody)) : null,
      body: normalizedBody,
    };
  });
}

function validateMockApiShape(label, logs) {
  const posts = logs.filter((entry) => entry.method === 'POST');
  const heads = logs.filter((entry) => entry.method === 'HEAD');
  const ok = heads.length === 1
    && posts.length === 1
    && posts[0].url?.startsWith('/v1/messages')
    && posts[0].stream === true
    && posts[0].rawBodyLength > 0
    && posts[0].bodyHash;
  if (!ok) {
    fail(label, `expected exactly HEAD + one streaming POST with non-empty JSON body, got ${stableStringify(logs)}`);
    return false;
  }
  note(`${label}: OK`);
  return true;
}

function normalizeAnthropicBody(body) {
  // Keep the full request payload, but sort object keys so byte-order in
  // JSON.stringify cannot mask meaningful differences. The CLI pair runs in
  // identical empty homes and the same process date, so date/system prompt
  // differences remain visible here. Per-install identifiers are randomized
  // by design, so normalize those before comparing behavior.
  const normalized = structuredClone(body);
  if (typeof normalized.metadata?.user_id === 'string') {
    try {
      const userId = JSON.parse(normalized.metadata.user_id);
      if (typeof userId.device_id === 'string') userId.device_id = '<device_id>';
      if (typeof userId.session_id === 'string') userId.session_id = '<session_id>';
      normalized.metadata.user_id = JSON.stringify(userId);
    } catch {
      normalized.metadata.user_id = '<metadata_user_id>';
    }
  }
  return JSON.parse(stableStringify(normalized));
}

function compareSmallExtensionVm() {
  section('golden compare: Chrome extension scripts in VM sandbox');
  const cases = [
    ['page-embed', projects.small.originals.pageEmbed, projects.small.generated.pageEmbed],
    ['offscreen-document', projects.small.originals.offscreen, projects.small.generated.offscreen],
    ['service-worker', projects.small.originals.serviceWorker, projects.small.generated.serviceWorker],
  ];
  let passed = 0;
  for (const [label, originalPath, generatedPath] of cases) {
    const original = runExtensionScriptVm(originalPath);
    const generated = runExtensionScriptVm(generatedPath);
    if (compareValues(`small-vm:${label}`, original, generated)) passed += 1;
  }
  summaries.push(`Chrome extension VM 行为 golden: ${passed}/${cases.length} 个入口完全一致`);
}

function runExtensionScriptVm(scriptPath) {
  const { chrome, listenerLog, callLog } = makeChromeMock();
  const sandbox = {
    console: makeQuietConsole(),
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    queueMicrotask,
    TextEncoder,
    TextDecoder,
    URL,
    URLSearchParams,
    Blob,
    ArrayBuffer,
    Uint8Array,
    atob: (value) => Buffer.from(value, 'base64').toString('binary'),
    btoa: (value) => Buffer.from(value, 'binary').toString('base64'),
    fetch: async () => ({ ok: true, status: 200, json: async () => ({}), text: async () => '' }),
    importScripts: (...specifiers) => callLog.push(['importScripts', ...specifiers]),
    navigator: { userAgent: 'golden-e2e' },
    location: { href: 'chrome-extension://mock/script.js', origin: 'chrome-extension://mock' },
    document: makeDocumentMock(callLog),
    addEventListener: (type) => listenerLog.push(['global', String(type)]),
    removeEventListener: () => {},
    dispatchEvent: () => true,
    attachEvent: (type) => listenerLog.push(['global', String(type)]),
    detachEvent: () => {},
    chrome,
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.global = sandbox;
  sandbox.globalThis = sandbox;

  let status = { ok: true };
  try {
    vm.runInNewContext(readFileSync(scriptPath, 'utf8'), sandbox, { filename: scriptPath, timeout: 5000 });
  } catch (error) {
    status = { ok: false, error: { name: error?.name, message: normalizeCliText(String(error?.message ?? error)) } };
  }

  return {
    status,
    docsGlobals: pickDocsGlobals(sandbox),
    listenerLog: listenerLog.map((entry) => entry.join('.')).sort(),
    callLog: callLog.map((entry) => entry.join('.')).sort(),
  };
}

function makeQuietConsole() {
  return { log() {}, info() {}, warn() {}, error() {}, debug() {} };
}

function makeDocumentMock(callLog) {
  return {
    readyState: 'complete',
    hidden: false,
    visibilityState: 'visible',
    body: { appendChild() {}, removeChild() {} },
    documentElement: { appendChild() {}, removeChild() {} },
    addEventListener(type) { callLog.push(['document.addEventListener', String(type)]); },
    removeEventListener() {},
    createElement(tag) { return makeElementMock(tag); },
    getElementById() { return makeElementMock('div'); },
    querySelector() { return makeElementMock('div'); },
  };
}

function makeElementMock(tag) {
  return {
    tagName: String(tag).toUpperCase(),
    style: {},
    dataset: {},
    classList: { add() {}, remove() {}, contains() { return false; } },
    appendChild() {},
    removeChild() {},
    setAttribute() {},
    getAttribute() { return null; },
    addEventListener() {},
    removeEventListener() {},
    click() {},
  };
}

function makeChromeMock() {
  const listenerLog = [];
  const callLog = [];
  const special = new Map([
    ['chrome.runtime.getManifest', () => ({ manifest_version: 3, version: '1.104.1', name: 'Google Docs Offline' })],
    ['chrome.runtime.getURL', (specifier = '') => `chrome-extension://mock/${specifier}`],
  ]);

  const makeEvent = (pathParts) => ({
    addListener(listener) { listenerLog.push(pathParts); return undefined; },
    removeListener() { return undefined; },
    hasListener() { return false; },
    hasListeners() { return false; },
  });

  const makeNode = (pathParts = ['chrome']) => new Proxy(function chromeMockFunction() {}, {
    get(target, prop) {
      if (prop === 'then') return undefined;
      if (prop === 'toJSON') return () => `[${pathParts.join('.')}]`;
      if (prop === Symbol.toPrimitive) return () => `[${pathParts.join('.')}]`;
      if (['addListener', 'removeListener', 'hasListener', 'hasListeners'].includes(prop)) {
        return makeEvent(pathParts)[prop];
      }
      if (!(prop in target)) {
        target[prop] = makeNode([...pathParts, String(prop)]);
      }
      return target[prop];
    },
    apply(_target, _thisArg, args) {
      const dotted = pathParts.join('.');
      callLog.push([dotted]);
      const handler = special.get(dotted);
      if (handler) return handler(...args);
      const callback = args.find((arg) => typeof arg === 'function');
      if (callback) callback();
      return undefined;
    },
  });

  return { chrome: makeNode(), listenerLog, callLog };
}

function pickDocsGlobals(sandbox) {
  const entries = Object.entries(sandbox)
    .filter(([key]) => key.startsWith('_docs_chrome_extension_'))
    .sort(([left], [right]) => left.localeCompare(right));
  return Object.fromEntries(entries);
}

function sha256(value) {
  return createHash('sha256').update(value).digest('hex');
}

function stableStringify(value) {
  return JSON.stringify(sortStable(value), null, 2);
}

function sortStable(value) {
  if (Array.isArray(value)) return value.map(sortStable);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, nested]) => [key, sortStable(nested)]));
  }
  return value;
}

function diffPreview(left, right) {
  if (left === right) return 'values differ';
  let index = 0;
  const max = Math.min(left.length, right.length);
  while (index < max && left[index] === right[index]) index += 1;
  const start = Math.max(0, index - 180);
  const endLeft = Math.min(left.length, index + 500);
  const endRight = Math.min(right.length, index + 500);
  return `first difference at byte ${index}\n--- original\n${left.slice(start, endLeft)}\n--- generated\n${right.slice(start, endRight)}`;
}

try {
  prepareProjects();
  compareCliGoldenMatrix();
  await compareMockApiPrint();
  compareSmallExtensionVm();
} finally {
  if (!keepWorkDir) {
    // Keep generated projects by default only on failure for debugging.
    if (failures === 0) rmSync(workDir, { recursive: true, force: true });
    else note(`preserved failing workdir: ${workDir}`);
  }
}

section('summary');
for (const summary of summaries) note(summary);
if (failures > 0) {
  console.error(`\n${failures} golden/e2e comparison(s) failed`);
  process.exit(1);
}
console.log('\nall golden/e2e comparisons passed');
