(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/x.json
  var require_x = __commonJS({
    "input/x.json"(exports, module) {
      module.exports = { x: true };
    }
  });

  // input/y.json
  var y_default = { y1: true, y2: false };

  // input/z.json
  var small = "some small text";
  var if2 = "test keyword imports";

  // input/entry.js
  var x_json = require_x();
  console.log(x_json, y_default, small, if2);
})();
