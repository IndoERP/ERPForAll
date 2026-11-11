/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from './dom.js';
import { openEditorOverlay, type EditorOverlay } from './editor-overlay.js';
import { editorEnvironment, editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT, type EditorEnvironment, type EditorHandle } from './editor-types.js';
import { ClientError } from './transport.js';

export type ChoiceValue = string | number | boolean;
export interface SelectChoice { readonly value: ChoiceValue; readonly label: string }
export interface SelectGroup { readonly label: string; readonly choices: readonly SelectChoice[] }
export interface SelectMenuOptions {
  environment: EditorEnvironment; label: string; choices: readonly SelectChoice[]; groups?: readonly SelectGroup[];
  value: ChoiceValue | null | readonly ChoiceValue[]; multiple?: boolean; required?: boolean; searchable?: boolean; disabled?: boolean; touch?: boolean;
  change(value: ChoiceValue | null | readonly ChoiceValue[]): void | Promise<void>;
}
let identity = 0;
const normalize = (value: string): string => value.normalize('NFD').replace(/\p{M}/gu, '').toLocaleLowerCase();
/** A searchable local choice menu. Choice identities never come from display labels. */
export function createSelectMenu(options: SelectMenuOptions): EditorHandle {
  editorOptions(options, ['environment','label','choices','groups','value','multiple','required','searchable','disabled','touch','change'], 'Select menu');
  editorEnvironment(options.environment); editorText(options.label, 'Select menu label');
  for (const key of ['multiple','required','searchable','disabled','touch'] as const) if (options[key] !== undefined && typeof options[key] !== 'boolean') throw new ClientError('validation', 'Invalid select menu policy.');
  if (typeof options.change !== 'function') throw new ClientError('validation', 'Select menu change must be a function.');
  if (!Array.isArray(options.choices) || options.groups !== undefined && !Array.isArray(options.groups)) throw new ClientError('validation', 'Select choices and groups must be arrays.');
  const admissionMessage = 'Select choices exceed 1024 entries or 262144 text bytes.';
  const suppliedGroups = options.groups === undefined ? [] : options.groups;
  // Counts are checked before spreading, iterating or reading array entries.
  if (suppliedGroups.length >= EDITOR_OPTIONS_LIMIT || options.choices.length > EDITOR_OPTIONS_LIMIT) throw new ClientError('validation', admissionMessage);
  const groups: SelectGroup[] = [], seen = new Map<ChoiceValue, SelectChoice>(); let totalBytes = 0, count = 0;
  const admit = (group: SelectGroup): void => {
    editorOptions(group, ['label','choices'], 'Choice group'); editorText(group.label, 'Choice group label'); totalBytes += new TextEncoder().encode(group.label).length;
    if (totalBytes > EDITOR_OPTIONS_BYTES) throw new ClientError('validation', admissionMessage);
    if (!Array.isArray(group.choices)) throw new ClientError('validation', 'Choice group choices must be an array.');
    if (group.choices.length > EDITOR_OPTIONS_LIMIT - count) throw new ClientError('validation', admissionMessage);
    const admitted: SelectChoice[] = [];
    for (const choice of group.choices) {
      editorOptions(choice, ['value','label'], 'Choice'); editorText(choice.label, 'Choice label');
      if (typeof choice.value !== 'string' && typeof choice.value !== 'number' && typeof choice.value !== 'boolean' || typeof choice.value === 'number' && !Number.isFinite(choice.value)) throw new ClientError('validation', 'Choice identity must be text, a finite number or a boolean.');
      if (typeof choice.value === 'string') editorText(choice.value, 'Choice identity');
      totalBytes += new TextEncoder().encode(choice.label + String(choice.value)).length;
      if (totalBytes > EDITOR_OPTIONS_BYTES) throw new ClientError('validation', admissionMessage);
      if (seen.has(choice.value)) throw new ClientError('validation', `Duplicate choice identity: ${String(choice.value)}.`);
      const snapshot = { value: choice.value, label: choice.label }; seen.set(snapshot.value, snapshot); admitted.push(snapshot); count++;
    }
    groups.push({ label: group.label, choices: admitted });
  };
  admit({ label: '', choices: options.choices });
  for (const group of suppliedGroups) admit(group);
  const multiple = options.multiple === true;
  if (multiple !== Array.isArray(options.value)) throw new ClientError('validation', 'Selected value must match the select menu mode.');
  if (Array.isArray(options.value) && options.value.length > EDITOR_OPTIONS_LIMIT) throw new ClientError('validation', 'Selected choices exceed 1024 entries.');
  let values: ChoiceValue[] = Array.isArray(options.value) ? [...options.value] : options.value === null ? [] : [options.value as ChoiceValue];
  const selected = new Set<ChoiceValue>();
  for (const value of values) {
    if (!seen.has(value)) throw new ClientError('validation', `Selected choice is not available: ${String(value)}.`);
    if (selected.has(value)) throw new ClientError('validation', `Duplicate selected identity: ${String(value)}.`);
    selected.add(value);
  }
  const id = `editor-select-${++identity}`, node = el('div', undefined, 'editor-select-menu'); node.setAttribute('role', 'group');
  const caption = el('span', options.label, 'editor-label'); caption.id = `${id}-label`; node.setAttribute('aria-labelledby', caption.id);
  const opener = button('', () => open(), 'editor-select-trigger'); opener.setAttribute('aria-label', options.label); opener.setAttribute('aria-haspopup', 'listbox'); opener.setAttribute('aria-expanded', 'false'); opener.disabled = options.disabled === true;
  const clear = button('Clear', () => { void choose(null); }, 'editor-select-clear'); clear.dataset['selectClear'] = ''; clear.setAttribute('aria-label', `Clear ${options.label}`);
  const status = el('span', undefined, 'editor-select-status'); status.setAttribute('role', 'status');
  node.append(caption, opener, status);
  let overlay: EditorOverlay | undefined, search: HTMLInputElement | undefined, list: HTMLElement | undefined, emptyStatus: HTMLElement | undefined;
  let visible: SelectChoice[] = [], active = -1, dead = false, pending = false;
  const display = (): void => {
    opener.textContent = values.length ? values.map(value => seen.get(value)!.label).join(', ') : 'Choose…';
    opener.disabled = options.disabled === true || pending; clear.disabled = pending; node.setAttribute('aria-busy', String(pending));
    clear.remove(); if (!multiple && !options.required && values.length && !options.disabled) node.insertBefore(clear, status);
  };
  const fail = (error: unknown): void => { if (dead || options.environment.signal?.aborted) return; status.textContent = errorMessage(error).slice(0, 1024); options.environment.report(error); };
  const close = (reason: 'select' | 'cancel' | 'dispose', restore = true): void => { overlay?.close(reason, restore); overlay = undefined; opener.setAttribute('aria-expanded', 'false'); };
  const choose = async (value: ChoiceValue | null, tab = false): Promise<void> => {
    if (dead || pending || options.disabled || options.environment.signal?.aborted) return;
    if (value === null && (multiple || options.required)) return;
    const next = value === null ? [] : multiple ? values.includes(value) ? values.filter(item => item !== value) : [...values, value] : [value];
    const previous = values; values = next; pending = true; status.textContent = ''; display(); render();
    try {
      await options.change(multiple ? [...next] : next[0] ?? null);
      if (!dead && (!multiple || tab)) {
        const focused = node.ownerDocument.activeElement;
        // Tab may already have moved to a live external control while the owner awaited its
        // callback. Preserve that focus; otherwise return it before removing the popup.
        const restore = !tab || !(focused instanceof HTMLElement) || focused === node.ownerDocument.body || !focused.isConnected || overlay?.node.contains(focused) === true;
        opener.disabled = false; close('select', restore);
      }
    } catch (error) { values = previous; fail(error); }
    finally { pending = false; if (!dead) { display(); render(); } }
  };
  const highlight = (index: number): void => {
    active = index; const items = list?.querySelectorAll<HTMLElement>('[role=option]');
    items?.forEach((item, i) => item.classList.toggle('active', i === active));
    const item = items?.[active]; const focusOwner = options.searchable === false ? list : search;
    if (item) { focusOwner?.setAttribute('aria-activedescendant', item.id); item.scrollIntoView?.({ block: 'nearest' }); } else focusOwner?.removeAttribute('aria-activedescendant');
  };
  const render = (): void => {
    if (!list || !search) return;
    list.replaceChildren(); visible = []; active = -1; search.removeAttribute('aria-activedescendant'); list.removeAttribute('aria-activedescendant');
    const query = normalize(search.value.trim());
    for (const group of groups) {
      const matches = group.choices.filter(choice => normalize(choice.label).includes(query));
      if (!matches.length) continue;
      if (group.label) { const heading = el('div', group.label, 'editor-choice-heading'); heading.setAttribute('role', 'presentation'); list.append(heading); }
      for (const choice of matches) {
        const index = visible.length; visible.push(choice);
        const item = el('div', choice.label, 'editor-choice-option'); item.id = `${id}-option-${index}`; item.setAttribute('role', 'option'); item.setAttribute('aria-selected', String(values.includes(choice.value))); item.setAttribute('aria-disabled', String(pending)); item.dataset['value'] = String(choice.value);
        item.addEventListener('mousedown', event => event.preventDefault());
        // Enter events can be generated by layout beneath a stationary pointer. Only actual
        // mouse/pen movement transfers navigation ownership away from a keyboard choice.
        item.addEventListener('pointermove', event => {
          if (event.pointerType !== 'mouse' && event.pointerType !== 'pen') return;
          if (!Number.isFinite(event.movementX) || !Number.isFinite(event.movementY)) return;
          if (event.movementX !== 0 || event.movementY !== 0) highlight(index);
        });
        item.addEventListener('click', () => { void choose(choice.value); }); list.append(item);
      }
    }
    if (emptyStatus) emptyStatus.textContent = visible.length ? '' : 'No matching choices';
    if (!visible.length) {
      const empty = el('div', 'No matching choices', 'editor-choice-empty'); empty.setAttribute('role', 'option'); empty.setAttribute('aria-disabled', 'true'); empty.setAttribute('aria-selected', 'false'); list.append(empty);
    }
  };
  const key = (event: KeyboardEvent): void => {
    if (event.isComposing) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault(); if (visible.length) highlight(event.key === 'ArrowDown' ? Math.min(visible.length - 1, active + 1) : Math.max(0, active - 1));
    } else if (event.key === 'Enter' || event.key === 'Tab') {
      const item = visible[active] ?? (search?.value ? visible[0] : undefined); if (!item) return;
      // Both keys activate a choice. Consume Tab before native focus movement can run
      // after an asynchronous close; a subsequent Tab from the trigger advances normally.
      event.preventDefault(); void choose(item.value, event.key === 'Tab');

    } else if (event.key === 'Escape') { event.preventDefault(); close('cancel'); }
  };
  const open = (): void => {
    if (dead || pending || opener.disabled || options.environment.signal?.aborted) return;
    if (overlay) { close('cancel'); return; }
    const content = el('div', undefined, 'editor-select-content');
    search = el('input', undefined, 'editor-select-search'); search.type = 'search'; search.setAttribute('aria-label', `Search ${options.label}`); search.autocomplete = 'off';
    search.hidden = options.searchable === false;
    list = el('div', undefined, 'editor-choice-list'); list.id = `${id}-list`; list.setAttribute('role', 'listbox'); list.setAttribute('aria-label', options.label); if (multiple) list.setAttribute('aria-multiselectable', 'true');
    search.setAttribute('role', 'combobox'); search.setAttribute('aria-controls', list.id); search.setAttribute('aria-expanded', 'true'); search.setAttribute('aria-autocomplete', 'list');
    emptyStatus = el('div', undefined, 'visually-hidden'); emptyStatus.setAttribute('role', 'status');
    content.append(search, list, emptyStatus); search.addEventListener('input', () => { try { editorText(search!.value, 'Choice search', 1024); render(); } catch (error) { visible = []; active = -1; list?.replaceChildren(); search?.removeAttribute('aria-activedescendant'); fail(error); } }); content.addEventListener('keydown', key);
    overlay = openEditorOverlay({ anchor: opener, content, environment: options.environment, title: options.label, ...(options.touch === undefined ? {} : { touch: options.touch }), onClose: () => { overlay = undefined; opener.setAttribute('aria-expanded', 'false'); } });
    opener.setAttribute('aria-expanded', 'true'); render();
    if (options.searchable === false) { list.tabIndex = 0; list.focus(); } else search.focus();
  };
  opener.addEventListener('keydown', event => { if (event.key === ' ' || event.key === 'ArrowDown') { event.preventDefault(); open(); } });
  display();
  const dispose = (): void => { if (dead) return; dead = true; close('dispose', false); options.environment.signal?.removeEventListener('abort', dispose); };
  options.environment.signal?.addEventListener('abort', dispose, { once: true }); if (options.environment.signal?.aborted) dispose();
  return { node, dispose };
}
