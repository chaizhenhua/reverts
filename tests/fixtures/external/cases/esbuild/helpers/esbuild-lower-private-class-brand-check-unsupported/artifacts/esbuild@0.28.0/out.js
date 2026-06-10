var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateIn = (member, obj) => Object(obj) !== obj ? __typeError('Cannot use the "in" operator on this value') : member.has(obj);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var _foo;
class Foo {
  constructor() {
    __privateAdd(this, _foo);
    this.#bar = void 0;
  }
  #bar;
  baz() {
    return [
      __privateGet(this, _foo),
      this.#bar,
      __privateIn(_foo, this)
    ];
  }
}
_foo = new WeakMap();
