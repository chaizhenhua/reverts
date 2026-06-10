var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var __privateWrapper = (obj, member, setter, getter) => ({
  set _(value) {
    __privateSet(obj, member, value, setter);
  },
  get _() {
    return __privateGet(obj, member, getter);
  }
});
var _x;
class Foo {
  constructor() {
    __privateAdd(this, _x);
  }
  unary() {
    __privateWrapper(this, _x)._++;
    __privateWrapper(this, _x)._--;
    ++__privateWrapper(this, _x)._;
    --__privateWrapper(this, _x)._;
  }
  binary() {
    var _a;
    __privateSet(this, _x, 1);
    __privateSet(this, _x, __privateGet(this, _x) + 1);
    __privateSet(this, _x, __privateGet(this, _x) - 1);
    __privateSet(this, _x, __privateGet(this, _x) * 1);
    __privateSet(this, _x, __privateGet(this, _x) / 1);
    __privateSet(this, _x, __privateGet(this, _x) % 1);
    __privateSet(this, _x, __privateGet(this, _x) ** 1);
    __privateSet(this, _x, __privateGet(this, _x) << 1);
    __privateSet(this, _x, __privateGet(this, _x) >> 1);
    __privateSet(this, _x, __privateGet(this, _x) >>> 1);
    __privateSet(this, _x, __privateGet(this, _x) & 1);
    __privateSet(this, _x, __privateGet(this, _x) | 1);
    __privateSet(this, _x, __privateGet(this, _x) ^ 1);
    __privateGet(this, _x) && __privateSet(this, _x, 1);
    __privateGet(this, _x) || __privateSet(this, _x, 1);
    (_a = __privateGet(this, _x)) != null ? _a : __privateSet(this, _x, 1);
  }
}
_x = new WeakMap();
