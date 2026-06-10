(() => {
  var __defProp = Object.defineProperty;
  var __name = (target, value) => __defProp(target, "name", { value, configurable: true });

  // entry.ts
  var ns;
  ((ns2) => {
    ns2.foo = /* @__PURE__ */ __name(() => {
    }, "foo");
    function bar() {
    }
    ns2.bar = bar;
    __name(bar, "bar");
    const _Baz = class _Baz {
    };
    __name(_Baz, "Baz");
    let Baz = _Baz;
    ns2.Baz = _Baz;
  })(ns || (ns = {}));
})();
