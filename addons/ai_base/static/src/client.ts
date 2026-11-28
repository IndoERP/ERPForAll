/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { registerClientAction, registerCommandProvider, registerFieldWidget, registerSystrayItem } from "@erpforall/client";
import { savedInsight } from "./saved-insight.js";
import { workspace } from "./workspace.js";
import { modelList } from "./model-list.js";
import { assistantChat, assistantCommands } from "./assistant-chat.js";

registerCommandProvider("ai_base.assistant", { provide: assistantCommands });
registerSystrayItem("ai_base.assistant", { render: assistantChat });
registerFieldWidget("ai_base.model_list", { kinds: ["text"], render: modelList });

registerClientAction("ai_base.workspace", { create: workspace });

registerClientAction("ai_base.saved_insight", { create: savedInsight });
