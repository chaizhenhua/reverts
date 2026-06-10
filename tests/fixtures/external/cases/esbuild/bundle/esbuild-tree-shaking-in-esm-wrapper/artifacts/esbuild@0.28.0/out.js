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

// input/lib.js
var keep1, keep2;
var init_lib = __esm({
  "input/lib.js"() {
    keep1 = () => "keep1";
    keep2 = () => "keep2";
  }
});

// input/cjs.js
var cjs_exports = {};
__export(cjs_exports, {
  default: () => cjs_default
});
var cjs_default;
var init_cjs = __esm({
  "input/cjs.js"() {
    init_lib();
    cjs_default = keep2();
  }
});

// input/entry.js
init_lib();
console.log(keep1(), (init_cjs(), __toCommonJS(cjs_exports)));
