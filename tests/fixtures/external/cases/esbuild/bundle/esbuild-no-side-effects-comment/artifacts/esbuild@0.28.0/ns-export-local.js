var ns;
((ns2) => {
  //! Only "c0" and "c2" should have "no side effects" (Rollup only respects "const" and only for the first one)
  ns2.v0 = function() {
  };
  ns2.v1 = function() {
  };
  ns2.l0 = function() {
  };
  ns2.l1 = function() {
  };
  ns2.c0 = /* @__NO_SIDE_EFFECTS__ */ function() {
  };
  ns2.c1 = function() {
  };
  ns2.v2 = () => {
  };
  ns2.v3 = () => {
  };
  ns2.l2 = () => {
  };
  ns2.l3 = () => {
  };
  ns2.c2 = /* @__NO_SIDE_EFFECTS__ */ () => {
  };
  ns2.c3 = () => {
  };
})(ns || (ns = {}));
