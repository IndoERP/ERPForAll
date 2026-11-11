/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import ace from "ace-builds";

interface Rule { token?: string; regex?: string; [key: string]: unknown }
interface HighlightRules { $rules: Record<string, Rule[]>; normalizeRules(): void }
interface TextMode { $id: string; HighlightRules: new () => HighlightRules }

/** Original presentation extension over the separately licensed upstream XML tokenizer.
 * QWeb directives receive a distinct token; their values remain source text, never expressions to run.
 * Studio's protected attributes/invalid-locator policy is deliberately not implemented here. */
export function registerCodeQWebMode(): void {
  const xml = ace.require("ace/mode/xml") as { Mode: new () => TextMode };
  const rules = ace.require("ace/mode/xml_highlight_rules") as { XmlHighlightRules: new () => HighlightRules };
  class QWebRules extends rules.XmlHighlightRules {
    constructor() {
      super();
      const attributes = this.$rules["attributes"];
      if (!attributes) throw new Error("The pinned XML tokenizer has no attribute state.");
      attributes.unshift({ token: "keyword.control.qweb", regex: "t-[a-z][a-z0-9_-]*(?=\\s|=|/?>|$)" });
      this.normalizeRules();
    }
  }
  class QWebMode extends xml.Mode {
    constructor() { super(); this.$id = "ace/mode/qweb"; this.HighlightRules = QWebRules; }
  }
  // ace.define is the pinned engine's in-memory registry; it does not evaluate or load source.
  const define = (ace as unknown as { define(name: string, dependencies: readonly string[], factory: (require: unknown, exports: { Mode?: typeof QWebMode }) => void): void }).define;
  define("ace/mode/qweb", ["require", "exports", "module"], (_require, exports) => { exports.Mode = QWebMode; });
}
