// Deterministic Electron mock for the decompiled-app boot smoke.
// Records BrowserWindow creation and lets app.whenReady() resolve so the
// recovered main's window-creation path runs without a real display.
'use strict';
const os = require('node:os');
const { EventEmitter } = require('node:events');

const recorder = (globalThis.__bootSmoke = globalThis.__bootSmoke || {
  windowsCreated: 0,
  urlsLoaded: [],
  whenReadyResolved: false,
  ipcHandlers: 0,
});

// Auto-mock: any unknown property is a callable/constructible Proxy that keeps
// returning auto-mocks, so arbitrary Electron API surface never throws.
const handler = {
  get(target, prop) {
    if (prop in target) return target[prop];
    if (prop === 'then') return undefined; // not a thenable
    // Make stubbed values survive primitive coercion (template literals,
    // arithmetic, comparisons) instead of throwing "cannot convert to primitive".
    if (prop === Symbol.toPrimitive) return () => '';
    if (prop === Symbol.iterator) return undefined;
    if (prop === 'toString' || prop === Symbol.toStringTag) return () => '';
    if (prop === 'valueOf') return () => 0;
    return makeAuto();
  },
  apply() {
    return makeAuto();
  },
  construct() {
    return new Proxy({}, handler);
  },
};
function makeAuto() {
  return new Proxy(function () {}, handler);
}

class BrowserWindow {
  constructor(opts) {
    recorder.windowsCreated += 1;
    this.id = recorder.windowsCreated;
    this.webContents = mockWebContents();
    this.contentView = new Proxy({ addChildView() {}, removeChildView() {} }, handler);
    this._opts = opts;
    // Class instances bypass the auto-mock Proxy fallback, so wrap the instance
    // to stub any window method the app calls that we didn't define.
    return new Proxy(this, handler);
  }
  getBounds() {
    return { x: 0, y: 0, width: 1200, height: 800 };
  }
  getContentBounds() {
    return { x: 0, y: 0, width: 1200, height: 800 };
  }
  setBounds() {}
  getContentSize() {
    return [1200, 800];
  }
  getPosition() {
    return [0, 0];
  }
  getSize() {
    return [1200, 800];
  }
  isVisible() {
    return true;
  }
  isFocused() {
    return true;
  }
  isMinimized() {
    return false;
  }
  loadURL(u) {
    recorder.urlsLoaded.push(String(u));
    return Promise.resolve();
  }
  loadFile(f) {
    recorder.urlsLoaded.push(String(f));
    return Promise.resolve();
  }
  on() {
    return this;
  }
  once() {
    return this;
  }
  show() {}
  focus() {}
  maximize() {}
  setMenuBarVisibility() {}
  isDestroyed() {
    return false;
  }
  static getAllWindows() {
    return [];
  }
  static fromWebContents() {
    return null;
  }
}

// app is an EventEmitter so the recovered main's `app.on('ready'|'activate', …)`
// window-creation handlers actually fire (the ready gate + macOS activate are the
// usual triggers). whenReady() resolves; lifecycle events emit after load.
const appEmitter = new EventEmitter();
appEmitter.setMaxListeners(0);
function mockWebContents() {
  const wc = new EventEmitter();
  wc.setMaxListeners(0);
  return new Proxy(
    Object.assign(wc, {
      id: Math.floor(Math.random() * 1e6),
      loadURL(u) {
        recorder.urlsLoaded.push(String(u));
        // Simulate a successful renderer load so "wait for mainView ready"
        // resolves: fire the navigation lifecycle events the app listens for.
        setImmediate(() => {
          wc.emit('did-start-loading');
          wc.emit('dom-ready');
          wc.emit('did-finish-load');
          wc.emit('did-frame-finish-load', {}, true, 1, 1);
        });
        return Promise.resolve();
      },
      loadFile(f) {
        recorder.urlsLoaded.push(String(f));
        setImmediate(() => {
          wc.emit('dom-ready');
          wc.emit('did-finish-load');
        });
        return Promise.resolve();
      },
      send() {},
      executeJavaScript() {
        return Promise.resolve();
      },
      setWindowOpenHandler() {},
      openDevTools() {},
      close() {},
      isDestroyed() {
        return false;
      },
      session: makeAuto(),
      navigationHistory: new Proxy({ canGoBack: () => false, canGoForward: () => false }, handler),
    }),
    handler,
  );
}

class WebContentsView {
  constructor(opts) {
    recorder.viewsCreated = (recorder.viewsCreated || 0) + 1;
    this.webContents = mockWebContents();
    this._opts = opts;
  }
  setBounds() {}
  setVisible() {}
  setBackgroundColor() {}
}
class BaseWindow {
  constructor(opts) {
    recorder.windowsCreated += 1;
    this._opts = opts;
    this.contentView = new Proxy({ addChildView() {}, removeChildView() {} }, handler);
    return new Proxy(this, handler);
  }
  on() { return this; }
  once() { return this; }
  show() {}
  isDestroyed() { return false; }
  getContentBounds() { return { x: 0, y: 0, width: 1200, height: 800 }; }
  getBounds() { return { x: 0, y: 0, width: 1200, height: 800 }; }
  setBounds() {}
  static getAllWindows() { return []; }
}

const app = new Proxy(
  {
    whenReady() {
      recorder.whenReadyResolved = true;
      return Promise.resolve();
    },
    on(event, handler) {
      appEmitter.on(event, handler);
      return app;
    },
    once(event, handler) {
      appEmitter.once(event, handler);
      return app;
    },
    emit(event, ...args) {
      return appEmitter.emit(event, ...args);
    },
    removeListener(event, handler) {
      appEmitter.removeListener(event, handler);
      return app;
    },
    getPath() {
      return os.tmpdir();
    },
    setPath() {},
    getName() {
      return 'Claude';
    },
    getVersion() {
      return '1.11187.4';
    },
    getLocale() {
      return 'en-US';
    },
    getAppPath() {
      return process.cwd();
    },
    setAppUserModelId() {},
    requestSingleInstanceLock() {
      return true;
    },
    setAsDefaultProtocolClient() {
      return true;
    },
    isPackaged: false,
    commandLine: { appendSwitch() {}, appendArgument() {}, hasSwitch() { return false; }, getSwitchValue() { return ''; } },
    dock: { setIcon() {}, setBadge() {}, hide() {}, show() {} },
    quit() {},
    exit() {},
    setLoginItemSettings() {},
    on_: null,
  },
  handler,
);

const ipcMain = new Proxy(
  {
    on() {
      recorder.ipcHandlers += 1;
      return ipcMain;
    },
    handle() {
      recorder.ipcHandlers += 1;
      return ipcMain;
    },
    once() {
      return ipcMain;
    },
    removeHandler() {},
    removeAllListeners() {},
  },
  handler,
);

const electron = new Proxy(
  {
    app,
    BrowserWindow,
    BaseWindow,
    WebContentsView,
    ipcMain,
    // Main process: ipcRenderer is undefined here (libraries like electron-store
    // branch on its presence to detect renderer vs main).
    ipcRenderer: undefined,
    Menu: new Proxy(
      { buildFromTemplate: () => makeAuto(), setApplicationMenu() {} },
      handler,
    ),
    MenuItem: function MenuItem() {},
    Tray: function Tray() {
      return makeAuto();
    },
    nativeImage: new Proxy(
      { createFromPath: () => makeAuto(), createEmpty: () => makeAuto() },
      handler,
    ),
    shell: makeAuto(),
    dialog: makeAuto(),
    session: makeAuto(),
    net: makeAuto(),
    safeStorage: new Proxy(
      {
        isEncryptionAvailable() {
          return false;
        },
        encryptString(s) {
          return Buffer.from(String(s));
        },
        decryptString(b) {
          return Buffer.from(b).toString();
        },
      },
      handler,
    ),
    systemPreferences: makeAuto(),
    powerMonitor: makeAuto(),
    screen: new Proxy(
      { getPrimaryDisplay: () => ({ workAreaSize: { width: 1440, height: 900 }, bounds: { x: 0, y: 0, width: 1440, height: 900 } }), getAllDisplays: () => [] },
      handler,
    ),
    globalShortcut: makeAuto(),
    protocol: makeAuto(),
    crashReporter: makeAuto(),
    nativeTheme: new Proxy({ shouldUseDarkColors: false, on() {} }, handler),
    autoUpdater: makeAuto(),
    clipboard: makeAuto(),
    webContents: makeAuto(),
  },
  handler,
);

module.exports = electron;

// Drive the lifecycle: once the recovered main has registered its handlers,
// fire the events that trigger window creation. Emit `ready`/`activate` a few
// times across the first seconds to cover whichever the app waits on.
for (const delay of [600, 1500, 4000, 9000]) {
  setTimeout(() => {
    try {
      appEmitter.emit('will-finish-launching');
      appEmitter.emit('ready', {}, {});
      appEmitter.emit('activate', {}, false);
    } catch (e) {
      recorder.lifecycleError = (e && (e.stack || String(e))) || 'unknown';
    }
  }, delay).unref?.();
}
