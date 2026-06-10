//! These should all have "no side effects"
x([
  /* @__NO_SIDE_EFFECTS__ */ function() {
  },
  /* @__NO_SIDE_EFFECTS__ */ function y() {
  },
  /* @__NO_SIDE_EFFECTS__ */ function* () {
  },
  /* @__NO_SIDE_EFFECTS__ */ function* y2() {
  },
  /* @__NO_SIDE_EFFECTS__ */ async function() {
  },
  /* @__NO_SIDE_EFFECTS__ */ async function y3() {
  },
  /* @__NO_SIDE_EFFECTS__ */ async function* () {
  },
  /* @__NO_SIDE_EFFECTS__ */ async function* y4() {
  }
]);
