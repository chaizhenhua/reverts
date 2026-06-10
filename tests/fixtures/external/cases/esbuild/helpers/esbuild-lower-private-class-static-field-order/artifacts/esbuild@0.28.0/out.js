var __defProp = Object.defineProperty;
var __typeError = (msg) => {
  throw TypeError(msg);
};
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var _foo, _foo2;
const _Foo = class _Foo {
};
_foo = new WeakMap();
__privateAdd(_Foo, _foo, 123);
// This must be set before "bar" is initialized
__publicField(_Foo, "bar", __privateGet(_Foo, _foo));
let Foo = _Foo;
console.log(Foo.bar === 123);
const _FooThis = class _FooThis {
};
_foo2 = new WeakMap();
__privateAdd(_FooThis, _foo2, 123);
// This must be set before "bar" is initialized
__publicField(_FooThis, "bar", __privateGet(_FooThis, _foo2));
let FooThis = _FooThis;
console.log(FooThis.bar === 123);
