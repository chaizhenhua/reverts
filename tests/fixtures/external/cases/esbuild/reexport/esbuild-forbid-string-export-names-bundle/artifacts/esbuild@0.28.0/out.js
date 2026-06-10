(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // internal.js
  var ok = true;

  // nested.js
  var nested_exports = {};
  __export(nested_exports, {
    "nested name": () => nested2,
    "very nested name": () => nested
  });

  // very-nested.js
  var nested = 2;

  // nested.js
  var nested2 = 1;
})();
