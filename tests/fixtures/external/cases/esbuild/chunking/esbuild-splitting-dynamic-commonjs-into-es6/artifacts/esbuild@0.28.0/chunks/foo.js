var __getOwnPropNames = Object.getOwnPropertyNames;
var __commonJS = (cb, mod) => function __require() {
  return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
};

// input/foo.js
var require_foo = __commonJS({
  "input/foo.js"(exports) {
    exports.bar = 123;
  }
});
export default require_foo();
