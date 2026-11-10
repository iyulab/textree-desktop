import type { RootId } from "./ipc";
import { publishChoices, setPublishChoice } from "./ipc";
import { explicitChoice, isIncluded, type PublishChoice } from "./publishScope.helpers";

/** What goes out when the open notes folder is published, for the sidebar and the Add version dialog. */
class PublishScope {
  root = $state<RootId | null>(null);
  choices = $state<PublishChoice[]>([]);

  async load(root: RootId): Promise<void> {
    // A reload of the folder already loaded keeps showing its choices until the new ones arrive
    // (and keeps them if reading fails); only a different folder starts empty.
    if (this.root !== root) {
      this.root = root;
      this.choices = [];
    }
    const found = await publishChoices(root);
    if (this.root === root) this.choices = found;
  }

  /** `rel`: a note or folder of the open notes folder; `""` is the folder itself. */
  isIncluded(rel: string): boolean {
    return this.root !== null && isIncluded(rel, this.choices);
  }

  explicit(rel: string): PublishChoice | null {
    return this.root === null ? null : explicitChoice(rel, this.choices);
  }

  async choose(rel: string, choice: boolean | null): Promise<void> {
    const root = this.root;
    if (root === null) return;
    const updated = await setPublishChoice(root, rel, choice);
    if (this.root === root) this.choices = updated;
  }
}

export const publishScope = new PublishScope();
