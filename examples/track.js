let comport = require("../packages/core");
let { from, switchMap } = require("rxjs");
let { tap, map, take } = require("rxjs/operators");

// Subscribe events of a Zephyr IOT device (with default Product/Vendor	ids) and
// wait for unplug
comport
  .track("COMPORT_DEMO", [["2FE3", "0100"]])
  .pipe(
    tap((ev) => console.log(`Plug ${ev.port}`)),
    switchMap((e) => from(e.unplugged()).pipe(map(() => e))),
    take(1)
  )
  .subscribe({
    next: (e) => console.log(`Unplug ${e.port}`),
    error: (e) => console.error(e),
  });
