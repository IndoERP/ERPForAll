/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { createSignaturePad } from "../src/client/signature-pad.js";
import { ClientError } from "../src/client/transport.js";

const png = (): Uint8Array<ArrayBuffer> => {
  const bytes=new Uint8Array(58),view=new DataView(bytes.buffer);bytes.set([137,80,78,71,13,10,26,10]);view.setUint32(8,13);bytes.set([73,72,68,82],12);view.setUint32(16,2);view.setUint32(20,1);bytes[24]=8;bytes[25]=6;view.setUint32(33,1);bytes.set([73,68,65,84],37);bytes.set([73,69,78,68],50);return bytes;
};
let context: {clearRect:ReturnType<typeof vi.fn>; fillText:ReturnType<typeof vi.fn>; measureText:ReturnType<typeof vi.fn>; beginPath:ReturnType<typeof vi.fn>; moveTo:ReturnType<typeof vi.fn>; lineTo:ReturnType<typeof vi.fn>; stroke:ReturnType<typeof vi.fn>; arc:ReturnType<typeof vi.fn>; fill:ReturnType<typeof vi.fn>; drawImage:ReturnType<typeof vi.fn>};
let imageLoads: (()=>void)[]=[];
let images: FakeImage[]=[];
class FakeImage {
  constructor(){images.push(this);}
  naturalWidth=2; naturalHeight=1; onload:(()=>void)|null=null; onerror:(()=>void)|null=null;
  set src(_url:string) { imageLoads.push(()=>this.onload?.()); }
}
beforeEach(()=>{
  context={clearRect:vi.fn(),fillText:vi.fn(),measureText:vi.fn(()=>({width:100})),beginPath:vi.fn(),moveTo:vi.fn(),lineTo:vi.fn(),stroke:vi.fn(),arc:vi.fn(),fill:vi.fn(),drawImage:vi.fn()};
  vi.spyOn(HTMLCanvasElement.prototype,"getContext").mockImplementation(()=>context as unknown as CanvasRenderingContext2D);
  vi.spyOn(HTMLCanvasElement.prototype,"toDataURL").mockReturnValue("data:image/png;base64,"+btoa(String.fromCharCode(...png())));
  vi.stubGlobal("Image",FakeImage);vi.stubGlobal("URL",{createObjectURL:vi.fn(()=>"blob:signature-test"),revokeObjectURL:vi.fn()});imageLoads=[];images=[];
});
afterEach(()=>{document.body.replaceChildren();vi.restoreAllMocks();vi.unstubAllGlobals();});
const imageReady = async (index: number): Promise<void> => { await vi.waitFor(() => expect(imageLoads).toHaveLength(index + 1)); };
/// (partial: component modes and notifications with fake canvas; real raster rendering belongs to browser proof)
test("signature modes preserve name identity, typed empty state, font and drawing notifications",()=>{
  const changed=vi.fn(),report=vi.fn();const pad=createSignaturePad({label:"Approval signature",name:"Ada Lovelace",change:changed,report});document.body.append(pad.node);
  expect(pad.state()).toMatchObject({mode:"type",name:"Ada Lovelace",empty:false,pending:false,invalid:false});
  expect(context.fillText).toHaveBeenCalledWith("Ada Lovelace",expect.any(Number),expect.any(Number));
  const font=pad.node.querySelector<HTMLSelectElement>("select")!;font.value="serif";font.dispatchEvent(new Event("change"));expect(changed).toHaveBeenLastCalledWith(expect.objectContaining({mode:"type",empty:false}));
  const input=pad.node.querySelector<HTMLInputElement>('input[type="text"]')!;input.value="   ";input.dispatchEvent(new Event("input"));expect(pad.state().empty).toBe(true);expect(pad.exportPng()).toBeNull();
  pad.setMode("draw");
  const canvas=pad.canvas;vi.spyOn(canvas,"getBoundingClientRect").mockReturnValue({left:0,top:0,width:400,height:100,bottom:100,right:400,x:0,y:0,toJSON:()=>({})});
  const event=(type:string)=>{const ev=new MouseEvent(type,{clientX:20,clientY:20,bubbles:true});Object.defineProperty(ev,"pointerId",{value:1});return ev;};
  canvas.dispatchEvent(event("pointerdown"));canvas.dispatchEvent(event("pointermove"));canvas.dispatchEvent(event("pointerup"));
  expect(pad.state()).toMatchObject({mode:"draw",empty:false});expect(changed).toHaveBeenLastCalledWith(expect.objectContaining({mode:"draw",empty:false}));expect(pad.exportPng()).toMatch(/^data:image\/png;base64,/);
  pad.clear();expect(pad.state().empty).toBe(true);expect(pad.node.querySelector('[role="status"]')?.textContent).toContain("empty");expect(report).not.toHaveBeenCalled();pad.dispose();
});
test("signature upload is validated before decoding, fits aspect ratio and revokes image resources",async()=>{
  const changed=vi.fn(),report=vi.fn(),pad=createSignaturePad({mode:"upload",change:changed,report});document.body.append(pad.node);
  const loading=pad.loadFile(new File([png().buffer],"signature.png",{type:"image/png"}));await imageReady(0);expect(pad.state().pending).toBe(true);expect(imageLoads).toHaveLength(1);imageLoads[0]!();await loading;
  expect(context.drawImage).toHaveBeenCalledWith(expect.any(FakeImage),expect.any(Number),expect.any(Number),expect.any(Number),expect.any(Number));expect(pad.state()).toMatchObject({mode:"upload",empty:false,pending:false});expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:signature-test");
  await expect(pad.loadFile(new File(["<svg/>"],"bad.svg",{type:"image/svg+xml"}))).rejects.toMatchObject({kind:"validation",message:"Signature images must be PNG or JPEG."});
  expect(pad.state().invalid).toBe(true);expect(pad.node.querySelector(".signature-status")?.textContent).toBe("Signature image is invalid.");expect(pad.node.querySelector('[role="alert"]')?.textContent).toBe("Signature images must be PNG or JPEG.");expect(report).toHaveBeenCalledWith(expect.any(ClientError));pad.dispose();
});
test("signature disposal and a mode change invalidate pending image callbacks",async()=>{
  const changed=vi.fn(),pad=createSignaturePad({mode:"upload",change:changed,report:vi.fn()});document.body.append(pad.node);
  const loading=pad.loadFile(new File([png().buffer],"signature.png",{type:"image/png"}));await imageReady(0);pad.setMode("draw");const count=changed.mock.calls.length;imageLoads[0]!();await loading;
  expect(changed).toHaveBeenCalledTimes(count);expect(pad.state().empty).toBe(true);expect(context.drawImage).not.toHaveBeenCalled();
  const second=pad.loadFile(new File([png().buffer],"signature.png",{type:"image/png"}));await imageReady(1);pad.dispose();imageLoads[1]!();await second;expect(context.drawImage).not.toHaveBeenCalled();expect(URL.revokeObjectURL).toHaveBeenCalledTimes(2);
});
test("signature initial image follows the same raster admission and readonly blocks edits",async()=>{
  const report=vi.fn(),changed=vi.fn(),pad=createSignaturePad({initialImage:"https://example.invalid/image.png",readonly:true,report,change:changed});document.body.append(pad.node);await vi.waitFor(() => expect(pad.state().invalid).toBe(true));
  expect(pad.state().invalid).toBe(true);expect(report).toHaveBeenCalledWith(expect.objectContaining({kind:"validation",message:"Signature image must be a PNG or JPEG base64 data URL."}));
  expect([...pad.node.querySelectorAll<HTMLInputElement|HTMLButtonElement|HTMLSelectElement>("input,button,select")].every(item=>item.disabled)).toBe(true);
  expect(()=>pad.setMode("draw")).toThrowError("The signature is readonly.");expect(changed).not.toHaveBeenCalled();pad.dispose();
});
test("signature fails explicitly on malformed options, unusable canvas and oversized export",()=>{
  expect(()=>createSignaturePad({change:vi.fn(),report:vi.fn(),mode:"wrong"} as never)).toThrowError("Invalid signature mode.");
  expect(()=>createSignaturePad({change:3,report:vi.fn()} as never)).toThrowError("Signature change and report must be functions.");
  expect(()=>createSignaturePad({change:vi.fn(),report:vi.fn(),unknown:true} as never)).toThrowError("Unknown signature option: unknown.");
  const report=vi.fn(),pad=createSignaturePad({name:"Ada",change:vi.fn(),report});vi.mocked(HTMLCanvasElement.prototype.toDataURL).mockReturnValue("data:image/png;base64,"+"A".repeat(2*1024*1024));
  expect(()=>pad.exportPng()).toThrowError("Signature export exceeds the 2 MiB output limit.");expect(pad.state().invalid).toBe(true);pad.dispose();
  vi.mocked(HTMLCanvasElement.prototype.getContext).mockReturnValue(null);expect(()=>createSignaturePad({change:vi.fn(),report:vi.fn()})).toThrowError("Canvas drawing is unavailable for signature capture.");
});

function failure(job:()=>unknown,message:string,kind:ClientError["kind"]="validation"):void {
  let actual:unknown;try{job();}catch(error){actual=error;}expect(actual).toBeInstanceOf(ClientError);expect(actual).toMatchObject({kind,message});
}
test("signature declaration errors carry the exact validation kind and message",()=>{
  const base={change:vi.fn(),report:vi.fn()};
  for(const value of [null,[],new Date()])failure(()=>createSignaturePad(value as never),"Signature options must be a plain object.");
  failure(()=>createSignaturePad({...base,label:2} as never),"Signature label must be text of at most 8192 bytes.");
  failure(()=>createSignaturePad({...base,name:"é".repeat(513)}),"Signature name must be text of at most 1024 bytes.");
  failure(()=>createSignaturePad({...base,initialImage:3} as never),"Signature initial image must be a data URL string.");
  failure(()=>createSignaturePad({...base,readonly:3} as never),"Signature readonly must be a boolean.");
  failure(()=>createSignaturePad({...base,signal:{}} as never),"Signature signal must be an AbortSignal.");
  failure(()=>createSignaturePad({...base,mode:"wrong"} as never),"Invalid signature mode.");
  failure(()=>createSignaturePad({...base,change:3} as never),"Signature change and report must be functions.");
  failure(()=>createSignaturePad({...base,unknown:true} as never),"Unknown signature option: unknown.");
});
test("signature decode refusal is visible and initial valid image uses the same decoder",async()=>{
  const report=vi.fn(),changed=vi.fn(),pad=createSignaturePad({mode:"upload",report,change:changed});document.body.append(pad.node);
  const loading=pad.loadFile(new File([png().buffer],"signature.png",{type:"image/png"}));await imageReady(0);images[0]!.onerror?.();
  await expect(loading).rejects.toMatchObject({kind:"validation",message:"Signature image could not be decoded."});expect(pad.state()).toMatchObject({invalid:true,pending:false,empty:true});expect(changed).not.toHaveBeenCalled();expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);pad.dispose();
  const initial=createSignaturePad({initialImage:"data:image/png;base64,"+btoa(String.fromCharCode(...png())),report,change:changed});document.body.append(initial.node);await imageReady(1);images[1]!.onload?.();await vi.waitFor(() => expect(initial.state().pending).toBe(false));expect(initial.state()).toMatchObject({empty:false,pending:false,invalid:false});expect(changed).not.toHaveBeenCalled();initial.dispose();
});
test("signature export and public edits reject disposed or readonly owners",()=>{
  const readonly=createSignaturePad({readonly:true,change:vi.fn(),report:vi.fn()});failure(()=>readonly.clear(),"The signature is readonly.");failure(()=>readonly.setMode("draw"),"The signature is readonly.");readonly.dispose();
  failure(()=>readonly.exportPng(),"The signature control is disposed.");
  const aborted=new AbortController();aborted.abort();const pad=createSignaturePad({signal:aborted.signal,change:vi.fn(),report:vi.fn()});failure(()=>pad.setMode("type"),"The signature control is disposed.");
});
