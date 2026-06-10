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
class Derived extends Base {
  test(key) {
    return __async(this, null, function* () {
      var _a, _b, _c, _d;
      return [
        yield __superGet(Derived.prototype, this, "foo"),
        yield __superGet(Derived.prototype, this, key),
        yield [__superWrapper(Derived.prototype, this, "foo")._] = [0],
        yield [__superWrapper(Derived.prototype, this, key)._] = [0],
        yield __superSet(Derived.prototype, this, "foo", 1),
        yield __superSet(Derived.prototype, this, key, 1),
        yield __superSet(Derived.prototype, this, "foo", __superGet(Derived.prototype, this, "foo") + 2),
        yield __superSet(Derived.prototype, this, key, __superGet(Derived.prototype, this, key) + 2),
        yield ++__superWrapper(Derived.prototype, this, "foo")._,
        yield ++__superWrapper(Derived.prototype, this, key)._,
        yield __superWrapper(Derived.prototype, this, "foo")._++,
        yield __superWrapper(Derived.prototype, this, key)._++,
        yield __superGet(Derived.prototype, this, "foo").name,
        yield __superGet(Derived.prototype, this, key).name,
        yield (_a = __superGet(Derived.prototype, this, "foo")) == null ? void 0 : _a.name,
        yield (_b = __superGet(Derived.prototype, this, key)) == null ? void 0 : _b.name,
        yield __superGet(Derived.prototype, this, "foo").call(this, 1, 2),
        yield __superGet(Derived.prototype, this, key).call(this, 1, 2),
        yield (_c = __superGet(Derived.prototype, this, "foo")) == null ? void 0 : _c.call(this, 1, 2),
        yield (_d = __superGet(Derived.prototype, this, key)) == null ? void 0 : _d.call(this, 1, 2),
        yield (() => __superGet(Derived.prototype, this, "foo"))(),
        yield (() => __superGet(Derived.prototype, this, key))(),
        yield (() => __superGet(Derived.prototype, this, "foo").call(this))(),
        yield (() => __superGet(Derived.prototype, this, key).call(this))(),
        yield __superGet(Derived.prototype, this, "foo").bind(this)``,
        yield __superGet(Derived.prototype, this, key).bind(this)``
      ];
    });
  }
}
let fn = () => __async(null, null, function* () {
  return class extends Base {
    constructor() {
      super(...arguments);
      __publicField(this, "a", super.a);
      __publicField(this, "b", () => super.b);
    }
    c() {
      return super.c;
    }
    d() {
      return () => super.d;
    }
  };
});
class Derived2 extends Base {
  constructor() {
    super(...arguments);
    __publicField(this, "b", () => __async(null, null, function* () {
      var _a;
      return _a = __superGet(Derived2.prototype, this, "foo"), class {
        constructor() {
          __publicField(this, _a, 123);
        }
      };
    }));
  }
  a() {
    return __async(this, null, function* () {
      var _a;
      return _a = __superGet(Derived2.prototype, this, "foo"), class {
        constructor() {
          __publicField(this, _a, 123);
        }
      };
    });
  }
}
for (let i = 0; i < 3; i++) {
  let _a;
  objs.push(_a = {
    __proto__: {
      foo() {
        return i;
      }
    },
    bar() {
      return __async(this, null, function* () {
        return __superGet(_a, this, "foo").call(this);
      });
    }
  });
}
