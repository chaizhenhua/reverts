var __async = (__this, __arguments, generator) => {
  return new Promise((resolve, reject) => {
    var fulfilled = (value) => {
      try {
        step(generator.next(value));
      } catch (e) {
        reject(e);
      }
    };
    var rejected = (value) => {
      try {
        step(generator.throw(value));
      } catch (e) {
        reject(e);
      }
    };
    var step = (x) => x.done ? resolve(x.value) : Promise.resolve(x.value).then(fulfilled, rejected);
    step((generator = generator.apply(__this, __arguments)).next());
  });
};
function foo(_0) {
  return __async(this, arguments, function* (bar) {
    yield bar;
    return [this, arguments];
  });
}
class Foo {
  foo() {
    return __async(this, null, function* () {
    });
  }
}
new class Bar extends class {
} {
  constructor() {
    let x = 1;
    (() => __async(null, null, function* () {
      console.log("before super", x);
      yield 1;
      console.log("after super", x);
    }))();
    super();
    x = 2;
  }
}();
export default [
  foo,
  Foo,
  function() {
    return __async(this, null, function* () {
    });
  },
  () => __async(null, null, function* () {
  }),
  { foo() {
    return __async(this, null, function* () {
    });
  } },
  class {
    foo() {
      return __async(this, null, function* () {
      });
    }
  },
  function() {
    var _arguments = arguments;
    return (bar) => __async(this, null, function* () {
      yield bar;
      return [this, _arguments];
    });
  }
];
