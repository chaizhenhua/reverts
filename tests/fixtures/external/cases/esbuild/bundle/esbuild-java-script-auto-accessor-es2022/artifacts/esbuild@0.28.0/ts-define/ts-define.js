var _a, _b;
class Foo {
  #one = 1;
  get one() {
    return this.#one;
  }
  set one(_) {
    this.#one = _;
  }
  #_two = 2;
  get #two() {
    return this.#_two;
  }
  set #two(_) {
    this.#_two = _;
  }
  #a = 3;
  get [_b = three()]() {
    return this.#a;
  }
  set [_b](_) {
    this.#a = _;
  }
  static #four = 4;
  static get four() {
    return this.#four;
  }
  static set four(_) {
    this.#four = _;
  }
  static #_five = 5;
  static get #five() {
    return this.#_five;
  }
  static set #five(_) {
    this.#_five = _;
  }
  static #b = 6;
  static get [_a = six()]() {
    return this.#b;
  }
  static set [_a](_) {
    this.#b = _;
  }
}
class Normal {
  #a = b;
  get a() {
    return this.#a;
  }
  set a(_) {
    this.#a = _;
  }
  c = d;
}
class Private {
  #_a = b;
  get #a() {
    return this.#_a;
  }
  set #a(_) {
    this.#_a = _;
  }
  c = d;
}
class StaticNormal {
  static #a = b;
  static get a() {
    return this.#a;
  }
  static set a(_) {
    this.#a = _;
  }
  static c = d;
}
class StaticPrivate {
  static #_a = b;
  static get #a() {
    return this.#_a;
  }
  static set #a(_) {
    this.#_a = _;
  }
  static c = d;
}
