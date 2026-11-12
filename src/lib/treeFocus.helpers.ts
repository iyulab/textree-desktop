/*
 * Which tree item takes Tab (roving tabindex, WAI-ARIA tree) — pure (vitest). Each notes folder in
 * the sidebar is a tree of its own, so each keeps a tab stop: the item focused last when it is in
 * this folder, else the folder's first item.
 */

import type { PickedNode } from "./treeDrag.helpers";

export function tabStop(
  focused: PickedNode | null,
  folder: string,
  path: string,
  top: boolean,
  index: number,
): 0 | -1 {
  if (focused?.root === folder) return focused.path === path ? 0 : -1;
  return top && index === 0 ? 0 : -1;
}
