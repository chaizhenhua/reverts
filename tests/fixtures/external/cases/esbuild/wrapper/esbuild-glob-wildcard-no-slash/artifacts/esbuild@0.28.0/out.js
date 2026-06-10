(() => {
  var __create = Object.create;
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __getProtoOf = Object.getPrototypeOf;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __glob = (map) => (path) => {
    var fn = map[path];
    if (fn) return fn();
    throw new Error("Module not found in bundle: " + path);
  };
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

  // src/file-a.js
  var require_file_a = __commonJS({
    "src/file-a.js"(exports, module) {
      module.exports = "a";
    }
  });

  // src/file-b.js
  var require_file_b = __commonJS({
    "src/file-b.js"(exports, module) {
      module.exports = "b";
    }
  });

  // require("./src/file-*.js") in entry.js
  var globRequire_src_file_js = __glob({
    "./src/file-a.js": () => require_file_a(),
    "./src/file-b.js": () => require_file_b()
  });

  // import("./src/file-*.js") in entry.js
  var globImport_src_file_js = __glob({
    "./src/file-a.js": () => Promise.resolve().then(() => __toESM(require_file_a())),
    "./src/file-b.js": () => Promise.resolve().then(() => __toESM(require_file_b()))
  });

  // entry.js
  var ab = Math.random() < 0.5 ? "a.js" : "b.js";
  console.log({
    concat: {
      require: globRequire_src_file_js("./src/file-" + ab + ".js"),
      import: globImport_src_file_js("./src/file-" + ab + ".js")
    },
    template: {
      require: globRequire_src_file_js(`./src/file-${ab}.js`),
      import: globImport_src_file_js(`./src/file-${ab}.js`)
    }
  });
})();
