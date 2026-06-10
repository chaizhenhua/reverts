export var x = /* @__PURE__ */ ((x2) => {
  x2["y"] = "a";
  x2["yy"] = "a" /* y */;
  return x2;
})(x || {});
var x = /* @__PURE__ */ ((x2) => {
  x2["z"] = "a" /* y */;
  return x2;
})(x || {});
((x2) => {
  console.log(y, z);
})(x || (x = {}));
console.log("a" /* y */, "a" /* z */);
