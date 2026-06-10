export var x = /* @__PURE__ */ ((x2) => {
  x2[x2["y"] = 0] = "y";
  x2[x2["yy"] = 0 /* y */] = "yy";
  return x2;
})(x || {});
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["z"] = 1] = "z";
  return x2;
})(x || {});
((x2) => {
  console.log(y, z);
})(x || (x = {}));
console.log(0 /* y */, 1 /* z */);
