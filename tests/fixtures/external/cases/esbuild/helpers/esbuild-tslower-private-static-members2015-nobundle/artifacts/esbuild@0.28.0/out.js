var __typeError = (msg) => {
  throw TypeError(msg);
};
var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);
var _x, _Foo_static, y_get, y_set, z_fn;
const _Foo = class _Foo {
  foo() {
    var _a;
    __privateSet(_Foo, _x, __privateGet(_Foo, _x) + 1);
    __privateSet(_Foo, _Foo_static, __privateGet(_Foo, _Foo_static, y_get) + 1, y_set);
    __privateMethod(_a = _Foo, _Foo_static, z_fn).call(_a);
  }
};
_x = new WeakMap();
_Foo_static = new WeakSet();
y_get = function() {
};
y_set = function(x) {
};
z_fn = function() {
};
__privateAdd(_Foo, _Foo_static);
__privateAdd(_Foo, _x);
let Foo = _Foo;
