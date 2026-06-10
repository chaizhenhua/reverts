import leftHelix from "./leftHelix";
import rightHelix from "./rightHelix";
Promise.all([leftHelix.run(), rightHelix.run()]).then(values => {
	console.log(values.length, leftHelix.leftHelixPrime, rightHelix.rightHelixPrime);
});
export default { leftHelix, rightHelix };