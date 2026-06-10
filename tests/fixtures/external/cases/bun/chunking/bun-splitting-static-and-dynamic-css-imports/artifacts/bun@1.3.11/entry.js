var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
  get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
}) : x)(function(x) {
  if (typeof require !== "undefined")
    return require.apply(this, arguments);
  throw Error('Dynamic require of "' + x + '" is not supported');
});

// tests/fixtures/external/cases/bun/chunking/bun-splitting-static-and-dynamic-css-imports/input/entry.js
import("./chunks/chunk-1.js").then(() => console.log("dynamic module loaded"));
