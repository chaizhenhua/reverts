(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/test.txt
  var require_test = __commonJS({
    "input/test.txt"(exports, module) {
      module.exports = "test.txt";
    }
  });

  // input/test.base64.txt
  var require_test_base64 = __commonJS({
    "input/test.base64.txt"(exports, module) {
      module.exports = "dGVzdC5iYXNlNjQudHh0";
    }
  });

  // input/entry.js
  console.log(require_test(), require_test_base64());
})();
