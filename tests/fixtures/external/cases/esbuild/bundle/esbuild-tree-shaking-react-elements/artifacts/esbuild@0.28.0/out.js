(() => {
  // input/entry.jsx
  function Foo() {
  }
  var d = /* @__PURE__ */ React.createElement("div", null);
  var e = /* @__PURE__ */ React.createElement(Foo, null, d);
  var f = /* @__PURE__ */ React.createElement(React.Fragment, null, e);
  console.log(f);
})();
