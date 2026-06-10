(() => {
  // input/class.js
  var Keep1 = class {
    *[Symbol.iterator]() {
    }
    [keep];
  };
  var Keep2 = class {
    [keep];
    *[Symbol.iterator]() {
    }
  };
  var Keep3 = class {
    *[Symbol.wtf]() {
    }
  };
})();
