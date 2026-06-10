(() => {
  // usr/lib/pkg/pkg1/foo.js
  console.log("pkg1");

  // lib/pkg/pkg2/bar.js
  console.log("pkg2");

  // var/lib/pkg/@scope/pkg3/baz-browser.js
  console.log("pkg3");

  // tmp/pkg/@scope/pkg4/bat.js
  console.log("pkg4");
})();
