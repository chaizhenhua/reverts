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
let foo = 1;
class Foo {
  method1(foo2 = 2) {
  }
  method2(foo2 = 3) {
  }
}
__decorateClass([
  __decorateParam(0, dec(foo))
], Foo.prototype, "method1", 1);
__decorateClass([
  __decorateParam(0, dec(() => foo))
], Foo.prototype, "method2", 1);
class Bar {
  static {
    this.x = class {
      static {
        this.y = () => {
          let bar = 1;
          let Baz = class {
            method1() {
            }
            method2() {
            }
            method3(bar2) {
            }
            method4(bar2) {
            }
          };
          __decorateClass([
            dec(bar)
          ], Baz.prototype, "method1", 1);
          __decorateClass([
            dec(() => bar)
          ], Baz.prototype, "method2", 1);
          __decorateClass([
            __decorateParam(0, dec(() => bar))
          ], Baz.prototype, "method3", 1);
          __decorateClass([
            __decorateParam(0, dec(() => bar))
          ], Baz.prototype, "method4", 1);
          Baz = __decorateClass([
            dec(bar),
            dec(() => bar)
          ], Baz);
          return Baz;
        };
      }
    };
  }
}
