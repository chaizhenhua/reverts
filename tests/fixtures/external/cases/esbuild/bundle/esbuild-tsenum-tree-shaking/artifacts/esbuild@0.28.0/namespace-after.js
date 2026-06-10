// input/namespace-after.ts
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["y"] = 123] = "y";
  return x2;
})(x || {});
((x2) => {
  console.log(x2, y);
})(x || (x = {}));
