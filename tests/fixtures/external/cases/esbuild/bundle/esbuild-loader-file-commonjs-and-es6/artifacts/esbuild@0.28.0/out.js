(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/x.txt
  var require_x = __commonJS({
    "input/x.txt"(exports, module) {
      module.exports = "./x-LSAMBFUD.txt";
    }
  });

  // input/y.txt
  var y_default = "./y-YE5AYNFB.txt";

  // input/entry.js
  var x_url = require_x();
  console.log(x_url, y_default);
})();
