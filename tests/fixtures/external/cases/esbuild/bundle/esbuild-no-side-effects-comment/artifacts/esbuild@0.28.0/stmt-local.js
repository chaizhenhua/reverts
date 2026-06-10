//! Only "c0" and "c2" should have "no side effects" (Rollup only respects "const" and only for the first one)
var v0 = function() {
}, v1 = function() {
};
let l0 = function() {
}, l1 = function() {
};
const c0 = /* @__NO_SIDE_EFFECTS__ */ function() {
}, c1 = function() {
};
var v2 = () => {
}, v3 = () => {
};
let l2 = () => {
}, l3 = () => {
};
const c2 = /* @__NO_SIDE_EFFECTS__ */ () => {
}, c3 = () => {
};
