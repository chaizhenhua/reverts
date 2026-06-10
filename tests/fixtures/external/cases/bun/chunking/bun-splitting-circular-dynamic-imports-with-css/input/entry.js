import("./a").then(module => {
  console.log("a loaded from entry");
  return import("./b");
}).then(module => {
  console.log("b loaded from entry, value:", module.bValue);
});
