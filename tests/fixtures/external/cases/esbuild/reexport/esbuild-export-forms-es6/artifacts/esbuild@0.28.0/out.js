var __defProp = Object.defineProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// input/a.js
var abc = void 0;

// input/b.js
var b_exports = {};
__export(b_exports, {
  xyz: () => xyz
});
var xyz = null;

// input/entry.js
var entry_default = 123;
var v = 234;
var l = 234;
var c = 234;
function Fn() {
}
var Class = class {
};
export {
  Class as C,
  Class,
  Fn,
  abc,
  b_exports as b,
  c,
  entry_default as default,
  l,
  v
};
