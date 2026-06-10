var __typeError = (msg) => {
  throw TypeError(msg);
};
var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
var _foo, _bar, _s_foo, _s_bar;
class Foo {
  constructor() {
    __privateAdd(this, _foo, 123);
    __privateAdd(this, _bar);
    this.foo = 123;
  }
}
_foo = new WeakMap();
_bar = new WeakMap();
_s_foo = new WeakMap();
_s_bar = new WeakMap();
__privateAdd(Foo, _s_foo, 123);
__privateAdd(Foo, _s_bar);
Foo.s_foo = 123;
