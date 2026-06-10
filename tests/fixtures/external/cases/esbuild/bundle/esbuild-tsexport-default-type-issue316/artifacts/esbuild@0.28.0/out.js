(() => {
  // input/keep/declare-class.ts
  var declare_class_default = foo;
  var bar = 123;

  // input/keep/declare-let.ts
  var declare_let_default = foo;
  var bar2 = 123;

  // input/keep/interface-merged.ts
  var foo2 = class _foo {
    static {
      this.x = new _foo();
    }
  };
  var interface_merged_default = foo2;
  var bar3 = 123;

  // input/keep/interface-nested.ts
  if (true) {
  }
  var interface_nested_default = foo;
  var bar4 = 123;

  // input/keep/type-nested.ts
  if (true) {
  }
  var type_nested_default = foo;
  var bar5 = 123;

  // input/keep/value-namespace.ts
  var foo3;
  ((foo5) => {
    foo5.num = 0;
  })(foo3 || (foo3 = {}));
  var value_namespace_default = foo3;
  var bar6 = 123;

  // input/keep/value-namespace-merged.ts
  var foo4;
  ((foo5) => {
    foo5.num = 0;
  })(foo4 || (foo4 = {}));
  var value_namespace_merged_default = foo4;
  var bar7 = 123;

  // input/remove/interface.ts
  var bar8 = 123;

  // input/remove/interface-exported.ts
  var bar9 = 123;

  // input/remove/type.ts
  var bar10 = 123;

  // input/remove/type-exported.ts
  var bar11 = 123;

  // input/remove/type-only-namespace.ts
  var bar12 = 123;

  // input/remove/type-only-namespace-exported.ts
  var bar13 = 123;

  // input/entry.ts
  var entry_default = [
    declare_class_default,
    bar,
    declare_let_default,
    bar2,
    interface_merged_default,
    bar3,
    interface_nested_default,
    bar4,
    type_nested_default,
    bar5,
    value_namespace_default,
    bar6,
    value_namespace_merged_default,
    bar7,
    bar8,
    bar9,
    bar10,
    bar11,
    bar12,
    bar13
  ];
})();
