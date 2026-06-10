//#region data.json
const name = "@test-fixture/named-exports";
var data_default = {
	name,
	"const": true,
	devDependencies: { "rolldown-tests": "workspace:*" },
	version: "1.0.0-beta.3"
};
//#endregion
export { data_default as json, name };
