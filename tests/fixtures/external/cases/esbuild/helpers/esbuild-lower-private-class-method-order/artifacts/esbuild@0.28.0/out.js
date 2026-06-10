var __defProp = Object.defineProperty;
var __typeError = (msg) => {
  throw TypeError(msg);
};
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);
var _Foo_instances, foo_fn;
class Foo {
  constructor() {
    __privateAdd(this, _Foo_instances);
    __publicField(this, "bar", __privateMethod(this, _Foo_instances, foo_fn).call(this));
  }
  // This must be set before "bar" is initialized
}
_Foo_instances = new WeakSet();
foo_fn = function() {
  return 123;
};
console.log(new Foo().bar === 123);
