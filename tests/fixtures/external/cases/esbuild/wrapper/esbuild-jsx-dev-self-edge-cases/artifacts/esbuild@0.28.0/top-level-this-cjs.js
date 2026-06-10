(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });
  var __commonJS = (cb, mod) => function __require2() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/top-level-this-cjs.jsx
  var require_top_level_this_cjs = __commonJS({
    "input/top-level-this-cjs.jsx"(exports) {
      var import_jsx_dev_runtime = __require("react/jsx-dev-runtime");
      exports.foo = /* @__PURE__ */ (0, import_jsx_dev_runtime.jsxDEV)("div", {}, void 0, false, {
        fileName: "input/top-level-this-cjs.jsx",
        lineNumber: 1,
        columnNumber: 15
      });
    }
  });
  require_top_level_this_cjs();
})();
