var globalName = (() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
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

  // entry.js
  var entry_exports = {};
  __export(entry_exports, {
    C: () => Class,
    Class: () => Class,
    Fn: () => Fn,
    abc: () => abc,
    b: () => b_exports,
    c: () => c,
    default: () => entry_default,
    l: () => l,
    v: () => v
  });

  // a.js
  var abc = void 0;

  // b.js
  var b_exports = {};
  __export(b_exports, {
    xyz: () => xyz
  });
  var xyz = null;

  // entry.js
  var entry_default = 123;
  var v = 234;
  var l = 234;
  var c = 234;
  function Fn() {
  }
  var Class = class {
  };
  return __toCommonJS(entry_exports);
})();
