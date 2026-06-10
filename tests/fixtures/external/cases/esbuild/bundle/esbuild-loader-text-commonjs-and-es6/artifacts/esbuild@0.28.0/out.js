(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // x.txt
  var require_x = __commonJS({
    "x.txt"(exports, module) {
      module.exports = "x";
    }
  });

  // y.txt
  var y_default = "y";

  // entry.js
  var x_txt = require_x();
  console.log(x_txt, y_default);
})();
