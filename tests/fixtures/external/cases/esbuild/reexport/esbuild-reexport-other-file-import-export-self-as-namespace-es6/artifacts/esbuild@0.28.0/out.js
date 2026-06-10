var __defProp = Object.defineProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// input/foo.js
var foo_exports = {};
__export(foo_exports, {
  foo: () => foo,
  ns: () => foo_exports
});
var foo = 123;
export {
  foo,
  foo_exports as ns
};
