var __getOwnPropNames = Object.getOwnPropertyNames;
var __commonJS = (cb, mod) => function __require() {
  return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
};

// input/node_modules/fs/abc.js
var require_abc = __commonJS({
  "input/node_modules/fs/abc.js"() {
    console.log("include this");
  }
});

// input/node_modules/fs/index.js
var require_fs = __commonJS({
  "input/node_modules/fs/index.js"() {
    console.log("include this too");
  }
});

// input/entry.js
console.log([
  // These are node core modules
  require("fs"),
  require("fs/promises"),
  require("node:foo"),
  // These are not node core modules
  require_abc(),
  require_fs()
]);
