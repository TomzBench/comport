let comport = require("../packages/core");
let { interval } = require("rxjs");
let { take } = require("rxjs/operators");

// Subscribe to Plug/Unplug events
let event$ = comport.listen("COMPORT_DEMO");

// Each interval, generate a scan event which emits another event
interval(100)
  .pipe(take(4))
  .subscribe({
    next: () => comport.scan("COMPORT_DEMO"),
    error: (e) => console.error(e),
  });

event$.pipe(take(5)).subscribe({
  next: (ev) => console.log(ev),
  error: (e) => console.error(e),
});
