(() => {
  // input/foo.js
  var Y = class {
  };

  // input/entry.jsx
  console.log(<Y tag-must-start-with-capital-letter />);
})();
