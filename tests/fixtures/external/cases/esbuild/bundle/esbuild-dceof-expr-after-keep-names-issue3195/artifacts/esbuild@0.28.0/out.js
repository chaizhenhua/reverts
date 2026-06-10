var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: !0 });
(() => {
  function f() {
  }
  __name(f, "f"), firstImportantSideEffect(void 0);
})(), (() => {
  function g() {
  }
  __name(g, "g");
  debugger;
  secondImportantSideEffect(void 0);
})();
