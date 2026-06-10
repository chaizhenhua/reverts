var __defProp = Object.defineProperty;
var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
class Foo {
  constructor() {
    __publicField(this, /* @__KEY__ */ "a", 123);
  }
}
__publicField(Foo, /* @__KEY__ */ "b", 234);
Foo.b = new Foo().a;
