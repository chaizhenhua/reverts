(() => {
  // input/custom-react.js
  function elem() {
  }
  function frag() {
  }

  // input/entry.jsx
  console.log(/* @__PURE__ */ elem("div", null), /* @__PURE__ */ elem(frag, null, "fragment"));
})();
