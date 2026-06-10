class Foo {
  constructor(KEEP_FIELD, MANGLE_FIELD_) {
    this.KEEP_FIELD = KEEP_FIELD;
    this.a = MANGLE_FIELD_;
  }
  KEEP_FIELD;
  MANGLE_FIELD_;
}
let foo = new Foo();
console.log(foo.KEEP_FIELD, foo.a);
