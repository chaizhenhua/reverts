(() => {
  var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
    get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
  }) : x)(function(x) {
    if (typeof require !== "undefined") return require.apply(this, arguments);
    throw Error('Dynamic require of "' + x + '" is not supported');
  });

  // input/index.js
  var import_aws_sdk = __require("aws-sdk");
  var import_dynamodb = __require("aws-sdk/clients/dynamodb");
  var s3 = new import_aws_sdk.S3();
  var dynamodb = new import_dynamodb.DocumentClient();
})();
