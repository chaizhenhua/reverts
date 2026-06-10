(() => {
  // input/dir1/style.css
  var button = "style_button";
  var style_default = {
    a: "style_a",
    button
  };

  // input/a.js
  console.log("file 1", button, style_default.a);

  // input/dir2/style.css
  var button2 = "style_button2";
  var style_default2 = {
    b: "style_b",
    button: button2
  };

  // input/b.js
  console.log("file 2", button2, style_default2.b);
})();
