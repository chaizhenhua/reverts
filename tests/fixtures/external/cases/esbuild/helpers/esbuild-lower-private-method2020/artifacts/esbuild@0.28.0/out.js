(() => {
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
  var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
  var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);

  // input/entry.js
  var _field, _Foo_instances, method_fn;
  var Foo = class {
    constructor() {
      __privateAdd(this, _Foo_instances);
      __privateAdd(this, _field);
    }
    baseline() {
      a().foo;
      b().foo(x);
      c()?.foo(x);
      d().foo?.(x);
      e()?.foo?.(x);
    }
    privateField() {
      var _a, _b, _c, _d, _e, _f, _g;
      __privateGet(a(), _field);
      __privateGet(_a = b(), _field).call(_a, x);
      (_b = c()) == null ? void 0 : __privateGet(_b, _field).call(_b, x);
      (_d = __privateGet(_c = d(), _field)) == null ? void 0 : _d.call(_c, x);
      ((_e = e()) == null ? void 0 : __privateGet(_e, _field))?.(x);
      (_f = f()) == null ? void 0 : __privateGet(_g = _f.foo, _field).call(_g, x).bar();
    }
    privateMethod() {
      var _a, _b, _c, _d, _e, _f, _g;
      __privateMethod(a(), _Foo_instances, method_fn);
      __privateMethod(_a = b(), _Foo_instances, method_fn).call(_a, x);
      (_b = c()) == null ? void 0 : __privateMethod(_b, _Foo_instances, method_fn).call(_b, x);
      (_d = __privateMethod(_c = d(), _Foo_instances, method_fn)) == null ? void 0 : _d.call(_c, x);
      ((_e = e()) == null ? void 0 : __privateMethod(_e, _Foo_instances, method_fn))?.(x);
      (_f = f()) == null ? void 0 : __privateMethod(_g = _f.foo, _Foo_instances, method_fn).call(_g, x).bar();
    }
  };
  _field = new WeakMap();
  _Foo_instances = new WeakSet();
  method_fn = function() {
  };
})();
