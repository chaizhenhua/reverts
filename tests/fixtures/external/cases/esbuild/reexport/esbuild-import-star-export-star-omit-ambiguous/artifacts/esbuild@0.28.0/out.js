(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/common.js
  var common_exports = {};
  __export(common_exports, {
    x: () => x,
    z: () => z
  });

  // input/foo.js
  var x = 1;

  // input/bar.js
  var z = 4;

  // input/entry.js
  console.log(common_exports);
})();
