/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Component, elangFieldWidget, registerFieldWidget, registerControlPanelExtension } from "@erpforall/client";
import type { FieldWidgetProps, ControlPanelProps } from "@erpforall/client";

class NoteTitle extends Component<FieldWidgetProps> {
  static override template = "acceptance_notes.NoteTitle";
  get labelValue(): string { return this.props.field.label + (this.props.field.required ? " *" : ""); }
  get textValue(): string {
    const value = this.props.value;
    if (value === false || value === null || value === undefined) return "";
    if (typeof value !== "string") throw new Error("Invalid note title value.");
    return value;
  }
  onChange(event: Event): void {
    if (this.props.field.readonly) return;
    const input = event.currentTarget;
    if (!(input instanceof HTMLInputElement)) throw new Error("Expected note title input.");
    this.props.change(input.value);
  }
}

class NotePanel extends Component<ControlPanelProps> {
  static override template = "acceptance_notes.NotePanel";
}

registerFieldWidget("acceptance_notes.note_title", {
  kinds: ["char"], render: elangFieldWidget(NoteTitle),
});
registerControlPanelExtension("acceptance_notes.guide", {
  model: "acceptance_notes.note", component: NotePanel,
});
