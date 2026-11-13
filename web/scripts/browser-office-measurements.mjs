/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Computed sRGB colors only; an unknown color or missing backing is an evidence error. */
export function contrastOnLayers(foreground, backgrounds) {
  const parse = value => {
    const hex = /^#([a-f0-9]{6})([a-f0-9]{2})?$/i.exec(value);
    if(hex) return [0,2,4].map(i=>parseInt(hex[1].slice(i,i+2),16)).concat(hex[2] ? parseInt(hex[2],16)/255 : 1);
    const match = /^rgba?\(([^)]+)\)$/.exec(value);
    if (!match) throw new Error(`Unsupported computed color: ${value}`);
    const parts = match[1].split(/[,\s/]+/).filter(Boolean).map(Number);
    if ((parts.length !== 3 && parts.length !== 4) || parts.some(n => !Number.isFinite(n))) throw new Error(`Unsupported computed color: ${value}`);
    return [...parts.slice(0, 3), parts[3] ?? 1];
  };
  const composite = (front, back) => front.slice(0, 3).map((v, i) => v * front[3] + back[i] * (1 - front[3]));
  const layers = backgrounds.map(parse), opaque = layers.findIndex(layer => layer[3] === 1);
  if (opaque < 0) throw new Error("Contrast needs an opaque ancestor background.");
  let background = layers[opaque].slice(0, 3);
  for (let i = opaque - 1; i >= 0; i--) background = composite(layers[i], background);
  const fg = composite(parse(foreground), background);
  const luminance = color => color.map(c => c / 255).map(c => c <= .04045 ? c / 12.92 : ((c + .055) / 1.055) ** 2.4)
    .reduce((sum, c, i) => sum + c * [.2126, .7152, .0722][i], 0);
  const a = luminance(fg), b = luminance(background);
  return { foreground: fg, background, ratio: (Math.max(a, b) + .05) / (Math.min(a, b) + .05) };
}

/** Runs inside the fixture browser. Records all rendered text pairs, plus reusable control geometry. */
export function measureOfficeScene(contrast) {
  const visible = node => {
    const r = node.getBoundingClientRect(), s = getComputedStyle(node);
    return r.width > 0 && r.height > 0 && s.visibility === "visible" && s.display !== "none";
  };
  const identify = node => `${node.tagName.toLowerCase()}${node.id ? `#${node.id}` : ""}.${typeof node.className === "string" ? node.className.trim().replaceAll(" ", ".") : node.getAttribute("class") ?? ""}`;
  const layers = node => { const result = []; for (let n = node; n; n = n.parentElement) result.push(getComputedStyle(n).backgroundColor); return result; };
  const unresolved = node => {
    const reasons=[];
    for(let n=node;n;n=n.parentElement) {
      const style=getComputedStyle(n);
      if(Number(style.opacity)!==1) reasons.push(`opacity=${style.opacity} on ${identify(n)}`);
      if(style.backgroundImage!=="none") reasons.push(`background-image on ${identify(n)}`);
      if(style.filter!=="none") reasons.push(`filter on ${identify(n)}`);
    }
    return reasons;
  };
  const controls = [...document.querySelectorAll('button,input:not([type=hidden]),select,textarea,summary,[role=tab],.field-read-value,.control-panel,.list-table th,.list-table td,.content-sheet,.erp-mail-chatter,.addon-info-title')]
    .filter(visible).map(node => {
      const s = getComputedStyle(node), r = node.getBoundingClientRect();
      return { element: identify(node), label: node.getAttribute("aria-label") ?? node.getAttribute("name") ?? node.textContent.trim().slice(0, 80),
        rect: r.toJSON(), padding: s.padding, radius: s.borderRadius, fontSize: s.fontSize, lineHeight: s.lineHeight,
        background: s.backgroundColor, color: s.color, border: s.borderColor, borderWidth: s.borderWidth,
        outline: s.outline, disabled: node.matches(':disabled,[aria-disabled=true]'),
        icons: [...node.querySelectorAll('svg.erp-icon')].map(icon => ({ width: icon.getBoundingClientRect().width, height: icon.getBoundingClientRect().height, stroke: getComputedStyle(icon).strokeWidth })) };
    });
  const colorChain = node => { const chain=[];for(let n=node;n;n=n.parentElement) { const style=getComputedStyle(n);chain.push({element:identify(n),color:style.color,textToken:style.getPropertyValue('--text').trim()}); }return chain; };
  const texts = [], walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let text = walker.nextNode(); text; text = walker.nextNode()) {
    const node = text.parentElement;
    if (!text.textContent.trim() || !node || node.matches('script,style,option') || !visible(node)) continue;
    const range = document.createRange(); range.selectNodeContents(text);
    if (!range.getBoundingClientRect().width) continue;
    const s = getComputedStyle(node);
    texts.push({ element: identify(node), text: text.textContent.trim().slice(0, 100), color: s.color,
      ancestors: colorChain(node), unresolved: unresolved(node), layers: layers(node), disabled: !!node.closest(':disabled,[aria-disabled=true]'), ...contrast(s.color, layers(node)) });
  }
  for (const node of document.querySelectorAll('input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=color]):not([type=range]):not([type=file]),textarea')) {
    if (!visible(node)) continue;
    const placeholder = !node.value && !!node.placeholder, s = getComputedStyle(node, placeholder ? '::placeholder' : null);
    texts.push({ element: identify(node), text: placeholder ? '[placeholder]' : '[input value]', color: s.color,
      unresolved: unresolved(node), layers: layers(node), disabled: node.disabled, ...contrast(s.color, layers(node)) });
  }
  const root = getComputedStyle(document.documentElement), tokens = {};
  for (const name of [...root].filter(name => name.startsWith('--'))) tokens[name] = root.getPropertyValue(name).trim();
  const pairs = [];
  if (document.documentElement.dataset.officeTheme === "office-blue") {
    const color = token => root.getPropertyValue(token).trim();
    for (const [fg,bg,minimum,role] of [
      ['--text','--surface',4.5,'body'],['--muted','--canvas',4.5,'secondary'],
      ['--on-accent','--accent',4.5,'primary'],['--on-accent','--accent-hover',4.5,'primary hover'],
      ['--selected-text','--selected',4.5,'selected'],['--selected-text','--selected-hover',4.5,'selected hover'],
      ['--danger','--danger-bg',4.5,'error'],['--warning','--warning-bg',4.5,'warning'],['--success','--success-bg',4.5,'success'],
      ['--office-nav-fg','--office-nav-bg',4.5,'navigation'],['--office-nav-fg','--office-nav-hover',4.5,'navigation hover'],
      ['--office-nav-fg','--office-nav-selected',4.5,'navigation selected'],['--office-nav-muted','--office-nav-bg',4.5,'navigation secondary'],
      ['--input-line','--surface',3,'input boundary'],['--input-line','--surface-subtle',3,'search boundary'],
      ['--focus','--surface',3,'light focus'],['--focus','--canvas',3,'canvas focus'],
      ['--office-nav-focus','--office-nav-bg',3,'nav focus'],['--office-nav-focus','--office-nav-selected',3,'selected nav focus'],
    ]) pairs.push({fg,bg,minimum,role,...contrast(color(fg),[color(bg)])});
  }
  return { pairs, viewport: { width: innerWidth, height: innerHeight }, office: document.documentElement.dataset.officeTheme ?? null,
    controls, texts, tokens, scrollWidth: document.documentElement.scrollWidth };
}

export const officeMeasurementExpression = `(${measureOfficeScene.toString()})(${contrastOnLayers.toString()})`;
