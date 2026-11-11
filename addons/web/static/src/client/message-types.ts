/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Addon-facing message contracts. These handles own browser UI only, never access authority. */
export type { NotificationLevel, NotificationOptions } from './notifications.js';
export type { DialogHandle, DialogOptions, ConfirmationOptions, ConfirmationHandle } from './dialog-service.js';
export type { OnchangeWarning } from './record.js';
