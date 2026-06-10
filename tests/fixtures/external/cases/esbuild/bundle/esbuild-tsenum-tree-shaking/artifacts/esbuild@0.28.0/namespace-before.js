// input/namespace-before.ts
((x2) => {
  console.log(x2, y);
})(x || (x = {}));
var x = /* @__PURE__ */ ((x2) => {
  x2[x2["y"] = 123] = "y";
  return x2;
})(x || {});
