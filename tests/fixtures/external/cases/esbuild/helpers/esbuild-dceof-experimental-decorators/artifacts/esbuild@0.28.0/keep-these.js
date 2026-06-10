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
  var __decorateParam = (index, decorator) => (target, key) => decorator(target, key, index);

  // input/decorator.ts
  var fn = () => {
    console.log("side effect");
  };

  // input/keep-these.ts
  var Class = class {
  };
  Class = __decorateClass([
    fn
  ], Class);
  var Field = class {
    field;
  };
  __decorateClass([
    fn
  ], Field.prototype, "field", 2);
  var Method = class {
    method() {
    }
  };
  __decorateClass([
    fn
  ], Method.prototype, "method", 1);
  var Accessor = class {
    accessor accessor;
  };
  __decorateClass([
    fn
  ], Accessor.prototype, "accessor", 1);
  var Parameter = class {
    foo(bar) {
    }
  };
  __decorateClass([
    __decorateParam(0, fn)
  ], Parameter.prototype, "foo", 1);
  var StaticField = class {
    static field;
  };
  __decorateClass([
    fn
  ], StaticField, "field", 2);
  var StaticMethod = class {
    static method() {
    }
  };
  __decorateClass([
    fn
  ], StaticMethod, "method", 1);
  var StaticAccessor = class {
    static accessor accessor;
  };
  __decorateClass([
    fn
  ], StaticAccessor, "accessor", 1);
  var StaticParameter = class {
    static foo(bar) {
    }
  };
  __decorateClass([
    __decorateParam(0, fn)
  ], StaticParameter, "foo", 1);
})();
