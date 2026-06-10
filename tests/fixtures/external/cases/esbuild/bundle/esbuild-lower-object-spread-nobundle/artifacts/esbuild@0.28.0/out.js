var __defProp = Object.defineProperty;
var __defProps = Object.defineProperties;
var __getOwnPropDescs = Object.getOwnPropertyDescriptors;
var __getOwnPropSymbols = Object.getOwnPropertySymbols;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __propIsEnum = Object.prototype.propertyIsEnumerable;
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __spreadValues = (a2, b2) => {
  for (var prop in b2 || (b2 = {}))
    if (__hasOwnProp.call(b2, prop))
      __defNormalProp(a2, prop, b2[prop]);
  if (__getOwnPropSymbols)
    for (var prop of __getOwnPropSymbols(b2)) {
      if (__propIsEnum.call(b2, prop))
        __defNormalProp(a2, prop, b2[prop]);
    }
  return a2;
};
var __spreadProps = (a2, b2) => __defProps(a2, __getOwnPropDescs(b2));
let tests = [
  __spreadValues(__spreadValues({}, a), b),
  __spreadValues({ a, b }, c),
  __spreadProps(__spreadValues({}, a), { b, c }),
  __spreadProps(__spreadValues({ a }, b), { c }),
  __spreadProps(__spreadValues(__spreadValues(__spreadProps(__spreadValues(__spreadValues({ a, b }, c), d), { e, f }), g), h), { i, j })
];
let jsx = [
  /* @__PURE__ */ React.createElement("div", __spreadValues(__spreadValues({}, a), b)),
  /* @__PURE__ */ React.createElement("div", __spreadValues({ a: true, b: true }, c)),
  /* @__PURE__ */ React.createElement("div", __spreadProps(__spreadValues({}, a), { b: true, c: true })),
  /* @__PURE__ */ React.createElement("div", __spreadProps(__spreadValues({ a: true }, b), { c: true })),
  /* @__PURE__ */ React.createElement("div", __spreadProps(__spreadValues(__spreadValues(__spreadProps(__spreadValues(__spreadValues({ a: true, b: true }, c), d), { e: true, f: true }), g), h), { i: true, j: true }))
];
