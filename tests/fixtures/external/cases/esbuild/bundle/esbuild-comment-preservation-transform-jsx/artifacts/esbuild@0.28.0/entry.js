(() => {
  // input/entry.jsx
  console.log(
    /* @__PURE__ */ React.createElement("div", { x: (
      /*before*/
      x
    ) }),
    /* @__PURE__ */ React.createElement("div", { x: (
      /*before*/
      "y"
    ) }),
    /* @__PURE__ */ React.createElement("div", { x: (
      /*before*/
      true
    ) }),
    /* @__PURE__ */ React.createElement("div", {
      /*before*/
      ...x
    }),
    /* @__PURE__ */ React.createElement(
      "div",
      null,
      /*before*/
      x
    ),
    /* @__PURE__ */ React.createElement(
      React.Fragment,
      null,
      /*before*/
      x
    ),
    // Comments on absent AST nodes
    /* @__PURE__ */ React.createElement("div", null, "before", "after"),
    /* @__PURE__ */ React.createElement("div", null, "before", "after"),
    /* @__PURE__ */ React.createElement("div", null, "before", "after"),
    /* @__PURE__ */ React.createElement(React.Fragment, null, "before", "after"),
    /* @__PURE__ */ React.createElement(React.Fragment, null, "before", "after"),
    /* @__PURE__ */ React.createElement(React.Fragment, null, "before", "after")
  );
})();
