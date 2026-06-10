(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __decorateClass = (decorators, target, key, kind) => {
    var result = kind > 1 ? void 0 : kind ? __getOwnPropDesc(target, key) : target;
    for (var i = decorators.length - 1, decorator; i >= 0; i--)
      if (decorator = decorators[i])
        result = (kind ? decorator(target, key, result) : decorator(result)) || result;
    if (kind && result) __defProp(target, key, result);
    return result;
  };

  // entry.ts
  var Foo = class {
    static {
      this.prop1 = null;
    }
    static {
      this.a = null;
    }
    static {
      this["prop3"] = null;
    }
    static {
      this["prop4_"] = null;
    }
    static {
      this[/* @__KEY__ */ "prop5"] = null;
    }
    static {
      this.b = null;
    }
  };
  __decorateClass([
    dec(1)
  ], Foo, "prop1", 2);
  __decorateClass([
    dec(2)
  ], Foo, /* @__KEY__ */ "a", 2);
  __decorateClass([
    dec(3)
  ], Foo, "prop3", 2);
  __decorateClass([
    dec(4)
  ], Foo, "prop4_", 2);
  __decorateClass([
    dec(5)
  ], Foo, /* @__KEY__ */ "prop5", 2);
  __decorateClass([
    dec(6)
  ], Foo, /* @__KEY__ */ "b", 2);
})();
