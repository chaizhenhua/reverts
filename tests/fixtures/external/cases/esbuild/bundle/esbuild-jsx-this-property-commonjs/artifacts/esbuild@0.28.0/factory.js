(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/factory.jsx
  var require_factory = __commonJS({
    "input/factory.jsx"(exports) {
      console.log([
        /* @__PURE__ */ exports.factory("x", null),
        /* @__PURE__ */ exports.factory("x", null)
      ]);
      f = function() {
        console.log([
          /* @__PURE__ */ this.factory("y", null),
          /* @__PURE__ */ this.factory("y", null)
        ]);
      };
    }
  });
  require_factory();
})();
