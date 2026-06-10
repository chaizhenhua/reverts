export var foo;
((foo2) => {
  let x;
  ((x2) => {
    x2[x2["y"] = 0] = "y";
    x2[x2["yy"] = 0 /* y */] = "yy";
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
((foo2) => {
  let x;
  ((x2) => {
    x2[x2["z"] = 1] = "z";
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
((foo2) => {
  let x;
  ((x2) => {
    console.log(y, z);
    console.log(0 /* y */, 1 /* z */);
  })(x = foo2.x || (foo2.x = {}));
})(foo || (foo = {}));
