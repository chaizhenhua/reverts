var __getOwnPropNames = Object.getOwnPropertyNames;
var __esm = (fn, res) => function __init() {
  return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
};

// input/c.js
var c_exports = {};
var init_c = __esm({
  async "input/c.js"() {
    await 0;
  }
});

// input/b.js
var b_exports = {};
var init_b = __esm({
  async "input/b.js"() {
    await init_c();
  }
});

// input/a.js
var a_exports = {};
var init_a = __esm({
  async "input/a.js"() {
    await init_b();
  }
});

// input/entry.js
var entry_exports = {};
var init_entry = __esm({
  async "input/entry.js"() {
    init_a();
    init_b();
    init_c();
    init_entry();
    await 0;
  }
});
await init_entry();
