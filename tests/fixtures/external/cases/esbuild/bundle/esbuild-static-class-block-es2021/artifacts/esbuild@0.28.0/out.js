(() => {
  var __getProtoOf = Object.getPrototypeOf;
  var __reflectGet = Reflect.get;
  var __reflectSet = Reflect.set;
  var __superGet = (cls, obj, key) => __reflectGet(__getProtoOf(cls), key, obj);
  var __superSet = (cls, obj, key, val) => (__reflectSet(__getProtoOf(cls), key, val, obj), val);
  var __superWrapper = (cls, obj, key) => ({
    get _() {
      return __superGet(cls, obj, key);
    },
    set _(val) {
      __superSet(cls, obj, key, val);
    }
  });

  // input/entry.js
  var _A = class _A {
  };
  _A.thisField++;
  _A.classField++;
  __superSet(_A, _A, "superField", __superGet(_A, _A, "superField") + 1);
  __superWrapper(_A, _A, "superField")._++;
  var A = _A;
  var _a;
  var B = (_a = class {
  }, _a.thisField++, __superSet(_a, _a, "superField", __superGet(_a, _a, "superField") + 1), __superWrapper(_a, _a, "superField")._++, _a);
})();
