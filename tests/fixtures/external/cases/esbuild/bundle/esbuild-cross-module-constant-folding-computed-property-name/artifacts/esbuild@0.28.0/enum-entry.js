(() => {
  // input/enum-entry.ts
  console.log({
    123: 123 /* a */,
    abc: "abc" /* b */
  });
  var Foo = class {
    ["__proto__"] = {};
    ["prototype"] = {};
    ["constructor"]() {
    }
  };
})();
