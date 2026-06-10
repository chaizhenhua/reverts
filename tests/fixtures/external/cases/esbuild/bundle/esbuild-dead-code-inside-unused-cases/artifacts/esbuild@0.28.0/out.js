(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // a.js
  var require_a = __commonJS({
    "a.js"() {
    }
  });

  // b.js
  var require_b = __commonJS({
    "b.js"() {
    }
  });

  // entry.js
  switch (x) {
    case 0:
      _ = require_a();
      break;
    case 1:
      _ = require_b();
      break;
  }
  switch (1) {
    case 0:
      _ = null;
      break;
    case 1:
      _ = require_a();
      break;
    case 1:
      _ = null;
      break;
    case 2:
      _ = null;
      break;
  }
  switch (0) {
    case 1:
      _ = null;
      break;
    default:
      _ = require_a();
      break;
  }
  switch (1) {
    case 1:
      _ = require_a();
      break;
    default:
      _ = null;
      break;
  }
  switch (0) {
    case 1:
      _ = null;
      break;
    default:
      _ = null;
      break;
    case 0:
      _ = require_a();
      break;
  }
  switch (1) {
    case x:
      _ = require_a();
      break;
    case 1:
      _ = require_b();
      break;
    case x:
      _ = null;
      break;
    default:
      _ = null;
      break;
  }
  for (const x2 of y)
    switch (1) {
      case 0:
        _ = null;
        continue;
      case 1:
        _ = require_a();
        continue;
      case 2:
        _ = null;
        continue;
    }
  x = () => {
    switch (1) {
      case 0:
        _ = null;
        return;
      case 1:
        _ = require_a();
        return;
      case 2:
        _ = null;
        return;
    }
  };
  switch ("b") {
    case "a":
      _ = null;
    case "b":
      _ = require_a();
    case "c":
      _ = require_b();
      break;
    case "d":
      _ = null;
  }
  switch ("b") {
    case "a":
      _ = null;
    case "b":
    case "c":
      _ = require_a();
    case "d":
      _ = require_b();
      break;
    case "e":
      _ = null;
  }
})();
