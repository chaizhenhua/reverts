export var a = /* @__PURE__ */ ((a2) => {
  a2[a2["b"] = 100] = "b";
  return a2;
})(a || {});
export var x = /* @__PURE__ */ ((x2) => {
  x2[x2["c"] = 100 /* b */] = "c";
  x2[x2["d"] = 200] = "d";
  x2[x2["e"] = 4e4] = "e";
  x2[x2["f"] = 1e4] = "f";
  return x2;
})(x || {});
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["g"] = 625] = "g";
  return x2;
})(x || {});
console.log(100 /* b */, 100 /* b */, 625 /* g */, 625 /* g */);
