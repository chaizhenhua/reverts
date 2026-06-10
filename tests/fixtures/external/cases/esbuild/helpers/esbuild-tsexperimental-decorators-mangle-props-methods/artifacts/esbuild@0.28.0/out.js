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
    prop1() {
    }
    a() {
    }
    ["prop3"]() {
    }
    ["prop4_"]() {
    }
    [/* @__KEY__ */ "prop5"]() {
    }
    [/* @__KEY__ */ "b"]() {
    }
  };
  __decorateClass([
    dec(1)
  ], Foo.prototype, "prop1", 1);
  __decorateClass([
    dec(2)
  ], Foo.prototype, /* @__KEY__ */ "a", 1);
  __decorateClass([
    dec(3)
  ], Foo.prototype, "prop3", 1);
  __decorateClass([
    dec(4)
  ], Foo.prototype, "prop4_", 1);
  __decorateClass([
    dec(5)
  ], Foo.prototype, /* @__KEY__ */ "prop5", 1);
  __decorateClass([
    dec(6)
  ], Foo.prototype, /* @__KEY__ */ "b", 1);
})();
