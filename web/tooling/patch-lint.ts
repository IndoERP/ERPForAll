/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Build tooling only: never imported by the browser runtime.
import * as ast from "typescript/unstable/ast";
import { SymbolFlags, SignatureKind, type Project, type Symbol as TSSymbol } from "typescript/unstable/sync";

/** Entries must be supplied in assembler/server load order. */
export interface AddonPatchSource { readonly addon: string; readonly file: string; }
/** Conservative static diagnostic; an unanalysable call is never counted as clean. */
export interface PatchDiagnostic {
  readonly code: "patch-conflict" | "patch-unanalysable";
  readonly addon: string; readonly previousAddon?: string;
  readonly file: string; readonly line: number; readonly member?: string;
  readonly message: string;
}
function unwrap(node: ast.Expression): ast.Expression {
  while (ast.isParenthesizedExpression(node) || ast.isAsExpression(node) || ast.isSatisfiesExpression(node)) node = node.expression;
  return node;
}
function memberName(node: ast.PropertyName): string | undefined {
  if (ast.isIdentifier(node) || ast.isStringLiteral(node) || ast.isNumericLiteral(node)) return node.text;
  if (ast.isComputedPropertyName(node) && ast.isStringLiteral(node.expression)) return node.expression.text;
  return undefined;
}
function registrationOf(node: ast.Node): ast.FunctionDeclaration | undefined {
  let ancestor: ast.Node | undefined = node.parent;
  while (ancestor !== undefined) {
    if (ast.isFunctionDeclaration(ancestor) && ancestor.name?.text === "register") return ancestor;
    ancestor = ancestor.parent;
  }
  return undefined;
}
function walk(root: ast.Node, visit: (node: ast.Node) => void | false): void {
  const pending: Array<{ node: ast.Node; depth: number }> = [{ node: root, depth: 0 }];
  let count = 0;
  while (pending.length > 0) {
    const { node, depth } = pending.pop()!;
    if (depth > 256 || ++count > 100_000) throw new Error("Patch lint AST exceeds 256 levels or 100000 nodes");
    if (visit(node) !== false) {
      const children: ast.Node[] = [];
      node.forEachChild(child => { children.push(child); });
      for (let i = children.length - 1; i >= 0; i--) pending.push({ node: children[i]!, depth: depth + 1 });
    }
  }
}

/** Analyze direct ctx.patch/patchStatic calls in named register functions, using TS's resolved targets. */
export function lintPatchConflicts(project: Project, addons: readonly AddonPatchSource[]): PatchDiagnostic[] {
  const diagnostics: PatchDiagnostic[] = [];
  const previous = new Map<string, string>();
  const checker = project.checker;
  const symbol = (node: ast.Node): TSSymbol | undefined => {
    const found = checker.getSymbolAtLocation(node);
    return found !== undefined && (found.flags & SymbolFlags.Alias) !== 0 ? checker.getAliasedSymbol(found) : found;
  };
  for (const entry of addons) {
    const source = project.program.getSourceFile(entry.file);
    if (source === undefined) throw new Error(`Patch lint source is not in the project: ${entry.file}`);
    if (source.text.length > 1_048_576) throw new Error(`Patch lint source exceeds 1048576 code units: ${entry.file}`);
    if (project.program.getSyntacticDiagnostics(entry.file).length > 0) throw new Error(`Patch lint source has syntax errors: ${entry.file}`);
    const emit = (node: ast.Node, message: string, member?: string, previousAddon?: string): void => {
      diagnostics.push({ code: previousAddon === undefined ? "patch-unanalysable" : "patch-conflict", ...entry,
        line: source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1,
        ...(member === undefined ? {} : { member }), ...(previousAddon === undefined ? {} : { previousAddon }), message });
    };
    // We inspect every patch-looking reference, so aliases/dynamic factories cannot silently disappear.
    walk(source, node => {
      if (ast.isElementAccessExpression(node)) {
        const registration = registrationOf(node);
        const context = registration?.parameters.length === 1 ? symbol(registration.parameters[0]!.name)?.id : undefined;
        if ((context !== undefined && symbol(node.expression)?.id === context) ||
            (ast.isStringLiteral(node.argumentExpression) && (node.argumentExpression.text === "patch" || node.argumentExpression.text === "patchStatic"))) {
          emit(node, "Computed patch capabilities are unanalysable; use direct ctx.patch/patchStatic"); return;
        }
      }
      if (ast.isBindingElement(node)) {
        const key = node.propertyName ?? node.name;
        const name = key !== undefined && (ast.isIdentifier(key) || ast.isStringLiteral(key) || ast.isNumericLiteral(key) || ast.isComputedPropertyName(key))
          ? memberName(key) : undefined;
        // Unknown computed bindings in register may alias a context capability; do not claim them clean.
        const dynamic = key !== undefined && ast.isComputedPropertyName(key) && name === undefined && registrationOf(node) !== undefined;
        if (name === "patch" || name === "patchStatic" || dynamic) {
          emit(node, "Destructured patch capabilities are unanalysable; use direct ctx.patch/patchStatic");
        }
      }
      const name = ast.isPropertyAccessExpression(node) ? node.name.text : undefined;
      if (name !== "patch" && name !== "patchStatic") return;
      if (!ast.isPropertyAccessExpression(node)) return;
      const call = node.parent;
      if (!ast.isCallExpression(call) || call.expression !== node) {
        emit(node, "Patch references must be direct ctx.patch/patchStatic calls; aliases are unanalysable"); return;
      }
      const statement = call.parent;
      const body = statement.parent;
      const registration = body.parent;
      if (!ast.isExpressionStatement(statement) || !ast.isBlock(body) || !ast.isFunctionDeclaration(registration) ||
          registration.name?.text !== "register" || registration.parameters.length !== 1 ||
          !ast.isIdentifier(node.expression) || !ast.isIdentifier(registration.parameters[0]!.name) ||
          symbol(node.expression)?.id !== symbol(registration.parameters[0]!.name)?.id) {
        emit(call, "Patch calls must be direct statements in register(ctx); dynamic execution is unanalysable"); return;
      }
      const targetArg = call.arguments[0]; const factoryArg = call.arguments[1];
      if (call.arguments.length !== 2 || targetArg === undefined || factoryArg === undefined) {
        emit(call, "Patch call requires a static target and inline descriptor factory"); return;
      }
      let target = unwrap(targetArg);
      let instance = name === "patch";
      if (ast.isPropertyAccessExpression(target) && target.name.text === "prototype") {
        const receiver = checker.getTypeAtLocation(target.expression);
        const receiverSymbol = receiver?.getSymbol();
        // Only class constructors normalize to instance identity. A POJO's own "prototype" is an ordinary target.
        if (receiver !== undefined && receiverSymbol !== undefined && (receiverSymbol.flags & SymbolFlags.Class) !== 0 &&
            checker.getSignaturesOfType(receiver, SignatureKind.Construct).length > 0) target = target.expression;
      }
      if (!ast.isIdentifier(target) && !ast.isPropertyAccessExpression(target)) {
        emit(call, "Dynamic patch targets are unanalysable"); return;
      }
      const type = checker.getTypeAtLocation(target);
      const targetSymbol = type?.getSymbol();
      const isClass = targetSymbol !== undefined && (targetSymbol.flags & SymbolFlags.Class) !== 0;
      if (targetSymbol === undefined || (isClass && checker.getSignaturesOfType(type!, SignatureKind.Construct).length === 0) ||
          (!isClass && targetSymbol.name !== "__object")) {
        emit(call, "Patch target identity is unanalysable (use a class or plain object target)"); return;
      }
      if (!isClass) instance = false;
      const factory = unwrap(factoryArg);
      if ((!ast.isArrowFunction(factory) && !ast.isFunctionExpression(factory)) || factory.parameters.length > 1 ||
          (factory.parameters[0] !== undefined && !ast.isIdentifier(factory.parameters[0].name))) {
        emit(call, "Patch factory must be an inline function with an optional named predecessor"); return;
      }
      const result = ast.isBlock(factory.body)
        ? factory.body.statements.length === 1 && ast.isReturnStatement(factory.body.statements[0]!) ? factory.body.statements[0]!.expression : undefined
        : factory.body;
      const object = result === undefined ? undefined : unwrap(result);
      if (object === undefined || !ast.isObjectLiteralExpression(object)) {
        emit(call, "Patch factory must return a statically visible object literal"); return;
      }
      const predecessor = factory.parameters[0] === undefined ? undefined : symbol(factory.parameters[0].name)?.id;
      for (const property of object.properties) {
        const member = ast.isSpreadAssignment(property) ? undefined : memberName(property.name);
        if (member === undefined) {
          emit(property, "Dynamic patch members and spreads are unanalysable"); continue;
        }
        let callsSup = false;
        const root = ast.isPropertyAssignment(property) ? property.initializer : property;
        walk(root, child => {
          // A call inside an uninvoked nested callback is not evidence of delegation.
          if (child !== root && (ast.isArrowFunction(child) || ast.isFunctionExpression(child) || ast.isFunctionDeclaration(child) ||
              ast.isMethodDeclaration(child) || ast.isGetAccessorDeclaration(child) || ast.isSetAccessorDeclaration(child))) return false;
          if (!ast.isCallExpression(child) || predecessor === undefined) return;
          let invoked = unwrap(child.expression);
          if ((ast.isGetAccessorDeclaration(property) || ast.isSetAccessorDeclaration(property)) &&
              ast.isPropertyAccessExpression(invoked) && ast.isIdentifier(invoked.expression) && invoked.expression.text === "Reflect" &&
              invoked.name.text === (ast.isGetAccessorDeclaration(property) ? "get" : "set") &&
              child.arguments[0] !== undefined && symbol(child.arguments[0])?.id === predecessor &&
              child.arguments[1] !== undefined && ast.isStringLiteral(child.arguments[1]) && child.arguments[1].text === member) callsSup = true;
          if (ast.isPropertyAccessExpression(invoked) && (invoked.name.text === "call" || invoked.name.text === "apply")) invoked = invoked.expression;
          const base = ast.isPropertyAccessExpression(invoked) ? invoked.expression : ast.isElementAccessExpression(invoked) ? invoked.expression : undefined;
          const key = ast.isPropertyAccessExpression(invoked) ? invoked.name.text :
            ast.isElementAccessExpression(invoked) && ast.isStringLiteral(invoked.argumentExpression) ? invoked.argumentExpression.text : undefined;
          if (base !== undefined && symbol(base)?.id === predecessor && key === member) callsSup = true;
        });
        const identity = `${targetSymbol.id}:${instance ? "instance" : "static"}:${member}`;
        const earlier = previous.get(identity);
        if (earlier !== undefined && earlier !== entry.addon && !callsSup) {
          emit(property, `Addon "${entry.addon}" replaces "${member}" from "${earlier}" without calling sup`, member, earlier);
        }
        previous.set(identity, entry.addon);
      }
    });
  }
  return diagnostics;
}
