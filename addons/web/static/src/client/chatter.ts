/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { RegistryHub, type Translate } from "@erpforall/elang";
import { MessagingStore, type ThreadRef } from "@erpforall/mail-store";
import {
  Composer,
  Thread,
  applyStorePayload,
  checkPostAnswer,
  defineThreadViewRegistries,
  glyphNode,
  formatBytes,
  initialsAvatar,
  registerBuiltins,
  type AttachmentUploadResult,
  type ThreadPager,
  type ThreadPostAnswer,
  type ThreadPostDto,
  type ThreadTransport,
  type ThreadViewBaseOptions,
} from "@erpforall/mail-thread-view";
import { Activities } from "./chatter-activities.js";
import { icon } from "./icons.js";
import { Followers } from "./chatter-followers.js";
import { el } from "./dom.js";
import { ClientError, type RpcClient } from "./transport.js";

/** Configuration options for mounting a chatter component. */
export interface ChatterOptions {
  /** The container slot element (e.g. .form-chatter-slot). */
  readonly host: HTMLElement;
  /** The model of the record (e.g. "res.partner"). */
  readonly model: string;
  /** The record id, or undefined if unsaved. */
  readonly recordId?: number;
  /** The RPC client to fetch store topics and post messages. */
  readonly rpc: RpcClient;
  /** Optional shared tab messaging store. If omitted, a scoped store is created. */
  readonly store?: MessagingStore;
  /** Optional translator for labels and aria descriptions. */
  readonly translate?: Translate;
  /**
   * Reporter for the default store created when no shared `store` is given. Chatter-local failures (load, post,
   * followers) render inline inside the chatter and never reach this reporter, so they cannot open a blocking dialog.
   */
  readonly reportError?: (error: unknown) => void;
  /** Reload the parent form when a message is posted (from the reload_on_post node attribute). */
  readonly reloadOnPost?: boolean;
  /** Whether the model has activities (the `has_activities` flag of the node); only then the card and button exist. */
  readonly hasActivities?: boolean;
  /** Reload the parent form after an activity change (the `reload_on_activity` node attribute). */
  readonly reloadOnActivity?: boolean;
  /** Callback to reload the parent form when reloadOnPost is true. */
  readonly reloadForm?: () => Promise<void>;
  /** Optional document owner. */
  readonly document?: Document;
  /** Timezone for formatting timestamps. */
  readonly timeZone?: string;
  /** Reload the parent form after a follower change (the `reload_on_follower` node attribute). */
  readonly reloadOnFollower?: boolean;
  /** Signed-in user id; lets the followers control find the user's own partner. */
  readonly uid?: number;
  /** Current viewer partner id. */
  readonly selfPartnerId?: number | null;
  /** Display name of the signed-in user; the composer shows its initials as the avatar. Without it the composer has no avatar. */
  readonly userName?: string;
  /** Asks the user to confirm a deletion; resolves true to go on. Deleting attachments fails loudly without it. */
  readonly confirm?: (message: string) => Promise<boolean>;
}

/** Fills `%s` placeholders in order; the host's translator does this when one is passed, this one only when none is. */
const defaultTranslate: Translate = (source: string, ...values: unknown[]): string => {
  let next = 0;
  return source.replace(/%s/g, () => String(values[next++] ?? ""));
};

/** Per-host default store cache when host doesn't provide one. */
interface HostStoreEntry {
  readonly store: MessagingStore;
  setReporter: (reportError: (err: unknown) => void) => void;
}

const hostStores = new WeakMap<HTMLElement, HostStoreEntry>();

function getOrCreateStore(host: HTMLElement, reportError: (err: unknown) => void): MessagingStore {
  let entry = hostStores.get(host);
  if (!entry) {
    let currentReporter = reportError;
    const store = new MessagingStore({
      reportError: (err) => currentReporter(err),
    });
    entry = {
      store,
      setReporter: (next) => { currentReporter = next; },
    };
    hostStores.set(host, entry);
  } else {
    entry.setReporter(reportError);
  }
  return entry.store;
}

/**
 * Largest file the client reads into memory and base64-encodes before sending (the composer's own cap).
 * shortcut: the server's inline limit is not sent to the client, so this fixed cap is the only early check; the
 * server still refuses what it does not accept. Read the real limit from the bootstrap once the server exposes it.
 */
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requireRecord(value: unknown, what: string): Record<string, unknown> {
  if (!isRecord(value)) throw new ClientError("server", `${what} returned an invalid response`);
  return value;
}

/** The store payload of an answer; a missing one is a server fault, never an empty store. */
function storePayloadOf(raw: unknown, what: string): string | Uint8Array {
  if (typeof raw === "string" || raw instanceof Uint8Array) return raw;
  if (isRecord(raw)) return JSON.stringify(raw);
  throw new ClientError("server", `${what} answered no store payload`);
}

/** The `topics` list of a `store_fetch` answer, and a finder for one named topic. */
function topicsOf(res: Record<string, unknown>): (name: string) => Record<string, unknown> | undefined {
  const topics = res["topics"];
  if (!Array.isArray(topics)) throw new ClientError("server", "store_fetch answered no topics list");
  return (name) => {
    const found = topics.find((t) => isRecord(t) && t["topic"] === name);
    return found as Record<string, unknown> | undefined;
  };
}

/** The message ids a `messages` document names; a document without them is a server fault. */
function messageIdsOf(doc: unknown, what: string): { ids: number[]; hasOlder: boolean } {
  const messages = isRecord(doc) ? doc : undefined;
  const ids = messages?.["message_ids"];
  if (!Array.isArray(ids)) throw new ClientError("server", `store_fetch answered no message ids for ${what}`);
  return { ids: [...(ids as number[])].sort((a, b) => a - b), hasOlder: Boolean(messages?.["has_more_before"]) };
}

async function readFileAsBase64(file: File): Promise<string> {
  if (typeof FileReader !== "undefined") {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const result = reader.result as string;
        const base64 = result.includes(",") ? result.split(",")[1]! : result;
        resolve(base64);
      };
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
  }
  // no FileReader (a non-browser host): encode with the platform's own base64, never a Node-only API
  let binary = "";
  for (const byte of new Uint8Array(await file.arrayBuffer())) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/** What `thread_upload` and `thread_attach` answer for the file they stored. */
interface UploadedFile {
  readonly id: number;
  readonly name: string;
  readonly mimetype: string;
  readonly size: number;
}

/**
 * Send one file to the server as base64: `thread_upload` keeps it for the composer until the post
 * names it, `thread_attach` attaches it to the record itself (the attachments panel).
 */
async function uploadFile(
  rpc: RpcClient,
  ref: ThreadRef,
  method: "thread_upload" | "thread_attach",
  file: File
): Promise<UploadedFile> {
  if (file.size > MAX_UPLOAD_BYTES) {
    throw new ClientError("validation", `"${file.name}" is larger than the ${formatBytes(MAX_UPLOAD_BYTES)} upload limit`);
  }
  const answer = (await rpc.call(
    ref.model,
    method,
    {
      ids: [ref.id],
      name: file.name,
      // a browser reports "" for a type it does not know; the server then detects it
      ...(file.type === "" ? {} : { mimetype: file.type }),
      data: await readFileAsBase64(file),
    },
    { write: true }
  )) as Record<string, unknown> | null;
  const id = answer?.["attachment_id"];
  const name = answer?.["name"];
  const mimetype = answer?.["mimetype"];
  const size = answer?.["file_size"];
  if (
    typeof id !== "number" ||
    typeof name !== "string" ||
    typeof mimetype !== "string" ||
    typeof size !== "number"
  ) {
    throw new ClientError("server", `${method} returned an unexpected answer`);
  }
  return { id, name, mimetype, size };
}

/**
 * Chatter component rendering the <chatter/> node on a record form.
 *
 * Hosts a message feed (newest first, author, date, body, tracking lines),
 * a composer (Log note, Send message), and an attachments panel (AT-01..03)
 * backed by the messaging store and RPCs.
 */
export class Chatter {
  /** The chatter container element. */
  readonly element: HTMLElement;
  /** The messaging store used by this chatter. */
  readonly store?: MessagingStore;
  /** The mounted thread component, once schema is loaded. */
  thread?: Thread;
  /** The mounted composer component, once schema is loaded. */
  composer?: Composer;
  /** The activities card and button (only when the model has activities and the record is saved). */
  activities?: Activities;
  /** The followers control of the header (absent on an unsaved record). */
  followers?: Followers;
  #pager?: ThreadPager;
  #ref?: ThreadRef;
  private statusArea?: HTMLElement;
  private disposed = false;
  private stopWatch?: () => void;

  constructor(options: ChatterOptions) {
    const doc = options.document ?? (typeof document !== "undefined" ? document : options.host.ownerDocument);
    this.element = el("div", undefined, "erp-chatter erp-mail-chatter");

    if (options.recordId === undefined || options.recordId === null || options.recordId < 1) {
      // Unsaved record placeholder (CH-07)
      const placeholder = el(
        "div",
        "Save the record to start discussion.",
        "erp-mail-chatter-empty form-chatter-unsaved"
      );
      this.element.append(placeholder);
      options.host.append(this.element);
      return;
    }

    const ref: ThreadRef = { model: options.model, id: options.recordId };
    const reportError = options.reportError ?? ((err: unknown) => console.error(err));
    const store = options.store ?? getOrCreateStore(options.host, reportError);
    this.store = store;

    const hub = new RegistryHub();
    const registries = defineThreadViewRegistries(hub);
    registerBuiltins(registries);

    let attachmentCount = 0;
    let recordAttachmentIds: number[] = [];
    type AttachmentItem = { id: number; name: string; fileSize?: number; mimetype?: string; checksum?: string };
    let recordAttachmentItems: AttachmentItem[] = [];
    let selectionMode = false;
    const selectedIds = new Set<number>();

    const topbarArea = el("div", undefined, "erp-mail-chatter-topbar erp-chatter-topbar");
    const paperclipBtn = el("button", undefined, "erp-mail-chatter-attachment-btn erp-chatter-btn");
    paperclipBtn.type = "button";
    paperclipBtn.setAttribute("aria-label", "Attachments");
    const paperclipIcon = glyphNode(doc, "paperclip", 18);
    const countBadge = el("span", "0", "erp-mail-chatter-attachment-count");
    paperclipBtn.append(paperclipIcon, countBadge);
    topbarArea.append(paperclipBtn);

    const recordFileInput = el("input", undefined, "erp-mail-chatter-file-input") as HTMLInputElement;
    recordFileInput.type = "file";
    recordFileInput.multiple = true;
    recordFileInput.hidden = true;

    const filesPanel = el("div", undefined, "erp-mail-chatter-files-panel");
    filesPanel.hidden = true;

    const filesToolbar = el("div", undefined, "erp-mail-chatter-files-toolbar");
    const attachFilesBtn = el("button", "Attach files", "erp-mail-chatter-attach-files-btn erp-chatter-btn");
    attachFilesBtn.type = "button";
    const selectFilesBtn = el("button", "Select files", "erp-mail-chatter-select-files-btn erp-chatter-btn");
    selectFilesBtn.type = "button";
    selectFilesBtn.hidden = true;

    const selectionActions = el("div", undefined, "erp-mail-chatter-selection-actions");
    selectionActions.hidden = true;
    const deleteSelectedBtn = el("button", "Delete (0)", "erp-mail-chatter-delete-selected erp-chatter-btn");
    deleteSelectedBtn.type = "button";
    const discardSelectionBtn = el("button", "Discard selection", "erp-mail-chatter-discard-selection erp-chatter-btn");
    discardSelectionBtn.type = "button";
    selectionActions.append(deleteSelectedBtn, discardSelectionBtn);

    filesToolbar.append(attachFilesBtn, selectFilesBtn, selectionActions);
    const filesList = el("div", undefined, "erp-mail-chatter-files-list");
    filesPanel.append(filesToolbar, filesList);

    const statusArea = el("div", undefined, "erp-mail-chatter-status");
    this.statusArea = statusArea;
    const bodyArea = el("div", undefined, "erp-mail-chatter-body");
    // The composer sits right under the button row, above the planned activities, files and thread (as in the reference).
    const composerSlot = el("div", undefined, "erp-mail-chatter-composer");
    // The error line sits directly after the composer, before the activities, the files and the thread, next to the control that failed.
    composerSlot.append(statusArea);
    const header = el("div", undefined, "erp-mail-chatter-header");
    const tools = el("div", undefined, "erp-mail-chatter-tools");
    // Reserved for the planned-activities card: rendered only once the client pieces for activities exist.
    const activitiesSlot = el("div", undefined, "erp-mail-chatter-activities");
    activitiesSlot.hidden = true;
    // Placeholder rows while the schema and the first page load; a failure or the mounted thread replaces them.
    const skeleton = el("div", undefined, "erp-mail-skeleton");
    skeleton.setAttribute("aria-hidden", "true");
    for (let i = 0; i < 3; i++) skeleton.append(el("div", undefined, "erp-mail-skeleton-row"));
    bodyArea.append(skeleton);
    this.followers = new Followers({
      rpc: options.rpc,
      store,
      model: options.model,
      recordId: options.recordId,
      ...(options.uid !== undefined ? { uid: options.uid } : {}),
      ...(options.reloadOnFollower && options.reloadForm ? { onChange: options.reloadForm } : {}),
      ...(options.translate ? { translate: options.translate } : {}),
    });
    if (options.hasActivities) {
      this.activities = new Activities({
        rpc: options.rpc, model: options.model, recordId: options.recordId,
        ...(options.uid !== undefined ? { uid: options.uid } : {}),
        ...(options.reloadOnActivity && options.reloadForm ? { onChange: options.reloadForm } : {}),
        onLogged: () => pager.loadLatest(ref),
      });
      activitiesSlot.hidden = false;
      activitiesSlot.append(this.activities.element);
      tools.append(this.activities.button);
    }
    tools.append(topbarArea, this.followers.element);
    header.append(tools);
    this.element.append(header, composerSlot, activitiesSlot, filesPanel, bodyArea, recordFileInput);
    options.host.append(this.element);

    const clearInline = (): void => {
      if (!this.disposed) statusArea.replaceChildren();
    };

    let threadLoadAlert: HTMLElement | undefined;
    const clearThreadLoadAlert = (): void => {
      if (threadLoadAlert && threadLoadAlert.parentElement === statusArea) {
        threadLoadAlert.remove();
      }
      threadLoadAlert = undefined;
    };

    // Chatter-local failures stay in the chatter: an inline alert, never the global error dialog.
    const showInline = (err: unknown, retry?: () => void): HTMLElement | undefined => {
      if (this.disposed) return undefined;
      statusArea.replaceChildren();
      skeleton.remove();
      const alert = el("div", undefined, "erp-mail-thread-error erp-mail-chatter-error");
      alert.setAttribute("role", "alert");
      alert.append(el("span", err instanceof Error ? err.message : String(err), "erp-mail-error-message"));
      if (retry) {
        const button = el("button", "Retry", "erp-mail-retry");
        button.type = "button";
        button.addEventListener("click", () => {
          alert.remove();
          retry();
        });
        alert.append(button);
      }
      statusArea.append(alert);
      return alert;
    };

    paperclipBtn.addEventListener("click", () => {
      if (attachmentCount === 0) {
        recordFileInput.click();
      } else {
        filesPanel.hidden = !filesPanel.hidden;
        paperclipBtn.setAttribute("aria-expanded", filesPanel.hidden ? "false" : "true");
      }
    });

    attachFilesBtn.addEventListener("click", () => {
      recordFileInput.click();
    });

    recordFileInput.addEventListener("change", async () => {
      const files = Array.from(recordFileInput.files ?? []);
      recordFileInput.value = "";
      if (files.length === 0) return;
      attachFilesBtn.disabled = true;
      // Files that went through are shown even when a later one failed, or the user would upload them again.
      let attached = 0;
      const failures: unknown[] = [];
      try {
        for (const file of files) {
          await uploadFile(options.rpc, ref, "thread_attach", file);
          attached++;
        }
      } catch (err) {
        failures.push(err);
      }
      if (attached > 0) {
        try { await pager.loadLatest(ref); } catch (err) { failures.push(err); }
      }
      if (failures.length) {
        showInline(failures[0]);
      } else {
        clearInline();
      }
      attachFilesBtn.disabled = false;
    });

    // One delete at a time; the buttons lock only once the user confirmed, so the keyboard focus stays on the one pressed.
    let deleting = false;
    const lockDeletes = (value: boolean): void => {
      for (const button of filesPanel.querySelectorAll<HTMLButtonElement>(".erp-mail-chatter-file-delete")) button.disabled = value;
      deleteSelectedBtn.disabled = value || selectedIds.size === 0;
    };
    const deleteAttachments = async (ids: number[], message: string, focusIndex: number, afterUnlink?: () => void): Promise<void> => {
      if (deleting) return;
      deleting = true;
      try {
        if (!options.confirm) throw new ClientError("unsupported", "Deleting attachments needs a confirmation dialog from the host");
        if (!(await options.confirm(message))) return;
        lockDeletes(true);
        await options.rpc.call("ir.attachment", "unlink", { ids }, { write: true });
        afterUnlink?.();
        await pager.loadLatest(ref);
        clearInline();
        // The list was rebuilt: focus goes to the next file's delete button, or the paperclip when none is left.
        const remaining = filesPanel.querySelectorAll<HTMLButtonElement>(".erp-mail-chatter-file-delete");
        (remaining[Math.min(focusIndex, remaining.length - 1)] ?? paperclipBtn).focus();
      } catch (err) {
        showInline(err);
      } finally {
        deleting = false;
        lockDeletes(false);
      }
    };

    const renderFilesPanel = (): void => {
      filesList.replaceChildren();
      countBadge.textContent = String(attachmentCount);
      paperclipBtn.setAttribute("aria-label", `Attachments: ${attachmentCount}`);

      const groupMap = new Map<string, AttachmentItem[]>();
      for (const item of recordAttachmentItems) {
        // Only a real checksum merges files; URL attachments have none and never group by a shared name.
        const key = item.checksum ? `sum:${item.checksum}` : `id:${item.id}`;
        const list = groupMap.get(key);
        if (list) {
          list.push(item);
        } else {
          groupMap.set(key, [item]);
        }
      }
      const groups = Array.from(groupMap.values());
      selectFilesBtn.hidden = groups.length <= 1;

      if (selectionMode) {
        filesToolbar.classList.add("is-selection-mode");
        selectionActions.hidden = false;
        selectFilesBtn.hidden = true;
        attachFilesBtn.hidden = true;
        deleteSelectedBtn.textContent = `Delete (${selectedIds.size})`;
        deleteSelectedBtn.disabled = selectedIds.size === 0;
      } else {
        filesToolbar.classList.remove("is-selection-mode");
        selectionActions.hidden = true;
        attachFilesBtn.hidden = false;
      }

      for (const group of groups) {
        const primary = group[0]!;
        const groupEl = el("div", undefined, "erp-mail-chatter-file-group");

        if (selectionMode) {
          const checkbox = el("input", undefined, "erp-mail-chatter-file-select") as HTMLInputElement;
          checkbox.type = "checkbox";
          const isSelected = group.some((f) => selectedIds.has(f.id));
          checkbox.checked = isSelected;
          checkbox.setAttribute("aria-label", `Select ${primary.name}`);
          checkbox.addEventListener("change", () => {
            if (checkbox.checked) {
              for (const f of group) selectedIds.add(f.id);
            } else {
              for (const f of group) selectedIds.delete(f.id);
            }
            deleteSelectedBtn.textContent = `Delete (${selectedIds.size})`;
        deleteSelectedBtn.disabled = selectedIds.size === 0;
          });
          groupEl.append(checkbox);
        }

        // A file-type icon stands in for a file without a preview and for an image that fails to load: never a broken image.
        const fileIcon = (name: "file-text" | "image"): HTMLElement => {
          const holder = el("span", undefined, "erp-mail-chatter-file-icon"); holder.setAttribute("aria-hidden", "true");
          holder.append(icon(name)); return holder;
        };
        if (primary.mimetype?.startsWith("image/")) {
          const img = el("img", undefined, "erp-mail-chatter-file-thumbnail") as HTMLImageElement;
          // Only our own content route: an attachment's stored URL may point anywhere and would be a tracking request.
          img.setAttribute("src", `/erp/content/${primary.id}`);
          img.alt = ""; // decorative: the file name link follows
          img.addEventListener("error", () => img.replaceWith(fileIcon("image")), { once: true });
          groupEl.append(img);
        } else groupEl.append(fileIcon("file-text"));

        const link = el("a", primary.name, "erp-mail-chatter-file-name");
        link.setAttribute("href", `/erp/content/${primary.id}?download=true`);
        link.setAttribute("download", primary.name);
        groupEl.append(link);

        if (primary.fileSize !== undefined) {
          const sizeEl = el("span", ` (${formatBytes(primary.fileSize)})`, "erp-mail-chatter-file-size");
          groupEl.append(sizeEl);
        }

        if (group.length > 1) {
          const dupBadge = el("span", ` (${group.length})`, "erp-mail-chatter-duplicate-count");
          groupEl.append(dupBadge);
        }

        if (!selectionMode) {
          const deleteBtn = el("button", "×", "erp-mail-chatter-file-delete erp-chatter-btn");
          deleteBtn.type = "button";
          deleteBtn.setAttribute("aria-label", `Delete ${primary.name}`);
          deleteBtn.addEventListener("click", () => {
            deleteAttachments(group.map((f) => f.id), `Delete "${primary.name}"?`, groups.indexOf(group)).catch((err) => showInline(err));
          });
          groupEl.append(deleteBtn);
        }

        filesList.append(groupEl);
      }
    };

    selectFilesBtn.addEventListener("click", () => {
      selectionMode = true;
      selectedIds.clear();
      renderFilesPanel();
    });

    discardSelectionBtn.addEventListener("click", () => {
      selectionMode = false;
      selectedIds.clear();
      renderFilesPanel();
    });

    deleteSelectedBtn.addEventListener("click", () => {
      if (selectedIds.size === 0) return;
      const ids = Array.from(selectedIds);
      deleteAttachments(ids, `Delete ${ids.length} selected file${ids.length === 1 ? "" : "s"}?`, 0, () => {
        selectionMode = false;
        selectedIds.clear();
      }).catch((err) => showInline(err));
    });
    const mountComponents = (): void => {
      if (this.disposed || this.composer || this.thread) return;
      const baseOptions: ThreadViewBaseOptions = {
        store,
        ref,
        registries,
        transport,
        pager,
        self: { partnerId: options.selfPartnerId ?? null },
        translate: options.translate ?? defaultTranslate,
        reportError: showInline,
        services: {
          now: () => new Date(),
          locale: "en-US",
          ...(options.timeZone ? { timeZone: options.timeZone } : {}),
          uploadAttachment: async (file: File): Promise<AttachmentUploadResult> => {
            const uploaded = await uploadFile(options.rpc, ref, "thread_upload", file);
            return {
              id: uploaded.id,
              name: uploaded.name,
              size: uploaded.size,
              mimetype: uploaded.mimetype,
            };
          },
        },
        document: doc,
      };

      this.composer = new Composer({ ...baseOptions, platform: "other" });
      this.thread = new Thread({ ...baseOptions, order: "desc" });
      skeleton.remove();
      this.mountComposerChrome(header, composerSlot, this.composer, options.userName, doc);
      bodyArea.append(this.thread.element);
    };

    // Answers of overlapping loads arrive in any order: only the latest request may replace the thread state.
    let latestLoad = 0;
    const pager: ThreadPager = {
      loadLatest: async (threadRef) => {
        const request = ++latestLoad;
        const topics: Array<{ name: string; params?: Record<string, unknown> }> = [];
        if (store.generation === null) {
          topics.push({ name: "schema" });
        }
        topics.push({
          name: "thread",
          params: { model: threadRef.model, id: threadRef.id, messages: true, attachments: true, limit: 30 },
        });

        const res = requireRecord(
          await options.rpc.call("mail.store", "store_fetch", { wire: 1, topics }),
          "store_fetch"
        );
        if (request !== latestLoad) return;
        const topicNamed = topicsOf(res);

        if (store.generation === null) {
          const schemaTopic = topicNamed("schema");
          if (!schemaTopic || schemaTopic["ok"] === false || !schemaTopic["data"]) {
            throw new ClientError("server", "store_fetch answered no schema for the messaging store");
          }
          store.setSchema(typeof schemaTopic["data"] === "string" ? schemaTopic["data"] : JSON.stringify(schemaTopic["data"]));
        }

        // Once schema is loaded, components can safely mount
        if (store.generation !== null && (!this.composer || !this.thread)) {
          mountComponents();
        }

        const threadTopic = topicNamed("thread");
        if (!threadTopic) throw new ClientError("server", "store_fetch answered no thread topic");
        if (threadTopic["ok"] === false) {
          const err = threadTopic["error"] as { message?: string } | undefined;
          throw new ClientError("permission", err?.message ?? "You are not allowed to read this thread: it does not exist or you may not read it.");
        }

        const threadData = requireRecord(threadTopic["data"], "the thread topic");
        const { ids: ascending, hasOlder } = messageIdsOf(threadData["messages"], "the thread");
        const threadFacts = threadData["thread"] as Record<string, unknown> | undefined;
        const attachmentsDoc = threadData["attachments"] as Record<string, unknown> | undefined;

        const result = applyStorePayload(store, storePayloadOf(res["store"], "store_fetch"), () => {
          store.windows.replace(threadRef, ascending, hasOlder);

          if (typeof threadFacts?.["attachment_count"] === "number") {
            attachmentCount = threadFacts["attachment_count"];
          } else if (Array.isArray(attachmentsDoc?.["attachment_ids"])) {
            attachmentCount = (attachmentsDoc["attachment_ids"] as number[]).length;
          } else {
            attachmentCount = 0;
          }

          if (Array.isArray(attachmentsDoc?.["attachment_ids"])) {
            recordAttachmentIds = attachmentsDoc["attachment_ids"] as number[];
          } else {
            recordAttachmentIds = [];
          }

          const items: AttachmentItem[] = [];
          for (const id of recordAttachmentIds) {
            const rec = store.record("ir.attachment", id) as { get(name: string): unknown };
            const name = String(rec.get("name") ?? `File #${id}`);
            const sizeVal = rec.get("file_size");
            const fileSize = typeof sizeVal === "number" ? sizeVal : undefined;
            const mimetype = String(rec.get("mimetype") ?? "");
            const checksum = String(rec.get("checksum") ?? "");
            items.push({ id, name, fileSize, mimetype, checksum });
          }
          recordAttachmentItems = items;

          renderFilesPanel();
        });
        if (result.status !== "applied") {
          throw new ClientError("server", `The thread could not be updated: the store answer was ${result.status}.`);
        }
      },

      loadOlder: async (threadRef, before) => {
        const res = requireRecord(
          await options.rpc.call("mail.store", "store_fetch", {
            wire: 1,
            topics: [
              {
                name: "thread.messages",
                params: { model: threadRef.model, id: threadRef.id, before, limit: 30 },
              },
            ],
          }),
          "store_fetch"
        );

        const messagesTopic = topicsOf(res)("thread.messages");
        if (!messagesTopic) throw new ClientError("server", "store_fetch answered no thread.messages topic");
        if (messagesTopic["ok"] === false) {
          const err = messagesTopic["error"] as { message?: string } | undefined;
          throw new ClientError("permission", err?.message ?? "You are not allowed to read this thread: it does not exist or you may not read it.");
        }

        const { ids: ascending, hasOlder } = messageIdsOf(messagesTopic["data"], "the older messages");
        const result = applyStorePayload(store, storePayloadOf(res["store"], "store_fetch"), () => {
          store.windows.prependOlder(threadRef, ascending, hasOlder);
        });
        if (result.status !== "applied") {
          throw new ClientError("server", `The older messages could not be added: the store answer was ${result.status}.`);
        }
      },
    };

    const transport: ThreadTransport = {
      post: async (threadRef: ThreadRef, postDto: ThreadPostDto): Promise<ThreadPostAnswer> => {
        const response = requireRecord(
          await options.rpc.call(
            threadRef.model,
            "thread_post",
            {
              ids: [threadRef.id],
              post: postDto,
            },
            { write: true }
          ),
          "thread_post"
        );

        const storePayload = storePayloadOf(response["store"], "thread_post");

        const answer = checkPostAnswer({
          message_id: Number(response["message_id"]),
          replayed: Boolean(response["replayed"]),
          store: storePayload,
        });

        // After post succeeds, reload the list (and the form if reloadOnPost is true)
        queueMicrotask(() => {
          pager.loadLatest(threadRef)
            .then(() => clearThreadLoadAlert())
            .catch((err) => { threadLoadAlert = showInline(err); });
          if (options.reloadOnPost && options.reloadForm) {
            options.reloadForm().catch((err) => showInline(err));
          }
        });

        return answer;
      },
    };

    const runInit = (): void => {
      this.activities?.load().catch((err) => showInline(err));
      pager.loadLatest(ref).then(
        () => {
          clearThreadLoadAlert();
          return this.followers?.load();
        },
        (err) => { threadLoadAlert = showInline(err, runInit); },
      );
    };

    this.#ref = ref;
    this.#pager = pager;
    runInit();
  }

  /** Reload the thread and attachments from the server. */
  async refresh(): Promise<void> {
    if (this.disposed || !this.#pager || !this.#ref) return;
    await this.#pager.loadLatest(this.#ref);
    await this.activities?.load();
    this.statusArea?.replaceChildren();
  }

  /**
   * Lays the composer out as the chatter's card: the post-type switch moves up into the header row, the composer
   * collapses to a one-line prompt until it is focused or a type is chosen, and `data-kind` on the chatter says
   * whether an internal note or a message to followers is being written (the note styling hangs on it).
   */
  private mountComposerChrome(header: HTMLElement, composerSlot: HTMLElement, composer: Composer, userName: string | undefined, doc: Document): void {
    const wrap = el("div", undefined, "erp-chatter-composer is-collapsed");
    const name = userName?.trim();
    if (name) {
      wrap.classList.add("has-avatar");
      wrap.append(initialsAvatar(doc, name));
    }
    wrap.append(composer.element);
    composerSlot.prepend(wrap);
    const kinds = composer.element.querySelector<HTMLElement>(".erp-mail-kinds");
    const reply = composer.element.querySelector<HTMLElement>(".erp-mail-reply");
    const field = composer.element.querySelector<HTMLTextAreaElement>("textarea");
    if (kinds) header.prepend(kinds);
    const expand = (on: boolean): void => { wrap.classList.toggle("is-collapsed", !on); };
    wrap.addEventListener("focusin", () => expand(true));
    kinds?.addEventListener("click", () => { expand(true); field?.focus(); });
    const maybeCollapse = (event: FocusEvent): void => {
      const to = event.relatedTarget as Node | null;
      if (to && (wrap.contains(to) || kinds?.contains(to))) return;
      if (!composer.canSend() && reply?.hidden !== false) expand(false);
    };
    wrap.addEventListener("focusout", maybeCollapse);
    kinds?.addEventListener("focusout", maybeCollapse);
    const syncKind = (): void => {
      const note = kinds?.querySelector('[data-kind="note"]')?.getAttribute("aria-pressed") === "true";
      this.element.dataset["kind"] = note ? "note" : "message";
    };
    syncKind();
    if (kinds) {
      const watch = new MutationObserver(syncKind);
      watch.observe(kinds, { attributes: true, attributeFilter: ["aria-pressed"], subtree: true });
      this.stopWatch = () => watch.disconnect();
    }
  }

  /** Dispose all components and event listeners. */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.stopWatch?.();
    this.followers?.dispose();
    this.activities?.dispose();
    this.composer?.dispose();
    this.thread?.dispose();
    this.element.remove();
  }
}
