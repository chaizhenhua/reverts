var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var _a, __a;
class Foo {
  accessor one = 1;
  accessor #two = 2;
  accessor [three()] = 3;
  static accessor four = 4;
  static accessor #five = 5;
  static accessor [six()] = 6;
}
class Normal {
  constructor() {
    __privateAdd(this, _a, b);
    this.c = d;
  }
  get a() {
    return __privateGet(this, _a);
  }
  set a(_) {
    __privateSet(this, _a, _);
  }
}
_a = new WeakMap();
class Private {
  constructor() {
    __privateAdd(this, __a, b);
    this.c = d;
  }
  get #a() {
    return __privateGet(this, __a);
  }
  set #a(_) {
    __privateSet(this, __a, _);
  }
}
__a = new WeakMap();
class StaticNormal {
  static accessor a = b;
  static {
    this.c = d;
  }
}
class StaticPrivate {
  static accessor #a = b;
  static {
    this.c = d;
  }
}
