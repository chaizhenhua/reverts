(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
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

  // input/c.js
  var c_exports = {};
  var init_c = __esm({
    "input/c.js"() {
      if (false) for (let x of y) ;
    }
  });

  // input/b.js
  var b_exports = {};
  var init_b = __esm({
    "input/b.js"() {
      init_c();
    }
  });

  // input/a.js
  var a_exports = {};
  var init_a = __esm({
    "input/a.js"() {
      init_b();
    }
  });

  // input/entry.js
  var entry_exports = {};
  var init_entry = __esm({
    "input/entry.js"() {
      init_a();
      init_b();
      init_c();
      init_entry();
      if (false) for (let x of y) ;
    }
  });
  init_entry();
})();
