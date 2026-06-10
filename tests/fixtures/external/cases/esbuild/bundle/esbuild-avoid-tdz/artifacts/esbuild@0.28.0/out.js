(() => {
  // input/entry.js
  var Foo = class _Foo {
    static foo = new _Foo();
  };
  var foo = Foo.foo;
  console.log(foo);
  var Bar = class {
  };
  var bar = 123;
})();
