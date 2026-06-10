(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/a.js
  var require_a = __commonJS({
    "input/a.js"(exports, module) {
      var foo = { bar: 123 };
      module.exports = foo;
    }
  });
  require_a();
})();
