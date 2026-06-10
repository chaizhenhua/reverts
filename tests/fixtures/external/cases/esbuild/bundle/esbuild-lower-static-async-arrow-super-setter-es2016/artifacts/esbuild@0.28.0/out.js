(() => {
  var __defProp = Object.defineProperty;
  var __getProtoOf = Object.getPrototypeOf;
  var __reflectSet = Reflect.set;
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
  var __superSet = (cls, obj, key, val) => (__reflectSet(__getProtoOf(cls), key, val, obj), val);
  var __async = (__this, __arguments, generator) => {
    return new Promise((resolve, reject) => {
      var fulfilled = (value) => {
        try {
          step(generator.next(value));
        } catch (e) {
          reject(e);
        }
      };
      var rejected = (value) => {
        try {
          step(generator.throw(value));
        } catch (e) {
          reject(e);
        }
      };
      var step = (x2) => x2.done ? resolve(x2.value) : Promise.resolve(x2.value).then(fulfilled, rejected);
      step((generator = generator.apply(__this, __arguments)).next());
    });
  };

  // input/foo1.js
  var foo1_default = class _foo1_default extends x {
    static foo1() {
      return () => __async(null, null, function* () {
        return __superSet(_foo1_default, this, "foo", "foo1");
      });
    }
  };

  // input/foo2.js
  var foo2_default = class _foo2_default extends x {
    static foo2() {
      return () => __async(null, null, function* () {
        return () => __superSet(_foo2_default, this, "foo", "foo2");
      });
    }
  };

  // input/foo3.js
  var foo3_default = class _foo3_default extends x {
    static foo3() {
      return () => () => __async(null, null, function* () {
        return __superSet(_foo3_default, this, "foo", "foo3");
      });
    }
  };

  // input/foo4.js
  var foo4_default = class _foo4_default extends x {
    static foo4() {
      return () => __async(null, null, function* () {
        return () => __async(null, null, function* () {
          return __superSet(_foo4_default, this, "foo", "foo4");
        });
      });
    }
  };

  // input/bar1.js
  var _bar1_default = class _bar1_default extends x {
  };
  __publicField(_bar1_default, "bar1", () => __async(null, null, function* () {
    return __superSet(_bar1_default, _bar1_default, "foo", "bar1");
  }));
  var bar1_default = _bar1_default;

  // input/bar2.js
  var _bar2_default = class _bar2_default extends x {
  };
  __publicField(_bar2_default, "bar2", () => __async(null, null, function* () {
    return () => __superSet(_bar2_default, _bar2_default, "foo", "bar2");
  }));
  var bar2_default = _bar2_default;

  // input/bar3.js
  var _bar3_default = class _bar3_default extends x {
  };
  __publicField(_bar3_default, "bar3", () => () => __async(null, null, function* () {
    return __superSet(_bar3_default, _bar3_default, "foo", "bar3");
  }));
  var bar3_default = _bar3_default;

  // input/bar4.js
  var _bar4_default = class _bar4_default extends x {
  };
  __publicField(_bar4_default, "bar4", () => __async(null, null, function* () {
    return () => __async(null, null, function* () {
      return __superSet(_bar4_default, _bar4_default, "foo", "bar4");
    });
  }));
  var bar4_default = _bar4_default;

  // input/baz1.js
  var baz1_default = class _baz1_default extends x {
    static baz1() {
      return __async(this, null, function* () {
        return () => __superSet(_baz1_default, this, "foo", "baz1");
      });
    }
  };

  // input/baz2.js
  var baz2_default = class _baz2_default extends x {
    static baz2() {
      return __async(this, null, function* () {
        return () => () => __superSet(_baz2_default, this, "foo", "baz2");
      });
    }
  };

  // input/outer.js
  var outer_default = (function() {
    return __async(this, null, function* () {
      const _y = class _y extends z {
      };
      __publicField(_y, "foo", () => __async(null, null, function* () {
        return __superSet(_y, _y, "foo", "foo");
      }));
      let y = _y;
      yield y.foo()();
    });
  })();
})();
