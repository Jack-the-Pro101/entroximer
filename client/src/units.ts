/** `value` as the engine takes milliseconds and percents: a whole number, at least 0. */
export function whole(value: number): number {
  return Math.max(0, Math.round(value));
}
