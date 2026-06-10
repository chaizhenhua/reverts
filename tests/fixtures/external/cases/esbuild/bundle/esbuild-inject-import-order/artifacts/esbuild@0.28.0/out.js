(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // inject-1.js
  var import_first = __require("first");
  console.log("first");

  // inject-2.js
  var import_second = __require("second");
  console.log("second");

  // entry.ts
  var import_third = __require("third");
  console.log("third");
})();
