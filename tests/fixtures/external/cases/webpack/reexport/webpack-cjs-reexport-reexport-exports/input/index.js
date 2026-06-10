const nested = require("./reexport-reexport-exports");
console.log(nested.reexport1.abc, nested.reexport2.abc, nested.reexport3.abc, nested.reexport4.abc);
module.exports = nested;