(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/bar.ts
  var bar_exports = {};
  __export(bar_exports, {
    foo: () => foo
  });

  // input/foo.ts
  var foo = 123;

  // input/entry.ts
  var foo2 = 234;
  console.log(bar_exports, foo, foo2);
})();
