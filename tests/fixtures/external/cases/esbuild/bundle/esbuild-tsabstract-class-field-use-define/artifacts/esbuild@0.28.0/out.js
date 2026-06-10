const keepThisToo = /* @__PURE__ */ Symbol("keepThisToo");
class Foo {
  keepThis;
  [keepThisToo];
}
(() => new Foo())();
