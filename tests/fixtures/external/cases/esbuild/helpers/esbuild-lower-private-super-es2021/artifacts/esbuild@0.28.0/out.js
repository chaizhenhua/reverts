(() => {
  var __getProtoOf = Object.getPrototypeOf;
  var __reflectGet = Reflect.get;
  var __reflectSet = Reflect.set;
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
  var __superGet = (cls, obj, key) => __reflectGet(__getProtoOf(cls), key, obj);
  var __superSet = (cls, obj, key, val) => (__reflectSet(__getProtoOf(cls), key, val, obj), val);
  var __superWrapper = (cls, obj, key) => ({
    get _() {
      return __superGet(cls, obj, key);
    },
    set _(val) {
      __superSet(cls, obj, key, val);
    }
  });

  // input/foo1.js
  var _default_instances, foo_fn;
  var _foo1_default = class _foo1_default extends x {
    constructor() {
      super(...arguments);
      __privateAdd(this, _default_instances);
    }
  };
  _default_instances = new WeakSet();
  foo_fn = function() {
    __superGet(_foo1_default.prototype, this, "foo").call(this);
  };
  var foo1_default = _foo1_default;

  // input/foo2.js
  var _default_instances2, foo_fn2;
  var _foo2_default = class _foo2_default extends x {
    constructor() {
      super(...arguments);
      __privateAdd(this, _default_instances2);
    }
  };
  _default_instances2 = new WeakSet();
  foo_fn2 = function() {
    __superWrapper(_foo2_default.prototype, this, "foo")._++;
  };
  var foo2_default = _foo2_default;

  // input/foo3.js
  var _default_static, foo_fn3;
  var _foo3_default = class _foo3_default extends x {
  };
  _default_static = new WeakSet();
  foo_fn3 = function() {
    __superGet(_foo3_default, this, "foo").call(this);
  };
  __privateAdd(_foo3_default, _default_static);
  var foo3_default = _foo3_default;

  // input/foo4.js
  var _default_static2, foo_fn4;
  var _foo4_default = class _foo4_default extends x {
  };
  _default_static2 = new WeakSet();
  foo_fn4 = function() {
    __superWrapper(_foo4_default, this, "foo")._++;
  };
  __privateAdd(_foo4_default, _default_static2);
  var foo4_default = _foo4_default;

  // input/foo5.js
  var _foo;
  var foo5_default = class extends x {
    constructor() {
      super(...arguments);
      __privateAdd(this, _foo, () => {
        super.foo();
      });
    }
  };
  _foo = new WeakMap();

  // input/foo6.js
  var _foo2;
  var foo6_default = class extends x {
    constructor() {
      super(...arguments);
      __privateAdd(this, _foo2, () => {
        super.foo++;
      });
    }
  };
  _foo2 = new WeakMap();

  // input/foo7.js
  var _foo3;
  var _foo7_default = class _foo7_default extends x {
  };
  _foo3 = new WeakMap();
  __privateAdd(_foo7_default, _foo3, () => {
    __superGet(_foo7_default, _foo7_default, "foo").call(this);
  });
  var foo7_default = _foo7_default;

  // input/foo8.js
  var _foo4;
  var _foo8_default = class _foo8_default extends x {
  };
  _foo4 = new WeakMap();
  __privateAdd(_foo8_default, _foo4, () => {
    __superWrapper(_foo8_default, _foo8_default, "foo")._++;
  });
  var foo8_default = _foo8_default;
})();
