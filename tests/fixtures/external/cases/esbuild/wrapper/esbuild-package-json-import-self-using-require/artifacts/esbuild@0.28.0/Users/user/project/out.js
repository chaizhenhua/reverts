(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // Users/user/project/src/foo-require.js
  var require_foo_require = __commonJS({
    "Users/user/project/src/foo-require.js"(exports, module) {
      module.exports = "foo";
    }
  });

  // Users/user/project/src/index.js
  var require_index = __commonJS({
    "Users/user/project/src/index.js"(exports, module) {
      module.exports = "index";
      console.log(
        require_index(),
        require_foo_require()
      );
    }
  });
  require_index();
})();
