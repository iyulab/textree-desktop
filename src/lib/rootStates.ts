/**
 * One state per open notes folder, keyed by the folder's id.
 *
 * Each folder's state is made the first time it is asked for and stays bound to that folder: it
 * reads and writes only there, so a slow write or a late answer can never land in another folder.
 * Plain (runes-free) so it is tested directly; the states it holds may be runes classes.
 */
export class RootStates<K, T> {
  #states = new Map<K, T>();

  /** The folder's state, made with `make` if the folder has none yet. */
  of(id: K, make: () => T): T {
    let state = this.#states.get(id);
    if (state === undefined) {
      state = make();
      this.#states.set(id, state);
    }
    return state;
  }

  /** The folder's state, if it has one. */
  get(id: K): T | undefined {
    return this.#states.get(id);
  }

  /** Let go of every folder but `id`. */
  keepOnly(id: K): void {
    for (const key of [...this.#states.keys()]) if (key !== id) this.#states.delete(key);
  }

  /** Let go of one folder. */
  drop(id: K): void {
    this.#states.delete(id);
  }

  /** The folders that have a state, in the order they were first asked for. */
  ids(): K[] {
    return [...this.#states.keys()];
  }
}
