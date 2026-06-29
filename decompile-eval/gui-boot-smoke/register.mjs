// Pre-load hook: intercept CJS require() of native/electron externals so the
// decompiled main boots against deterministic mocks (no real display / native
// ABI needed). The 5 pure-JS ESM externals (@opentelemetry/*, iitm, ritm,
// semver) resolve normally from node_modules.
import Module from 'node:module';
import { createRequire } from 'node:module';

// The recovered main parses process.versions.electron (and runs as an Electron
// main process); plain Node doesn't set it, so provide the target runtime's
// values the boot smoke is standing in for (Electron 41.6.1).
try {
  process.versions.electron = process.versions.electron || '41.6.1';
  process.versions.chrome = process.versions.chrome || '138.0.0.0';
  // Electron adds these to `process`; plain Node lacks them.
  if (typeof process.getSystemVersion !== 'function') {
    process.getSystemVersion = () => '14.5.0';
  }
  if (typeof process.getCPUUsage !== 'function') {
    process.getCPUUsage = () => ({ percentCPUUsage: 0, idleWakeupsPerSecond: 0 });
  }
  if (typeof process.getProcessMemoryInfo !== 'function') {
    process.getProcessMemoryInfo = async () => ({ residentSet: 0, private: 0, shared: 0 });
  }
  process.type = process.type || 'browser';
} catch {}

// Keep the boot alive on a late throw so the window-creation path still has a
// chance to run; record it for the report instead of crashing the smoke.
const rec = (globalThis.__bootSmoke = globalThis.__bootSmoke || {});
process.on('uncaughtException', (e) => {
  rec.uncaught = (rec.uncaught || []);
  if (rec.uncaught.length < 5) rec.uncaught.push((e && (e.stack || String(e))).slice(0, 1500));
});
process.on('unhandledRejection', (e) => {
  rec.rejections = (rec.rejections || []);
  if (rec.rejections.length < 5) rec.rejections.push(String(e && (e.stack || e)).slice(0, 1500));
});

const require = createRequire(import.meta.url);
const electronMock = require('./electron-mock.cjs');

function autoNativeMock() {
  const handler = {
    get(t, p) {
      if (p in t) return t[p];
      if (p === 'then') return undefined;
      return new Proxy(function () {}, handler);
    },
    apply() {
      return new Proxy(function () {}, handler);
    },
    construct() {
      return new Proxy({}, handler);
    },
  };
  return new Proxy({}, handler);
}

const MOCKS = new Map([
  ['electron', electronMock],
  ['node-pty', autoNativeMock()],
  ['ws', autoNativeMock()],
  ['@ant/claude-native', autoNativeMock()],
  ['@ant/claude-swift', autoNativeMock()],
  ['form-data', autoNativeMock()],
]);

const originalLoad = Module._load;
Module._load = function (request, parent, isMain) {
  if (MOCKS.has(request)) return MOCKS.get(request);
  return originalLoad.call(this, request, parent, isMain);
};
