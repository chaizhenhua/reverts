(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // foo.js
  var foo_exports = {};
  __export(foo_exports, {
    x: () => x
  });

  // bar.js
  var x = 123;

  // entry.js
  console.log(foo_exports, void 0);
})();
