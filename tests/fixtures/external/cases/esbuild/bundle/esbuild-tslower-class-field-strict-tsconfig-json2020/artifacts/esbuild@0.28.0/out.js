(() => {
  var __defProp = Object.defineProperty;
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);

  // input/loose/index.ts
  var loose_default = class {
  };

  // input/strict/index.ts
  var strict_default = class {
    constructor() {
      __publicField(this, "foo");
    }
  };

  // input/entry.js
  console.log(loose_default, strict_default);
})();
