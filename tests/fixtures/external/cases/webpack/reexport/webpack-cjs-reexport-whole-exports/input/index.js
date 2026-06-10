const all = require("./reexport-whole-exports");
console.log(all.module1.abc, all.module2.abc, all.module3.abc, all.module4.abc);
module.exports = all;