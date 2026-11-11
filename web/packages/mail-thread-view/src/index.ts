/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The shared thread, message and composer components over the messaging store (5, D5.6;
// ).
export { registerBuiltins, MAX_SUGGESTIONS } from "./builtin";
export { Composer } from "./composer";
export type { ComposerOptions, SendOutcome } from "./composer";
export { checkBaseOptions, createContext } from "./context";
export type { ThreadViewBaseOptions } from "./context";
export {
  ATTACHMENT_MODEL,
  PARTNER_MODEL,
  REACTION_MODEL,
  REQUIRED_MESSAGE_FIELDS,
  assertContract,
  capabilitiesOf,
  declares,
  parseTracking,
  parseWireDatetime,
  partnerName,
  readMessage,
} from "./contract";
export type { AttachmentData, Capabilities, MessageData, ReactionGroup, TrackingChange } from "./contract";
export { OWNING_TASK, ThreadViewError, unavailable } from "./errors";
export type { ThreadViewErrorCode } from "./errors";
export { MAX_BODY_BYTES, MAX_MENTIONS, reconcileMentions, toSpans, utf8Length } from "./mentions";
export type { MentionRange, MentionSpan } from "./mentions";
export { Message, formatter } from "./message";
export type { HiddenAction, MessageOptions } from "./message";
export { formatBytes } from "./dom";
export { performOperation } from "./operations";
export { buildPostDto, discardPending, sendPending, submitPost } from "./posting";
export type { PostInput } from "./posting";
export {
  defineThreadViewRegistries,
  isComposerAction,
  isMessageAction,
  isMessageRenderer,
  isSuggestionProvider,
  threadViewRegistries,
} from "./registries";
export type {
  AttachmentUploadResult,
  ComposerAction,
  ComposerActionContext,
  ComposerState,
  MessageAction,
  MessageActionContext,
  MessageLayout,
  MessageRenderer,
  PartnerSuggestion,
  RendererContext,
  Suggestion,
  SuggestionContext,
  SuggestionProvider,
  ThreadViewContext,
  ThreadViewRegistries,
  ThreadViewServices,
} from "./registries";
export { BODY_MAX_DEPTH, BODY_MAX_LENGTH, BODY_MAX_WALK_DEPTH, BODY_MAX_NODES, MENTION_CLASS, MENTION_ID_ATTRIBUTE, bodyText, bodyTextOf, mentionPartnerId, renderBody, safeHref } from "./sanitize";
export { MAX_PENDING, PENDING_MODEL, THREAD_UI_MODEL, pendingUids, threadKey, threadState, uiModels } from "./state";
export type { ComposerKind, PendingState } from "./state";
export { MAX_TERM_LENGTH, findTrigger } from "./suggestions";
export type { Trigger } from "./suggestions";
export { Thread, anchorDelta } from "./thread";
export type { ThreadOptions } from "./thread";
export { applyStorePayload, checkPostAnswer, requireOperation } from "./transport";
export type {
  MessageOperations,
  OperationAnswer,
  PostSubtype,
  ThreadPager,
  ThreadPostAnswer,
  ThreadPostDto,
  ThreadTransport,
} from "./transport";
