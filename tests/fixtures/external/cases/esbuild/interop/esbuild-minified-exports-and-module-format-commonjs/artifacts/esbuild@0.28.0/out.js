var f = Object.defineProperty;
var p = (a, t) => {
  for (var e in t)
    f(a, e, { get: t[e], enumerable: true });
};

// input/foo/test.js
var o = {};
p(o, {
  foo: () => l
});
var l = 123;

// input/bar/test.js
var r = {};
p(r, {
  bar: () => m
});
var m = 123;

// input/entry.js
console.log(exports, module.exports, o, r);
