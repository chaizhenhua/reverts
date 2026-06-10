(() => {
  // factory.jsx
  console.log([
    /* @__PURE__ */ (void 0).factory("x", null),
    /* @__PURE__ */ (void 0).factory("x", null)
  ]);
  f = function() {
    console.log([
      /* @__PURE__ */ this.factory("y", null),
      /* @__PURE__ */ this.factory("y", null)
    ]);
  };
})();
