import type { RootId } from "./ipc";
import { publishChoices, setPublishChoice } from "./ipc";
import { explicitChoice, isIncluded, type PublishChoice } from "./publishScope.helpers";

/**
 * What goes out when one notes folder is published, for the sidebar and the Add version dialog.
 * One instance per open folder (`folderState.svelte.ts`).
 */
export class FolderPublishScope {
  choices = $state<PublishChoice[]>([]);

  constructor(private readonly root: RootId) {}

  /** Read the folder's choices. A reload keeps showing the current ones until the new ones arrive
   *  (and keeps them if reading fails). */
  async load(): Promise<void> {
    this.choices = await publishChoices(this.root);
  }

  /** `rel`: a note or folder of this notes folder; `""` is the folder itself. */
  isIncluded(rel: string): boolean {
    return isIncluded(rel, this.choices);
  }

  explicit(rel: string): PublishChoice | null {
    return explicitChoice(rel, this.choices);
  }

  async choose(rel: string, choice: boolean | null): Promise<void> {
    this.choices = await setPublishChoice(this.root, rel, choice);
  }
}
