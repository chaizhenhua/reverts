(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/foo.js
  var foo_exports = {};
  __export(foo_exports, {
    bar_ns: () => bar_exports
  });

  // input/bar.js
  var bar_exports = {};
  __export(bar_exports, {
    bar: () => bar
  });
  var bar = 123;

  // input/entry.js
  console.log(foo_exports);
})();
