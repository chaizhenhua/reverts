(() => {
  var __create = Object.create;
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.
  getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.
  getOwnPropertyNames;
  var __getProtoOf = Object.getPrototypeOf;
  var __hasOwnProp = Object.prototype.
  hasOwnProperty;
  var __require = /* @__PURE__ */ ((x) => typeof require !==
  "undefined" ? require : typeof Proxy !==
  "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !==
    "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "unde\
fined") return require.apply(this,
    arguments);
    throw Error('Dynamic require\
 of "' + x + '" is not supported');
  });
  var __copyProps = (to, from, except, desc) => {
    if (from && typeof from === "\
object" || typeof from === "func\
tion") {
      for (let key of __getOwnPropNames(
      from))
        if (!__hasOwnProp.call(to,
        key) && key !== except)
          __defProp(to, key, { get: () => from[key],
          enumerable: !(desc = __getOwnPropDesc(
          from, key)) || desc.enumerable });
    }
    return to;
  };
  var __toESM = (mod, isNodeMode, target) => (target =
  mod != null ? __create(__getProtoOf(
  mod)) : {}, __copyProps(
    // If the importer is in node compatibility mode or this is not an ESM
    // file that has been converted to a CommonJS file using a Babel-
    // compatible transform (i.e. "__esModule" has not been set), then set
    // "default" to the CommonJS "module.exports" for node compatibility.
    isNodeMode || !mod || !mod.__esModule ?
    __defProp(target, "default",
    { value: mod, enumerable: true }) :
    target,
    mod
  ));

  // x.file
  var x_default = "./x-TZ25B4WH.file";

  // script.jsx
  var import_x2 = __toESM(__require("./x-UF3O47Y3.copy"));

  // x.data
  var x_default2 = "data:text/pl\
ain;charset=utf-8,...lots of lon\
g data...lots of long data...";

  // script.jsx
  var SignUpForm = (props) => {
    return /* @__PURE__ */ React.
    createElement("p", { class: "\
signup" }, /* @__PURE__ */ React.
    createElement("label", null,
    "Username: ", /* @__PURE__ */ React.
    createElement("input", { class: "\
username", type: "text" })), /* @__PURE__ */ React.
    createElement("label", null,
    "Password: ", /* @__PURE__ */ React.
    createElement("input", { class: "\
password", type: "password" })),
    /* @__PURE__ */ React.createElement(
    "div", { class: "primary dis\
abled" }, props.buttonText), /* @__PURE__ */ React.
    createElement("small", null,
    "By signing up, you are agre\
eing to our ", /* @__PURE__ */ React.
    createElement("a", { href: "\
/tos/" }, "terms of service"), "\
."), /* @__PURE__ */ React.createElement(
    "img", { src: x_default }), /* @__PURE__ */ React.
    createElement("img", { src: import_x2.default }),
    /* @__PURE__ */ React.createElement(
    "img", { src: x_default2 }));
  };
})();
