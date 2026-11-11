/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { RegistryHub, type Translate } from "@erpforall/elang";
import { MessagingStore, type ThreadRef } from "@erpforall/mail-store";
import {
  Composer,
  Thread,
  applyStorePayload,
  checkPostAnswer,
  defineThreadViewRegistries,
  formatBytes,
  registerBuiltins,
  type AttachmentUploadResult,
  type ThreadPager,
  type ThreadPostAnswer,
  type ThreadPostDto,
  type ThreadTransport,
  type ThreadViewBaseOptions,
} from "@erpforall/mail-thread-view";
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
}

/** Fills `%s` placeholders in order; the host's translator does this when one is passed, this one only when none is. */
const defaultTranslate: Translate = (source: string, ...values: unknown[]): string => {
  let next = 0;
  return source.replace(/%s/g, () => String(values[next++] ?? ""));
};

/** Global default store cache when host doesn't provide one. */
let defaultStore: MessagingStore | undefined;
function getOrCreateStore(reportError: (err: unknown) => void): MessagingStore {
  if (!defaultStore) {
    defaultStore = new MessagingStore({
      reportError: (err) => reportError(err),
    });
  }
  return defaultStore;
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
  /** The mounted thread component, once schema is loaded. */
  thread?: Thread;
  /** The mounted composer component, once schema is loaded. */
  composer?: Composer;
  /** The followers control of the header (absent on an unsaved record). */
  followers?: Followers;
  #pager?: ThreadPager;
  #ref?: ThreadRef;
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
    const store = options.store ?? getOrCreateStore(reportError);

    const hub = new RegistryHub();
    const registries = defineThreadViewRegistries(hub);
    registerBuiltins(registries);

    let attachmentCount = 0;
    let recordAttachmentIds: number[] = [];
    type AttachmentItem = { id: number; name: string; fileSize?: number; mimetype?: string; checksum?: string; url?: string };
    let recordAttachmentItems: AttachmentItem[] = [];
    let selectionMode = false;
    const selectedIds = new Set<number>();

    const topbarArea = el("div", undefined, "erp-mail-chatter-topbar erp-chatter-topbar");
    const paperclipBtn = el("button", undefined, "erp-mail-chatter-attachment-btn erp-chatter-btn");
    paperclipBtn.type = "button";
    paperclipBtn.setAttribute("aria-label", "Attachments");
    const paperclipIcon = el("span", "📎", "erp-mail-chatter-attachment-icon");
    const countBadge = el("span", "0", "erp-mail-chatter-attachment-count");
    paperclipBtn.append(paperclipIcon, " ", countBadge);
    topbarArea.append(paperclipBtn);

    const recordFileInput = el("input", undefined, "erp-mail-chatter-file-input") as HTMLInputElement;
    recordFileInput.type = "file";
    recordFileInput.multiple = true;
    recordFileInput.style.display = "none";

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
    const bodyArea = el("div", undefined, "erp-mail-chatter-body");
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
    tools.append(topbarArea, this.followers.element);
    header.append(tools);
    this.element.append(header, activitiesSlot, filesPanel, statusArea, bodyArea, recordFileInput);
    options.host.append(this.element);

    // Chatter-local failures stay in the chatter: an inline alert, never the global error dialog.
    const showInline = (err: unknown, retry?: () => void): void => {
      if (this.disposed) return;
      statusArea.replaceChildren();
      skeleton.remove();
      const alert = el("div", undefined, "erp-mail-thread-error erp-mail-chatter-error");
      alert.setAttribute("role", "alert");
      alert.append(el("span", err instanceof Error ? err.message : String(err), "erp-mail-error-message"));
      if (retry) {
        const button = el("button", "Retry", "erp-mail-retry");
        button.type = "button";
        button.addEventListener("click", () => {
          statusArea.replaceChildren();
          retry();
        });
        alert.append(button);
      }
      statusArea.append(alert);
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
      try {
        for (const file of files) {
          await uploadFile(options.rpc, ref, "thread_attach", file);
        }
        await pager.loadLatest(ref);
      } catch (err) {
        showInline(err);
      }
    });

    const renderFilesPanel = (): void => {
      filesList.replaceChildren();
      countBadge.textContent = String(attachmentCount);

      const groupMap = new Map<string, AttachmentItem[]>();
      for (const item of recordAttachmentItems) {
        const key = item.checksum || item.name || String(item.id);
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

        if (primary.mimetype?.startsWith("image/")) {
          const img = el("img", undefined, "erp-mail-chatter-file-thumbnail") as HTMLImageElement;
          img.setAttribute("src", primary.url || `/web/content/${primary.id}`);
          img.alt = primary.name;
          groupEl.append(img);
        }

        const link = el("a", primary.name, "erp-mail-chatter-file-name");
        link.setAttribute("href", `/web/content/${primary.id}?download=true`);
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
          deleteBtn.addEventListener("click", async () => {
            try {
              await options.rpc.call(
                "ir.attachment",
                "unlink",
                { ids: group.map((f) => f.id) },
                { write: true }
              );
              await pager.loadLatest(ref);
            } catch (err) {
              showInline(err);
            }
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

    deleteSelectedBtn.addEventListener("click", async () => {
      if (selectedIds.size === 0) return;
      try {
        const ids = Array.from(selectedIds);
        selectionMode = false;
        selectedIds.clear();
        await options.rpc.call("ir.attachment", "unlink", { ids }, { write: true });
        await pager.loadLatest(ref);
      } catch (err) {
        showInline(err);
      }
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
      this.mountComposerChrome(header, bodyArea, this.composer);
      bodyArea.append(this.thread.element);
    };

    const pager: ThreadPager = {
      loadLatest: async (threadRef) => {
        const topics: Array<{ name: string; params?: Record<string, unknown> }> = [];
        if (store.generation === null) {
          topics.push({ name: "schema" });
        }
        topics.push({
          name: "thread",
          params: { model: threadRef.model, id: threadRef.id, messages: true, attachments: true, limit: 30 },
        });

        const res = (await options.rpc.call("mail.store", "store_fetch", {
          wire: 1,
          topics,
        })) as Record<string, unknown>;

        if (!res || typeof res !== "object") {
          throw new ClientError("server", "store_fetch returned an invalid response");
        }

        const returnedTopics = Array.isArray(res["topics"]) ? (res["topics"] as Array<Record<string, unknown>>) : [];
        if (store.generation === null) {
          const schemaTopic = returnedTopics.find((t) => t && t["topic"] === "schema");
          if (schemaTopic && schemaTopic["data"]) {
            const schemaDoc =
              typeof schemaTopic["data"] === "string" ? schemaTopic["data"] : JSON.stringify(schemaTopic["data"]);
            store.setSchema(schemaDoc);
          }
        }

        // Once schema is loaded, components can safely mount
        if (store.generation !== null && (!this.composer || !this.thread)) {
          mountComponents();
        }

        const threadTopic = returnedTopics.find((t) => t && t["topic"] === "thread");
        if (threadTopic && threadTopic["ok"] === false) {
          const err = threadTopic["error"] as { message?: string } | undefined;
          throw new ClientError("permission", err?.message ?? "You are not allowed to read this thread: it does not exist or you may not read it.");
        }

        const threadData = threadTopic?.["data"] as Record<string, unknown> | undefined;
        const messagesData = threadData?.["messages"] as Record<string, unknown> | undefined;
        const threadFacts = threadData?.["thread"] as Record<string, unknown> | undefined;
        const attachmentsDoc = threadData?.["attachments"] as Record<string, unknown> | undefined;

        const rawStore = res["store"];
        const storePayload =
          typeof rawStore === "string" || rawStore instanceof Uint8Array
            ? rawStore
            : JSON.stringify(rawStore ?? {});

        applyStorePayload(store, storePayload, () => {
          const rawIds: number[] = Array.isArray(messagesData?.["message_ids"])
            ? (messagesData["message_ids"] as number[])
            : [];
          const ascending = [...rawIds].sort((a, b) => a - b);
          const hasOlder = Boolean(messagesData?.["has_more_before"]);
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
            const urlVal = rec.get("url");
            const url = typeof urlVal === "string" && urlVal.length > 0 ? urlVal : undefined;
            items.push({ id, name, fileSize, mimetype, checksum, url });
          }
          recordAttachmentItems = items;

          renderFilesPanel();
        });
      },

      loadOlder: async (threadRef, before) => {
        const res = (await options.rpc.call("mail.store", "store_fetch", {
          wire: 1,
          topics: [
            {
              name: "thread.messages",
              params: { model: threadRef.model, id: threadRef.id, before, limit: 30 },
            },
          ],
        })) as Record<string, unknown>;

        if (!res || typeof res !== "object") {
          throw new ClientError("server", "store_fetch returned an invalid response");
        }

        const returnedTopics = Array.isArray(res["topics"]) ? (res["topics"] as Array<Record<string, unknown>>) : [];
        const messagesTopic = returnedTopics.find((t) => t && t["topic"] === "thread.messages");
        if (messagesTopic && messagesTopic["ok"] === false) {
          const err = messagesTopic["error"] as { message?: string } | undefined;
          throw new ClientError("permission", err?.message ?? "You are not allowed to read this thread: it does not exist or you may not read it.");
        }

        const messagesData = messagesTopic?.["data"] as Record<string, unknown> | undefined;
        const rawStore = res["store"];
        const storePayload =
          typeof rawStore === "string" || rawStore instanceof Uint8Array
            ? rawStore
            : JSON.stringify(rawStore ?? {});

        applyStorePayload(store, storePayload, () => {
          const rawIds: number[] = Array.isArray(messagesData?.["message_ids"])
            ? (messagesData["message_ids"] as number[])
            : [];
          const ascending = [...rawIds].sort((a, b) => a - b);
          const hasOlder = Boolean(messagesData?.["has_more_before"]);
          store.windows.prependOlder(threadRef, ascending, hasOlder);
        });
      },
    };

    const transport: ThreadTransport = {
      post: async (threadRef: ThreadRef, postDto: ThreadPostDto): Promise<ThreadPostAnswer> => {
        const response = (await options.rpc.call(
          threadRef.model,
          "thread_post",
          {
            ids: [threadRef.id],
            post: postDto,
          },
          { write: true }
        )) as Record<string, unknown>;

        if (!response || typeof response !== "object") {
          throw new ClientError("server", "thread_post returned an invalid response");
        }

        const rawStore = response["store"];
        const storePayload =
          typeof rawStore === "string" || rawStore instanceof Uint8Array
            ? rawStore
            : JSON.stringify(rawStore ?? {});

        const answer = checkPostAnswer({
          message_id: Number(response["message_id"]),
          replayed: Boolean(response["replayed"]),
          store: storePayload,
        });

        // After post succeeds, reload the list (and the form if reloadOnPost is true)
        queueMicrotask(() => {
          pager.loadLatest(threadRef).catch((err) => showInline(err));
          if (options.reloadOnPost && options.reloadForm) {
            options.reloadForm().catch((err) => showInline(err));
          }
        });

        return answer;
      },
    };

    const runInit = (): void => {
      pager.loadLatest(ref).then(() => this.followers?.load(), (err) => showInline(err, runInit));
    };

    this.#ref = ref;
    this.#pager = pager;
    runInit();
  }

  /** Reload the thread and attachments from the server. */
  async refresh(): Promise<void> {
    if (this.disposed || !this.#pager || !this.#ref) return;
    await this.#pager.loadLatest(this.#ref);
  }

  /**
   * Lays the composer out as the chatter's card: the post-type switch moves up into the header row, the composer
   * collapses to a one-line prompt until it is focused or a type is chosen, and `data-kind` on the chatter says
   * whether an internal note or a message to followers is being written (the note styling hangs on it).
   */
  private mountComposerChrome(header: HTMLElement, bodyArea: HTMLElement, composer: Composer): void {
    const wrap = el("div", undefined, "erp-chatter-composer is-collapsed");
    wrap.append(composer.element);
    bodyArea.prepend(wrap);
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
    this.composer?.dispose();
    this.thread?.dispose();
    this.element.remove();
  }
}
