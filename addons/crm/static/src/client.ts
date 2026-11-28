/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { registerClientAction } from "@erpforall/client";
import { overview } from "./overview.js";

registerClientAction("crm.overview", { create: overview });
