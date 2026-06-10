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
var __async = (__this, __arguments, generator) => {
  return new Promise((resolve, reject) => {
    var fulfilled = (value) => {
      try {
        step(generator.next(value));
      } catch (e) {
        reject(e);
      }
    };
    var rejected = (value) => {
      try {
        step(generator.throw(value));
      } catch (e) {
        reject(e);
      }
    };
    var step = (x) => x.done ? resolve(x.value) : Promise.resolve(x.value).then(fulfilled, rejected);
    step((generator = generator.apply(__this, __arguments)).next());
  });
};
const _Derived = class _Derived extends Base {
};
__publicField(_Derived, "test", (key) => __async(null, null, function* () {
  var _a, _b, _c, _d;
  return [
    yield __superGet(_Derived, _Derived, "foo"),
    yield __superGet(_Derived, _Derived, key),
    yield [__superWrapper(_Derived, _Derived, "foo")._] = [0],
    yield [__superWrapper(_Derived, _Derived, key)._] = [0],
    yield __superSet(_Derived, _Derived, "foo", 1),
    yield __superSet(_Derived, _Derived, key, 1),
    yield __superSet(_Derived, _Derived, "foo", __superGet(_Derived, _Derived, "foo") + 2),
    yield __superSet(_Derived, _Derived, key, __superGet(_Derived, _Derived, key) + 2),
    yield ++__superWrapper(_Derived, _Derived, "foo")._,
    yield ++__superWrapper(_Derived, _Derived, key)._,
    yield __superWrapper(_Derived, _Derived, "foo")._++,
    yield __superWrapper(_Derived, _Derived, key)._++,
    yield __superGet(_Derived, _Derived, "foo").name,
    yield __superGet(_Derived, _Derived, key).name,
    yield (_a = __superGet(_Derived, _Derived, "foo")) == null ? void 0 : _a.name,
    yield (_b = __superGet(_Derived, _Derived, key)) == null ? void 0 : _b.name,
    yield __superGet(_Derived, _Derived, "foo").call(this, 1, 2),
    yield __superGet(_Derived, _Derived, key).call(this, 1, 2),
    yield (_c = __superGet(_Derived, _Derived, "foo")) == null ? void 0 : _c.call(this, 1, 2),
    yield (_d = __superGet(_Derived, _Derived, key)) == null ? void 0 : _d.call(this, 1, 2),
    yield (() => __superGet(_Derived, _Derived, "foo"))(),
    yield (() => __superGet(_Derived, _Derived, key))(),
    yield (() => __superGet(_Derived, _Derived, "foo").call(this))(),
    yield (() => __superGet(_Derived, _Derived, key).call(this))(),
    yield __superGet(_Derived, _Derived, "foo").bind(this)``,
    yield __superGet(_Derived, _Derived, key).bind(this)``
  ];
}));
let Derived = _Derived;
let fn = () => __async(null, null, function* () {
  var _a;
  return _a = class extends Base {
    static c() {
      return super.c;
    }
    static d() {
      return () => super.d;
    }
  }, __publicField(_a, "a", __superGet(_a, _a, "a")), __publicField(_a, "b", () => __superGet(_a, _a, "b")), _a;
});
const _Derived2 = class _Derived2 extends Base {
  static a() {
    return __async(this, null, function* () {
      var _a;
      return _a = __superGet(_Derived2, this, "foo"), class {
        constructor() {
          __publicField(this, _a, 123);
        }
      };
    });
  }
};
__publicField(_Derived2, "b", () => __async(null, null, function* () {
  var _a;
  return _a = __superGet(_Derived2, _Derived2, "foo"), class {
    constructor() {
      __publicField(this, _a, 123);
    }
  };
}));
let Derived2 = _Derived2;
