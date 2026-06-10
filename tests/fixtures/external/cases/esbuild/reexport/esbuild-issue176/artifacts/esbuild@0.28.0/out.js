(() => {
  var __defProp = Object.defineProperty;
  var __export = (target, all) => {
    for (var name in all)
      __defProp(target, name, { get: all[name], enumerable: true });
  };

  // input/folders/index.js
  var folders_exports = {};
  __export(folders_exports, {
    foo: () => foo
  });

  // input/folders/child/foo.js
  var foo = () => "hi there";

  // input/entry.js
  console.log(JSON.stringify(folders_exports));
})();
