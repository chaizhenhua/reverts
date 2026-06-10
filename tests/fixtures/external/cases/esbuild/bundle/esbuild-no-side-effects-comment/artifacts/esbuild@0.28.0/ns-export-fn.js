var ns;
((ns2) => {
  //! These should all have "no side effects"
  // @__NO_SIDE_EFFECTS__
  function a() {
  }
  ns2.a = a;
  // @__NO_SIDE_EFFECTS__
  function* b() {
  }
  ns2.b = b;
  // @__NO_SIDE_EFFECTS__
  async function c() {
  }
  ns2.c = c;
  // @__NO_SIDE_EFFECTS__
  async function* d() {
  }
  ns2.d = d;
})(ns || (ns = {}));
