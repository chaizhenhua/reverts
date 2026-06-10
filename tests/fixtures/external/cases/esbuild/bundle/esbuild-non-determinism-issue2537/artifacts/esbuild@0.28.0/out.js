"use strict";
(() => {
  // input/entry.ts
  function i(o, e) {
    let r = "teun";
    if (o) {
      let u = function(n) {
        return n * 2;
      }, t = function(n) {
        return n / 2;
      };
      var b = u, f = t;
      r = u(e) + t(e);
    }
    return r;
  }
})();
