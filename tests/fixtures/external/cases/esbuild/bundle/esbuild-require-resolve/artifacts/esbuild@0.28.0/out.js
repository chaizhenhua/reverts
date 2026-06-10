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
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));

// entry.js
console.log(require.resolve);
console.log(require.resolve());
console.log(require.resolve(foo));
console.log(require.resolve("a", "b"));
console.log(require.resolve("./present-file"));
console.log(require.resolve("./missing-file"));
console.log(require.resolve("./external-file"));
console.log(require.resolve("missing-pkg"));
console.log(require.resolve("external-pkg"));
console.log(require.resolve("@scope/missing-pkg"));
console.log(require.resolve("@scope/external-pkg"));
try {
  console.log(require.resolve("inside-try"));
} catch (e) {
}
if (false) {
  console.log(null);
}
console.log(false ? null : 0);
console.log(true ? 0 : null);
console.log(false);
console.log(true);
console.log(true);
