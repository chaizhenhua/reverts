(() => {
  var __create = Object.create;
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __getProtoOf = Object.getPrototypeOf;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };
  var __copyProps = (to, from, except, desc) => {
    if (from && typeof from === "object" || typeof from === "function") {
      for (let key of __getOwnPropNames(from))
        if (!__hasOwnProp.call(to, key) && key !== except)
          __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
    }
    return to;
  };
  var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
    // If the importer is in node compatibility mode or this is not an ESM
    // file that has been converted to a CommonJS file using a Babel-
    // compatible transform (i.e. "__esModule" has not been set), then set
    // "default" to the CommonJS "module.exports" for node compatibility.
    isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
    mod
  ));

  // empty.js
  var require_empty = __commonJS({
    "empty.js"() {
    }
  });

  // node_modules/pkg/empty.js
  var require_empty2 = __commonJS({
    "node_modules/pkg/empty.js"() {
    }
  });

  // entry.js
  var empty_js2 = __toESM(require_empty());
  var pkg_empty_js = __toESM(require_empty2());

  // node_modules/pkg/index.js
  var empty_js = __toESM(require_empty2());
  console.log(
    void 0,
    void 0,
    void 0,
    void 0,
    void 0,
    void 0
  );

  // entry.js
  console.log(
    void 0,
    void 0,
    void 0,
    void 0,
    void 0,
    void 0
  );
  console.log(
    void 0,
    void 0,
    void 0,
    void 0,
    void 0,
    void 0
  );
})();
