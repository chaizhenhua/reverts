var __defProp = Object.defineProperty;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __esm = (fn, res) => function __init() {
  return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
};
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// input/src/a.js
var A;
var init_a = __esm({
  "input/src/a.js"() {
    A = 42;
  }
});

// input/src/b.js
var B;
var init_b = __esm({
  "input/src/b.js"() {
    B = async () => (await Promise.resolve().then(() => (init_src(), src_exports))).A;
  }
});

// input/src/index.js
var src_exports = {};
__export(src_exports, {
  A: () => A,
  B: () => B
});
var init_src = __esm({
  "input/src/index.js"() {
    init_a();
    init_b();
  }
});
init_src();
export {
  A,
  B
};
