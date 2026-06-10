(() => {
  var __defProp = Object.defineProperty;
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);

  // entry.ts
  var Foo = class {
    constructor(b1 = 2.1, b2 = 2.2) {
      __publicField(this, "b1", b1);
      __publicField(this, "b2", b2);
      __publicField(this, "a", 1);
      __publicField(this, "c", 3);
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
