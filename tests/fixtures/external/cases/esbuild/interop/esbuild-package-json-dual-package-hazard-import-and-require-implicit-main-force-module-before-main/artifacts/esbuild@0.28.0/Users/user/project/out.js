(() => {
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };
  var __copyProps = (to, from, except, desc) => {
    if (from && typeof from === "object" || typeof from === "function") {
      for (let key of __getOwnPropNames(from))
        if (!__hasOwnProp.call(to, key) && key !== except)
          __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
    }
    return to;
  };
  var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

  // Users/user/project/node_modules/demo-pkg/module.js
  var module_exports = {};
  __export(module_exports, {
    default: () => module_default
  });
  var module_default;
  var init_module = __esm({
    "Users/user/project/node_modules/demo-pkg/module.js"() {
      module_default = "module";
    }
  });

  // Users/user/project/src/test-index.js
  console.log((init_module(), __toCommonJS(module_exports)));

  // Users/user/project/src/test-module.js
  init_module();
  console.log(module_default);
})();
