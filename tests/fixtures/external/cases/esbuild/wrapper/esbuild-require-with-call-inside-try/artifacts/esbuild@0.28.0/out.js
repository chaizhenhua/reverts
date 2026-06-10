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

  // input/entry.js
  var require_entry = __commonJS({
    "input/entry.js"(exports) {
      try {
        const supportsColor = __require("supports-color");
        if (supportsColor && (supportsColor.stderr || supportsColor).level >= 2) {
          exports.colors = [];
        }
      } catch (error) {
      }
    }
  });
  require_entry();
})();
