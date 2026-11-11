/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  AuthoringError,
  checkAuthoredOutput,
  checkAuthoredSource,
  type ConditionLiteral,
  resolveTreeLimits,
  type TreeLimits,
  validateConditionLiteral,
} from "./condition-tree.js";

/** Bounded lexical reader shared by the two authoring codecs; never executes source. */
export class AuthoringReader {
  offset = 0;
  private items = 0;
  readonly limits: Required<TreeLimits>;
  constructor(
    readonly source: string,
    limits?: TreeLimits,
    private readonly domainEnvelope = false,
  ) {
    this.limits = resolveTreeLimits(limits);
    checkAuthoredSource(source, limits);
  }
  skip(): void {
    while (this.offset < this.source.length) {
      const character = this.source[this.offset]!;
      if (/[ \t\r\n]/.test(character)) {
        this.offset++;
        continue;
      }
      if (/\s/.test(character)) {
        const code = character.charCodeAt(0).toString(16).toUpperCase().padStart(
          4,
          "0",
        );
        this.error(`Invalid whitespace U+${code} in authored source.`);
      }
      break;
    }
  }
  peek(): string {
    this.skip();
    return this.source[this.offset] ?? "";
  }
  consume(text: string): boolean {
    this.skip();
    if (!this.source.startsWith(text, this.offset)) return false;
    this.offset += text.length;
    return true;
  }
  word(text: string): boolean {
    this.skip();
    if (
      !this.source.startsWith(text, this.offset) ||
      /[a-zA-Z0-9_]/.test(this.source[this.offset + text.length] ?? "")
    ) return false;
    this.offset += text.length;
    return true;
  }
  error(
    message: string,
    kind: "validation" | "unsupported" = "validation",
  ): never {
    throw new AuthoringError(message, this.offset, false, kind);
  }
  depth(depth: number): void {
    if (depth > this.limits.depth + (this.domainEnvelope ? 1 : 0)) {
      throw new AuthoringError(
        `Condition tree exceeds ${this.limits.depth} levels.`,
        this.offset,
        true,
      );
    }
  }
  charge(): void {
    if (
      ++this.items >
        (this.domainEnvelope ? 5 * this.limits.nodes + 1 : this.limits.nodes)
    ) {
      throw new AuthoringError(
        `Condition tree exceeds ${this.limits.nodes} nodes.`,
        this.offset,
        true,
      );
    }
  }
  end(message: string): void {
    if (this.peek() !== "") this.error(message);
  }
  literal(depth = 1): ConditionLiteral {
    this.depth(depth);
    const start = this.offset;
    const character = this.peek();
    if (character === "'" || character === '"') {
      this.charge();
      return { kind: "string", value: this.string() };
    }
    if (this.word("True")) {
      this.charge();
      return { kind: "boolean", value: true };
    }
    if (this.word("False")) {
      this.charge();
      return { kind: "boolean", value: false };
    }
    if (this.word("None")) {
      this.charge();
      return { kind: "none" };
    }
    if (character === "[" || character === "(") {
      if (character === "[") this.charge();
      this.offset++;
      const closing = character === "[" ? "]" : ")";
      const items: ConditionLiteral[] = [];
      if (this.consume(closing)) {
        if (character === "(") this.charge();
        return { kind: character === "[" ? "list" : "tuple", items };
      }
      while (true) {
        items.push(this.literal(depth + 1));
        if (this.consume(closing)) break;
        if (!this.consume(",")) {
          this.error(`Expected comma or ${closing} in literal collection.`);
        }
        if (this.consume(closing)) break;
      }
      if (
        character === "(" && items.length === 1 &&
        !this.source.slice(start, this.offset).match(/,\s*\)$/)
      ) return items[0]!;
      if (character === "(") this.charge();
      return { kind: character === "[" ? "list" : "tuple", items };
    }
    if (/[+\-0-9.]/.test(character) && character !== "") {
      this.charge();
      const match =
        /^[+-]?(?:(?:0|[1-9][0-9]*)(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?/
          .exec(this.source.slice(this.offset));
      if (!match) this.error("Malformed numeric literal.");
      const token = match[0];
      this.offset += token.length;
      if (/[a-zA-Z0-9_.]/.test(this.source[this.offset] ?? "")) {
        this.error("Malformed numeric literal.");
      }
      if (/[.eE]/.test(token)) {
        const value = Number(token);
        if (!Number.isFinite(value)) this.error("Float must be finite.");
        return { kind: "float", value };
      }
      if (token.length > 21) {
        this.error("Integer is outside the signed 64-bit range.");
      }
      const value = BigInt(token);
      if (value < -9223372036854775808n || value > 9223372036854775807n) {
        this.error("Integer is outside the signed 64-bit range.");
      }
      return { kind: "integer", value: value.toString() };
    }
    if (
      character === "" || character === "]" || character === ")" ||
      character === ","
    ) this.error("Expected a literal.");
    this.error(
      "Dynamic operands are outside the authoring grammar.",
      "unsupported",
    );
  }
  private string(): string {
    const quote = this.source[this.offset++]!;
    let result = "";
    while (this.offset < this.source.length) {
      const character = this.source[this.offset++]!;
      if (character === quote) return result;
      if (character === "\n" || character === "\r") {
        this.error("Unescaped newline in string literal.");
      }
      if (character !== "\\") {
        result += character;
        continue;
      }
      const escape = this.source[this.offset++];
      switch (escape) {
        case "\\":
        case "'":
        case '"':
          result += escape;
          break;
        case "n":
          result += "\n";
          break;
        case "r":
          result += "\r";
          break;
        case "t":
          result += "\t";
          break;
        case "b":
          result += "\b";
          break;
        case "f":
          result += "\f";
          break;
        case "x":
        case "u":
        case "U": {
          const width = escape === "x" ? 2 : escape === "u" ? 4 : 8;
          const hex = this.source.slice(this.offset, this.offset + width);
          if (hex.length !== width || !/^[0-9a-fA-F]+$/.test(hex)) {
            this.error("Malformed Unicode string escape.");
          }
          const point = Number.parseInt(hex, 16);
          if (point > 0x10ffff) {
            this.error("Unicode string escape is out of range.");
          }
          result += String.fromCodePoint(point);
          this.offset += width;
          break;
        }
        case undefined:
          this.error("Unterminated string literal.");
        default:
          this.error(`Unsupported string escape \\${escape}.`);
      }
    }
    this.error("Unterminated string literal.");
  }
}
/** Strict standalone literal conversion for value controls; refuses rather than defaulting. */
export function parseConditionLiteral(
  source: string,
  limits?: TreeLimits,
): ConditionLiteral {
  const reader = new AuthoringReader(source, limits);
  try {
    const value = reader.literal();
    reader.end("Unexpected text after the literal.");
    validateConditionLiteral(value, limits);
    return value;
  } catch (error) {
    if (error instanceof AuthoringError && error.kind === "unsupported") {
      throw new AuthoringError(error.message, error.offset);
    }
    throw error;
  }
}
/** Incremental UTF-8 output admission avoids building an oversized serialization. */
export class AuthoringWriter {
  private readonly chunks: string[] = [];
  private bytes = 0;
  constructor(private readonly limits?: TreeLimits) {
    resolveTreeLimits(limits);
  }
  push(value: string): void {
    const maximum = resolveTreeLimits(this.limits).outputBytes;
    if (value.length > maximum - this.bytes) {
      throw new AuthoringError(
        `Authored output exceeds ${maximum} UTF-8 bytes.`,
        0,
        true,
      );
    }
    this.bytes += new TextEncoder().encode(value).length;
    if (this.bytes > maximum) {
      throw new AuthoringError(
        `Authored output exceeds ${maximum} UTF-8 bytes.`,
        0,
        true,
      );
    }
    this.chunks.push(value);
  }
  finish(): string {
    return this.chunks.join("");
  }
}
/** Write a previously admitted literal, retaining all semantic kind distinctions. */
export function writeConditionLiteral(
  value: ConditionLiteral,
  writer: AuthoringWriter,
): void {
  switch (value.kind) {
    case "none":
      writer.push("None");
      break;
    case "boolean":
      writer.push(value.value ? "True" : "False");
      break;
    case "integer":
      writer.push(value.value);
      break;
    case "float": {
      const text = Object.is(value.value, -0) ? "-0" : String(value.value);
      writer.push(/[.eE]/.test(text) ? text : `${text}.0`);
      break;
    }
    case "string": {
      writer.push("'");
      for (const character of value.value) {
        const escaped = character === "'"
          ? "\\'"
          : character === "\\"
          ? "\\\\"
          : character === "\n"
          ? "\\n"
          : character === "\r"
          ? "\\r"
          : character === "\t"
          ? "\\t"
          : character === "\b"
          ? "\\b"
          : character === "\f"
          ? "\\f"
          : character.charCodeAt(0) < 32 ||
              (character.length === 1 && character.charCodeAt(0) >= 0xd800 &&
                character.charCodeAt(0) <= 0xdfff)
          ? `\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`
          : character;
        writer.push(escaped);
      }
      writer.push("'");
      break;
    }
    case "list":
    case "tuple":
      writer.push(value.kind === "list" ? "[" : "(");
      value.items.forEach((item, index) => {
        if (index > 0) writer.push(", ");
        writeConditionLiteral(item, writer);
      });
      if (value.kind === "tuple" && value.items.length === 1) writer.push(",");
      writer.push(value.kind === "list" ? "]" : ")");
      break;
  }
}
/** Serialize a strictly admitted literal without losing large integer precision. */
export function serializeConditionLiteral(
  value: ConditionLiteral,
  limits?: TreeLimits,
): string {
  validateConditionLiteral(value, limits);
  const writer = new AuthoringWriter(limits);
  writeConditionLiteral(value, writer);
  const source = writer.finish();
  checkAuthoredOutput(source, limits);
  return source;
}
