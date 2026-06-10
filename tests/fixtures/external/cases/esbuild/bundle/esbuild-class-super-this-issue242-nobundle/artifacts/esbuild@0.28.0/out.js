var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var _e;
export class A {
}
export class B extends A {
  constructor(c) {
    var _a;
    super();
    __privateAdd(this, _e);
    __privateSet(this, _e, (_a = c.d) != null ? _a : "test");
  }
  f() {
    return __privateGet(this, _e);
  }
}
_e = new WeakMap();
