x = class {
  a = 1;
  b = 2;
  _doNotMangleThis = 3;
}, x = {
  a: 1,
  b: 2,
  _doNotMangleThis: 3
}, x.a = 1, x.b = 2, x._doNotMangleThis = 3, x([
  `${foo}.a = bar.b`,
  `${foo}.notMangled = bar.notMangledEither`
]);
