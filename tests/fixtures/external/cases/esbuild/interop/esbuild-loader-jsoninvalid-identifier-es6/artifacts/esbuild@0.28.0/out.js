(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/test.json
  var invalid_identifier = true;

  // input/test2.json
  var test2_exports = {};
  __export(test2_exports, {
    default: () => test2_default,
    "invalid-identifier": () => invalid_identifier2
  });
  var invalid_identifier2 = true;
  var test2_default = { "invalid-identifier": invalid_identifier2 };

  // input/entry.js
  console.log(invalid_identifier, test2_exports);
})();
