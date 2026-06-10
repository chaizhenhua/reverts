(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/entry.jsx
  var import_foo = __require("foo");
  var import_foo2 = __require("foo");
  var import_foo3 = __require("foo");
  var import_foo4 = __require("foo");
  var import_foo5 = __require("foo");
})();
