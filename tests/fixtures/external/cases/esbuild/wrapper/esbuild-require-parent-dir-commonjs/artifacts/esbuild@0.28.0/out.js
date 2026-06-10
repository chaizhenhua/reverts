(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/src/index.js
  var require_index = __commonJS({
    "input/Users/user/project/src/index.js"(exports, module) {
      module.exports = 123;
    }
  });

  // input/Users/user/project/src/dir/entry.js
  console.log(require_index());
})();
