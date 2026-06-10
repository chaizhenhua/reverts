// entry.js
var import_somewhere = require("somewhere");
_ = class {
  #bar;
  classes = [
    class {
      @import_somewhere.imported @((0, import_somewhere.imported)()) imported;
    },
    class {
      @unbound @unbound() unbound;
    },
    class {
      @(123) @(123()) constant;
    },
    class {
      @(void 0) @((void 0)()) undef;
    },
    class {
      @(element[access]) indexed;
    },
    class {
      @foo.#bar private;
    },
    class {
      @(foo["\u30FF"]) unicode;
    },
    class {
      @(() => {
      }) arrow;
    }
  ];
};
