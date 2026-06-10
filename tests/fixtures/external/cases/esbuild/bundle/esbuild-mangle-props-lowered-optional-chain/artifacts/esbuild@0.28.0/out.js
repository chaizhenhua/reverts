export default function(x) {
  var _a;
  x.a;
  (_a = x.a) == null ? void 0 : _a.call(x);
  x == null ? void 0 : x.a;
  x == null ? void 0 : x.a();
  x == null ? void 0 : x.a.b;
  x == null ? void 0 : x.a.b();
  x == null ? void 0 : x["foo_"].b;
  x == null ? void 0 : x.a["bar_"];
}
