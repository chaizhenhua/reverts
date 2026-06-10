var __getOwnPropNames = Object.getOwnPropertyNames;
var __commonJS = (cb, mod) => function __require() {
  return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
};

// input/is-main.js
var require_is_main = __commonJS({
  "input/is-main.js"(exports2, module2) {
    module2.exports = require.main === module2;
  }
});

// input/entry.js
console.log("is main:", require.main === module);
console.log(require_is_main());
console.log("cache:", require.cache);
