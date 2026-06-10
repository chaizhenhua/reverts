var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var _a, _b, _one, __two, _Foo_instances, two_get, two_set, _a2, _four, __five, _Foo_static, five_get, five_set, _b2;
class Foo {
  constructor() {
    __privateAdd(this, _Foo_instances);
    __privateAdd(this, _one, 1);
    __privateAdd(this, __two, 2);
    __privateAdd(this, _a2, 3);
  }
  get one() {
    return __privateGet(this, _one);
  }
  set one(_) {
    __privateSet(this, _one, _);
  }
  get [_b = three()]() {
    return __privateGet(this, _a2);
  }
  set [_b](_) {
    __privateSet(this, _a2, _);
  }
  static get four() {
    return __privateGet(this, _four);
  }
  static set four(_) {
    __privateSet(this, _four, _);
  }
  static get [_a = six()]() {
    return __privateGet(this, _b2);
  }
  static set [_a](_) {
    __privateSet(this, _b2, _);
  }
}
_one = new WeakMap();
__two = new WeakMap();
_Foo_instances = new WeakSet();
two_get = function() {
  return __privateGet(this, __two);
};
two_set = function(_) {
  __privateSet(this, __two, _);
};
_a2 = new WeakMap();
_four = new WeakMap();
__five = new WeakMap();
_Foo_static = new WeakSet();
five_get = function() {
  return __privateGet(this, __five);
};
five_set = function(_) {
  __privateSet(this, __five, _);
};
_b2 = new WeakMap();
__privateAdd(Foo, _Foo_static);
__privateAdd(Foo, _four, 4);
__privateAdd(Foo, __five, 5);
__privateAdd(Foo, _b2, 6);
