(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/Users/user/project/node_modules/demo-pkg/index.js
  var demo_pkg_exports = {};
  __export(demo_pkg_exports, {
    foo: () => foo
  });
  var foo = 123;
  console.log("hello");

  // input/Users/user/project/src/entry.js
  console.log(demo_pkg_exports);
})();
