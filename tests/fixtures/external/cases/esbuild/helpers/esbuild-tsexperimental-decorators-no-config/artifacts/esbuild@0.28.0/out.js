(() => {
  // input/entry.ts
  var Foo = @x.y() @(new y.x()) class _Foo {
    @x @y mUndef;
    @x @y mDef = 1;
    @x @y method() {
      return new _Foo();
    }
    @x @y accessor aUndef;
    @x @y accessor aDef = 1;
    @x @y static sUndef;
    @x @y static sDef = new _Foo();
    @x @y static sMethod() {
      return new _Foo();
    }
    @x @y static accessor asUndef;
    @x @y static accessor asDef = 1;
    @x @y #mUndef;
    @x @y #mDef = 1;
    @x @y #method() {
      return new _Foo();
    }
    @x @y accessor #aUndef;
    @x @y accessor #aDef = 1;
    @x @y static #sUndef;
    @x @y static #sDef = 1;
    @x @y static #sMethod() {
      return new _Foo();
    }
    @x @y static accessor #asUndef;
    @x @y static accessor #asDef = 1;
  };
})();
