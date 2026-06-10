class Foo {
  constructor() {
    this.#foo = 123;
    this.foo = 123;
  }
  #foo;
  #bar;
  static #s_foo = 123;
  static #s_bar;
  static {
    this.s_foo = 123;
  }
}
