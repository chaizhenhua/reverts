(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/project/node_modules/pkg/button.css
  var require_button = __commonJS({
    "input/project/node_modules/pkg/button.css"(exports, module) {
      module.exports = {};
    }
  });

  // input/project/node_modules/pkg/components.jsx
  require_button();
  var Button = () => /* @__PURE__ */ React.createElement("button", null);

  // input/project/test.jsx
  render(/* @__PURE__ */ React.createElement(Button, null));
})();
