(() => {
  // input/b.ts
  var Foo = class {
  };
  ((Foo2) => {
    Foo2.foo = 1;
  })(Foo || (Foo = {}));
  ((Foo2) => {
    Foo2.bar = 2;
  })(Foo || (Foo = {}));

  // input/a.ts
  console.log(new Foo());
})();
