var __freeze = Object.freeze;
var __defProp = Object.defineProperty;
var __template = (cooked, raw) => __freeze(__defProp(cooked, "raw", { value: __freeze(raw || cooked.slice()) }));
var _a, _b, _c, _d, _e, _f, _g, _h;
x = () => [
  tag(_a || (_a = __template(["x"]))),
  tag(_b || (_b = __template(["\xFF"], ["\\xFF"]))),
  tag(_c || (_c = __template([void 0], ["\\x"]))),
  tag(_d || (_d = __template([void 0], ["\\u"])))
];
y = () => [
  tag(_e || (_e = __template(["x", "z"])), y),
  tag(_f || (_f = __template(["\xFF", "z"], ["\\xFF", "z"])), y),
  tag(_g || (_g = __template(["x", "z"], ["x", "\\z"])), y),
  tag(_h || (_h = __template(["x", void 0], ["x", "\\u"])), y)
];
