import "./b.css";
console.log("b.js executed");
export const bValue = "B";
import * as A from "./a";
console.log("b.js imports a", A);
