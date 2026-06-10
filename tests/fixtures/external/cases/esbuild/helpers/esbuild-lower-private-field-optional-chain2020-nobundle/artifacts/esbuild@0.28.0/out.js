var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var _x;
class Foo {
  constructor() {
    __privateAdd(this, _x);
  }
  foo() {
    this == null ? void 0 : __privateGet(this, _x).y;
    this == null ? void 0 : __privateGet(this.y, _x);
    __privateGet(this, _x)?.y;
  }
}
_x = new WeakMap();
