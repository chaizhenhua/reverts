(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/src/dir/index.js
  var require_dir = __commonJS({
    "input/Users/user/project/src/dir/index.js"(exports, module) {
      module.exports = 123;
    }
  });

  // input/Users/user/project/src/entry.js
  console.log(require_dir());
})();
