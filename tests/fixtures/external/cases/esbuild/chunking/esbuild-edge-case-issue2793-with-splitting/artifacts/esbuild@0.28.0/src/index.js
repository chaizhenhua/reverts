// input/src/a.js
var A = 42;

// input/src/b.js
var B = async () => (await import("./index.js")).A;
export {
  A,
  B
};
