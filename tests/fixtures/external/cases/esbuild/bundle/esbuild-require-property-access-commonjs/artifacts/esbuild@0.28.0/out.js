// input/entry.js
console.log(Object.keys(require.cache));
console.log(Object.keys(require.extensions));
delete require.cache["fs"];
delete require.extensions[".json"];
