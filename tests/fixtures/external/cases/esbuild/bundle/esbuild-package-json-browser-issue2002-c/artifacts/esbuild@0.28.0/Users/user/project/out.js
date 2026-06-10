(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/src/node_modules/sub/index.js
  var require_sub = __commonJS({
    "input/Users/user/project/src/node_modules/sub/index.js"() {
      works();
    }
  });

  // input/Users/user/project/src/node_modules/pkg/sub/foo.js
  var require_foo = __commonJS({
    "input/Users/user/project/src/node_modules/pkg/sub/foo.js"() {
      require_sub();
    }
  });

  // input/Users/user/project/src/entry.js
  require_foo();
})();
