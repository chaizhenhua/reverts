(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/top-level-this-esm.jsx
  var import_jsx_dev_runtime = __require("react/jsx-dev-runtime");
  var foo = /* @__PURE__ */ (0, import_jsx_dev_runtime.jsxDEV)("div", {}, void 0, false, {
    fileName: "input/top-level-this-esm.jsx",
    lineNumber: 1,
    columnNumber: 18
  });
  if (Foo) {
    foo = /* @__PURE__ */ (0, import_jsx_dev_runtime.jsxDEV)(Foo, { children: "nested top-level this" }, void 0, false, {
      fileName: "input/top-level-this-esm.jsx",
      lineNumber: 1,
      columnNumber: 43
    });
  }
})();
