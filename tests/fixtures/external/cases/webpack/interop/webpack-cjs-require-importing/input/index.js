const prop = require("./module").abc;
const { abc } = require("./module");
const moduleCopy = require("./module");
console.log(prop, abc, moduleCopy.def);
module.exports = { prop, abc, moduleCopy };