(() => {
  // input/entry.js
  var A = class _A {
    static {
    }
    static {
      this.thisField++;
      _A.classField++;
      super.superField = super.superField + 1;
      super.superField++;
    }
  };
  var B = class {
    static {
    }
    static {
      this.thisField++;
      super.superField = super.superField + 1;
      super.superField++;
    }
  };
})();
