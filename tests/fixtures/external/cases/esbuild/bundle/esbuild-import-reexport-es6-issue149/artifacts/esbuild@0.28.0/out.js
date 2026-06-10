(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/import.js
  var import_preact = __require("preact");
  var p = "p";

  // input/in2.jsx
  var Internal = () => /* @__PURE__ */ (0, import_preact.h)(p, null, " Test 2 ");

  // input/app.jsx
  var App = () => /* @__PURE__ */ (0, import_preact.h)(p, null, " ", /* @__PURE__ */ (0, import_preact.h)(Internal, null), " T ");
  (0, import_preact.render)(/* @__PURE__ */ (0, import_preact.h)(App, null), document.getElementById("app"));
})();
