(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/bar.js
  var bar_exports = {};
  __export(bar_exports, {
    foo: () => foo
  });

  // input/foo.js
  var foo = 123;

  // input/entry.js
  var foo2 = 234;
  console.log(bar_exports, foo, foo2);
})();
