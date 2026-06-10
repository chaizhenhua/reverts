// inject.js
var obj = {};
var sideEffects = console.log("side effects");

// node_modules/unused/index.js
console.log("This is unused but still has side effects");

// replacement.js
var replace = {
  test() {
  }
};
var replace2 = {
  test() {
  }
};

// re-export.js
var import_external_pkg = require("external-pkg");
var import_external_pkg2 = require("external-pkg2");

// entry.js
var sideEffects2 = console.log("this should be renamed");
var collide = 123;
console.log(obj.prop);
console.log("defined");
console.log("should be used");
console.log("should be used");
console.log(replace.test);
console.log(replace2.test);
console.log(collide);
console.log(import_external_pkg.re_export);
console.log(re_export2);
