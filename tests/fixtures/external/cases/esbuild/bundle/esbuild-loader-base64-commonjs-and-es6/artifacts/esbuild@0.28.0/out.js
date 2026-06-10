(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/x.b64
  var require_x = __commonJS({
    "input/x.b64"(exports, module) {
      module.exports = "eA==";
    }
  });

  // input/y.b64
  var y_default = "eQ==";

  // input/entry.js
  var x_b64 = require_x();
  console.log(x_b64, y_default);
})();
