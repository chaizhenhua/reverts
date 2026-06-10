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

  // input/src/a.ts
  var require_a = __commonJS({
    "input/src/a.ts"(exports, module) {
      module.exports = "a";
    }
  });

  // input/src/b.ts
  var require_b = __commonJS({
    "input/src/b.ts"(exports, module) {
      module.exports = "b";
    }
  });

  // require("./src/**/*") in input/entry.ts
  var globRequire_src = __glob({
    "./src/a.ts": () => require_a(),
    "./src/b.ts": () => require_b()
  });

  // import("./src/**/*") in input/entry.ts
  var globImport_src = __glob({
    "./src/a.ts": () => Promise.resolve().then(() => __toESM(require_a())),
    "./src/b.ts": () => Promise.resolve().then(() => __toESM(require_b()))
  });

  // input/entry.ts
  var ab = Math.random() < 0.5 ? "a.ts" : "b.ts";
  console.log({
    concat: {
      require: globRequire_src("./src/" + ab),
      import: globImport_src("./src/" + ab)
    },
    template: {
      require: globRequire_src(`./src/${ab}`),
      import: globImport_src(`./src/${ab}`)
    }
  });
})();
