(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/a/test.txt
  var require_test = __commonJS({
    "input/a/test.txt"(exports, module) {
      module.exports = "./test-J7OMUXO3.txt";
    }
  });

  // input/b/test.txt
  var require_test2 = __commonJS({
    "input/b/test.txt"(exports, module) {
      module.exports = "./test-J7OMUXO3.txt";
    }
  });

  // input/entry.js
  console.log(
    require_test(),
    require_test2()
  );
})();
