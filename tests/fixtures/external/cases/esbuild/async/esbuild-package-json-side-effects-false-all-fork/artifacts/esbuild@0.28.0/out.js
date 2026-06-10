(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/Users/user/project/node_modules/c/index.js
  var foo;
  var init_c = __esm({
    "input/Users/user/project/node_modules/c/index.js"() {
      foo = "foo";
    }
  });

  // input/Users/user/project/node_modules/b/index.js
  var init_b = __esm({
    "input/Users/user/project/node_modules/b/index.js"() {
      init_c();
    }
  });

  // input/Users/user/project/node_modules/a/index.js
  var a_exports = {};
  __export(a_exports, {
    foo: () => foo
  });
  var init_a = __esm({
    "input/Users/user/project/node_modules/a/index.js"() {
      init_b();
    }
  });

  // input/Users/user/project/src/entry.js
  Promise.resolve().then(() => (init_a(), a_exports)).then((x) => assert(x.foo === "foo"));
})();
