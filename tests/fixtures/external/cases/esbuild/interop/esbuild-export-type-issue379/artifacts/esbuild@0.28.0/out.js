(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // a.ts
  var a_exports = {};
  __export(a_exports, {
    foo: () => foo
  });
  var foo = 123;

  // b.ts
  var b_exports = {};
  __export(b_exports, {
    foo: () => foo2
  });
  var foo2 = 123;

  // c.ts
  var c_exports = {};
  __export(c_exports, {
    foo: () => foo3
  });
  var foo3 = 123;

  // d.ts
  var d_exports = {};
  __export(d_exports, {
    foo: () => foo4
  });
  var foo4 = 123;

  // entry.ts
  console.log(a_exports, b_exports, c_exports, d_exports);
})();
