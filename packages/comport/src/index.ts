import binding from "@comport/binding";
import type { Port } from "@comport/binding";

export function scan(): Record<string, Port>;
export function scan(name: string): undefined;
export function scan(name?: string): Record<string, Port> | void {
  return name ? binding.rescan(name) : binding.scan();
}

console.log(scan());
