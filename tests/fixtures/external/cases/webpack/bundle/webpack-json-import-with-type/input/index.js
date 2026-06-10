import value from "./data.json" with { type: "json" };
import named from "./named.json" with { type: "json" };
console.log(value, named.answer);
export { value, named };