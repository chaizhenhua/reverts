(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __glob = (map) => (path) => {
    var fn = map[path];
    if (fn) return fn();
    throw new Error("Module not found in bundle: " + path);
  };
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/foo.js
  var foo_exports = {};
  __export(foo_exports, {
    default: () => foo_default
  });
  var foo_default;
  var init_foo = __esm({
    "input/foo.js"() {
      foo_default = { "this is json not js": true };
    }
  });

  // import("./foo*") in input/entry.js
  var globImport_foo = __glob({
    "./foo.js": () => Promise.resolve().then(() => (init_foo(), foo_exports))
  });

  // input/entry.js
  globImport_foo("./foo" + bar).then(console.log);
})();
