var __pow = Math.pow;
var _a, _b, _c, _d, _e, _f, _g, _h, _i, _j;
let tests = {
  // Exponentiation operator
  0: __pow(a, __pow(b, c)),
  1: __pow(__pow(a, b), c),
  // Exponentiation assignment operator
  2: a = __pow(a, b),
  3: a.b = __pow(a.b, c),
  4: a[b] = __pow(a[b], c),
  5: (_a = a()).b = __pow(_a.b, c),
  6: (_b = a())[b] = __pow(_b[b], c),
  7: a[_c = b()] = __pow(a[_c], c),
  8: (_d = a())[_e = b()] = __pow(_d[_e], c),
  // These all should not need capturing (no object identity)
  9: a[0] = __pow(a[0], b),
  10: a[false] = __pow(a[false], b),
  11: a[null] = __pow(a[null], b),
  12: a[void 0] = __pow(a[void 0], b),
  13: a[/* @__PURE__ */ BigInt("123")] = __pow(a[/* @__PURE__ */ BigInt("123")], b),
  14: a[this] = __pow(a[this], b),
  // These should need capturing (have object identitiy)
  15: a[_f = /x/] = __pow(a[_f], b),
  16: a[_g = {}] = __pow(a[_g], b),
  17: a[_h = []] = __pow(a[_h], b),
  18: a[_i = () => {
  }] = __pow(a[_i], b),
  19: a[_j = function() {
  }] = __pow(a[_j], b)
};
