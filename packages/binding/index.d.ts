/* tslint:disable */
/* eslint-disable */

/* auto-generated by NAPI-RS */

export interface Port {
  vendor: string
  product: string
}
export function scan(): Record<string, Port>
export function rescan(name: string): void
