(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/foo.ts
  var foo_exports = {};
  __export(foo_exports, {
    foo: () => foo
  });
  var foo = 123;

  // input/entry.ts
  var foo2 = 234;
  console.log(foo_exports, foo, foo2);
})();
