var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });
class A {
  static {
    __name(this, "A");
  }
  static foo;
}
class B {
  static name;
}
class C {
  static name() {
  }
}
class D {
  static get name() {
  }
}
class E {
  static set name(x) {
  }
}
class F {
  static ["name"] = 0;
}
let a = class a3 {
  static {
    __name(this, "a");
  }
  static foo;
};
let b = class b3 {
  static name;
};
let c = class c3 {
  static name() {
  }
};
let d = class d3 {
  static get name() {
  }
};
let e = class e3 {
  static set name(x) {
  }
};
let f = class f3 {
  static ["name"] = 0;
};
let a2 = class {
  static {
    __name(this, "a2");
  }
  static foo;
};
let b2 = class {
  static name;
};
let c2 = class {
  static name() {
  }
};
let d2 = class {
  static get name() {
  }
};
let e2 = class {
  static set name(x) {
  }
};
let f2 = class {
  static ["name"] = 0;
};
