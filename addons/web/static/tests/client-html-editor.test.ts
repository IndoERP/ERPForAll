/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createHtmlEditor } from "../src/client/html-editor.js";
import { captureSelection, editableRange, restoreSelection, toggleBold } from "../src/client/html-editor-selection.js";
import { MessageServices } from "../src/client/messages.js";
import { ClientError } from "../src/client/transport.js";
import { markHtmlEditorTransient } from "../src/client/html-editor-document.js";

const disposers: (() => void)[] = [];
afterEach(() => { for (const dispose of disposers.splice(0)) dispose(); document.getSelection()?.removeAllRanges(); document.body.replaceChildren(); });
function setup(value = "<p>Hello world</p>", extra: Partial<Parameters<typeof createHtmlEditor>[0]> = {}) {
  const report = vi.fn(), change = vi.fn(), messages = new MessageServices(document.body, vi.fn());
  const editor = createHtmlEditor({environment: {messages, report}, label: "Notes", value, change, ...extra});
  document.body.append(editor.node); disposers.push(() => editor.dispose(), () => messages.dispose());
  return {editor, report, change};
}
function select(root: HTMLElement, start: Node, from: number, end: Node = start, to = from): void {
  root.focus(); const selection = document.getSelection()!; selection.setBaseAndExtent(start, from, end, to); document.dispatchEvent(new Event("selectionchange"));
}
function edit(root: HTMLElement, text: string): void { root.textContent = text; select(root, root.firstChild!, text.length); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: text})); }
function command(root: HTMLElement, name: string): HTMLButtonElement { return root.querySelector<HTMLButtonElement>(`[data-html-command="${name}"]`)!; }
function paste(root: HTMLElement, values: Record<string, string>, files: File[] = []): Event {
  const event = new Event("paste", {bubbles: true, cancelable: true});
  Object.defineProperty(event, "clipboardData", {value: {types: Object.keys(values), files, getData: (key: string) => values[key] ?? ""}});
  root.dispatchEvent(event); return event;
}
function failure(job: () => unknown, message: string, kind: ClientError["kind"] = "validation"): void {
  let error: unknown; try { job(); } catch (value) { error = value; }
  expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({kind, message});
}

// Cohesive component admission journeys. Native Shift+Arrow selection, rendered contextual
// visibility and coarse-pointer presentation are supplemental root-run browser evidence:
// These jsdom events exercise command routing; they do not emulate browser arrow-key defaults.
/// (partial: jsdom command routing with synthetic events; native selection, rendering and pointer proof is the Chromium driver's)
test("HTML editing journey inserts text and formats only the active pointer or keyboard selection", () => {
  const {editor,change}=setup(),root=editor.editable,text=root.firstChild!.firstChild! as Text;
  select(root,text,text.length);root.dispatchEvent(new InputEvent("beforeinput",{bubbles:true,cancelable:true,inputType:"insertText",data:"!"}));
  text.appendData("!");select(root,text,text.length);root.dispatchEvent(new InputEvent("input",{bubbles:true,inputType:"insertText",data:"!"}));
  expect(editor.exportHtml()).toBe("<p>Hello world!</p>");expect(change).toHaveBeenLastCalledWith("<p>Hello world!</p>");
  const bold=command(editor.node,"bold");select(root,text,0,text,5);expect(bold.disabled).toBe(false);expect(bold.dataset["selection"]).toBe("true");
  bold.dispatchEvent(new MouseEvent("pointerdown",{bubbles:true,cancelable:true}));bold.click();expect(editor.exportHtml()).toBe("<p><strong>Hello</strong> world!</p>");expect(document.getSelection()?.toString()).toBe("Hello");
  const selected=root.querySelector("strong")!.firstChild!;select(root,selected,1,selected,4);bold.dispatchEvent(new MouseEvent("pointerdown",{bubbles:true,cancelable:true}));bold.click();
  expect(editor.exportHtml()).toBe("<p><strong>H</strong>ell<strong>o</strong> world!</p>");expect(root.textContent).toBe("Hello world!");expect(document.getSelection()?.toString()).toBe("ell");expect(change).toHaveBeenCalledTimes(3);
  const keyboard=setup().editor,keyboardText=keyboard.editable.firstChild!.firstChild!,keyboardBold=command(keyboard.node,"bold");
  select(keyboard.editable,keyboardText,0);expect(keyboardBold.disabled).toBe(true);expect(keyboardBold.dataset["selection"]).toBe("false");
  document.getSelection()!.setBaseAndExtent(keyboardText,0,keyboardText,2);keyboard.editable.dispatchEvent(new KeyboardEvent("keydown",{key:"b",ctrlKey:true,bubbles:true,cancelable:true}));
  expect(keyboard.exportHtml()).toBe("<p><strong>He</strong>llo world</p>");expect(document.getSelection()?.toString()).toBe("He");expect(keyboardBold.dataset["selection"]).toBe("true");expect(keyboardBold.getAttribute("aria-pressed")).toBe("true");
  const outside=document.createElement("button");document.body.append(outside);outside.focus();expect(keyboardBold.disabled).toBe(true);expect(keyboardBold.dataset["selection"]).toBe("false");
});

/// (partial: synthetic clipboard routing; authoritative HTML-field sanitization is not exercised while Html fields are refused)
test("HTML clipboard journey routes each format through cleanup and preserves editable ownership", () => {
  const {editor,change}=setup("<p>Replace</p>"),root=editor.editable;
  select(root,root.firstChild!.firstChild!,0,root.firstChild!.firstChild!,7);paste(root,{"text/plain":"Plain <text>"});
  expect(editor.exportHtml()).toBe("<p>Plain &lt;text&gt;</p>");expect(change).toHaveBeenLastCalledWith("<p>Plain &lt;text&gt;</p>");editor.undo();expect(editor.exportHtml()).toBe("<p>Replace</p>");editor.redo();expect(editor.exportHtml()).toBe("<p>Plain &lt;text&gt;</p>");
  editor.setValue("<p>Start end</p>");select(root,root.firstChild!.firstChild!,0,root.firstChild!.firstChild!,5);const externalCalls=change.mock.calls.length;
  paste(root,{"text/html":"<span style='color:red'><b>Earth</b></span><script>unsafe()</script>","text/plain":"wrong"});
  expect(editor.exportHtml()).toBe("<p><strong>Earth</strong> end</p>");expect(root.querySelector("span,script,[style]")).toBeNull();expect(editor.node.querySelector(".html-editor-status")?.textContent).toBe("Pasted formatting was simplified.");expect(change).toHaveBeenCalledTimes(externalCalls+1);
  editor.undo();expect(editor.exportHtml()).toBe("<p>Start end</p>");expect(document.getSelection()?.toString()).toBe("Start");editor.redo();expect(editor.exportHtml()).toBe("<p><strong>Earth</strong> end</p>");
  editor.setValue("");select(root,root,0);paste(root,{"application/vnd.erpforall.html-editor+json":JSON.stringify({version:1,html:"<i>Own</i>"}),"text/html":"<b>wrong</b>"});
  expect(editor.exportHtml()).toBe("<em>Own</em>");expect(typeof editor.exportHtml()).toBe("string");editor.undo();expect(editor.exportHtml()).toBe("");editor.redo();expect(editor.exportHtml()).toBe("<em>Own</em>");
  const plain=setup("<p>Replace</p>",{clipboardMode:"plain-text"}).editor;select(plain.editable,plain.editable.firstChild!.firstChild!,0,plain.editable.firstChild!.firstChild!,7);paste(plain.editable,{"text/html":"<strong>rich</strong>","text/plain":"<b>literal</b>\nline"});expect(plain.exportHtml()).toBe("<p>&lt;b&gt;literal&lt;/b&gt;<br>line</p>");
  const protectedOwner=setup("<p>Left</p><p>Locked</p><p>Right</p>"),protectedRoot=protectedOwner.editor.editable,island=protectedRoot.children[1]!;markHtmlEditorTransient(island);island.setAttribute("contenteditable","false");
  select(protectedRoot,island.firstChild!,0,island.firstChild!,2);paste(protectedRoot,{"text/plain":"blocked"});
  select(protectedRoot,protectedRoot.firstChild!.firstChild!,1,protectedRoot.lastChild!.firstChild!,2);paste(protectedRoot,{"text/html":"<p>blocked</p>"});expect(protectedOwner.editor.exportHtml()).toBe("<p>Left</p><p>Locked</p><p>Right</p>");expect(protectedOwner.change).not.toHaveBeenCalled();expect(protectedOwner.editor.state().canUndo).toBe(false);
  const readonly=setup("<p>Readonly</p>",{readonly:true});paste(readonly.editor.editable,{"text/plain":"blocked"});expect(readonly.editor.exportHtml()).toBe("<p>Readonly</p>");expect(readonly.change).not.toHaveBeenCalled();
  const detached=setup("<p>Detached</p>");select(detached.editor.editable,detached.editor.editable.firstChild!.firstChild!,0);detached.editor.node.remove();paste(detached.editor.editable,{"text/plain":"blocked"});expect(detached.editor.exportHtml()).toBe("<p>Detached</p>");expect(detached.change).not.toHaveBeenCalled();expect(detached.editor.state().canUndo).toBe(false);
  // All results above are ordinary local draft strings. No stored-field sanitizer or RPC
  // acceptance is exercised or certified by this reusable clipboard journey.
});

/// (partial: jsdom history journey; native history and browser proof separate, redo of unstable content is refused rather than replayed)
test("HTML history journey shares touch controls and platform shortcuts with one connected history", () => {
  const {editor,change}=setup("<p>Baseline</p>"),root=editor.editable,undo=command(editor.node,"undo"),redo=command(editor.node,"redo");
  expect(editor.state()).toMatchObject({canUndo:false,canRedo:false});expect(undo.disabled).toBe(true);expect(redo.disabled).toBe(true);undo.click();redo.click();editor.undo();expect(editor.exportHtml()).toBe("<p>Baseline</p>");expect(change).not.toHaveBeenCalled();
  edit(root,"first");edit(root,"second");expect(undo.disabled).toBe(false);expect(redo.disabled).toBe(true);
  // These native buttons are also the coarse-pointer/touch history controls; their rendered
  // phone reachability is admitted by the supplemental compiled-browser capture, not jsdom.
  undo.click();expect(editor.exportHtml()).toBe("first");expect(editor.state()).toMatchObject({canUndo:true,canRedo:true});expect(redo.disabled).toBe(false);
  redo.click();expect(editor.exportHtml()).toBe("second");expect(redo.disabled).toBe(true);undo.click();undo.click();expect(editor.exportHtml()).toBe("<p>Baseline</p>");expect(undo.disabled).toBe(true);expect(editor.state()).toMatchObject({canUndo:false,canRedo:true});
  for(const modifier of ["ctrlKey","metaKey"]){
    const platform=setup("<p>Initial</p>").editor,editable=platform.editable;edit(editable,"one");edit(editable,"two");
    const key=(key:string,shiftKey=false):void=>{editable.dispatchEvent(new KeyboardEvent("keydown",{key,[modifier]:true,shiftKey,bubbles:true,cancelable:true}));};
    key("z");expect(platform.exportHtml()).toBe("one");key("Z",true);expect(platform.exportHtml()).toBe("two");
    if(modifier==="ctrlKey"){key("z");key("y");expect(platform.exportHtml()).toBe("two");}
    key("z");edit(editable,"branch");expect(platform.state().canRedo).toBe(false);expect(command(platform.node,"redo").disabled).toBe(true);platform.redo();expect(platform.exportHtml()).toBe("branch");
  }
});

/// (partial: reusable DOM editing, accessibility and owner replacement; compiled browser proof separate)
test("HTML editor labels its draft, exports strings and replaces the baseline without a change callback", () => {
  const {editor, change} = setup(); expect(editor.editable.getAttribute("role")).toBe("textbox");
  expect(editor.editable.getAttribute("aria-label")).toBe("Notes"); expect(editor.editable.getAttribute("aria-multiline")).toBe("true");
  expect(editor.exportHtml()).toBe("<p>Hello world</p>"); expect(editor.state()).toEqual({canUndo:false, canRedo:false, pending:false, dirty:false});
  edit(editor.editable, "Changed & <safe>"); expect(editor.exportHtml()).toBe("Changed &amp; &lt;safe&gt;"); expect(change).toHaveBeenLastCalledWith("Changed &amp; &lt;safe&gt;");
  expect(editor.state()).toMatchObject({canUndo:true, dirty:true}); editor.setValue("<p>Owner</p>");
  expect(editor.state()).toEqual({canUndo:false, canRedo:false, pending:false, dirty:false}); expect(change).toHaveBeenCalledTimes(1);
  expect(editor.exportHtml()).toBe("<p>Owner</p>");
});

/// (partial: selected formatting, partial unbold and native selection bookmarks in jsdom)
test("bold splits selected runs, partial unbold preserves surrounding content and restores selection", () => {
  const {editor} = setup(); const root = editor.editable, text = root.querySelector("p")!.firstChild!;
  select(root, text, 1, text, 4); expect(toggleBold(root)).toBe(true); expect(editor.exportHtml()).toBe("<p>H<strong>ell</strong>o world</p>");
  expect(document.getSelection()?.toString()).toBe("ell"); const bold = root.querySelector("strong")!.firstChild!;
  select(root, bold, 1, bold, 2); expect(toggleBold(root)).toBe(true); expect(editor.exportHtml()).toBe("<p>H<strong>e</strong>l<strong>l</strong>o world</p>");
  expect(document.getSelection()?.toString()).toBe("l"); const bookmark = captureSelection(root)!; document.getSelection()?.removeAllRanges(); restoreSelection(root, bookmark); expect(document.getSelection()?.toString()).toBe("l");
});

test("bold handles backwards cross-block nested selections without moving unselected text", () => {
  const {editor} = setup("<p>A<em>BC</em>D</p><p>EF</p>"), root = editor.editable;
  const first = root.querySelector("em")!.firstChild!, last = root.lastChild!.firstChild!;
  select(root, last, 1, first, 1); expect(toggleBold(root)).toBe(true);
  expect(editor.exportHtml()).toBe("<p>A<em>B<strong>C</strong></em><strong>D</strong></p><p><strong>E</strong>F</p>");
  expect(document.getSelection()?.toString()).toBe("CDE"); expect(document.getSelection()?.anchorNode?.textContent).toBe("E");
});

test("unbold removes every selected nested run after ancestor splits", () => {
  const {editor}=setup("<p><strong>A<em>BC</em>D</strong>E</p>"),root=editor.editable,strong=root.querySelector("strong")!;
  select(root,strong.firstChild!,0,strong.lastChild!,1);command(editor.node,"bold").click();expect(editor.exportHtml()).toBe("<p>A<em>BC</em>DE</p>");expect(document.getSelection()?.toString()).toBe("ABCD");
  editor.undo();expect(editor.exportHtml()).toBe("<p><strong>A<em>BC</em>D</strong>E</p>");
});

test("partial unbold preserves untouched protected islands through nested ancestor splits", () => {
  for(const value of ["<p><strong>AB<em>LOCK</em>CD</strong></p>","<p><strong><u>AB<em>LOCK</em>CD</u></strong></p>"]){
    const {editor,change,report}=setup(value),root=editor.editable,island=root.querySelector("em")!,lockedText=island.firstChild!;
    markHtmlEditorTransient(island);island.setAttribute("contenteditable","false");const first=root.querySelector("strong")!.firstChild!;const text=first instanceof Text?first:first.firstChild!;
    select(root,text,0,text,1);command(editor.node,"bold").click();
    const expected=value.includes("<u>")?"<p><u>A</u><strong><u>B<em>LOCK</em>CD</u></strong></p>":"<p>A<strong>B<em>LOCK</em>CD</strong></p>";
    expect(editor.exportHtml()).toBe(expected);expect(root.querySelector("em")).toBe(island);expect(island.firstChild).toBe(lockedText);expect(island.textContent).toBe("LOCK");expect(document.getSelection()?.toString()).toBe("A");expect(change).toHaveBeenCalledTimes(1);expect(report).not.toHaveBeenCalled();
    editor.undo();expect(editor.exportHtml()).toBe(value);expect(root.querySelector("em")!.getAttribute("contenteditable")).toBe("false");editor.redo();expect(editor.exportHtml()).toBe(expected);expect(change).toHaveBeenCalledTimes(3);
  }
});

test("foreign detached collapsed and protected selections cannot authorize editing", () => {
  const {editor} = setup("<p>Before</p><p>Locked</p><p>After</p>"), root = editor.editable;
  const locked = root.children[1]!; markHtmlEditorTransient(locked); locked.setAttribute("contenteditable", "false");
  select(root, root.firstChild!.firstChild!, 0, root.lastChild!.firstChild!, 5); expect(editableRange(root)).toBeNull(); expect(toggleBold(root)).toBe(false);
  select(root, locked.firstChild!, 0, locked.firstChild!, 2); expect(editableRange(root)).toBeNull();
  select(root, root.firstChild!.firstChild!, 1); expect(toggleBold(root)).toBe(false);
  const foreign = document.createElement("div"); foreign.textContent = "Other"; document.body.append(foreign); select(foreign, foreign.firstChild!, 0, foreign.firstChild!, 3); expect(editableRange(root)).toBeNull(); expect(captureSelection(root)).toBeNull();
  editor.node.remove(); expect(editableRange(root)).toBeNull();
});

test("toolbar pointer and keyboard activation retain only this owner's selected range", () => {
  const {editor, change} = setup(), root = editor.editable, text = root.firstChild!.firstChild!;
  select(root, text, 0, text, 5); const bold = command(editor.node, "bold"); expect(bold.disabled).toBe(false);
  bold.dispatchEvent(new MouseEvent("pointerdown", {bubbles:true, cancelable:true})); bold.click(); expect(editor.exportHtml()).toBe("<p><strong>Hello</strong> world</p>"); expect(change).toHaveBeenCalledTimes(1);
  expect(bold.getAttribute("aria-pressed")).toBe("true"); bold.focus(); bold.click(); expect(editor.exportHtml()).toBe("<p>Hello world</p>");
  const other = setup("<p>Other</p>").editor; select(other.editable, other.editable.firstChild!.firstChild!, 0, other.editable.firstChild!.firstChild!, 3);
  expect(bold.disabled).toBe(true); bold.click(); expect(editor.exportHtml()).toBe("<p>Hello world</p>");
});

test("keyboard and pointer formatting capture the current owned selection before asynchronous selectionchange", () => {
  for(const activation of ["keyboard","pointer"]){
    const {editor,change}=setup(),root=editor.editable,text=root.firstChild!.firstChild!;select(root,text,0,text,1);
    document.getSelection()!.setBaseAndExtent(text,0,text,2);
    if(activation==="keyboard")root.dispatchEvent(new KeyboardEvent("keydown",{key:"b",ctrlKey:true,bubbles:true,cancelable:true}));
    else{const bold=command(editor.node,"bold");bold.dispatchEvent(new MouseEvent("pointerdown",{bubbles:true,cancelable:true}));bold.click();}
    expect(editor.exportHtml()).toBe("<p><strong>He</strong>llo world</p>");expect(document.getSelection()?.toString()).toBe("He");expect(change).toHaveBeenCalledTimes(1);
  }
});

/// (partial: explicit history and synthetic clipboard routing; native clipboard proof separate)
test("paste routes HTML and plain text as one undo transaction and restores selection", () => {
  const {editor, change} = setup(), root = editor.editable, text = root.firstChild!.firstChild!;
  select(root, text, 6, text, 11); expect(paste(root, {"text/html":"<b>Earth</b>", "text/plain":"wrong"}).defaultPrevented).toBe(true);
  expect(editor.exportHtml()).toBe("<p>Hello <strong>Earth</strong></p>"); expect(change).toHaveBeenCalledTimes(1);
  editor.undo(); expect(editor.exportHtml()).toBe("<p>Hello world</p>"); expect(document.getSelection()?.toString()).toBe("world");
  editor.redo(); expect(editor.exportHtml()).toBe("<p>Hello <strong>Earth</strong></p>");
  editor.undo(); paste(root, {"text/plain":"<literal>\nnext"}); expect(editor.exportHtml()).toBe("<p>Hello &lt;literal&gt;<br>next</p>"); expect(editor.state().canRedo).toBe(false);
});

test("private clipboard format is validated and rich paste simplification is visible", () => {
  const {editor, report, change} = setup(""), root = editor.editable; select(root, root, 0);
  paste(root, {"application/vnd.erpforall.html-editor+json":JSON.stringify({version:1, html:"<strong>Mine</strong>"})}); expect(editor.exportHtml()).toBe("<strong>Mine</strong>");
  paste(root, {"application/vnd.erpforall.html-editor+json":JSON.stringify({version:2, html:"bad"})});
  expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"validation", message:"Invalid ERPForAll HTML clipboard payload."})); expect(change).toHaveBeenCalledTimes(1);
  paste(root, {"text/html":"<span style='color:red'>clean</span><script>danger()</script>"}); expect(editor.node.querySelector(".html-editor-status")?.textContent).toBe("Pasted formatting was simplified."); expect(editor.exportHtml()).not.toContain("danger");
});

test("plain destinations insert literal text and detached foreign protected selections refuse paste", () => {
  const {editor, change} = setup("<p>abc</p>", {clipboardMode:"plain-text"}), root = editor.editable;
  select(root, root.firstChild!.firstChild!, 0, root.firstChild!.firstChild!, 3); paste(root, {"text/html":"<b>rich</b>", "text/plain":"<b>literal</b>"}); expect(editor.exportHtml()).toBe("<p>&lt;b&gt;literal&lt;/b&gt;</p>");
  root.firstChild!.parentElement!.setAttribute("contenteditable", "false"); paste(root, {"text/plain":"blocked"}); expect(change).toHaveBeenCalledTimes(1);
  editor.node.remove(); paste(root, {"text/plain":"detached"}); expect(change).toHaveBeenCalledTimes(1);
});

test("file paste drop and native rich insertion paths are explicitly blocked", () => {
  const {editor, report, change} = setup(), root=editor.editable; select(root, root.firstChild!.firstChild!, 0);
  paste(root, {}, [new File(["x"],"image.png",{type:"image/png"})]); expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"unsupported", message:"HTML editor file paste and drop require attachment support (a later release)."}));
  const drop=new Event("drop",{bubbles:true,cancelable:true}); root.dispatchEvent(drop); expect(drop.defaultPrevented).toBe(true);
  const rich=new InputEvent("beforeinput",{bubbles:true,cancelable:true,inputType:"insertFromPaste"}); root.dispatchEvent(rich); expect(rich.defaultPrevented).toBe(true); expect(change).not.toHaveBeenCalled();
});

test("history keyboard paths are editor-scoped and readonly controls stay disabled", () => {
  const {editor} = setup(), root=editor.editable; edit(root,"one"); edit(root,"two");
  for (const key of [{key:"z",ctrlKey:true},{key:"z",metaKey:true}]) { root.dispatchEvent(new KeyboardEvent("keydown",{...key,bubbles:true,cancelable:true})); }
  expect(editor.exportHtml()).toBe("<p>Hello world</p>"); root.dispatchEvent(new KeyboardEvent("keydown",{key:"y",ctrlKey:true,bubbles:true})); expect(editor.exportHtml()).toBe("one");
  root.dispatchEvent(new KeyboardEvent("keydown",{key:"Z",metaKey:true,shiftKey:true,bubbles:true})); expect(editor.exportHtml()).toBe("two");
  root.dispatchEvent(new KeyboardEvent("keydown",{key:"z",ctrlKey:true,bubbles:true})); root.dispatchEvent(new KeyboardEvent("keydown",{key:"z",ctrlKey:true,shiftKey:true,bubbles:true})); expect(editor.exportHtml()).toBe("two");
  const locked=setup("<p>Locked</p>",{readonly:true}).editor; expect(locked.editable.getAttribute("contenteditable")).toBe("false"); expect([...locked.node.querySelectorAll("button")].every(button=>button.disabled)).toBe(true);
  failure(()=>locked.undo(),"The HTML editor is readonly.");
});

test("composition commits once and parent replacement invalidates unfinished composition", () => {
  const {editor,change}=setup(), root=editor.editable; root.dispatchEvent(new CompositionEvent("compositionstart")); edit(root,"あ"); edit(root,"あい"); expect(change).not.toHaveBeenCalled();
  root.dispatchEvent(new CompositionEvent("compositionend")); expect(change).toHaveBeenCalledTimes(1); expect(editor.exportHtml()).toBe("あい"); editor.undo(); expect(editor.exportHtml()).toBe("<p>Hello world</p>");
  root.dispatchEvent(new CompositionEvent("compositionstart")); edit(root,"discard"); editor.setValue("<p>Owner</p>"); root.dispatchEvent(new CompositionEvent("compositionend")); expect(editor.exportHtml()).toBe("<p>Owner</p>"); expect(change).toHaveBeenCalledTimes(2);
});

/// (partial: rejecting owner callback retains local draft; no server field-write acceptance)
test("async change coalesces to the latest draft and replacement suppresses stale completions", async () => {
  let reject: (error: unknown)=>void=()=>{}; const change=vi.fn(()=>new Promise<void>((_resolve,fail)=>{reject=fail;}));
  const {editor,report}=setup("",{change}), root=editor.editable; edit(root,"one"); edit(root,"two"); edit(root,"three"); expect(change).toHaveBeenCalledTimes(1); expect(editor.state().pending).toBe(true);
  reject(new ClientError("validation","Owner refused draft.")); await vi.waitFor(()=>expect(change).toHaveBeenCalledTimes(2)); expect(change).toHaveBeenLastCalledWith("three"); expect(editor.exportHtml()).toBe("three"); expect(report).toHaveBeenCalledTimes(1);
  editor.setValue("<p>Replacement</p>"); reject(new Error("stale")); await Promise.resolve(); await Promise.resolve(); expect(report).toHaveBeenCalledTimes(1); expect(editor.state()).toMatchObject({pending:false,dirty:false});
});

test("reentrant replacement disposal and abort cannot leave stale callback presentation", async () => {
  let editor: ReturnType<typeof createHtmlEditor> | undefined;
  const change=vi.fn(()=>{editor!.setValue("<p>Parent</p>"); return Promise.reject(new Error("stale"));}); const created=setup("",{change}); editor=created.editor;
  edit(editor.editable,"draft"); await Promise.resolve(); await Promise.resolve(); expect(editor.exportHtml()).toBe("<p>Parent</p>"); expect(editor.state().pending).toBe(false); expect(created.report).not.toHaveBeenCalled();
  editor.dispose(); editor.dispose(); failure(()=>editor!.setValue("x"),"The HTML editor is disposed."); failure(()=>editor!.exportHtml(),"The HTML editor is disposed.");
  const controller=new AbortController(), messages=new MessageServices(document.body, vi.fn()); disposers.push(()=>messages.dispose()); controller.abort(); const aborted=createHtmlEditor({environment:{messages,report:vi.fn(),signal:controller.signal},label:"Gone",value:"",change:vi.fn()}); failure(()=>aborted.focus(),"The HTML editor is disposed.");
});

test("declarations reject unknown keys and wrong scalar types with exact validation errors", () => {
  const {editor}=setup(); editor.dispose();
  const messages=new MessageServices(document.body, vi.fn()); disposers.push(()=>messages.dispose()); const base={environment:{messages,report:vi.fn()},label:"Notes",value:"",change:vi.fn()};
  failure(()=>createHtmlEditor({...base,other:true} as never),"Unknown html editor option: other.");
  failure(()=>createHtmlEditor({...base,readonly:2} as never),"HTML editor readonly must be a boolean.");
  failure(()=>createHtmlEditor({...base,change:2} as never),"HTML editor change must be a function.");
  failure(()=>createHtmlEditor({...base,clipboardMode:"bad"} as never),"HTML editor clipboard mode must be rich or plain-text.");
  failure(()=>createHtmlEditor({...base,label:2} as never),"HTML editor label must be text of at most 4096 bytes.");
});

test("copy and cut publish admitted versioned formats preserving inline ancestors and one undo step", () => {
  const {editor,change}=setup("<p>A<strong>Hello</strong>Z</p>"), root=editor.editable, text=root.querySelector("strong")!.firstChild!;
  select(root,text,1,text,4); const values:Record<string,string>={};
  const event=(type:string):Event=>{const event=new Event(type,{bubbles:true,cancelable:true});Object.defineProperty(event,"clipboardData",{value:{setData:(key:string,value:string)=>{values[key]=value;}}});root.dispatchEvent(event);return event;};
  expect(event("copy").defaultPrevented).toBe(true); expect(values["text/plain"]).toBe("ell"); expect(values["text/html"]).toBe("<strong>ell</strong>");
  expect(JSON.parse(values["application/vnd.erpforall.html-editor+json"]!)).toEqual({version:1,html:"<strong>ell</strong>"}); expect(change).not.toHaveBeenCalled();
  event("cut"); expect(editor.exportHtml()).toBe("<p>A<strong>Ho</strong>Z</p>"); expect(change).toHaveBeenCalledTimes(1); editor.undo(); expect(editor.exportHtml()).toBe("<p>A<strong>Hello</strong>Z</p>"); expect(document.getSelection()?.toString()).toBe("ell");
});

test("failed parent native input and paste admission retain the accepted draft and history", () => {
  const {editor,change,report}=setup("<p>abc</p>",{limits:{inputBytes:64,outputBytes:64}}),root=editor.editable;
  failure(()=>editor.setValue("<img src='x'>"),"HTML editor initial content contains unsupported markup.","unsupported"); expect(editor.exportHtml()).toBe("<p>abc</p>");
  select(root,root.firstChild!.firstChild!,0,root.firstChild!.firstChild!,3); paste(root,{"text/plain":"x".repeat(65)});
  expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"validation",message:"HTML editor input exceeds inputBytes limit."})); expect(editor.exportHtml()).toBe("<p>abc</p>"); expect(editor.state().canUndo).toBe(false);
  root.dispatchEvent(new InputEvent("beforeinput",{bubbles:true,cancelable:true,inputType:"insertText"}));
  const img=document.createElement("img");root.append(img);root.dispatchEvent(new InputEvent("input",{bubbles:true})); expect(editor.exportHtml()).toBe("<p>abc</p>");
  expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"unsupported",message:"HTML editor export contains unsupported markup."})); expect(change).not.toHaveBeenCalled(); expect(editor.state().canUndo).toBe(false);
});

/// (partial: bounded connected history and explicit availability in component tests)
test("history pressure shortens reachable undo transitions visibly and preserves redo", () => {
  const {editor}=setup("",{limits:{historyEntries:2}}); edit(editor.editable,"first");edit(editor.editable,"second");
  expect(editor.node.querySelector(".html-editor-status")?.textContent).toBe("Earlier undo history was shortened to stay within the editor limit.");
  editor.undo();expect(editor.exportHtml()).toBe("first");expect(editor.state()).toMatchObject({canUndo:false,canRedo:true});expect(command(editor.node,"undo").disabled).toBe(true);expect(command(editor.node,"redo").disabled).toBe(false);
  editor.redo();expect(editor.exportHtml()).toBe("second");
});

test("undefined callback rejections report once and retain the local draft", async () => {
  const {editor,report}=setup("",{change:()=>Promise.reject(undefined)});edit(editor.editable,"draft");
  await vi.waitFor(()=>expect(editor.state().pending).toBe(false));expect(report).toHaveBeenCalledTimes(1);expect(editor.exportHtml()).toBe("draft");expect(editor.node.querySelector(".html-editor-error")?.textContent).toBe("The operation failed. Please reload and try again.");
});

test("undefined callback throws also report once", () => {
  const {editor,report}=setup("",{change:()=>{throw undefined;}});edit(editor.editable,"draft");expect(report).toHaveBeenCalledTimes(1);expect(editor.state()).toMatchObject({pending:false,dirty:true});
});

test("caller mutation cannot replace the reporter callback or editing policy", () => {
  const report=vi.fn(),replacement=vi.fn(),change=vi.fn(()=>{throw new Error("refused");}),messages=new MessageServices(document.body,report);
  const options={environment:{messages,report},label:"Notes",value:"",readonly:false,change};
  const editor=createHtmlEditor(options);document.body.append(editor.node);disposers.push(()=>editor.dispose(),()=>messages.dispose());
  options.readonly=true;options.environment.report=replacement;options.change=vi.fn();edit(editor.editable,"draft");
  expect(editor.exportHtml()).toBe("draft");expect(change).toHaveBeenCalledTimes(1);expect(report).toHaveBeenCalledTimes(1);expect(replacement).not.toHaveBeenCalled();
});

test("blank labels are rejected with a precise accessible-name validation error", () => {
  const messages=new MessageServices(document.body,vi.fn());disposers.push(()=>messages.dispose());
  failure(()=>createHtmlEditor({environment:{messages,report:vi.fn()},label:" \n ",value:"",change:vi.fn()}),"HTML editor label must not be blank.");
});

test("replacement starts its own callback lifetime without waiting for an unresolved old owner", async () => {
  const completions: (()=>void)[]=[];
  const change=vi.fn(()=>new Promise<void>(resolve=>{completions.push(()=>resolve());}));
  const {editor,report}=setup("",{change});edit(editor.editable,"old");editor.setValue("<p>Replacement</p>");edit(editor.editable,"new");
  expect(change).toHaveBeenCalledTimes(2);expect(change).toHaveBeenLastCalledWith("new");expect(editor.state().pending).toBe(true);
  completions[0]!();await Promise.resolve();await Promise.resolve();expect(editor.state().pending).toBe(true);expect(report).not.toHaveBeenCalled();
  completions[1]!();await vi.waitFor(()=>expect(editor.state().pending).toBe(false));expect(editor.exportHtml()).toBe("new");
});

test("protected islands survive formatting and history without live subtree normalization", () => {
  const {editor}=setup("<p>Open</p><p>Locked</p><p>Tail</p>"),root=editor.editable,island=root.children[1]!;
  markHtmlEditorTransient(island);island.setAttribute("contenteditable","false");const first=island.firstChild!;(first as Text).splitText(3);
  select(root,root.firstChild!.firstChild!,0,root.firstChild!.firstChild!,4);command(editor.node,"bold").click();
  expect(island.firstChild).toBe(first);expect(island.childNodes).toHaveLength(2);
  editor.undo();expect(root.children[1]!.getAttribute("contenteditable")).toBe("false");
  const locked=root.children[1]!.firstChild!;select(root,locked,0,locked,2);const cut=new Event("cut",{bubbles:true,cancelable:true});root.dispatchEvent(cut);expect(cut.defaultPrevented).toBe(true);expect(editor.exportHtml()).toBe("<p>Open</p><p>Locked</p><p>Tail</p>");
});

test("protected cut always prevents unchecked native deletion", () => {
  const {editor,change}=setup("<p>Before</p><p>Lock</p><p>After</p>"),root=editor.editable,island=root.children[1]!;markHtmlEditorTransient(island);island.setAttribute("contenteditable","false");
  select(root,root.firstChild!.firstChild!,1,root.lastChild!.firstChild!,2);const event=new Event("cut",{bubbles:true,cancelable:true});root.dispatchEvent(event);expect(event.defaultPrevented).toBe(true);expect(change).not.toHaveBeenCalled();
});

test("composition and native input cannot mutate or remove a protected island", () => {
  const {editor,report,change}=setup("<p>Open</p><p>Locked</p>"),root=editor.editable,island=root.children[1]!;
  markHtmlEditorTransient(island);island.setAttribute("contenteditable","false");select(root,root.firstChild!.firstChild!,1);
  root.dispatchEvent(new CompositionEvent("compositionstart"));island.textContent="Changed";root.dispatchEvent(new InputEvent("input",{bubbles:true,inputType:"insertCompositionText"}));root.dispatchEvent(new CompositionEvent("compositionend"));
  expect(editor.exportHtml()).toBe("<p>Open</p><p>Locked</p>");expect(root.children[1]!.getAttribute("contenteditable")).toBe("false");expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"validation",message:"HTML editor edits cannot change protected content."}));expect(change).not.toHaveBeenCalled();expect(editor.state().canUndo).toBe(false);
  select(root,root.firstChild!.firstChild!,1);root.dispatchEvent(new InputEvent("beforeinput",{bubbles:true,cancelable:true,inputType:"deleteContentForward"}));root.children[1]!.remove();root.dispatchEvent(new InputEvent("input",{bubbles:true,inputType:"deleteContentForward"}));
  expect(editor.exportHtml()).toBe("<p>Open</p><p>Locked</p>");expect(change).not.toHaveBeenCalled();expect(editor.state().canUndo).toBe(false);
});

test("cross-block paste preserves prefix suffix siblings and its complete undo selection", () => {
  const {editor}=setup("<p>Keep</p><h2>ABCD</h2><p>EFGH</p><p>Tail</p>"),root=editor.editable;const keep=root.firstChild,tail=root.lastChild;
  select(root,root.children[1]!.firstChild!,2,root.children[2]!.firstChild!,2);paste(root,{"text/html":"<p>Inserted</p>"});
  expect(editor.exportHtml()).toBe("<p>Keep</p><h2>AB</h2><p>Inserted</p><p>GH</p><p>Tail</p>");expect(root.firstChild).toBe(keep);expect(root.lastChild).toBe(tail);
  editor.undo();expect(editor.exportHtml()).toBe("<p>Keep</p><h2>ABCD</h2><p>EFGH</p><p>Tail</p>");expect(document.getSelection()?.toString()).toBe("CDEF");editor.redo();expect(editor.exportHtml()).toBe("<p>Keep</p><h2>AB</h2><p>Inserted</p><p>GH</p><p>Tail</p>");
});

test("block paste preserves admitted list quote and nested division destinations with connected history", () => {
  for(const fixture of [
    {value:"<ul><li>Item</li><li>Other</li></ul>",selector:"li",expected:"<ul><li><p>It</p><p>Block</p><p>em</p></li><li>Other</li></ul>"},
    {value:"<blockquote>Item</blockquote>",selector:"blockquote",expected:"<blockquote><p>It</p><p>Block</p><p>em</p></blockquote>"},
    {value:"<div><div><p>Item</p><p>Other</p></div></div>",selector:"p",expected:"<div><div><p>It</p><p>Block</p><p>em</p><p>Other</p></div></div>"},
  ]){
    const {editor,report,change}=setup(fixture.value),root=editor.editable,target=root.querySelector(fixture.selector)!,container=target.localName==="p"?target.parentElement!:target;
    select(root,target.firstChild!,2);paste(root,{"text/html":"<p>Block</p>"});expect(editor.exportHtml()).toBe(fixture.expected);expect(root.contains(container)).toBe(true);expect(change).toHaveBeenCalledTimes(1);expect(report).not.toHaveBeenCalled();
    editor.undo();expect(editor.exportHtml()).toBe(fixture.value);editor.redo();expect(editor.exportHtml()).toBe(fixture.expected);expect(change).toHaveBeenCalledTimes(3);
  }
});

test("nested block clipboard under inline formatting refuses atomically with connected redo", () => {
  const {editor,report,change}=setup("<p>Item</p>"),root=editor.editable,text=root.firstChild!.firstChild! as Text;
  select(root,text,4);root.dispatchEvent(new InputEvent("beforeinput",{bubbles:true,cancelable:true,inputType:"insertText",data:"!"}));text.appendData("!");select(root,text,5);root.dispatchEvent(new InputEvent("input",{bubbles:true,inputType:"insertText",data:"!"}));
  editor.undo();const current=root.firstChild!.firstChild!;select(root,current,1,current,3);const before=captureSelection(root),availability=editor.state(),calls=change.mock.calls.length;
  paste(root,{"text/html":"<strong><p>Block</p></strong>"});
  expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind:"unsupported",message:"HTML editor block content inside inline formatting is unsupported until a later release."}));
  expect(editor.exportHtml()).toBe("<p>Item</p>");expect(document.getSelection()?.toString()).toBe("te");expect(captureSelection(root)).toEqual(before);expect(editor.state()).toEqual(availability);expect(change).toHaveBeenCalledTimes(calls);
  editor.redo();expect(editor.exportHtml()).toBe("<p>Item!</p>");editor.undo();expect(editor.exportHtml()).toBe("<p>Item</p>");expect(document.getSelection()?.toString()).toBe("te");
});

test("block paste splits the paragraph and round trips through undo and redo without reparenting", () => {
  const {editor}=setup("<p>Hello world</p>"),root=editor.editable;select(root,root.firstChild!.firstChild!,6,root.firstChild!.firstChild!,11);
  paste(root,{"text/html":"<p>First</p><p>Second</p>"});expect(editor.exportHtml()).toBe("<p>Hello </p><p>First</p><p>Second</p>");
  editor.undo();expect(editor.exportHtml()).toBe("<p>Hello world</p>");editor.redo();expect(editor.exportHtml()).toBe("<p>Hello </p><p>First</p><p>Second</p>");
});

test("disposal invalidates pending notifications and foreign focus clears staged formatting", async () => {
  let reject: (value:unknown)=>void=()=>{};const calls=vi.fn();const {editor,report}=setup("",{change:html=>{calls(html);return new Promise<void>((_resolve,fail)=>{reject=fail;});}});
  edit(editor.editable,"draft");editor.dispose();reject(new Error("stale"));await Promise.resolve();await Promise.resolve();expect(report).not.toHaveBeenCalled();expect(editor.state().pending).toBe(false);expect(calls).toHaveBeenCalledTimes(1);
  const other=setup().editor;select(other.editable,other.editable.firstChild!.firstChild!,0,other.editable.firstChild!.firstChild!,5);const foreign=document.createElement("button");document.body.append(foreign);foreign.focus();expect(command(other.node,"bold").disabled).toBe(true);
});
