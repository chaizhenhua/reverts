(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/b.js
  var require_b = __commonJS({
    "input/b.js"(exports) {
      exports.x = 123;
    }
  });

  // input/a.js
  console.log(require_b());
  console.log(require_b());
})();
