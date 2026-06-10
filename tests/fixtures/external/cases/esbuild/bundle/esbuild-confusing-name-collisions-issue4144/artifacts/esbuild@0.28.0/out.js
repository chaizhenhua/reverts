(() => {
  // input/node_modules/mydependency/package/utils/utils.js
  function it() {
    return works;
  }

  // input/node_modules/mydependency/package/index.js
  var works = true;

  // input/entry.js
  console.log(it());
})();
