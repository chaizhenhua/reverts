var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var _a, _b, _a2, __a;
class Foo {
  #one = 1;
  get one() {
    return this.#one;
  }
  set one(_) {
    this.#one = _;
  }
  #_two = 2;
  get #two() {
    return this.#_two;
  }
  set #two(_) {
    this.#_two = _;
  }
  #a = 3;
  get [_b = three()]() {
    return this.#a;
  }
  set [_b](_) {
    this.#a = _;
  }
  static #four = 4;
  static get four() {
    return this.#four;
  }
  static set four(_) {
    this.#four = _;
  }
  static #_five = 5;
  static get #five() {
    return this.#_five;
  }
  static set #five(_) {
    this.#_five = _;
  }
  static #b = 6;
  static get [_a = six()]() {
    return this.#b;
  }
  static set [_a](_) {
    this.#b = _;
  }
}
class Normal {
  constructor() {
    __privateAdd(this, _a2, b);
    this.c = d;
  }
  get a() {
    return __privateGet(this, _a2);
  }
  set a(_) {
    __privateSet(this, _a2, _);
  }
}
_a2 = new WeakMap();
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
  static #a = b;
  static get a() {
    return this.#a;
  }
  static set a(_) {
    this.#a = _;
  }
  static {
    this.c = d;
  }
}
class StaticPrivate {
  static #_a = b;
  static get #a() {
    return this.#_a;
  }
  static set #a(_) {
    this.#_a = _;
  }
  static {
    this.c = d;
  }
}
