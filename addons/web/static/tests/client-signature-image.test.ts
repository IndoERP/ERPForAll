/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { fitSignatureImage, inspectSignatureImage, signatureDataUrlBytes, SIGNATURE_INPUT_BYTES, SIGNATURE_OUTPUT_BYTES } from "../src/client/signature-image.js";
import { ClientError } from "../src/client/transport.js";

function png(width = 2, height = 1): Uint8Array {
  const bytes = new Uint8Array(58), view = new DataView(bytes.buffer);
  bytes.set([137,80,78,71,13,10,26,10]); view.setUint32(8,13);
  bytes.set([73,72,68,82],12); view.setUint32(16,width); view.setUint32(20,height); bytes[24]=8; bytes[25]=6;
  view.setUint32(33,1); bytes.set([73,68,65,84],37); view.setUint32(46,0); bytes.set([73,69,78,68],50);
  return bytes;
}
function failure(job: () => unknown, kind: ClientError["kind"], message: string): void {
  let error: unknown; try { job(); } catch (caught) { error = caught; }
  expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({kind,message});
}
/// (partial: raster header admission and fitting; actual image decoding is browser acceptance)
test("signature image admits bounded PNG identity and fits without changing aspect ratio", () => {
  expect(inspectSignatureImage(png(),"image/png")).toEqual({mime:"image/png",width:2,height:1});
  expect(fitSignatureImage(800,200,400,200)).toEqual({x:0,y:50,width:400,height:100});
  expect(fitSignatureImage(100,400,400,200)).toEqual({x:175,y:0,width:50,height:200});
  expect(inspectSignatureImage(png(2048,2048),"image/png").width).toBe(2048);
});
test("signature image refuses wrong types, truncated chunks, MIME mismatch and dimensions before decode", () => {
  failure(()=>inspectSignatureImage(png(),"image/svg+xml"),"validation","Signature images must be PNG or JPEG.");
  failure(()=>inspectSignatureImage(png(),"image/jpeg"),"validation","Signature image content does not match its declared type.");
  failure(()=>inspectSignatureImage(png().slice(0,30),"image/png"),"validation","Invalid or truncated PNG signature image.");
  failure(()=>inspectSignatureImage(png(2049),"image/png"),"validation","Signature image dimensions must be between 1 and 2048 pixels.");
  failure(()=>inspectSignatureImage(png(0),"image/png"),"validation","Signature image dimensions must be between 1 and 2048 pixels.");
  failure(()=>inspectSignatureImage(new Uint8Array(SIGNATURE_INPUT_BYTES+1),"image/png"),"validation","Signature image exceeds the 2 MiB input limit.");
});
test("signature JPEG dimensions come from a bounded frame header", () => {
  const jpeg=new Uint8Array([255,216,255,192,0,11,8,0,2,0,3,1,1,17,0,255,218,0,8,1,1,0,0,63,0,1,255,217]);
  expect(inspectSignatureImage(jpeg,"image/jpeg")).toEqual({mime:"image/jpeg",width:3,height:2});
  failure(()=>inspectSignatureImage(jpeg.slice(0,-2),"image/jpeg"),"validation","Invalid or truncated JPEG signature image.");
  const broken=jpeg.slice(); broken[4]=255;
  failure(()=>inspectSignatureImage(broken,"image/jpeg"),"validation","Invalid or truncated JPEG signature image.");
});
test("initial signature URLs are local bounded raster data and export cap is explicit", () => {
  const url="data:image/png;base64,"+btoa(String.fromCharCode(...png()));
  expect(signatureDataUrlBytes(url).mime).toBe("image/png");
  expect(signatureDataUrlBytes(url).bytes).toEqual(png());
  failure(()=>signatureDataUrlBytes("https://example.invalid/signature.png"),"validation","Signature image must be a PNG or JPEG base64 data URL.");
  failure(()=>signatureDataUrlBytes("data:image/svg+xml;base64,PHN2Zz4="),"validation","Signature image must be a PNG or JPEG base64 data URL.");
  failure(()=>signatureDataUrlBytes("data:image/png;base64,%%%"),"validation","Signature image must be a PNG or JPEG base64 data URL.");
  expect(SIGNATURE_OUTPUT_BYTES).toBe(2*1024*1024);
});
