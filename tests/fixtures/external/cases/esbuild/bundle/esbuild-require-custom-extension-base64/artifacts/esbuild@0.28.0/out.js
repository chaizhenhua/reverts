(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/test.custom
  var require_test = __commonJS({
    "input/test.custom"(exports, module) {
      module.exports = "YQBiwoBjw79k";
    }
  });

  // input/entry.js
  console.log(require_test());
})();
