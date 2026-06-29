import './register.mjs'; // patch Module._load BEFORE the bundle loads
const t0 = Date.now();
let reported = false;
function report(status, err) {
  if (reported) return; reported = true;
  const r = globalThis.__bootSmoke || {};
  console.log('BOOT_SMOKE_RESULT ' + JSON.stringify({
    status, ms: Date.now() - t0,
    windowsCreated: r.windowsCreated || 0,
    viewsCreated: r.viewsCreated || 0,
    whenReadyResolved: !!r.whenReadyResolved,
    ipcHandlers: r.ipcHandlers || 0,
    urlsLoaded: (r.urlsLoaded || []).slice(0, 5),
    uncaught: (r.uncaught || []).slice(0, 3),
    rejections: (r.rejections || []).slice(0, 3),
    error: err ? (err.stack || String(err)).slice(0, 3000) : undefined,
  }, null, 1));
  process.exit(0);
}
// Poll for window creation (PASS as soon as a window is constructed).
const poll = setInterval(() => {
  const r = globalThis.__bootSmoke || {};
  if (r.windowsCreated > 0 || r.viewsCreated > 0) { clearInterval(poll); report('window-created'); }
  else if (Date.now() - t0 > 30000) { clearInterval(poll); report('timeout-no-window'); }
}, 250);
import('./main.bundle.mjs')
  .then(() => { /* top-level await done; poll still checks for the window */ })
  .catch((e) => { clearInterval(poll); report('threw', e); });
