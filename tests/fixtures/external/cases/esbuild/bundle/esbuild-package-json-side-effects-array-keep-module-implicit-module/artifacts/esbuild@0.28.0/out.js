(() => {
  // input/Users/user/project/node_modules/demo-pkg/index-module.js
  console.log("this should be kept");

  // input/Users/user/project/src/entry.js
  console.log("unused import");
})();
