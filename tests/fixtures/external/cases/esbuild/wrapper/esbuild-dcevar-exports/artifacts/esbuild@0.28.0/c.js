(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/c.js
  var require_c = __commonJS({
    "input/c.js"(exports, module) {
      var module = { bar: 123 };
      exports.foo = module;
    }
  });
  require_c();
})();
