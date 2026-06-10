(() => {
  // input/b.js
  var Base = class {
  };

  // input/a.js
  var Keep = class extends Base {
  };

  // input/entry.js
  new Keep();
})();
