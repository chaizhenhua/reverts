(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/bar.js
  var bar_exports = {};
  __export(bar_exports, {
    x: () => x
  });
  var x = 123;

  // input/entry.js
  console.log(bar_exports.foo);
})();
