(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };
  var __copyProps = (to, from, except, desc) => {
    if (from && typeof from === "object" || typeof from === "function") {
      for (let key of __getOwnPropNames(from))
        if (!__hasOwnProp.call(to, key) && key !== except)
          __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
    }
    return to;
  };
  var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

  // input/a.js
  var abc;
  var init_a = __esm({
    "input/a.js"() {
      abc = void 0;
    }
  });

  // input/b.js
  var b_exports = {};
  __export(b_exports, {
    xyz: () => xyz
  });
  var xyz;
  var init_b = __esm({
    "input/b.js"() {
      xyz = null;
    }
  });

  // input/commonjs.js
  var commonjs_exports = {};
  __export(commonjs_exports, {
    C: () => Class,
    Class: () => Class,
    Fn: () => Fn,
    abc: () => abc,
    b: () => b_exports,
    c: () => c,
    default: () => commonjs_default,
    l: () => l,
    v: () => v
  });
  function Fn() {
  }
  var commonjs_default, v, l, c, Class;
  var init_commonjs = __esm({
    "input/commonjs.js"() {
      init_a();
      init_b();
      commonjs_default = 123;
      v = 234;
      l = 234;
      c = 234;
      Class = class {
      };
    }
  });

  // input/c.js
  var c_exports = {};
  __export(c_exports, {
    default: () => c_default
  });
  var c_default;
  var init_c = __esm({
    "input/c.js"() {
      c_default = class {
      };
    }
  });

  // input/d.js
  var d_exports = {};
  __export(d_exports, {
    default: () => Foo
  });
  var Foo;
  var init_d = __esm({
    "input/d.js"() {
      Foo = class {
      };
      Foo.prop = 123;
    }
  });

  // input/e.js
  var e_exports = {};
  __export(e_exports, {
    default: () => e_default
  });
  function e_default() {
  }
  var init_e = __esm({
    "input/e.js"() {
    }
  });

  // input/f.js
  var f_exports = {};
  __export(f_exports, {
    default: () => foo
  });
  function foo() {
  }
  var init_f = __esm({
    "input/f.js"() {
      foo.prop = 123;
    }
  });

  // input/g.js
  var g_exports = {};
  __export(g_exports, {
    default: () => g_default
  });
  async function g_default() {
  }
  var init_g = __esm({
    "input/g.js"() {
    }
  });

  // input/h.js
  var h_exports = {};
  __export(h_exports, {
    default: () => foo2
  });
  async function foo2() {
  }
  var init_h = __esm({
    "input/h.js"() {
      foo2.prop = 123;
    }
  });

  // input/entry.js
  init_commonjs();
  init_c();
  init_d();
  init_e();
  init_f();
  init_g();
  init_h();
})();
