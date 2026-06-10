(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // project/entry.js
  var import_pkg1 = __require("pkg1");

  // project/file.js
  console.log("file");

  // project/node_modules/pkg2/index.js
  console.log("pkg2");

  // project/libs/pkg3.js
  console.log("pkg3");
})();
