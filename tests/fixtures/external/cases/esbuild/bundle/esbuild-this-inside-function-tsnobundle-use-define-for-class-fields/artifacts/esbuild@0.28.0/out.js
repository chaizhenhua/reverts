function foo(x = this) {
  console.log(this);
}
const objFoo = {
  foo(x = this) {
    console.log(this);
  }
};
class Foo {
  x = this;
  static y = this.z;
  foo(x = this) {
    console.log(this);
  }
  static bar(x = this) {
    console.log(this);
  }
}
new Foo(foo(objFoo));
if (nested) {
  let bar2 = function(x = this) {
    console.log(this);
  };
  var bar = bar2;
  const objBar = {
    foo(x = this) {
      console.log(this);
    }
  };
  class Bar {
    x = this;
    static y = this.z;
    foo(x = this) {
      console.log(this);
    }
    static bar(x = this) {
      console.log(this);
    }
  }
  new Bar(bar2(objBar));
}
