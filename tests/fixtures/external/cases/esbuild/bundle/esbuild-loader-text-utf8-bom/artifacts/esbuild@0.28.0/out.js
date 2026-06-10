(() => {
  // data1.txt
  var data1_default = "\xEF\xBB\xBFtext";

  // data2.txt
  var data2_default = "text\xEF\xBB\xBF";

  // entry.js
  console.log(data1_default, data2_default);
})();
