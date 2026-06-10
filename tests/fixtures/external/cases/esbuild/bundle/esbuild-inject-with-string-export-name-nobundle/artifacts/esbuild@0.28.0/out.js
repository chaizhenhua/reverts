var old = console.log;
var fn = (...args) => old.apply(console, ["log:"].concat(args));
fn(test);
