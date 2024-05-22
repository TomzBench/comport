import binding from "@comport/binding";
import type { PortMeta, AbortHandle } from "@comport/binding";
import { Subject, Observable } from "rxjs";

/*
 * Event
 */
export type EventKind = "Plug" | "Unplug";
export type Event = { type: EventKind; port: string };
export type Events = PlugEvent | UnplugEvent;
export interface PlugEvent extends Event {
  type: "Plug";
  meta: PortMeta;
}
export interface UnplugEvent extends Event {
  type: "Unplug";
}
function valid_event(ev: any): ev is Events {
  return (
    (ev.type == "Plug" || ev.type == "Unplug") && typeof ev.port == "string"
  );
}

/*
 * Scan
 */
export function scan(): Record<string, PortMeta>;
export function scan(name: string): void;
export function scan(name?: string): Record<string, PortMeta> | void {
  return name ? binding.rescan(name) : binding.scan();
}

/*
 * Listen
 */
export function listen(name: string): [AbortHandle, Observable<Events>] {
  const subj: Subject<Events> = new Subject();
  const abortHandle = binding.listen(name, (err, event) => {
    if (err) {
      subj.error(err);
    } else if (valid_event(event)) {
      subj.next(event);
    }
  });
  return [abortHandle, subj.asObservable()];
}
