(() => {

function $parcel$export(e, n, v, s) {
  Object.defineProperty(e, n, {get: v, set: s, enumerable: true, configurable: true});
}

var $parcel$bundleURL;
function $parcel$resolve(url) {
  url = parcelRequire.i?.[url] || url;
  if (!$parcel$bundleURL) {
    try {
      throw new Error();
    } catch (err) {
      var matches = ('' + err.stack).match(
        /(https?|file|ftp|(chrome|moz|safari-web)-extension):\/\/[^)\n]+/g,
      );
      if (matches) {
        $parcel$bundleURL = matches[0];
      } else {
        return $parcel$distDir + url;
      }
    }
  }
  return new URL($parcel$distDir + url, $parcel$bundleURL).toString();
}

      var $parcel$global = globalThis;
    var $parcel$distDir = "./";
var parcelRequire = $parcel$global["parcelRequire94c2"];
var parcelRegister = parcelRequire.register;
parcelRegister("kMHhW", function(module, exports) {

$parcel$export(module.exports, "default", () => $f219de82b9cb8805$export$2e2bcd8739ae039);

var $f219de82b9cb8805$export$2e2bcd8739ae039 = (parcelRequire("cNAhc")).then((b)=>b.default + 1);

});
parcelRegister("cNAhc", function(module, exports) {

module.exports = (parcelRequire("45Ioh"))($parcel$resolve("h4Z7G")).then(()=>parcelRequire('7g1eu'));

});


})();
