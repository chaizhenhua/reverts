(() => {
  var __create = Object.create;
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __getProtoOf = Object.getPrototypeOf;
  var __hasOwnProp = Object.prototype.hasOwnProperty;
  var __esm = (fn, res) => function __init() {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  };
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
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
  var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
    // If the importer is in node compatibility mode or this is not an ESM
    // file that has been converted to a CommonJS file using a Babel-
    // compatible transform (i.e. "__esModule" has not been set), then set
    // "default" to the CommonJS "module.exports" for node compatibility.
    isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
    mod
  ));
  var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

  // cjs.js
  var require_cjs = __commonJS({
    "cjs.js"(exports) {
      console.log(exports);
    }
  });

  // dummy.js
  var dummy_exports = {};
  __export(dummy_exports, {
    dummy: () => dummy
  });
  var dummy;
  var init_dummy = __esm({
    "dummy.js"() {
      dummy = 123;
    }
  });

  // es6-import-stmt.js
  var require_es6_import_stmt = __commonJS({
    "es6-import-stmt.js"(exports) {
      init_dummy();
      console.log(exports);
    }
  });

  // es6-import-assign.ts
  var require_es6_import_assign = __commonJS({
    "es6-import-assign.ts"(exports) {
      var x2 = (init_dummy(), __toCommonJS(dummy_exports));
      console.log(exports);
    }
  });

  // es6-import-dynamic.js
  var require_es6_import_dynamic = __commonJS({
    "es6-import-dynamic.js"(exports) {
      Promise.resolve().then(() => init_dummy());
      console.log(exports);
    }
  });

  // es6-expr-import-dynamic.js
  var require_es6_expr_import_dynamic = __commonJS({
    "es6-expr-import-dynamic.js"(exports) {
      Promise.resolve().then(() => init_dummy());
      console.log(exports);
    }
  });

  // es6-export-assign.ts
  var require_es6_export_assign = __commonJS({
    "es6-export-assign.ts"(exports, module) {
      console.log(exports);
      module.exports = 123;
    }
  });

  // es6-ns-export-variable.ts
  var require_es6_ns_export_variable = __commonJS({
    "es6-ns-export-variable.ts"(exports) {
      var ns;
      ((ns2) => {
        ns2.foo = 123;
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-function.ts
  var require_es6_ns_export_function = __commonJS({
    "es6-ns-export-function.ts"(exports) {
      var ns;
      ((ns2) => {
        function foo() {
        }
        ns2.foo = foo;
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-async-function.ts
  var require_es6_ns_export_async_function = __commonJS({
    "es6-ns-export-async-function.ts"(exports) {
      var ns;
      ((ns2) => {
        async function foo() {
        }
        ns2.foo = foo;
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-enum.ts
  var require_es6_ns_export_enum = __commonJS({
    "es6-ns-export-enum.ts"(exports) {
      var ns;
      ((ns2) => {
        let Foo;
        ((Foo2) => {
        })(Foo = ns2.Foo || (ns2.Foo = {}));
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-const-enum.ts
  var require_es6_ns_export_const_enum = __commonJS({
    "es6-ns-export-const-enum.ts"(exports) {
      var ns;
      ((ns2) => {
        let Foo;
        ((Foo2) => {
        })(Foo = ns2.Foo || (ns2.Foo = {}));
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-module.ts
  var require_es6_ns_export_module = __commonJS({
    "es6-ns-export-module.ts"(exports) {
      console.log(exports);
    }
  });

  // es6-ns-export-namespace.ts
  var require_es6_ns_export_namespace = __commonJS({
    "es6-ns-export-namespace.ts"(exports) {
      console.log(exports);
    }
  });

  // es6-ns-export-class.ts
  var require_es6_ns_export_class = __commonJS({
    "es6-ns-export-class.ts"(exports) {
      var ns;
      ((ns2) => {
        class Foo {
        }
        ns2.Foo = Foo;
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // es6-ns-export-abstract-class.ts
  var require_es6_ns_export_abstract_class = __commonJS({
    "es6-ns-export-abstract-class.ts"(exports) {
      var ns;
      ((ns2) => {
        class Foo {
        }
        ns2.Foo = Foo;
      })(ns || (ns = {}));
      console.log(exports);
    }
  });

  // entry.js
  var import_cjs = __toESM(require_cjs());
  var import_es6_import_stmt = __toESM(require_es6_import_stmt());
  var import_es6_import_assign = __toESM(require_es6_import_assign());
  var import_es6_import_dynamic = __toESM(require_es6_import_dynamic());

  // es6-import-meta.js
  console.log(void 0);

  // entry.js
  var import_es6_expr_import_dynamic = __toESM(require_es6_expr_import_dynamic());

  // es6-expr-import-meta.js
  console.log(void 0);

  // es6-export-variable.js
  console.log(void 0);

  // es6-export-function.js
  console.log(void 0);

  // es6-export-async-function.js
  console.log(void 0);

  // es6-export-enum.ts
  console.log(void 0);

  // es6-export-const-enum.ts
  console.log(void 0);

  // es6-export-module.ts
  console.log(void 0);

  // es6-export-namespace.ts
  console.log(void 0);

  // es6-export-class.js
  console.log(void 0);

  // es6-export-abstract-class.ts
  console.log(void 0);

  // es6-export-default.js
  console.log(void 0);

  // es6-export-clause.js
  console.log(void 0);

  // es6-export-clause-from.js
  init_dummy();
  console.log(void 0);

  // es6-export-star.js
  init_dummy();
  console.log(void 0);

  // es6-export-star-as.js
  init_dummy();
  console.log(void 0);

  // entry.js
  var import_es6_export_assign = __toESM(require_es6_export_assign());

  // es6-export-import-assign.ts
  var x = (init_dummy(), __toCommonJS(dummy_exports));
  console.log(void 0);

  // entry.js
  var import_es6_ns_export_variable = __toESM(require_es6_ns_export_variable());
  var import_es6_ns_export_function = __toESM(require_es6_ns_export_function());
  var import_es6_ns_export_async_function = __toESM(require_es6_ns_export_async_function());
  var import_es6_ns_export_enum = __toESM(require_es6_ns_export_enum());
  var import_es6_ns_export_const_enum = __toESM(require_es6_ns_export_const_enum());
  var import_es6_ns_export_module = __toESM(require_es6_ns_export_module());
  var import_es6_ns_export_namespace = __toESM(require_es6_ns_export_namespace());
  var import_es6_ns_export_class = __toESM(require_es6_ns_export_class());
  var import_es6_ns_export_abstract_class = __toESM(require_es6_ns_export_abstract_class());
})();
