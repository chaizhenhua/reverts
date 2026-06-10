(() => {
  // entry.js
  var _a, _b, _c;
  console.log([
    a.b.c,
    a == null ? void 0 : a.b.c,
    (_a = a.b) == null ? void 0 : _a.c
  ], [
    a["b"]["c"],
    a == null ? void 0 : a["b"]["c"],
    (_b = a["b"]) == null ? void 0 : _b["c"]
  ], [
    a[b][c],
    a == null ? void 0 : a[b][c],
    (_c = a[b]) == null ? void 0 : _c[c]
  ]);
})();
