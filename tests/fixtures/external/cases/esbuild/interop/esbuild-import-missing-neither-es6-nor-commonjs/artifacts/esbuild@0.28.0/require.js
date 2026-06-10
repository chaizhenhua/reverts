(() => {
  var __getOwnPropNames = Object.getOwnPropertyNames;
  var __commonJS = (cb, mod) => function __require() {
    return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
  };

  // foo.js
  var require_foo = __commonJS({
    "foo.js"() {
      console.log("no exports here");
    }
  });

  // require.js
  console.log(require_foo());
})();
