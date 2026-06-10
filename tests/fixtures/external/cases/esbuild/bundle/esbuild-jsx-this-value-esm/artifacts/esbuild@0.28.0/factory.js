(() => {
  // factory.jsx
  console.log([
    /* @__PURE__ */ (void 0)("x", null),
    /* @__PURE__ */ (void 0)("x", null)
  ]);
  f = function() {
    console.log([
      /* @__PURE__ */ this("y", null),
      /* @__PURE__ */ this("y", null)
    ]);
  };
})();
