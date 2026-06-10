(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // entry.js
  var import_pkg = __require("alias/pkg");
  var import_foo = __require("alias/pkg_foo");
  var import_bar = __require("alias/pkg_foo_bar");
  var import_baz = __require("alias/pkg_foo_bar/baz");
  var import_baz2 = __require("alias/pkg/bar/baz");
  var import_baz3 = __require("alias/pkg/baz");
})();
