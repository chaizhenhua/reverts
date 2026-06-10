(() => {

function $parcel$interopDefault(a) {
  return a && a.__esModule ? a.default : a;
}

      var $parcel$global = globalThis;
    
var $parcel$modules = {};
var $parcel$inits = {};

var parcelRequire = $parcel$global["parcelRequire94c2"];

if (parcelRequire == null) {
  parcelRequire = function(id) {
    if (id in $parcel$modules) {
      return $parcel$modules[id].exports;
    }
    if (id in $parcel$inits) {
      var init = $parcel$inits[id];
      delete $parcel$inits[id];
      var module = {id: id, exports: {}};
      $parcel$modules[id] = module;
      init.call(module.exports, module, module.exports);
      return module.exports;
    }
    var err = new Error("Cannot find module '" + id + "'");
    err.code = 'MODULE_NOT_FOUND';
    throw err;
  };

  parcelRequire.register = function register(id, init) {
    $parcel$inits[id] = init;
  };

  $parcel$global["parcelRequire94c2"] = parcelRequire;
}

var parcelRegister = parcelRequire.register;
parcelRegister("hYKxo", function(module, exports) {
// triggers wrapping
eval('void 0');
module.exports = ()=>'foo';

});


var $hYKxo = parcelRequire("hYKxo");
var $73c25223f96921d8$exports = {};
$73c25223f96921d8$exports = ()=>'bar';


function $1548c5f168fdeb6b$var$calc() {
    return (0, (/*@__PURE__*/$parcel$interopDefault($hYKxo)))() + (0, (/*@__PURE__*/$parcel$interopDefault($73c25223f96921d8$exports)))();
}
output = $1548c5f168fdeb6b$var$calc() + ':' + (0, (/*@__PURE__*/$parcel$interopDefault($hYKxo)))() + ':' + (0, (/*@__PURE__*/$parcel$interopDefault($73c25223f96921d8$exports)))();

})();
