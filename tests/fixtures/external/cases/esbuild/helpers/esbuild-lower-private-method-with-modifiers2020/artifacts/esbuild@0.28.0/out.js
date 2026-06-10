(() => {
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);

  // input/entry.js
  var _Foo_instances, g_fn, a_fn, ag_fn, _Foo_static, sg_fn, sa_fn, sag_fn;
  var Foo = class {
    constructor() {
      __privateAdd(this, _Foo_instances);
    }
  };
  _Foo_instances = new WeakSet();
  g_fn = function* () {
  };
  a_fn = async function() {
  };
  ag_fn = async function* () {
  };
  _Foo_static = new WeakSet();
  sg_fn = function* () {
  };
  sa_fn = async function() {
  };
  sag_fn = async function* () {
  };
  __privateAdd(Foo, _Foo_static);
})();
