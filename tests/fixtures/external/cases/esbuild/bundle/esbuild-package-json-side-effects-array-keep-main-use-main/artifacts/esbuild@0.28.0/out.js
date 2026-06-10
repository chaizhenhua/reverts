(() => {
  // Users/user/project/node_modules/demo-pkg/index-main.js
  console.log("this should be kept");

  // Users/user/project/src/entry.js
  console.log("unused import");
})();
