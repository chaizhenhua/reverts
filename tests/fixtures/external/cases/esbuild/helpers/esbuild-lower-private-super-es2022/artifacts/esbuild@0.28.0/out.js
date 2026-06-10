(() => {
  // input/foo1.js
  var foo1_default = class extends x {
    #foo() {
      super.foo();
    }
  };

  // input/foo2.js
  var foo2_default = class extends x {
    #foo() {
      super.foo++;
    }
  };

  // input/foo3.js
  var foo3_default = class extends x {
    static #foo() {
      super.foo();
    }
  };

  // input/foo4.js
  var foo4_default = class extends x {
    static #foo() {
      super.foo++;
    }
  };

  // input/foo5.js
  var foo5_default = class extends x {
    #foo = () => {
      super.foo();
    };
  };

  // input/foo6.js
  var foo6_default = class extends x {
    #foo = () => {
      super.foo++;
    };
  };

  // input/foo7.js
  var foo7_default = class extends x {
    static #foo = () => {
      super.foo();
    };
  };

  // input/foo8.js
  var foo8_default = class extends x {
    static #foo = () => {
      super.foo++;
    };
  };
})();
