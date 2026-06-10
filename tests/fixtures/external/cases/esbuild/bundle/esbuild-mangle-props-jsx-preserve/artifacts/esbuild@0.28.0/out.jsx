let Foo = {
  a(props) {
    return <>{props.b}</>;
  },
  c: "hello, world"
};
export default <Foo.a b={Foo.c} />;
