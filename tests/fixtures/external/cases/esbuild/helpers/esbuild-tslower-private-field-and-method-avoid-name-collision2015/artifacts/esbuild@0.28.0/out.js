(() => {
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);

  // input/entry.ts
  var _x;
  var WeakMap2 = class {
    constructor() {
      __privateAdd(this, _x);
    }
  };
  _x = new WeakMap();
  var _WeakSet_instances, y_fn;
  var WeakSet2 = class {
    constructor() {
      __privateAdd(this, _WeakSet_instances);
    }
  };
  _WeakSet_instances = new WeakSet();
  y_fn = function() {
  };
})();
