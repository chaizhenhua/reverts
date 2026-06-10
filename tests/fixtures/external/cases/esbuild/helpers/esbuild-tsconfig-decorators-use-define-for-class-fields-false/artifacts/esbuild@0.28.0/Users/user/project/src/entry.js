(() => {
  // input/Users/user/project/src/entry.ts
  var Class = class {
  };
  var ClassMethod = class {
    foo() {
    }
  };
  var ClassField = class {
    constructor() {
      this.foo = 123;
    }
  };
  var ClassAccessor = class {
    accessor foo = 123;
    accessor bar;
  };
  new Class();
  new ClassMethod();
  new ClassField();
  new ClassAccessor();
})();
