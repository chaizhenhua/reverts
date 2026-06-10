// input/sibling-enum-middle.ts
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["y"] = 123] = "y";
  return x2;
})(x || {});
console.log(x);
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["z"] = 246] = "z";
  return x2;
})(x || {});
