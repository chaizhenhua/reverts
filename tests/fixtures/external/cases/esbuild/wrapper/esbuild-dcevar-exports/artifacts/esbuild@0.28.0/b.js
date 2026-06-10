(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/b.js
  var require_b = __commonJS({
    "input/b.js"(exports, module) {
      var exports = { bar: 123 };
      module.exports = exports;
    }
  });
  require_b();
})();
