/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, labelled } from './dom.js';
import { createAutocomplete, type AutocompleteOption } from './autocomplete.js';
import { editorEnvironment, editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT, type EditorEnvironment } from './editor-types.js';
import { ClientError } from './transport.js';

export interface ModelChoice { readonly technical: string; readonly label: string }
export interface ModelSelectorOptions {
  environment: EditorEnvironment; label: string; models: readonly ModelChoice[]; value?: string | null;
  visibleLimit?: number; readonly?: boolean; touch?: boolean; select(model: ModelChoice): void | Promise<void>;
}
export interface ModelSelectorHandle { node: HTMLElement; input: HTMLInputElement; dispose(): void }
const bytes = (value: string): number => new TextEncoder().encode(value).length;
/** Select only caller-authorized model identities. Labels are presentation, never discovery authority. */
export function createModelSelector(options: ModelSelectorOptions): ModelSelectorHandle {
  editorOptions(options, ['environment','label','models','value','visibleLimit','readonly','touch','select'], 'Model selector');
  editorEnvironment(options.environment); editorText(options.label, 'Model selector label');
  if (!Array.isArray(options.models) || options.models.length > EDITOR_OPTIONS_LIMIT) throw new ClientError('validation', 'Model choices must be an array of at most 1024 entries.');
  if (typeof options.select !== 'function' || options.readonly !== undefined && typeof options.readonly !== 'boolean' || options.touch !== undefined && typeof options.touch !== 'boolean') throw new ClientError('validation', 'Invalid model selector policy.');
  const limit = options.visibleLimit === undefined ? 8 : options.visibleLimit;
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > 1000) throw new ClientError('validation', 'Model visible limit must be an integer from 1 to 1000.');
  const seen = new Set<string>(); let totalBytes = 0;
  const models: ModelChoice[] = [];
  for (const model of options.models) {
    editorOptions(model, ['technical','label'], 'Model choice'); editorText(model.technical, 'Model identity', 256); editorText(model.label, 'Model label');
    if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(model.technical)) throw new ClientError('validation', 'Invalid model identity.');
    if (seen.has(model.technical)) throw new ClientError('validation', `Duplicate model identity: ${model.technical}.`);
    seen.add(model.technical); totalBytes += bytes(model.technical) + bytes(model.label);
    if (totalBytes > EDITOR_OPTIONS_BYTES) throw new ClientError('validation', 'Model choice text exceeds 262144 bytes.');
    models.push({ technical: model.technical, label: model.label });
  }
  if (options.value !== undefined && options.value !== null) editorText(options.value, 'Selected model identity', 256);
  let value = options.value === undefined || options.value === null ? '' : options.value;
  const input = el('input', undefined, 'editor-model-input'); input.type = 'text'; input.autocomplete = 'off'; input.setAttribute('aria-label', options.label); input.readOnly = options.readonly === true;
  const node = labelled(options.label, input); node.classList.add('editor-model-selector');
  const status = el('span', undefined, 'editor-model-status'); status.setAttribute('role', 'status'); node.append(status);
  const render = (): void => {
    const model = models.find(item => item.technical === value);
    input.value = model ? model.label : value;
    node.dataset['state'] = value === '' ? 'empty' : model ? 'valid' : 'invalid';
    input.setAttribute('aria-invalid', String(value !== '' && !model)); status.textContent = value !== '' && !model ? 'Selected model is unavailable.' : '';
  };
  render();
  if (options.readonly) return { node, input, dispose: () => {} };
  let dead = false, selecting = false;
  const complete = createAutocomplete({ input, owner: node, environment: options.environment, debounceMs: 0, autoSelect: true, representedValue: () => models.find(item => item.technical === value)?.label ?? value,
    ...(options.touch === undefined ? {} : { touch: options.touch }),
    sources: [{ options: (query: string): readonly AutocompleteOption[] => {
      editorText(query, 'Model search', 1024);
      const normalized = query.trim().toLocaleLowerCase();
      const matches = normalized === '' ? models.slice(0, limit) : models.filter(model => `${model.technical} ${model.label}`.toLocaleLowerCase().includes(normalized));
      const choices: AutocompleteOption[] = matches.map(model => ({ kind: 'option', label: model.label, select: async () => {
        if (dead || selecting || options.environment.signal?.aborted) return;
        selecting = true; input.disabled = true; node.setAttribute('aria-busy', 'true');
        try {
          await options.select({ ...model });
          if (dead || options.environment.signal?.aborted) return;
          value = model.technical; render();
        } catch (error) {
          if (!dead && !options.environment.signal?.aborted) render();
          throw error;
        } finally {
          selecting = false;
          if (!dead) { input.disabled = false; node.setAttribute('aria-busy', 'false'); }
        }
      } }));
      if (normalized === '' && models.length > matches.length) choices.push({ kind: 'status', label: 'Type to find more models' });
      if (!matches.length) choices.push({ kind: 'status', label: 'No matching models' });
      return choices;
    } }]
  });
  const click = (): void => { if (!dead && !selecting) complete.toggle(input.value === (models.find(model => model.technical === value)?.label ?? value) ? '' : input.value); };
  input.addEventListener('click', click);
  return { node, input, dispose: () => { if (dead) return; dead = true; input.removeEventListener('click', click); complete.dispose(); } };
}
