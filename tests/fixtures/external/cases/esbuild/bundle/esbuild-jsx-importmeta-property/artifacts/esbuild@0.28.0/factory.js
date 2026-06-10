(() => {
  // factory.jsx
  var import_meta = {};
  console.log([
    /* @__PURE__ */ import_meta.factory("x", null),
    /* @__PURE__ */ import_meta.factory("x", null)
  ]);
  f = function() {
    console.log([
      /* @__PURE__ */ import_meta.factory("y", null),
      /* @__PURE__ */ import_meta.factory("y", null)
    ]);
  };
})();
