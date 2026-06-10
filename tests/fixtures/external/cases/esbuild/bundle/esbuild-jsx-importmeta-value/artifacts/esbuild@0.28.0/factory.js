(() => {
  // factory.jsx
  var import_meta = {};
  console.log([
    /* @__PURE__ */ import_meta("x", null),
    /* @__PURE__ */ import_meta("x", null)
  ]);
  f = function() {
    console.log([
      /* @__PURE__ */ import_meta("y", null),
      /* @__PURE__ */ import_meta("y", null)
    ]);
  };
})();
