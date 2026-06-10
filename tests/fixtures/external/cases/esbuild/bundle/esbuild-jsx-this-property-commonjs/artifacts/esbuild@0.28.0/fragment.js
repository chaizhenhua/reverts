(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/fragment.jsx
  var require_fragment = __commonJS({
    "input/fragment.jsx"(exports) {
      console.log([
        /* @__PURE__ */ exports.factory(exports.fragment, null, "x"),
        /* @__PURE__ */ exports.factory(exports.fragment, null, "x")
      ]), f = function() {
        console.log([
          /* @__PURE__ */ this.factory(this.fragment, null, "y"),
          /* @__PURE__ */ this.factory(this.fragment, null, "y")
        ]);
      };
    }
  });
  require_fragment();
})();
