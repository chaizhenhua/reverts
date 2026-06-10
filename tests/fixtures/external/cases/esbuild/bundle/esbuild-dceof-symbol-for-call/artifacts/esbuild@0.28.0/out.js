(() => {
  // input/entry.js
  var n0 = Symbol.for();
  var n1 = Symbol.for({});
  var n2 = Symbol.for(/./);
  var n3 = Symbol.for(() => 0);
  var n4 = Symbol.for(x);
  var n5 = new Symbol.for("abc");
  var n6 = Symbol.for(1, 2, 3);
  var n7 = /* @__PURE__ */ Symbol.for((() => Math.random() < 0.5)() ? "x" : "y");
})();
