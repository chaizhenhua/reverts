var __defProp = Object.defineProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// input/foo/test.js
var test_exports = {};
__export(test_exports, {
  foo: () => foo
});
var foo = 123;

// input/bar/test.js
var test_exports2 = {};
__export(test_exports2, {
  bar: () => bar
});
var bar = 123;

// input/entry.js
console.log(exports, module.exports, test_exports, test_exports2);
