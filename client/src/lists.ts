/**
 * The queries in a list of them, one per line, e.g. links pasted from several sites: blank lines
 * and lines starting with `#` are left out. `queue.add(parseQueries(text))` queues them all at
 * once; the engine loads them in the background, in order.
 */
export function parseQueries(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#"));
}
