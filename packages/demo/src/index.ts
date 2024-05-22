import * as comport from "@comport/comport";
import { interval } from "rxjs";
import { take } from "rxjs/operators";

let [abortHandle, event$] = comport.listen("listener");

interval(1000)
  .pipe(take(4))
  .subscribe({
    next: () => comport.scan("listener"),
    error: (e) => console.error(e),
  });

event$.pipe(take(5)).subscribe({
  next: (ev) => console.log(ev),
  complete: () => abortHandle.abort(),
  error: (e) => console.error(e),
});
