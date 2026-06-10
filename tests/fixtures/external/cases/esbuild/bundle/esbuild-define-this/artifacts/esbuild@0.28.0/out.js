(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // entry.js
  var require_entry = __commonJS({
    "entry.js"(exports) {
      ok(
        // These should be fully substituted
        exports,
        exports.foo,
        exports.foo.bar,
        // Should just substitute "this.foo"
        exports.foo.baz,
        // This should not be substituted
        exports.bar
      );
      (() => {
        ok(
          exports,
          exports.foo,
          exports.foo.bar,
          exports.foo.baz,
          exports.bar
        );
      })();
      (function() {
        doNotSubstitute(
          this,
          this.foo,
          this.foo.bar,
          this.foo.baz,
          this.bar
        );
      })();
    }
  });
  require_entry();
})();
