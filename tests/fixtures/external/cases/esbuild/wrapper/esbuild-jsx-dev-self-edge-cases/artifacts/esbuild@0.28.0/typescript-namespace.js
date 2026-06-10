(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/typescript-namespace.tsx
  var import_jsx_dev_runtime = __require("react/jsx-dev-runtime");
  var Foo;
  ((Foo2) => {
    Foo2.foo = /* @__PURE__ */ (0, import_jsx_dev_runtime.jsxDEV)("div", {}, void 0, false, {
      fileName: "input/typescript-namespace.tsx",
      lineNumber: 1,
      columnNumber: 41
    });
  })(Foo || (Foo = {}));
})();
