(() => {
  // input/entry.ts
  var A_keep = class {
    static {
      foo;
    }
  }, B_keep = class {
    static {
      this.foo;
    }
  }, C_keep = class {
    static {
      try {
        foo;
      } catch {
      }
    }
  }, D_keep = class {
    static {
      foo;
    }
  };
})();
