(() => {
  // inject.js
  function el() {
  }
  function frag() {
  }

  // entry.jsx
  console.log(/* @__PURE__ */ el(frag, null, /* @__PURE__ */ el("div", null)));
})();
