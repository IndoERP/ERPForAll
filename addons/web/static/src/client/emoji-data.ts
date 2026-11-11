/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Independently authored, intentionally small catalogue. Glyphs use the platform emoji font.
 * Labels, keywords and categories are original project data; see emoji-data.LICENSE. */
export interface EmojiEntry { value: string; label: string; keywords: readonly string[]; category: string }
export const EMOJI_DATA: readonly EmojiEntry[] = [
  {value:"😀",label:"Bright smile",keywords:["happy","smile"],category:"Faces"},
  {value:"😊",label:"Warm smile",keywords:["thanks","smile"],category:"Faces"},
  {value:"😂",label:"Laughing tears",keywords:["laugh","funny"],category:"Faces"},
  {value:"🤔",label:"Considering",keywords:["think","question"],category:"Faces"},
  {value:"😐",label:"Neutral face",keywords:["neutral"],category:"Faces"},
  {value:"😮",label:"Surprised face",keywords:["surprise"],category:"Faces"},
  {value:"👍",label:"Thumb raised",keywords:["yes","approve"],category:"Gestures"},
  {value:"👋",label:"Waving hand",keywords:["hello","goodbye"],category:"Gestures"},
  {value:"👏",label:"Applause",keywords:["congratulations"],category:"Gestures"},
  {value:"🙏",label:"Hands together",keywords:["thanks","please"],category:"Gestures"},
  {value:"🎉",label:"Celebration",keywords:["party","done"],category:"Work"},
  {value:"✅",label:"Check mark",keywords:["complete","approved"],category:"Work"},
  {value:"💡",label:"Idea light",keywords:["idea","suggestion"],category:"Work"},
  {value:"📌",label:"Pinned item",keywords:["pin","remember"],category:"Work"},
  {value:"📅",label:"Calendar",keywords:["date","schedule"],category:"Work"},
  {value:"📦",label:"Parcel",keywords:["package","delivery"],category:"Work"},
  {value:"❤️",label:"Heart",keywords:["love","care"],category:"Symbols"},
  {value:"⭐",label:"Star",keywords:["favorite","highlight"],category:"Symbols"},
  {value:"🔥",label:"Flame",keywords:["fire","urgent"],category:"Symbols"},
  {value:"🌱",label:"Seedling",keywords:["grow","plant"],category:"Symbols"},
];
