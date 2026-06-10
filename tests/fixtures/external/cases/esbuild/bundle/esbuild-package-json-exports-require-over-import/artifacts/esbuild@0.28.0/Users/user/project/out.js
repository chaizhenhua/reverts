(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/node_modules/pkg/require.js
  var require_require = __commonJS({
    "input/Users/user/project/node_modules/pkg/require.js"() {
      console.log("SUCCESS");
    }
  });

  // input/Users/user/project/src/entry.js
  require_require();
})();
