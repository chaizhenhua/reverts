function earlyReturn() {
  onlyWithKeep();
}
function loop() {
  if (foo()) {
    bar();
    return;
  }
}
