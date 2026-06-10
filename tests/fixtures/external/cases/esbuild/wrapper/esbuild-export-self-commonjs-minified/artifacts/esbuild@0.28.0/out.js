var s = (l, o) => () => (o || l((o = { exports: {} }).exports, o), o.exports);

// input/entry.js
var r = s((f, e) => {
  e.exports = { foo: 123 };
  console.log(r());
});
module.exports = r();
