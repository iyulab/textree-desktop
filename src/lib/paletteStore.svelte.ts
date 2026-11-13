/*
 * Unified palette state — open/query/selection index. Mode is derived from the query ('>' = command).
 */

import { paletteMode, paletteTerm, type ContentScope, type PaletteMode } from "./palette.helpers";

class PaletteStore {
  open = $state(false);
  query = $state("");
  /** Keyboard selection index within the result list. */
  selected = $state(0);
  /** Where content search looks when the sidebar shows several notes folders — kept between openings. */
  contentScope = $state<ContentScope>("all");

  /** Derive mode from the query prefix ('>' = command, '/' = content search, otherwise = file). */
  get mode(): PaletteMode {
    return paletteMode(this.query);
  }

  /** Actual search term per mode (prefix removed). */
  get term(): string {
    return paletteTerm(this.query);
  }

  show(): void {
    this.open = true;
    this.query = "";
    this.selected = 0;
  }

  hide(): void {
    this.open = false;
  }

  setQuery(q: string): void {
    this.query = q;
    this.selected = 0; // Reset selection on input change
  }

  setContentScope(scope: ContentScope): void {
    this.contentScope = scope;
    this.selected = 0;
  }

  move(delta: number, count: number): void {
    if (count === 0) return;
    this.selected = (this.selected + delta + count) % count;
  }
}

export const palette = new PaletteStore();
