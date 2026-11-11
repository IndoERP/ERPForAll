/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";

/** Browser-local admission limits; these do not grant attachment storage or signing authority. */
export const SIGNATURE_INPUT_BYTES = 2 * 1024 * 1024;
export const SIGNATURE_OUTPUT_BYTES = 2 * 1024 * 1024;
export const SIGNATURE_MAX_DIMENSION = 2048;
export interface SignatureImageInfo { mime: "image/png" | "image/jpeg"; width: number; height: number }
const PNG = [137,80,78,71,13,10,26,10] as const;
function invalid(format: "PNG" | "JPEG"): never { throw new ClientError("validation", `Invalid or truncated ${format} signature image.`); }
function dimensions(width: number, height: number): void {
  if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height) || width < 1 || height < 1 || width > SIGNATURE_MAX_DIMENSION || height > SIGNATURE_MAX_DIMENSION) {
    throw new ClientError("validation", "Signature image dimensions must be between 1 and 2048 pixels.");
  }
}
/** Inspect byte identity and bounded dimensions before asking the browser to decode image data.
 * Header admission is not a substitute for the browser decoder's corruption checks. */
export function inspectSignatureImage(bytes: Uint8Array, mime: string): SignatureImageInfo {
  if (!(bytes instanceof Uint8Array)) throw new ClientError("validation", "Signature image data must be bytes.");
  if (bytes.byteLength > SIGNATURE_INPUT_BYTES) throw new ClientError("validation", "Signature image exceeds the 2 MiB input limit.");
  if (mime !== "image/png" && mime !== "image/jpeg") throw new ClientError("validation", "Signature images must be PNG or JPEG.");
  if (mime === "image/png") {
    if (!PNG.every((value,index)=>bytes[index]===value)) {
      if (bytes.length < PNG.length) invalid("PNG");
      throw new ClientError("validation", "Signature image content does not match its declared type.");
    }
    const view = new DataView(bytes.buffer,bytes.byteOffset,bytes.byteLength);
    let offset=8, width=0, height=0, hasData=false, ended=false;
    while (offset < bytes.length) {
      if (bytes.length-offset < 12) invalid("PNG");
      const size=view.getUint32(offset);
      if (size > bytes.length-offset-12) invalid("PNG");
      const tag=String.fromCharCode(bytes[offset+4]!,bytes[offset+5]!,bytes[offset+6]!,bytes[offset+7]!);
      if (offset===8) {
        if (tag!=="IHDR" || size!==13) invalid("PNG");
        width=view.getUint32(offset+8);height=view.getUint32(offset+12);dimensions(width,height);
      } else if (tag==="IHDR") invalid("PNG");
      if (tag==="IDAT") hasData=true;
      offset+=size+12;
      if (tag==="IEND") { if (size!==0 || offset!==bytes.length || !hasData) invalid("PNG"); ended=true; break; }
    }
    if (!ended) invalid("PNG");
    return {mime,width,height};
  }
  if (bytes[0]!==255 || bytes[1]!==216) {
    if (bytes.length < 2) invalid("JPEG");
    throw new ClientError("validation", "Signature image content does not match its declared type.");
  }
  if (bytes.length < 4 || bytes[bytes.length-2]!==255 || bytes[bytes.length-1]!==217) invalid("JPEG");
  let offset=2, width=0, height=0, sawScan=false;
  while (offset < bytes.length-2) {
    if (bytes[offset++]!==255) invalid("JPEG");
    while (bytes[offset]===255) offset++;
    const marker=bytes[offset++];
    if (marker===undefined || marker===0 || marker===216 || marker===217) invalid("JPEG");
    if (marker===1 || marker>=208 && marker<=215) continue;
    if (offset+2>bytes.length) invalid("JPEG");
    const length=bytes[offset]!*256+bytes[offset+1]!;
    if (length<2 || length>bytes.length-offset) invalid("JPEG");
    // All SOF variants carry these dimensions; the browser decoder decides supported coding modes.
    if (marker>=192 && marker<=207 && marker!==196 && marker!==200 && marker!==204) {
      if (width!==0 || length<8) invalid("JPEG");
      height=bytes[offset+3]!*256+bytes[offset+4]!;width=bytes[offset+5]!*256+bytes[offset+6]!;dimensions(width,height);
      const components=bytes[offset+7]!;
      if (components<1 || components>4 || length!==8+3*components) invalid("JPEG");
    }
    if (marker===218) {
      if (!width || length<6 || offset+length>=bytes.length-2) invalid("JPEG");
      sawScan=true;break;
    }
    offset+=length;
  }
  if (!sawScan) invalid("JPEG");
  return {mime,width,height};
}
/** Only local raster base64 is admitted. External URLs and active SVG content never reach Image.src. */
export function signatureDataUrlBytes(url: string): {mime: "image/png" | "image/jpeg"; bytes: Uint8Array} {
  const message="Signature image must be a PNG or JPEG base64 data URL.";
  if (typeof url!=="string") throw new ClientError("validation",message);
  if (url.length>Math.ceil(SIGNATURE_INPUT_BYTES/3)*4+32) throw new ClientError("validation","Signature image exceeds the 2 MiB input limit.");
  const match=/^data:(image\/(?:png|jpeg));base64,([A-Za-z0-9+/]+={0,2})$/.exec(url);
  if (!match || match[2]!.length%4!==0) throw new ClientError("validation",message);
  let binary:string;
  try { binary=atob(match[2]!); } catch { throw new ClientError("validation",message); }
  // Require canonical padding, not merely a browser-tolerated base64 string.
  if (btoa(binary)!==match[2]) throw new ClientError("validation",message);
  const bytes=Uint8Array.from(binary,value=>value.charCodeAt(0));
  const mime=match[1] as "image/png" | "image/jpeg";
  inspectSignatureImage(bytes,mime);
  return {mime,bytes};
}
/** Center an admitted image without changing its aspect ratio. */
export function fitSignatureImage(width:number,height:number,canvasWidth:number,canvasHeight:number): {x:number;y:number;width:number;height:number} {
  dimensions(width,height);
  if (!Number.isSafeInteger(canvasWidth) || !Number.isSafeInteger(canvasHeight) || canvasWidth<1 || canvasHeight<1 || canvasWidth>SIGNATURE_MAX_DIMENSION || canvasHeight>SIGNATURE_MAX_DIMENSION) throw new ClientError("validation","Invalid signature canvas dimensions.");
  const ratio=Math.min(canvasWidth/width,canvasHeight/height),scaledWidth=width*ratio,scaledHeight=height*ratio;
  return {x:(canvasWidth-scaledWidth)/2,y:(canvasHeight-scaledHeight)/2,width:scaledWidth,height:scaledHeight};
}
