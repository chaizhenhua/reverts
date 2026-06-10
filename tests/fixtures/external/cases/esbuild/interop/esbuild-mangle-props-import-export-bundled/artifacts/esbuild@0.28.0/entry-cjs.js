(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
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

  // esm.js
  var esm_exports = {};
  __export(esm_exports, {
    esm_foo_: () => esm_foo_
  });
  var esm_foo_;
  var init_esm = __esm({
    "esm.js"() {
      esm_foo_ = "foo";
    }
  });

  // cjs.js
  var require_cjs = __commonJS({
    "cjs.js"(exports) {
      exports.a = "foo";
    }
  });

  // entry-cjs.js
  var require_entry_cjs = __commonJS({
    "entry-cjs.js"(exports) {
      var { b: esm_foo_2 } = (init_esm(), __toCommonJS(esm_exports));
      var { a: cjs_foo_ } = require_cjs();
      exports.c = [
        esm_foo_2,
        cjs_foo_
      ];
    }
  });
  require_entry_cjs();
})();
