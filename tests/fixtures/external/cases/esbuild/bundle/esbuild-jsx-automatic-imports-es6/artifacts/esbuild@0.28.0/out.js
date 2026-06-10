(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/custom-react.js
  function jsx() {
  }
  function Fragment() {
  }

  // input/entry.jsx
  var import_jsx_runtime = __require("react/jsx-runtime");
  console.log(/* @__PURE__ */ (0, import_jsx_runtime.jsx)("div", { jsx }), /* @__PURE__ */ (0, import_jsx_runtime.jsx)(import_jsx_runtime.Fragment, { children: /* @__PURE__ */ (0, import_jsx_runtime.jsx)(Fragment, {}) }));
})();
