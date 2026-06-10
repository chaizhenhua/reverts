var x;
((x2) => {
  let y;
  ((y2) => {
    let Foo;
    ((Foo2) => {
      Foo2["Div"] = "div";
    })(Foo = y2.Foo || (y2.Foo = {}));
  })(y = x2.y || (x2.y = {}));
})(x || (x = {}));
((x2) => {
  let y;
  ((y2) => {
    console.log(/* @__PURE__ */ React.createElement("div" /* Div */, null));
  })(y = x2.y || (x2.y = {}));
})(x || (x = {}));
