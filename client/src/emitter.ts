/** Listener arguments by event name. */
export type EventMap = Record<string, unknown[]>;

type Listeners<Events extends EventMap> = {
  [E in keyof Events]?: Set<(...args: Events[E]) => void>;
};

/** A minimal typed event emitter. */
export class Emitter<Events extends EventMap> {
  readonly #listeners: Listeners<Events> = {};

  /** Calls `listener` on every `event`; returns a function that removes it again. */
  on<E extends keyof Events>(event: E, listener: (...args: Events[E]) => void): () => void {
    (this.#listeners[event] ??= new Set()).add(listener);
    return () => this.off(event, listener);
  }

  /** Calls `listener` on the next `event` only. */
  once<E extends keyof Events>(event: E, listener: (...args: Events[E]) => void): () => void {
    const remove = this.on(event, (...args) => {
      remove();
      listener(...args);
    });
    return remove;
  }

  off<E extends keyof Events>(event: E, listener: (...args: Events[E]) => void): void {
    this.#listeners[event]?.delete(listener);
  }

  /**
   * Calls the listeners of `event`. A listener that throws does not keep the others from
   * running; its error is rethrown asynchronously, as an uncaught error.
   */
  protected emit<E extends keyof Events>(event: E, ...args: Events[E]): void {
    for (const listener of [...(this.#listeners[event] ?? [])]) {
      try {
        listener(...args);
      } catch (error) {
        queueMicrotask(() => {
          throw error;
        });
      }
    }
  }
}
