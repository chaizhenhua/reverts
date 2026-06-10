var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);
var _field, _Foo_instances, method_fn, _a, _staticField, _Foo_static, staticMethod_fn;
export let Foo = (_a = class {
  constructor() {
    __privateAdd(this, _Foo_instances);
    __privateAdd(this, _field);
  }
  foo() {
    var _a2;
    __privateSet(this, _field, __privateMethod(this, _Foo_instances, method_fn).call(this));
    __privateSet(Foo, _staticField, __privateMethod(_a2 = Foo, _Foo_static, staticMethod_fn).call(_a2));
  }
}, _field = new WeakMap(), _Foo_instances = new WeakSet(), method_fn = function() {
}, _staticField = new WeakMap(), _Foo_static = new WeakSet(), staticMethod_fn = function() {
}, __privateAdd(_a, _Foo_static), __privateAdd(_a, _staticField), _a);
