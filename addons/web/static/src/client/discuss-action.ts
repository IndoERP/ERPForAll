/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { registerClientAction } from "./registry.js";
import { ClientError } from "./transport.js";

export const DISCUSS_ACTION = "mail.discuss";

registerClientAction(DISCUSS_ACTION, {
  async create(environment) {
    if (!environment.mountDiscuss) throw new ClientError("unsupported", "The Discuss workspace is unavailable in this session.");
    return { dispose: environment.mountDiscuss(environment.host) };
  },
});
