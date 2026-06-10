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
    class Baz {
      static {
        __name(this, "Baz");
      }
    }
    ns2.Baz = Baz;
  })(ns || (ns = {}));
})();
