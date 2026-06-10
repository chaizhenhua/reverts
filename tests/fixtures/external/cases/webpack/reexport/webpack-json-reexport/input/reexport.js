export { default as data } from "./data.json" with { type: "json" };
export { named } from "./other.json" with { type: "json" };
import * as ns from "./other.json" with { type: "json" };
export { ns };