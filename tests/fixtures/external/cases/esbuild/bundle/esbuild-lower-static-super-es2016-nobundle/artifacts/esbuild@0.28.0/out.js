var __defProp = Object.defineProperty;
var __getProtoOf = Object.getPrototypeOf;
var __reflectGet = Reflect.get;
var __reflectSet = Reflect.set;
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
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
const _Derived = class _Derived extends Base {
};
__publicField(_Derived, "test", (key) => {
  var _a, _b, _c, _d;
  return [
    __superGet(_Derived, _Derived, "foo"),
    __superGet(_Derived, _Derived, key),
    [__superWrapper(_Derived, _Derived, "foo")._] = [0],
    [__superWrapper(_Derived, _Derived, key)._] = [0],
    __superSet(_Derived, _Derived, "foo", 1),
    __superSet(_Derived, _Derived, key, 1),
    __superSet(_Derived, _Derived, "foo", __superGet(_Derived, _Derived, "foo") + 2),
    __superSet(_Derived, _Derived, key, __superGet(_Derived, _Derived, key) + 2),
    ++__superWrapper(_Derived, _Derived, "foo")._,
    ++__superWrapper(_Derived, _Derived, key)._,
    __superWrapper(_Derived, _Derived, "foo")._++,
    __superWrapper(_Derived, _Derived, key)._++,
    __superGet(_Derived, _Derived, "foo").name,
    __superGet(_Derived, _Derived, key).name,
    (_a = __superGet(_Derived, _Derived, "foo")) == null ? void 0 : _a.name,
    (_b = __superGet(_Derived, _Derived, key)) == null ? void 0 : _b.name,
    __superGet(_Derived, _Derived, "foo").call(this, 1, 2),
    __superGet(_Derived, _Derived, key).call(this, 1, 2),
    (_c = __superGet(_Derived, _Derived, "foo")) == null ? void 0 : _c.call(this, 1, 2),
    (_d = __superGet(_Derived, _Derived, key)) == null ? void 0 : _d.call(this, 1, 2),
    (() => __superGet(_Derived, _Derived, "foo"))(),
    (() => __superGet(_Derived, _Derived, key))(),
    (() => __superGet(_Derived, _Derived, "foo").call(this))(),
    (() => __superGet(_Derived, _Derived, key).call(this))(),
    __superGet(_Derived, _Derived, "foo").bind(this)``,
    __superGet(_Derived, _Derived, key).bind(this)``
  ];
});
let Derived = _Derived;
