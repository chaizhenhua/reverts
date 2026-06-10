(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // internal.js
  var internal_exports = {};
  __export(internal_exports, {
    default: () => internal_default
  });
  var internal_default = 123;

  // internal-ns.js
  console.log(internal_default, internal_exports);
})();
