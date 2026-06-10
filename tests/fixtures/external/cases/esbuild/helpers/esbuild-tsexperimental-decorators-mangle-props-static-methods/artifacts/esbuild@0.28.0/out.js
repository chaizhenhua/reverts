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
    static prop1() {
    }
    static a() {
    }
    static ["prop3"]() {
    }
    static ["prop4_"]() {
    }
    static [/* @__KEY__ */ "prop5"]() {
    }
    static [/* @__KEY__ */ "b"]() {
    }
  };
  __decorateClass([
    dec(1)
  ], Foo, "prop1", 1);
  __decorateClass([
    dec(2)
  ], Foo, /* @__KEY__ */ "a", 1);
  __decorateClass([
    dec(3)
  ], Foo, "prop3", 1);
  __decorateClass([
    dec(4)
  ], Foo, "prop4_", 1);
  __decorateClass([
    dec(5)
  ], Foo, /* @__KEY__ */ "prop5", 1);
  __decorateClass([
    dec(6)
  ], Foo, /* @__KEY__ */ "b", 1);
})();
