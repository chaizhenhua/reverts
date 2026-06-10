(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // test.txt
  var require_test = __commonJS({
    "test.txt"(exports, module) {
      module.exports = "This is a test.";
    }
  });

  // entry.js
  console.log(require_test());
})();
