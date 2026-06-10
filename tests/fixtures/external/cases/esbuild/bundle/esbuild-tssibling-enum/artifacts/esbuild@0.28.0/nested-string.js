export var foo;
((foo2) => {
  let x;
  ((x2) => {
    x2["y"] = "a";
    x2["yy"] = "a" /* y */;
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
((foo2) => {
  let x;
  ((x2) => {
    x2["z"] = "a" /* y */;
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
((foo2) => {
  let x;
  ((x2) => {
    console.log(y, z);
    console.log("a" /* y */, "a" /* z */);
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
