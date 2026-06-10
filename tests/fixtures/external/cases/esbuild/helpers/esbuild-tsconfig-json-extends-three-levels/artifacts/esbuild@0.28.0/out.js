(() => {
  // input/Users/user/project/src/path2/works/import.js
  console.log("works");

  // input/Users/user/project/src/entry.jsx
  console.log(/* @__PURE__ */ baseFactory("div", null), /* @__PURE__ */ baseFactory(derivedFragment, null));
})();
