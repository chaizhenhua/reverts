(() => {
  // entry.ts
  var Foo = class {
    constructor(b1 = 2.1, b2 = 2.2) {
      this.b1 = b1;
      this.b2 = b2;
      this.a = 1;
      this.c = 3;
    }
    static {
      console.log("a");
    }
    static {
      console.log("b");
    }
    static {
      console.log("c");
    }
  };
})();
