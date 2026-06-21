# Code Analysis Patterns

Common patterns in webpack/esbuild bundled JavaScript and how to name them.

**Important**: All helper names shown below (e.g., `<require>`, `<toESM>`) are placeholders. Each bundle uses different minified names. Always use the **runtime helper mapping** from Phase 1b discovery to identify which minified function corresponds to which role.

## Module Wrapper Patterns

### ESM Module
```js
var xyz = <esm_lazy>(() => {
    dep1();           // dependency init calls
    dep2();
    myVar = "value";  // global variable assignments
});
```
The ESM lazy wrapper indicates an ESM module. Can be either app code or a package — classify by content, not wrapper.

### CJS Module
```js
var xyz = <commonjs>((exports, module) => {
    var fs = <require>('fs');           // Node.js require
    exports.parse = function(s) { ... };
    module.exports = MyClass;
});
```
The CJS wrapper indicates a CommonJS module. Can be either app code or a package — classify by content, not wrapper.

### Classification: Package vs App

`cat: "pkg"` = **published third-party npm package** (e.g., lodash, ws, tslib, zod, sentry).
`cat: "app"` = everything else, including project-internal utilities.

If you cannot name the specific npm package, classify as `"app"`.

For tiny init-shim modules (`var X = O(() => { Y = Z; })` shape, ≤50B, `wrapper_kind ∈ {pure,composite}_init_wrapper`), their classification must inherit from the owner of their target global, not from the shape alone. See [init-shim-classification.md](init-shim-classification.md) for the recovery protocol, parent-chain rules, source fingerprints, and the cross-project mislabel cascade pitfall.

**Do NOT classify by wrapper type alone.** Use content-based signals:

Package indicators — module IS code from a **specific, identifiable npm package**:
- Copyright/license comments (MIT, Apache, ISC) or version metadata
- You can identify WHICH npm package this code belongs to by name
- Code structure matches a well-known published package (WebSocket server → `ws`, schema validation → `zod`)
- Contains `name`/`version`/`author` package metadata fields

App indicators — everything else:
- Project-specific business logic, config, UI components, domain concepts
- Internal utility/helper modules (even generic ones — if you can't name the npm package, it's app)
- Glue code integrating multiple packages
- Imports heavily from other modules in the same bundle

### Init-Only Module
```js
var xyz = <esm_lazy>(() => {
    dep1();
    dep2();
    // lots of dependency calls, then data definitions
    CONFIG = { key: "value", ... };
});
```
A module that only initializes dependencies and defines global variables. Name by its data content.

## Common Runtime Patterns

### React Import
```js
FT = <toESM>(X6(), 1)   // equivalent to: import React from 'react'
```
`<toESM>(X6(), 1)` is the interop pattern for default imports from CJS modules.

### Zod Schema
```js
schema = u.object({
    id: u.string(),
    name: u.string(),
    status: u.enum(['active', 'inactive'])
});
```
`u` is typically Zod. Module name: `schemas/xxx`.

### Node.js Require
```js
var fs = <require>('fs');      // require('fs')
var path = <require>('path');  // require('path')
```
The require helper is the bundled `require` function. Its minified name varies per bundle — check the helper mapping.

## Structural Patterns

### Factory Function
```js
function c3(a, b) { return new k7(a, b); }
```
Name: `createSomething` (camelCase, verb prefix `create`)

### Singleton
```js
var x; function g() { if (!x) x = new T(); return x; }
```
Name: `getInstance` or `getSharedClient`

### React Component
```js
function C(props) { return React.createElement("div", null, props.children); }
```
Name: `PascalCase` — `UserProfile`, `NavigationBar`

### React Hook
```js
function u() { var [s, setS] = useState(null); useEffect(...); return s; }
```
Name: `useCurrentUser`, `useFetchData` (verb prefix `use`)

### React Context
```js
ctx = FT.createContext({ marker: '' });
```
Module name: `ui/xxx-context`

### Error Class
```js
MyError = class extends Error {
    constructor() {
        super('Download stalled: no data received');
        this.name = 'StallTimeoutError';
    }
};
```
The `this.name` string reveals the class name directly.

### Event Handler
```js
function h(e) { e.preventDefault(); /* ... */ }
```
Name: `handleSubmit`, `handleClick` (verb prefix `handle`)

### Higher-Order Function
```js
function w(fn) { return function(...args) { /* ... */ fn(...args); }; }
```
Name: `withRetry`, `withLogging`, `memoize`

## Domain Inference from Content

### String Arrays / Config Objects
```js
LIST = ['pending', 'in_progress', 'completed'];
CONFIG = { timeout: 30000, retries: 3 };
```
The values reveal the domain. Task status → `schemas/task-status`. Timeout config → `config/request-options`.

### Browser/Platform Config
```js
browsers = {
    chrome: { name: 'Google Chrome', macos: { appName: 'Google Chrome', ... } },
    brave: { name: 'Brave', ... }
};
```
Module name: `config/browser-definitions`

### CLI Command Definitions
```js
PATTERNS = [/^sudo\s/, /\brm\s+-rf/, /\bdd\s+if=/];
```
Dangerous command patterns → `cli/dangerous-command-patterns`

### Keybinding Maps
```js
bindings = { 'ctrl+c': 'app:interrupt', 'ctrl+d': 'app:exit', ... };
```
Module name: `keybindings/default-bindings`

### MCP Tool Names
```js
TOOLS = ['mcp__slack__send_message', 'mcp__chrome__navigate'];
```
Module name: `mcp/tool-allowlist` or similar

## Export Pattern Clues

### Named Export Helper (esbuild)
```js
var m = {};
__export(m, {
  UserService: () => C3,
  fetchUser: () => f7,
  default: () => D2
});
```
The keys (`UserService`, `fetchUser`) are the real names. Use them directly as symbol semantic names.

### CommonJS Exports
```js
module.exports = { createClient: f3, Client: C1 };
exports.createClient = f3;
```

### Re-export / Barrel Module
```js
var m = {}; __export(m, { ...otherModule });
```
Often an index module. Name by what it re-exports (e.g., `components/index`).

## High-Signal String Patterns

- Error messages: `throw new Error("Invalid user ID")` → user-related module
- API paths: `"/api/v2/users"` → API client
- Class names: `this.name = 'HttpClient'` → class name revealed
- Log prefixes: `console.log("[Auth]")` → authentication module
- Config keys: `process.env.DATABASE_URL` → database config
- UI labels: `label: 'Submit'`, `description: 'Configure...'` → UI component or config

## Package Import Misclassification Detection

After output generation, scan for `__reverts_pkg_X.Y` patterns where `Y` is NOT a real export of package `X`. These indicate application-internal symbols incorrectly routed through a package import during flow analysis.

### Red flags in generated output

```typescript
// BAD: app-module-path prefix as package property
var terminal_ascii_table_AT: any = __reverts_pkg_axios.terminal_ascii_table_AT;
// → "terminal_ascii_table_" is an app module path prefix, not an axios export

// BAD: init/config module name as package property  
var init_oauth_config_D9: any = __reverts_pkg_axios.init_oauth_config_D9;
// → "init_oauth_config_" names an internal init module

// BAD: minified 2-3 char name as package property
let QW1: any = __reverts_pkg_axios.bqq;
// → real package APIs don't export minified names like "bqq"

// OK: legitimate package sub-module export
var CanceledError: any = __reverts_pkg_axios.CanceledError;
// → CanceledError is a real axios export
```

### Detection heuristics

1. **App-path prefix**: Property name starts with `terminal_`, `init_`, `app_`, `config_`, `runtime_`, `ui_`, `auth_`, `tools_`, `utils_`, `helpers_`, `state_`, `schemas_` — these are application module path conventions
2. **Minified property**: Property name is 1-4 chars (`bqq`, `dzA`, `AT`) — real package exports have meaningful names
3. **Semantic name mismatch**: Property has a semantic name that encodes a module path (underscores acting as path separators) that doesn't match the package name
4. **DB cross-check**: `query(project_id, entity="symbols", search="<property_name>")` shows the symbol belongs to an `application` category module

### Root cause

Low-confidence (0.2) import bindings from the bundle analyzer. These arise when init-wrapper dependency chains create transitive paths: module A → init-wrapper B → package C. The flow analysis may attribute A's symbols to C's package namespace. Fix by reclassifying the intermediary module or correcting the import binding.
