(() => {
  // fragment.jsx
  console.log([
    /* @__PURE__ */ (void 0).factory((void 0).fragment, null, "x"),
    /* @__PURE__ */ (void 0).factory((void 0).fragment, null, "x")
  ]), f = function() {
    console.log([
      /* @__PURE__ */ this.factory(this.fragment, null, "y"),
      /* @__PURE__ */ this.factory(this.fragment, null, "y")
    ]);
  };
})();
