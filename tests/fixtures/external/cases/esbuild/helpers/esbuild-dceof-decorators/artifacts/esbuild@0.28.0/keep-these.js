(() => {
  // input/decorator.js
  var fn = () => {
    console.log("side effect");
  };

  // input/keep-these.js
  var Class = @fn class {
  };
  var Field = class {
    @fn field;
  };
  var Method = class {
    @fn method() {
    }
  };
  var Accessor = class {
    @fn accessor accessor;
  };
  var StaticField = class {
    @fn static field;
  };
  var StaticMethod = class {
    @fn static method() {
    }
  };
  var StaticAccessor = class {
    @fn static accessor accessor;
  };
})();
