(() => {
  // input/const-constants.js
  var proto = "__proto__", ptype = "prototype", ctor = "constructor";

  // input/const-entry.js
  console.log({
    456: 456,
    xyz: "xyz"
  });
  var Foo = class {
    [proto] = {};
    [ptype] = {};
    [ctor]() {
    }
  };
})();
