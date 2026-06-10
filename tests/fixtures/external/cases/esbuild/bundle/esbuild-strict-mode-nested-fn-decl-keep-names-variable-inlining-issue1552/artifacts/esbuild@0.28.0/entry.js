var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: !0 });
export function outer() {
  {
    let inner = function() {
      return Math.random();
    };
    __name(inner, "inner");
    const x = inner();
    console.log(x);
  }
}
__name(outer, "outer"), outer();
