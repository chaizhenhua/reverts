(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/index.js
  var import_foo = __require("@scope/foo");
  var import_bar = __require("@scope/foo/bar");
  var foo = new import_foo.Foo();
  var bar = new import_bar.Bar();
})();
