(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/node_modules/demo-pkg/main.js
  var require_main = __commonJS({
    "input/Users/user/project/node_modules/demo-pkg/main.js"(exports, module) {
      module.exports = "main";
    }
  });

  // input/Users/user/project/src/entry.js
  console.log(require_main());
})();
