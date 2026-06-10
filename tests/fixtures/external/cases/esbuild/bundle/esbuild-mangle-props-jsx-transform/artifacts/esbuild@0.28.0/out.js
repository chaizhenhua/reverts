let Foo = {
  b(props) {
    return /* @__PURE__ */ Foo.a(Foo.d, null, props.c);
  },
  e: "hello, world",
  a(...args) {
    console.log("createElement", ...args);
  },
  d(...args) {
    console.log("Fragment", ...args);
  }
};
export default /* @__PURE__ */ Foo.a(Foo.b, { c: Foo.e });
