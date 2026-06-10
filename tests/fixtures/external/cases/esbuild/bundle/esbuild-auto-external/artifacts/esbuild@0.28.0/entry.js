(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/entry.js
  var import_code = __require("http://example.com/code.js");
  var import_code2 = __require("https://example.com/code.js");
  var import_code3 = __require("//example.com/code.js");
  var import_javascript_base64_ZXhwb3J0IGRlZmF1bHQgMTIz = __require("data:application/javascript;base64,ZXhwb3J0IGRlZmF1bHQgMTIz");
})();
