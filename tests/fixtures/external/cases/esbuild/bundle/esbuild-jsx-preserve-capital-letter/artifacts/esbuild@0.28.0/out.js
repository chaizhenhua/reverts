(() => {
  // input/foo.js
  var MustStartWithUpperCaseLetter = class {
  };

  // input/entry.jsx
  console.log(<MustStartWithUpperCaseLetter />);
})();
