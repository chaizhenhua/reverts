(() => {
  // input/dir1/style.css
  var t = "l";
  var l = {
    a: "o",
    button: t
  };

  // input/a.js
  console.log("file 1", t, l.a);

  // input/dir2/style.css
  var e = "n";
  var n = {
    b: "e",
    button: e
  };

  // input/b.js
  console.log("file 2", e, n.b);
})();
