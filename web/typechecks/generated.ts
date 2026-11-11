/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { WireDocument } from "../src/generated/WireDocument";

// Compile a real consumer with skipLibCheck disabled: declaration syntax and
// referenced declaration imports are checked independently of the application.
export const scalar: WireDocument = {
  schema: 1,
  nodes: [{ kind: "literal", value: { kind: "int", value: "9223372036854775807" } }],
};
