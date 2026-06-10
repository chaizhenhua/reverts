(() => {
  var __defProp = Object.defineProperty;
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);

  // input/entry.js
  var KeepMe1 = class {
  };
  __publicField(KeepMe1, "x", "x");
  __publicField(KeepMe1, "y", sideEffects());
  __publicField(KeepMe1, "z", "z");
  var KeepMe2 = class {
  };
  __publicField(KeepMe2, "x", "x");
  __publicField(KeepMe2, "y", "y");
  __publicField(KeepMe2, "z", "z");
  new KeepMe2();
})();
