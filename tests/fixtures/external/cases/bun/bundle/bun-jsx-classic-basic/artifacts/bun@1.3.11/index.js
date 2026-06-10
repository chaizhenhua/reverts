// @bun
var __defProp = Object.defineProperty;
var __returnValue = (v) => v;
function __exportSetter(name, newValue) {
  this[name] = __returnValue.bind(null, newValue);
}
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, {
      get: all[name],
      enumerable: true,
      configurable: true,
      set: __exportSetter.bind(all, name)
    });
};

// tests/fixtures/external/cases/bun/bundle/bun-jsx-classic-basic/input/node_modules/bun-test-helpers/index.js
function print(arg) {
  console.log(JSON.stringify(arg));
}

// tests/fixtures/external/cases/bun/bundle/bun-jsx-classic-basic/input/node_modules/custom-classic/index.js
var exports_custom_classic = {};
__export(exports_custom_classic, {
  something: () => something,
  createElement: () => createElement,
  Fragment: () => Fragment
});
function createElement(type, props, ...children) {
  return ["custom-classic", type, props, children];
}
var Fragment = "CustomFragment";
var something = "something";

// tests/fixtures/external/cases/bun/bundle/bun-jsx-classic-basic/input/index.jsx
print([/* @__PURE__ */ createElement("div", {
  props: 123
}, "Hello World"), /* @__PURE__ */ createElement(Fragment, null, "Fragment")]);
