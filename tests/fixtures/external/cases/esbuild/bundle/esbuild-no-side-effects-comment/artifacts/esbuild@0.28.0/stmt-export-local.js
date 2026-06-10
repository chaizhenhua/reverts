//! Only "c0" and "c2" should have "no side effects" (Rollup only respects "const" and only for the first one)
export var v0 = function() {
}, v1 = function() {
};
export let l0 = function() {
}, l1 = function() {
};
export const c0 = /* @__NO_SIDE_EFFECTS__ */ function() {
}, c1 = function() {
};
export var v2 = () => {
}, v3 = () => {
};
export let l2 = () => {
}, l3 = () => {
};
export const c2 = /* @__NO_SIDE_EFFECTS__ */ () => {
}, c3 = () => {
};
