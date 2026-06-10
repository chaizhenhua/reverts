(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/test.svg
  var require_test = __commonJS({
    "input/test.svg"(exports, module) {
      module.exports = "./test-IPILGNO5.svg";
    }
  });

  // input/entry.js
  console.log(require_test());
})();
