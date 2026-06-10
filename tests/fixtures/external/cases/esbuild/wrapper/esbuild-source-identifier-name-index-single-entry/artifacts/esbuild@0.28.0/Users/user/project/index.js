(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/Users/user/project/node_modules/pkg/index.js
  var require_pkg = __commonJS({
    "input/Users/user/project/node_modules/pkg/index.js"(exports) {
      exports.pkg = true;
    }
  });

  // input/Users/user/project/nested/index.js
  var require_nested = __commonJS({
    "input/Users/user/project/nested/index.js"(exports) {
      exports.nested = true;
    }
  });

  // input/Users/user/project/index.js
  var require_project = __commonJS({
    "input/Users/user/project/index.js"() {
      require_project();
      require_pkg();
      require_nested();
    }
  });
  require_project();
})();
