(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // input/a.js
  var require_a = __commonJS({
    "input/a.js"() {
    }
  });

  // input/b.js
  var require_b = __commonJS({
    "input/b.js"() {
    }
  });

  // input/d.js
  var require_d = __commonJS({
    "input/d.js"() {
    }
  });

  // input/entry.js
  try {
    foo();
  } catch {
    require_a();
  } finally {
    require_b();
  }
  try {
  } catch {
  } finally {
    require_d();
  }
})();
