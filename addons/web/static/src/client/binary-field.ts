/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import { icon } from "./icons.js";
import { attachmentRequestCap, humanBytes } from "./attachment-policy.js";
import { binaryContent, isBinarySize } from "./binary-content.js";
import { createDropzone } from "./dropzone.js";
import { saveFile } from "./file-save.js";
import { fileAccepted } from "./image-field.js";
import { ClientError } from "./transport.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;
/** Room left in the write request for the record's other changed values. */
const REQUEST_MARGIN = 4096;

/** A record value of a binary field: empty, base64 content or the `bin_size` text; anything else is refused. */
export function fileValue(field: string, value: unknown): { content: string; size?: string } {
  if (value === false || value === null || value === undefined || value === "") return { content: "" };
  if (isBinarySize(value)) return { content: "", size: value };
  if (typeof value === "string" && value.length % 4 === 0 && BASE64.test(value)) return { content: value };
  throw new ClientError("server", `Field ${field} holds a value that is neither file content nor a size.`);
}

/** Bytes of base64 content, for the size shown before a staged file is saved. */
function decodedBytes(content: string): number {
  return content.length / 4 * 3 - (content.endsWith("==") ? 2 : content.endsWith("=") ? 1 : 0);
}

/**
 * `binary` widget, the default for a binary field that names no widget. Shows the file name (the `filename` option
 * names a char field of the same record that holds it), the size and a Download action; an editable form adds
 * Upload or Replace, Clear and dropping a file on the field. The `accepted_file_extensions` option is a chooser hint
 * only. Uploading stages the content and, with `filename`, the chosen file's name in one change; Clear empties both.
 * A value read with `bin_size` is a size: Download then reads the content of the saved record once
 * (`binary-content.ts`). In a list or a card the widget is the file name as a download link, or nothing.
 */
export function binaryControl(options: FieldControlOptions): FieldControl {
  const { field, rpc } = options, config = field.options ?? {};
  if (field.kind !== "binary") throw new ClientError("server", `Field ${field.name} with the binary widget must be a binary field.`);
  const compact = options.viewContext === "list" || options.viewContext === "card";
  const editable = !field.readonly && !compact;
  const value = fileValue(field.name, options.value);
  const nameField = config.filename?.name;
  if (nameField !== undefined && options.values !== undefined && Object.hasOwn(options.values, nameField)) {
    const stored = options.values[nameField];
    if (stored !== false && stored !== null && typeof stored !== "string") throw new ClientError("server", `Field ${nameField} holds a value that is not a file name.`);
  }
  const storedName = nameField === undefined ? "" : typeof options.values?.[nameField] === "string" ? options.values[nameField] as string : "";
  const hasFile = value.content !== "" || value.size !== undefined;
  const sizeText = value.size ?? (value.content ? humanBytes(decodedBytes(value.content)) : "");
  const downloadName = storedName || field.name;
  // Upload writes the content and its name in one change; a host without changeValues cannot, so refuse at once.
  if (editable && nameField !== undefined && !options.changeValues) throw new ClientError("unsupported", `${field.label} names a file name field, but this view cannot change two fields at once.`);

  const root = el(compact ? "span" : "div", undefined, `field-binary field-binary-${options.viewContext ?? "form"}`);
  const shell = fieldShell(field, root, options.viewContext);
  const status = el("span", undefined, "field-binary-status"); status.setAttribute("role", "status");
  const life = new AbortController();
  let dead = false, saving = false;

  // Size-only value of a saved record: Download reads the content from the server.
  const recordId = options.values?.["id"], recordModel = options.model;
  const target = value.size !== undefined && recordModel !== undefined && typeof recordId === "number" && Number.isInteger(recordId) && recordId > 0
    ? { model: recordModel, id: recordId, size: value.size } : undefined;
  const reported = (error: unknown) => {
    if (dead || error instanceof DOMException && error.name === "AbortError") return;
    status.textContent = `${field.label} could not be downloaded: ${error instanceof Error ? error.message : String(error)}`;
  };
  const save = (content: string) => {
    let bytes: Uint8Array<ArrayBuffer>;
    try { bytes = Uint8Array.from(atob(content), char => char.charCodeAt(0)); } catch { status.textContent = `${field.label} could not be saved.`; return; }
    saveFile(new Blob([bytes], { type: "application/octet-stream" }), downloadName);
  };
  const download = el("a", undefined, "field-binary-download");
  download.href = "#";
  download.textContent = storedName || (compact ? sizeText : "Download");
  download.setAttribute("aria-label", `Download ${field.label}${storedName ? ` (${storedName})` : ""}`);
  download.hidden = !hasFile;
  download.addEventListener("click", event => {
    event.preventDefault();
    if (value.content) { save(value.content); return; }
    if (saving || dead) return;
    if (!target) { status.textContent = `${field.label} has no saved record to read from.`; return; }
    saving = true; root.setAttribute("aria-busy", "true"); status.textContent = "";
    binaryContent(rpc, target.model, target.id, field.name, options.context, target.size, life.signal).then(content => {
      if (dead) return;
      if (content) save(content); else status.textContent = `${field.label} is no longer stored on this record. Reload to see its current state.`;
    }, reported).finally(() => { saving = false; root.removeAttribute("aria-busy"); });
  });

  if (compact) {
    root.append(download, status);
    return { node: shell.node, dispose: () => { dead = true; life.abort(); } };
  }

  const summary = el("span", undefined, "field-binary-summary");
  summary.append(icon("file-text"), el("span", hasFile ? storedName || field.label : `No ${field.label}`, "field-binary-name"));
  if (sizeText) summary.append(el("span", sizeText, "field-binary-size"));
  const actions = el("span", undefined, "field-binary-actions");
  actions.append(download);
  root.append(summary, actions);

  if (editable) {
    const input = el("input"); input.type = "file"; input.hidden = true; input.tabIndex = -1; input.name = field.name;
    const accept = config.accepted_file_extensions;
    if (accept !== undefined) input.accept = accept;
    const verb = hasFile ? "Replace" : "Upload";
    const upload = button(verb, () => input.click(), "field-binary-button field-binary-upload");
    upload.setAttribute("aria-label", `${verb} ${field.label}`);
    const clear = button("Clear", () => { shell.clear(); status.textContent = ""; stage(false, false); }, "field-binary-button field-binary-clear");
    clear.setAttribute("aria-label", `Clear ${field.label}`); clear.hidden = !hasFile;
    actions.prepend(upload); actions.append(clear);
    root.append(input);

    const stage = (content: string | false, name: string | false) => {
      if (nameField !== undefined) options.changeValues!({ [field.name]: content, [nameField]: name });
      else options.change(content);
    };
    const acceptFile = (file: File) => {
      shell.clear(); status.textContent = "";
      if (file.size === 0) { shell.invalid(`"${file.name}" is empty.`); return; }
      if (accept !== undefined && !fileAccepted(file, accept)) { shell.invalid(`"${file.name}" (${file.type || "unknown type"}) is not accepted here; accepted: ${accept}.`); return; }
      // The content travels as base64 inside the record's write, so the request body limit bounds it.
      const cap = attachmentRequestCap(rpc);
      const encodedLimit = cap === undefined ? Infinity : Math.floor((cap - REQUEST_MARGIN) / 4) * 4;
      if (Math.ceil(file.size / 3) * 4 > encodedLimit) { shell.invalid(`"${file.name}" is ${humanBytes(file.size)}; a file saved with the form can be up to ${humanBytes(encodedLimit / 4 * 3)}.`); return; }
      upload.disabled = true; root.setAttribute("aria-busy", "true");
      options.run(async () => {
        try {
          const content = await readBase64(file);
          if (dead) return;
          stage(content, file.name);
        } catch (error) {
          if (dead) return;
          if (error instanceof ClientError && error.kind === "validation") shell.invalid(error.message); else throw error;
        } finally {
          if (!dead) { upload.disabled = false; root.removeAttribute("aria-busy"); }
        }
      });
    };
    input.addEventListener("change", () => { const file = input.files?.[0]; input.value = ""; if (file && !dead) acceptFile(file); });
    const dropzone = createDropzone({ target: root, signal: life.signal, disabled: () => dead || upload.disabled, onDrop: event => {
      const files = event.dataTransfer?.files;
      if (!files || files.length === 0) return;
      if (files.length > 1) { shell.invalid(`Drop one file on ${field.label}.`); return; }
      acceptFile(files[0]!);
    } });
    root.append(dropzone.node);
  }
  root.append(status);
  return { node: shell.node, dispose: () => { dead = true; life.abort(); } };
}

/** The base64 content of `file`; a file the browser cannot read is refused by name. */
function readBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    const unreadable = () => new ClientError("validation", `"${file.name}" could not be read.`);
    reader.onload = () => {
      const text = String(reader.result), comma = text.indexOf(",");
      if (comma < 0) reject(unreadable()); else resolve(text.slice(comma + 1));
    };
    reader.onerror = () => reject(unreadable());
    reader.onabort = () => reject(unreadable());
    reader.readAsDataURL(file);
  });
}
