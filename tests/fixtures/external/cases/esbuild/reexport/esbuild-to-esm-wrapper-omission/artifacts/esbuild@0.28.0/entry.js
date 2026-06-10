var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __reExport = (target, mod, secondTarget) => (__copyProps(target, mod, "default"), secondTarget && __copyProps(secondTarget, mod, "default"));
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);
var entry_exports = {};
module.exports = __toCommonJS(entry_exports);
var import_a_nowrap = require("a_nowrap");
var import_b_nowrap = require("b_nowrap");
__reExport(entry_exports, require("c_nowrap"), module.exports);
var d = __toESM(require("d_WRAP"));
var import_e_WRAP = __toESM(require("e_WRAP"));
var import_f_WRAP = __toESM(require("f_WRAP"));
var import_g_WRAP = __toESM(require("g_WRAP"));
var h = __toESM(require("h_WRAP"));
var i = __toESM(require("i_WRAP"));
var j = __toESM(require("j_WRAP"));
(0, import_b_nowrap.b)();
x = d.x;
(0, import_e_WRAP.default)();
(0, import_f_WRAP.default)();
(0, import_g_WRAP.__esModule)();
x = h;
i.x();
j.x``;
x = Promise.resolve().then(() => __toESM(require("k_WRAP")));
