(() => {
  var __defProp = Object.defineProperty;
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);

  // input/define-false/index.ts
  () => null, c, () => null, C;
  var Foo = class {
  };
  (() => new Foo())();

  // input/define-true/index.ts
  var _a;
  var Bar = class {
    constructor() {
      __publicField(this, "a");
      __publicField(this, _a);
    }
    static A;
    static [(_a = (() => null, c), () => null, C)];
  };
  (() => new Bar())();
})();
