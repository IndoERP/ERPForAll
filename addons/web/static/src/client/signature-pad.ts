/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { ClientError } from "./transport.js";
import { fitSignatureImage, inspectSignatureImage, signatureDataUrlBytes, SIGNATURE_INPUT_BYTES, SIGNATURE_OUTPUT_BYTES } from "./signature-image.js";

export type SignatureMode = "type" | "draw" | "upload";
export interface SignatureState { mode: SignatureMode; name: string; empty: boolean; pending: boolean; invalid: boolean }
export interface SignaturePadOptions {
  label?: string; name?: string; mode?: SignatureMode; initialImage?: string; readonly?: boolean; signal?: AbortSignal;
  change(state: SignatureState): void; report(error: unknown): void;
}
export interface SignaturePadHandle {
  node: HTMLElement; canvas: HTMLCanvasElement; state(): SignatureState; exportPng(): string | null;
  setMode(mode: SignatureMode): void; clear(): void; loadFile(file: File): Promise<void>; dispose(): void;
}
const MODES: readonly SignatureMode[]=["type","draw","upload"];
const FONTS=["cursive","serif","sans-serif"] as const;
const bytes=(value:string):number=>new TextEncoder().encode(value).length;
function boundedText(value:unknown,label:string,limit:number): asserts value is string {
  if (typeof value!=="string" || bytes(value)>limit) throw new ClientError("validation",`Signature ${label} must be text of at most ${limit} bytes.`);
}
function modeValue(value: unknown): asserts value is SignatureMode {
  if (!MODES.includes(value as SignatureMode)) throw new ClientError("validation","Invalid signature mode.");
}
/** A browser-local image authoring control, not signer identity or document approval.
 * Its owner disposes it with the containing form; all emitted states are snapshots. */
export function createSignaturePad(options: SignaturePadOptions): SignaturePadHandle {
  if (options===null || typeof options!=="object" || ![Object.prototype,null].includes(Object.getPrototypeOf(options))) throw new ClientError("validation","Signature options must be a plain object.");
  for (const key of Reflect.ownKeys(options)) if (typeof key!=="string" || !["label","name","mode","initialImage","readonly","signal","change","report"].includes(key)) throw new ClientError("validation",`Unknown signature option: ${String(key)}.`);
  if (typeof options.change!=="function" || typeof options.report!=="function") throw new ClientError("validation","Signature change and report must be functions.");
  if (options.label!==undefined) boundedText(options.label,"label",8192);
  if (options.name!==undefined) boundedText(options.name,"name",1024);
  if (options.mode!==undefined) modeValue(options.mode);
  if (options.initialImage!==undefined && typeof options.initialImage!=="string") throw new ClientError("validation","Signature initial image must be a data URL string.");
  if (options.readonly!==undefined && typeof options.readonly!=="boolean") throw new ClientError("validation","Signature readonly must be a boolean.");
  if (options.signal!==undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation","Signature signal must be an AbortSignal.");
  const node=el("fieldset",undefined,"signature-pad choice-editor"),legend=el("legend",options.label??"Signature");node.append(legend);
  const modes=el("div",undefined,"signature-modes");modes.setAttribute("role","group");modes.setAttribute("aria-label","Signature input mode");
  const name=el("input");name.type="text";name.value=options.name??"";name.setAttribute("aria-label","Signature name");name.autocomplete="name";
  const nameLabel=el("label",undefined,"signature-name");nameLabel.append(el("span","Name"),name);
  const fonts=el("select");fonts.setAttribute("aria-label","Signature font");
  for (const family of FONTS) fonts.append(new Option(family=== "sans-serif"?"Sans serif":family[0]!.toUpperCase()+family.slice(1),family));
  const canvas=el("canvas",undefined,"signature-canvas");canvas.width=1024;canvas.height=256;canvas.setAttribute("role","img");canvas.setAttribute("aria-label","Signature preview. Use Type or Upload for a keyboard alternative to drawing.");canvas.style.touchAction="none";
  const context=canvas.getContext("2d");if (!context) throw new ClientError("unsupported","Canvas drawing is unavailable for signature capture.");
  const upload=el("input");upload.type="file";upload.accept="image/png,image/jpeg";upload.setAttribute("aria-label","Upload a signature image");
  const uploadLabel=el("label",undefined,"signature-upload");uploadLabel.append(el("span","Image (PNG or JPEG, up to 2 MiB)"),upload);
  const status=el("p",undefined,"signature-status");status.setAttribute("role","status");status.setAttribute("aria-live","polite");
  const alert=el("p",undefined,"signature-error");alert.setAttribute("role","alert");alert.hidden=true;
  let mode:SignatureMode=options.mode??"type",empty=true,pending=false,invalid=false,dead=false,generation=0,pointer:number|undefined,currentFont:typeof FONTS[number]="cursive";
  let cancelImage:(()=>void)|undefined,cancelRead:(()=>void)|undefined;
  const state=():SignatureState=>({mode,name:name.value,empty,pending,invalid});
  const live=():void=>{if(dead)throw new ClientError("validation","The signature control is disposed.");};
  const editable=():void=>{live();if(options.readonly)throw new ClientError("validation","The signature is readonly.");};
  const update=():void=>{
    nameLabel.hidden=mode!=="type";fonts.hidden=mode!=="type";uploadLabel.hidden=mode!=="upload";
    canvas.dataset["mode"]=mode;canvas.setAttribute("aria-busy",String(pending));node.dataset["invalid"]=String(invalid);
    status.textContent=pending?"Loading signature image…":invalid?"Signature image is invalid.":empty?"Signature is empty.":"Signature is ready.";
    for(const [key,control] of modeButtons) control.setAttribute("aria-pressed",String(key===mode));
    for(const control of node.querySelectorAll<HTMLInputElement|HTMLButtonElement|HTMLSelectElement>("input,button,select"))control.disabled=Boolean(options.readonly)||dead;
  };
  const failure=(error:unknown):void=>{if(dead)return;invalid=true;alert.hidden=false;alert.textContent=errorMessage(error);update();options.report(error);};
  const notify=():void=>{if(!dead){try{options.change(state());}catch(error){failure(error);}}};
  const resetError=():void=>{invalid=false;alert.hidden=true;alert.textContent="";};
  const cancel=():void=>{generation++;cancelRead?.();cancelRead=undefined;cancelImage?.();cancelImage=undefined;pending=false;if(pointer!==undefined&&canvas.hasPointerCapture?.(pointer))canvas.releasePointerCapture(pointer);pointer=undefined;};
  const erase=():void=>{context.clearRect(0,0,canvas.width,canvas.height);empty=true;};
  const drawName=():void=>{
    boundedText(name.value,"name",1024);erase();const cleaned=name.value.trim().replace(/\s+/gu," ");
    if(cleaned){context.textAlign="center";context.textBaseline="middle";context.fillStyle="#17232e";context.font=`80px ${currentFont}`;
      const measured=context.measureText(cleaned).width;
      if(!Number.isFinite(measured)||measured<0)throw new ClientError("unsupported","Signature text measurement failed.");
      if(measured>canvas.width-48)context.font=`${Math.max(1,80*(canvas.width-48)/measured)}px ${currentFont}`;
      context.fillText(cleaned,canvas.width/2,canvas.height/2);empty=false;
    }
  };
  const setMode=(next:SignatureMode):void=>{editable();modeValue(next);cancel();mode=next;resetError();erase();if(mode==="type")drawName();update();notify();};
  const safe=(job:()=>void):void=>{try{job();}catch(error){failure(error);}};
  const modeButtons=new Map<SignatureMode,HTMLButtonElement>();
  for(const [key,label] of [["type","Type"],["draw","Draw"],["upload","Upload"]] as const){const control=button(label,()=>safe(()=>setMode(key)));modeButtons.set(key,control);modes.append(control);}
  const clear=():void=>{editable();cancel();erase();resetError();if(mode==="type")name.value="";upload.value="";update();notify();};
  const clearButton=button("Clear signature",()=>safe(clear));
  node.append(modes,nameLabel,fonts,canvas,uploadLabel,clearButton,status,alert);
  const read=(file:File,request:number):Promise<Uint8Array|undefined>=>new Promise((resolve,reject)=>{
    const reader=new FileReader();
    const finish=():void=>{reader.onload=null;reader.onerror=null;reader.onabort=null;cancelRead=undefined;};
    cancelRead=()=>{finish();if(reader.readyState===FileReader.LOADING)reader.abort();resolve(undefined);};
    reader.onload=()=>{const result=reader.result;finish();if(dead||request!==generation){resolve(undefined);return;}if(!(result instanceof ArrayBuffer)){reject(new ClientError("validation","Signature image could not be read."));return;}resolve(new Uint8Array(result));};
    reader.onerror=()=>{finish();reject(new ClientError("validation","Signature image could not be read."));};reader.onabort=()=>{finish();resolve(undefined);};
    reader.readAsArrayBuffer(file);
  });
  const paintImage=(data:Uint8Array,mime:string,request:number,emit:boolean):Promise<void>=>new Promise((resolve,reject)=>{
    const info=inspectSignatureImage(data,mime),image=new Image();
    const url=URL.createObjectURL(new Blob([data.slice().buffer],{type:info.mime}));let done=false;
    const finish=():void=>{if(done)return;done=true;image.onload=null;image.onerror=null;URL.revokeObjectURL(url);cancelImage=undefined;};
    cancelImage=()=>{finish();resolve();};
    image.onload=()=>{
      if(dead||request!==generation){finish();resolve();return;}
      try{
        if(image.naturalWidth!==info.width||image.naturalHeight!==info.height)throw new ClientError("validation","Decoded signature image dimensions do not match its header.");
        const fit=fitSignatureImage(info.width,info.height,canvas.width,canvas.height);erase();context.drawImage(image,fit.x,fit.y,fit.width,fit.height);empty=false;pending=false;resetError();finish();update();if(emit)notify();resolve();
      }catch(error){finish();reject(error);}
    };
    image.onerror=()=>{finish();reject(new ClientError("validation","Signature image could not be decoded."));};
    try{image.src=url;}catch(error){finish();reject(error);}
  });
  const loadFile=async(file:File):Promise<void>=>{
    editable();cancel();mode="upload";const request=generation;pending=true;resetError();update();
    try{
      if(!(file instanceof File))throw new ClientError("validation","Signature upload must be a file.");
      if(file.size>SIGNATURE_INPUT_BYTES)throw new ClientError("validation","Signature image exceeds the 2 MiB input limit.");
      if(file.type!=="image/png"&&file.type!=="image/jpeg")throw new ClientError("validation","Signature images must be PNG or JPEG.");
      const data=await read(file,request);if(data===undefined||dead||request!==generation)return;
      await paintImage(data,file.type,request,true);
    }catch(error){if(!dead&&request===generation){pending=false;failure(error);}throw error;}
  };
  const exportPng=():string|null=>{
    live();if(pending)throw new ClientError("validation","Wait for the signature image to finish loading.");if(invalid)throw new ClientError("validation","Correct the invalid signature image before exporting.");if(empty)return null;
    try{const value=canvas.toDataURL("image/png");if(!value.startsWith("data:image/png;base64,"))throw new ClientError("unsupported","Signature PNG export is unavailable.");
      if(bytes(value)>SIGNATURE_OUTPUT_BYTES)throw new ClientError("validation","Signature export exceeds the 2 MiB output limit.");return value;
    }catch(error){failure(error);throw error;}
  };
  const onName=():void=>safe(()=>{editable();cancel();resetError();drawName();update();notify();});
  const onFont=():void=>safe(()=>{editable();if(!FONTS.includes(fonts.value as typeof FONTS[number]))throw new ClientError("validation","Invalid signature font.");currentFont=fonts.value as typeof FONTS[number];cancel();resetError();drawName();update();notify();});
  const onUpload=():void=>{const file=upload.files?.[0];if(!file)return;void loadFile(file).catch(()=>{ /* loadFile already exposes and reports this failure; drain the browser event promise. */ });upload.value="";};
  const point=(event:PointerEvent):{x:number;y:number}=>{const box=canvas.getBoundingClientRect();if(box.width<=0||box.height<=0)throw new ClientError("validation","Signature drawing area has no visible size.");return{x:Math.max(0,Math.min(canvas.width,(event.clientX-box.left)*canvas.width/box.width)),y:Math.max(0,Math.min(canvas.height,(event.clientY-box.top)*canvas.height/box.height))};};
  const onDown=(event:PointerEvent):void=>{if(dead||options.readonly||mode!=="draw"||event.button!==0)return;safe(()=>{cancel();resetError();pointer=event.pointerId;const p=point(event);context.lineWidth=3;context.lineCap="round";context.lineJoin="round";context.strokeStyle="#17232e";context.fillStyle="#17232e";context.beginPath();context.arc(p.x,p.y,1.5,0,Math.PI*2);context.fill();context.beginPath();context.moveTo(p.x,p.y);empty=false;canvas.setPointerCapture?.(pointer);update();event.preventDefault();});};
  const onMove=(event:PointerEvent):void=>{if(pointer===undefined||event.pointerId!==pointer||dead)return;safe(()=>{const p=point(event);context.lineTo(p.x,p.y);context.stroke();event.preventDefault();});};
  const onUp=(event:PointerEvent):void=>{if(pointer===undefined||event.pointerId!==pointer||dead)return;const captured=pointer;pointer=undefined;if(canvas.hasPointerCapture?.(captured))canvas.releasePointerCapture(captured);notify();};
  name.addEventListener("input",onName);fonts.addEventListener("change",onFont);upload.addEventListener("change",onUpload);canvas.addEventListener("pointerdown",onDown);canvas.addEventListener("pointermove",onMove);canvas.addEventListener("pointerup",onUp);canvas.addEventListener("pointercancel",onUp);
  const dispose=():void=>{if(dead)return;dead=true;cancel();options.signal?.removeEventListener("abort",dispose);name.removeEventListener("input",onName);fonts.removeEventListener("change",onFont);upload.removeEventListener("change",onUpload);canvas.removeEventListener("pointerdown",onDown);canvas.removeEventListener("pointermove",onMove);canvas.removeEventListener("pointerup",onUp);canvas.removeEventListener("pointercancel",onUp);update();};
  options.signal?.addEventListener("abort",dispose,{once:true});
  if(options.signal?.aborted)dispose();
  else if(options.initialImage!==undefined){pending=true;update();const request=generation;void(async()=>{try{const parsed=signatureDataUrlBytes(options.initialImage!);await paintImage(parsed.bytes,parsed.mime,request,false);}catch(error){if(!dead&&request===generation){pending=false;failure(error);}}})();}
  else{if(mode==="type")drawName();update();}
  return{node,canvas,state,exportPng,setMode,clear,loadFile,dispose};
}
