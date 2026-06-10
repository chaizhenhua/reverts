(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/entry.js
  var require_entry = __commonJS({
    "input/entry.js"(exports) {
      if (shouldBeExportsNotThis) {
        console.log(exports);
        console.log((x = exports) => exports);
        console.log({ x: exports });
        console.log(class extends exports.foo {
        });
        console.log(class {
          [exports.foo];
        });
        console.log(class {
          [exports.foo]() {
          }
        });
        console.log(class {
          static [exports.foo];
        });
        console.log(class {
          static [exports.foo]() {
          }
        });
      }
      if (shouldBeThisNotExports) {
        console.log(class {
          foo = this;
        });
        console.log(class {
          foo() {
            this;
          }
        });
        console.log(class {
          static foo = this;
        });
        console.log(class {
          static foo() {
            this;
          }
        });
      }
    }
  });
  require_entry();
})();
