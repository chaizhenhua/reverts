(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/test.json
  var require_test = __commonJS({
    "input/test.json"(exports, module) {
      module.exports = {
        a: true,
        b: 123,
        c: [null]
      };
    }
  });

  // input/entry.js
  console.log(require_test());
})();
