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

  // input/foo.js
  var foo_exports = {};
  __export(foo_exports, {
    a: () => a,
    b: () => b,
    c: () => c,
    d: () => d,
    e: () => e,
    f: () => f,
    g: () => g,
    h: () => h,
    i: () => i,
    j: () => j
  });
  var a, b, c, d, e, x, f, g, h, i, y, j, y;
  var init_foo = __esm({
    "input/foo.js"() {
      a = "a";
      for (b = "b"; 0; ) ;
      if (true) {
        c = "c";
      }
      if (true) d = "d";
      if (false) {
      } else e = "e";
      x = 1;
      while (x--) f = "f";
      do
        g = "g";
      while (0);
      for (; x++; ) h = "h";
      for (y in "y") i = "i";
      for (y of "y") j = "j";
    }
  });

  // input/entry.js
  init_foo();
})();
