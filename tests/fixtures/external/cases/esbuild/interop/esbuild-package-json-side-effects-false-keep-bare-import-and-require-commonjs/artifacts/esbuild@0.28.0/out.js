(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // Users/user/project/node_modules/demo-pkg/index.js
  var require_demo_pkg = __commonJS({
    "Users/user/project/node_modules/demo-pkg/index.js"(exports) {
      exports.foo = 123;
      console.log("hello");
    }
  });

  // Users/user/project/src/entry.js
  require_demo_pkg();
  console.log("unused import");
})();
