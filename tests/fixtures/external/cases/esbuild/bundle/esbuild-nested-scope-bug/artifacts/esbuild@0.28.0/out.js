(() => {
  // input/entry.js
  (() => {
    function a() {
      b();
    }
    {
      var b = () => {
      };
    }
    a();
  })();
})();
