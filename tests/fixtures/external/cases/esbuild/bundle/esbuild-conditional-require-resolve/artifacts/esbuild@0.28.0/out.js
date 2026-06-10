// input/a.js
x ? require.resolve("a") : y ? require.resolve("b") : require.resolve("c");
x ? y ? require.resolve("a") : require.resolve("b") : require.resolve(c);
