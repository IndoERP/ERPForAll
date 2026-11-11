/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from './dom.js';
import { editorEnvironment, editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT, type EditorEnvironment } from './editor-types.js';
import { openEditorOverlay, type EditorOverlay } from './editor-overlay.js';
import type { Field } from './metadata.js';
import { ClientError } from './transport.js';

export interface ModelFieldSelectorOptions {
  environment: EditorEnvironment; label: string; model: string; path: string;
  fields(model: string, signal: AbortSignal): ReadonlyMap<string, Field> | Promise<ReadonlyMap<string, Field>>;
  readonly?: boolean; allowEmpty?: boolean; followRelation?: boolean | ((field: Field) => boolean);
  filter?: (field: Field) => boolean; sort?: (left: Field, right: Field) => number; touch?: boolean;
  change(path: string): void | Promise<void>;
}
export interface ModelFieldSelectorHandle { node: HTMLElement; ready: Promise<void>; dispose(): void }
let identity = 0;
const modelPattern = /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/;
const fieldPattern = /^[a-zA-Z_][a-zA-Z0-9_]*$/;
function pathSegments(path: string): string[] {
  editorText(path, 'Field path');
  const segments = path === '' ? [] : path.split('.');
  if (new TextEncoder().encode(path).length > 1024 || segments.length > 32) throw new ClientError('validation', 'Field path must contain at most 32 segments and 1024 bytes.');
  return segments;
}
/** Browse caller-authorized field maps without discovering models or granting field access. */
export function createModelFieldSelector(options: ModelFieldSelectorOptions): ModelFieldSelectorHandle {
  editorOptions(options, ['environment','label','model','path','fields','readonly','allowEmpty','followRelation','filter','sort','touch','change'], 'Model field selector');
  editorEnvironment(options.environment); editorText(options.label, 'Field selector label'); editorText(options.model, 'Field selector model', 256);
  if (!modelPattern.test(options.model)) throw new ClientError('validation', 'Invalid field selector model.');
  pathSegments(options.path);
  for (const key of ['readonly','allowEmpty','touch'] as const) if (options[key] !== undefined && typeof options[key] !== 'boolean') throw new ClientError('validation', 'Invalid field selector policy.');
  if (typeof options.fields !== 'function' || typeof options.change !== 'function' || options.filter !== undefined && typeof options.filter !== 'function' || options.sort !== undefined && typeof options.sort !== 'function' || options.followRelation !== undefined && typeof options.followRelation !== 'boolean' && typeof options.followRelation !== 'function') throw new ClientError('validation', 'Invalid field selector callback or relation policy.');
  const id = `editor-field-${++identity}`, node = el('div', undefined, 'editor-model-field-selector'); node.setAttribute('role', 'group');
  const label = el('span', options.label, 'editor-label'); label.id = `${id}-label`; node.setAttribute('aria-labelledby', label.id);
  const caption = el('span', undefined, 'editor-field-caption'), status = el('span', undefined, 'editor-field-status'); status.setAttribute('role', 'status');
  let path = options.path, dead = false, committing = false, epoch = 0, overlay: EditorOverlay | undefined;
  let pageEpoch = 0, pageAbort: AbortController | undefined;
  const lifetime = new AbortController();
  const trigger = button('', () => { void open(); }, 'editor-field-trigger'); trigger.dataset['fieldOpen'] = ''; trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-expanded', 'false');
  const clear = button('Clear', () => { void commit(''); }, 'editor-field-clear'); clear.dataset['fieldClear'] = ''; clear.setAttribute('aria-label', `Clear ${options.label}`);
  node.append(label, options.readonly ? caption : trigger, status);
  if (!options.readonly && options.allowEmpty) node.insertBefore(clear, status);
  const canFollow = (field: Field): boolean => {
    if (field.relation === null) return false;
    const result = typeof options.followRelation === 'function' ? options.followRelation(field) : options.followRelation === undefined ? true : options.followRelation;
    if (typeof result !== 'boolean') throw new ClientError('validation', 'Relation policy must return a boolean.');
    return result;
  };
  const allowed = (field: Field): boolean => {
    const result = options.filter === undefined ? true : options.filter(field);
    if (typeof result !== 'boolean') throw new ClientError('validation', 'Field filter must return a boolean.');
    return result;
  };
  const setState = (state: 'pending'|'valid'|'empty'|'invalid'|'error', text: string, message = ''): void => {
    node.dataset['state'] = state; caption.textContent = text; trigger.textContent = text || 'Choose a field…'; trigger.setAttribute('aria-invalid', String(state === 'invalid' || state === 'error')); status.textContent = message;
    if (state === 'invalid') status.setAttribute('role', 'alert'); else status.setAttribute('role', 'status');
  };
  const report = (error: unknown): void => {
    if (dead || lifetime.signal.aborted) return;
    setState('error', path, errorMessage(error).slice(0, 1024)); options.environment.report(error);
  };
  const load = async (model: string, signal: AbortSignal): Promise<ReadonlyMap<string, Field>> => {
    const fields = await options.fields(model, signal);
    if (!(fields instanceof Map) || fields.size > EDITOR_OPTIONS_LIMIT) throw new ClientError('validation', 'Field metadata must be a map of at most 1024 fields.');
    let bytes = 0;
    for (const [name, field] of fields) {
      editorText(name, 'Field identity', 256);
      if (!field || typeof field !== 'object' || !fieldPattern.test(name) || field.name !== name) throw new ClientError('validation', 'Invalid field metadata identity.');
      editorText(field.label, 'Field label');
      if (field.relation !== null) {
        editorText(field.relation, 'Field relation identity', 256);
        if (!modelPattern.test(field.relation)) throw new ClientError('validation', 'Invalid field relation metadata.');
      }
      bytes += new TextEncoder().encode(name + field.label + (field.relation ?? '')).length;
      if (bytes > EDITOR_OPTIONS_BYTES) throw new ClientError('validation', 'Field metadata text exceeds 262144 bytes.');
    }
    return fields;
  };
  const describe = async (): Promise<void> => {
    const generation = ++epoch; setState('pending', path, 'Loading field metadata…');
    try {
      const segments = pathSegments(path);
      if (!segments.length) { if (!dead && generation === epoch) setState(options.allowEmpty ? 'empty' : 'invalid', '', options.allowEmpty ? '' : 'Choose a field.'); return; }
      let model = options.model; const labels: string[] = [];
      for (const [index, name] of segments.entries()) {
        if (!fieldPattern.test(name)) { if (!dead && generation === epoch) setState('invalid', path, 'The field path is invalid.'); return; }
        const fields = await load(model, lifetime.signal);
        if (dead || generation !== epoch || lifetime.signal.aborted) return;
        const field = fields.get(name);
        if (!field || !allowed(field)) { setState('invalid', path, 'The field path is invalid.'); return; }
        labels.push(field.label);
        if (index < segments.length - 1) {
          if (!canFollow(field) || !field.relation) { setState('invalid', path, 'The field path cannot follow this relation.'); return; }
          model = field.relation;
        }
      }
      if (!dead && generation === epoch) setState('valid', labels.join(' / '));
    } catch (error) { if (!dead && generation === epoch && !lifetime.signal.aborted) report(error); }
  };
  const commit = async (next: string): Promise<void> => {
    if (dead || committing || options.readonly || lifetime.signal.aborted) return;
    if (next === '' && !options.allowEmpty) return;
    const previous = path; committing = true; node.setAttribute('aria-busy', 'true'); trigger.disabled = true; clear.disabled = true;
    try {
      pathSegments(next); path = next;
      await options.change(next);
      if (dead || lifetime.signal.aborted) return;
      trigger.disabled = false; clear.disabled = false; overlay?.close('select'); overlay = undefined; await describe();
    } catch (error) { path = previous; report(error); }
    finally { committing = false; if (!dead) { node.setAttribute('aria-busy', 'false'); trigger.disabled = false; clear.disabled = false; } }
  };
  const open = async (): Promise<void> => {
    if (dead || committing || options.readonly || lifetime.signal.aborted) return;
    if (overlay) { overlay.close('cancel'); return; }
    const content = el('div', undefined, 'editor-field-content');
    const input = el('input', undefined, 'editor-field-search'); input.type = 'search'; input.autocomplete = 'off'; input.setAttribute('aria-label', `Search ${options.label}`); input.setAttribute('role', 'combobox'); input.setAttribute('aria-expanded', 'true'); input.setAttribute('aria-autocomplete', 'list');
    const breadcrumb = el('div', options.model, 'editor-field-breadcrumb');
    const list = el('div', undefined, 'editor-choice-list'); list.id = `${id}-list`; list.setAttribute('role', 'listbox'); list.setAttribute('aria-label', options.label); input.setAttribute('aria-controls', list.id);
    const relations = el('div', undefined, 'editor-field-relations'); relations.setAttribute('role', 'group'); relations.setAttribute('aria-label', 'Browse related fields');
    const message = el('div', undefined, 'editor-field-message'); message.setAttribute('role', 'status');
    const emptyStatus = el('div', undefined, 'visually-hidden'); emptyStatus.setAttribute('role', 'status');
    content.append(breadcrumb, input, list, relations, emptyStatus, message);
    const prefixes: string[] = [], pageModels = [options.model]; let fields: Field[] = [], visible: Field[] = [], active = -1;
    const focus = (index: number): void => {
      active = index; const items = list.querySelectorAll<HTMLElement>('[role=option]'); items.forEach((item, i) => item.classList.toggle('active', active === i));
      const item = items[index]; if (item) { input.setAttribute('aria-activedescendant', item.id); item.scrollIntoView?.({ block: 'nearest' }); } else input.removeAttribute('aria-activedescendant');
    };
    const follow = async (field: Field): Promise<void> => {
      if (dead || committing || !overlay || lifetime.signal.aborted) return;
      try {
        if (!canFollow(field) || !field.relation) return;
        if (prefixes.length >= 31 || new TextEncoder().encode([...prefixes, field.name].join('.')).length > 1024) throw new ClientError('validation', 'Field path must contain at most 32 segments and 1024 bytes.');
        prefixes.push(field.name); pageModels.push(field.relation); input.value = ''; await page();
      } catch (error) {
        if (dead || !overlay || lifetime.signal.aborted) return;
        message.textContent = errorMessage(error).slice(0, 1024); message.setAttribute('role', 'alert'); options.environment.report(error);
      }
    };
    const render = (): void => {
      list.replaceChildren(); relations.replaceChildren(); active = -1; input.removeAttribute('aria-activedescendant');
      const query = input.value.trim().toLocaleLowerCase(); visible = fields.filter(field => `${field.name} ${field.label}`.toLocaleLowerCase().includes(query));
      visible.forEach((field, index) => {
        const item = el('div', field.label, 'editor-choice-option'); item.id = `${id}-field-${index}`; item.dataset['fieldName'] = field.name; item.setAttribute('role', 'option'); item.setAttribute('aria-selected', String(path === [...prefixes, field.name].join('.')));
        item.addEventListener('mousedown', event => event.preventDefault());
        item.addEventListener('pointermove', event => {
          if (event.pointerType !== 'mouse' && event.pointerType !== 'pen') return;
          if (!Number.isFinite(event.movementX) || !Number.isFinite(event.movementY)) return;
          if (event.movementX !== 0 || event.movementY !== 0) focus(index);
        });
        item.addEventListener('click', () => { void commit([...prefixes, field.name].join('.')); }); list.append(item);
        if (canFollow(field)) { const more = button(`Fields of ${field.label}`,  () => { void follow(field); }, 'editor-field-follow'); more.setAttribute('aria-label', `Fields of ${field.label}`); relations.append(more); }
      });
      relations.hidden = relations.childElementCount === 0;
      emptyStatus.textContent = visible.length ? '' : 'No matching fields';
      if (!visible.length) { const empty = el('div', 'No matching fields', 'editor-choice-empty'); empty.setAttribute('role', 'option'); empty.setAttribute('aria-disabled', 'true'); empty.setAttribute('aria-selected', 'false'); list.append(empty); }
    };
    const page = async (): Promise<void> => {
      const generation = ++pageEpoch; pageAbort?.abort(); pageAbort = new AbortController();
      const abort = (): void => pageAbort?.abort(); lifetime.signal.addEventListener('abort', abort, { once: true });
      fields = []; visible = []; active = -1; input.removeAttribute('aria-activedescendant'); list.replaceChildren(); relations.replaceChildren(); relations.hidden = true; input.setAttribute('aria-busy', 'true'); emptyStatus.textContent = ''; message.textContent = 'Loading fields…';
      try {
        const model = pageModels.at(-1)!; const loaded = await load(model, pageAbort.signal);
        if (dead || !overlay || generation !== pageEpoch) return;
        fields = [...loaded.values()].filter(allowed); if (options.sort) fields.sort((left, right) => { const order = options.sort!(left, right); if (!Number.isFinite(order)) throw new ClientError('validation', 'Field sort must return a finite number.'); return order; }); else fields.sort((left, right) => left.label.localeCompare(right.label));
        breadcrumb.textContent = [options.model, ...prefixes].join(' / '); message.textContent = ''; render();
      } catch (error) { if (!dead && overlay && generation === pageEpoch && !pageAbort.signal.aborted) { list.replaceChildren(); message.textContent = errorMessage(error).slice(0, 1024); options.environment.report(error); } }
      finally { lifetime.signal.removeEventListener('abort', abort); if (generation === pageEpoch) input.removeAttribute('aria-busy'); }
    };
    input.addEventListener('input', () => { try { editorText(input.value, 'Field search', 1024); render(); } catch (error) { visible = []; active = -1; list.replaceChildren(); input.removeAttribute('aria-activedescendant'); message.textContent = errorMessage(error); options.environment.report(error); } });
    input.addEventListener('keydown', event => {
      if (event.isComposing) return;
      if (event.key === 'ArrowDown' && input.selectionStart === input.value.length) { event.preventDefault(); if (visible.length) focus(Math.min(visible.length - 1, active + 1)); }
      else if (event.key === 'ArrowUp' && input.selectionStart === 0) { event.preventDefault(); if (visible.length) focus(Math.max(0, active - 1)); }
      else if (event.key === 'ArrowRight' && input.selectionStart === input.value.length && visible[active]) { event.preventDefault(); void follow(visible[active]!); }
      else if (event.key === 'ArrowLeft' && input.selectionStart === 0 && prefixes.length) { event.preventDefault(); prefixes.pop(); pageModels.pop(); input.value = ''; void page(); }
      else if (event.key === 'Enter') { event.preventDefault(); const field = visible[active]; if (field) void commit([...prefixes, field.name].join('.')); }
      else if (event.key === 'Escape') { event.preventDefault(); overlay?.close('cancel'); }
    });
    overlay = openEditorOverlay({ anchor: trigger, content, environment: options.environment, title: options.label, ...(options.touch === undefined ? {} : { touch: options.touch }), onClose: () => { pageEpoch++; pageAbort?.abort(); overlay = undefined; trigger.setAttribute('aria-expanded', 'false'); } });
    trigger.setAttribute('aria-expanded', 'true'); input.focus(); await page();
  };
  const dispose = (): void => { if (dead) return; dead = true; epoch++; pageEpoch++; lifetime.abort(); pageAbort?.abort(); overlay?.dispose(); overlay = undefined; options.environment.signal?.removeEventListener('abort', dispose); };
  options.environment.signal?.addEventListener('abort', dispose, { once: true }); if (options.environment.signal?.aborted) dispose();
  const ready = dead ? Promise.resolve() : describe();
  return { node, ready, dispose };
}
