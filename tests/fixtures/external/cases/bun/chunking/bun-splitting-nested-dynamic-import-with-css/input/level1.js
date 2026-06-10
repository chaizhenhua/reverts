import "./level1.css";
console.log("level1.js executed");
import("./level2").then(() => console.log("level2 loaded from level1"));
