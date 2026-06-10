(() => {
  // input/foo.js
  var foo_default = Uint8Array.fromBase64("ZXhwb3J0IGRlZmF1bHQgJ2pzJw==");

  // input/entry.js
  console.log(foo_default);
})();
