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

  // input/Users/user/project/common/index.js
  var require_common = __commonJS({
    "input/Users/user/project/common/index.js"(exports) {
      exports.common = true;
    }
  });

  // input/Users/user/project/home/index.js
  var require_home = __commonJS({
    "input/Users/user/project/home/index.js"() {
      require_home();
      require_pkg();
      require_common();
    }
  });
  require_home();
})();
