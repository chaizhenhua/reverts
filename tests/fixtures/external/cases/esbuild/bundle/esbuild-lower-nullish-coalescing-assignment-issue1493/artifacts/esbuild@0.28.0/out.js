(() => {
  // input/entry.js
  var A = class {
    #a;
    f() {
      this.#a ?? (this.#a = 1);
    }
  };
})();
