var ns;
((ns2) => {
  ns2.c = 1;
  ns2.d = 2;
  ns2.e = 3;
  ({ i: { a: ns2.a } } = 4);
  function MANGLE_FUNCTION_() {
  }
  ns2.g = MANGLE_FUNCTION_;
  class MANGLE_CLASS_ {
  }
  ns2.h = MANGLE_CLASS_;
  let MANGLE_NAMESPACE_;
  ((MANGLE_NAMESPACE_2) => {
    ;
  })(MANGLE_NAMESPACE_ = ns2.f || (ns2.f = {}));
  let MANGLE_ENUM_;
  ((MANGLE_ENUM_2) => {
  })(MANGLE_ENUM_ = ns2.b || (ns2.b = {}));
  console.log({
    VAR: ns2.c,
    LET: ns2.d,
    CONST: ns2.e,
    DESTRUCTURING: ns2.a,
    FUNCTION: MANGLE_FUNCTION_,
    CLASS: MANGLE_CLASS_,
    NAMESPACE: MANGLE_NAMESPACE_,
    ENUM: MANGLE_ENUM_
  });
})(ns || (ns = {}));
console.log({
  VAR: ns.c,
  LET: ns.d,
  CONST: ns.e,
  DESTRUCTURING: ns.a,
  FUNCTION: ns.g,
  CLASS: ns.h,
  NAMESPACE: ns.f,
  ENUM: ns.b
});
((ns2) => {
  console.log({
    VAR: ns2.c,
    LET: ns2.d,
    CONST: ns2.e,
    DESTRUCTURING: ns2.a,
    FUNCTION: ns2.g,
    CLASS: ns2.h,
    NAMESPACE: ns2.f,
    ENUM: ns2.b
  });
})(ns || (ns = {}));
