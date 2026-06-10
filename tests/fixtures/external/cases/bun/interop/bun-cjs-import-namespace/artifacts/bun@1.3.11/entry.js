// @bun
var __defProp = Object.defineProperty;
var __returnValue = (v) => v;
function __exportSetter(name, newValue) {
  this[name] = __returnValue.bind(null, newValue);
}
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, {
      get: all[name],
      enumerable: true,
      configurable: true,
      set: __exportSetter.bind(all, name)
    });
};

// tests/fixtures/external/cases/bun/interop/bun-cjs-import-namespace/input/lib.cjs
var exports_lib = {};
__export(exports_lib, {
  foo: () => $foo,
  bar: () => $bar
});
var $foo = "foo";
var $bar = "bar";

// tests/fixtures/external/cases/bun/interop/bun-cjs-import-namespace/input/entry.js
console.log(JSON.stringify(exports_lib));
