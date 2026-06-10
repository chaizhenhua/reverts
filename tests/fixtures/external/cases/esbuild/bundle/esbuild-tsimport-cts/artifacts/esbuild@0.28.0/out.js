var __getOwnPropNames = Object.getOwnPropertyNames;
var __commonJS = (cb, mod) => function __require() {
  return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
};

// input/required.cjs
var require_required = __commonJS({
  "input/required.cjs"() {
    console.log("works");
  }
});

// input/entry.ts
require_required();
