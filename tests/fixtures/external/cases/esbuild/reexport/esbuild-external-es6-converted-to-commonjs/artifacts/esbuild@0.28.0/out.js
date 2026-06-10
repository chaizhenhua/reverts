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
var __reExport = (target, mod, secondTarget) => (__copyProps(target, mod, "default"), secondTarget && __copyProps(secondTarget, mod, "default"));
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// input/a.js
var a_exports = {};
__export(a_exports, {
  ns: () => ns
});
import * as ns from "x";
var init_a = __esm({
  "input/a.js"() {
  }
});

// input/b.js
var b_exports = {};
__export(b_exports, {
  ns: () => ns2
});
import * as ns2 from "x";
var init_b = __esm({
  "input/b.js"() {
  }
});

// input/c.js
var c_exports = {};
__export(c_exports, {
  ns: () => ns3
});
import * as ns3 from "x";
var init_c = __esm({
  "input/c.js"() {
  }
});

// input/d.js
var d_exports = {};
__export(d_exports, {
  ns: () => ns4
});
import { ns as ns4 } from "x";
var init_d = __esm({
  "input/d.js"() {
  }
});

// input/e.js
var e_exports = {};
import * as x_star from "x";
var init_e = __esm({
  "input/e.js"() {
    __reExport(e_exports, x_star);
  }
});

// input/entry.js
init_a();
init_b();
init_c();
init_d();
init_e();
