/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from 'vitest';
import { RecordState } from '../src/client/record.js';
import { RpcClient } from '../src/client/transport.js';
import { MessageServices } from '../src/client/messages.js';
import { NotificationService } from '../src/client/notifications.js';
import type { Field } from '../src/client/metadata.js';

const fields: Field[] = [{ name: 'name', kind: 'char', label: 'Name', readonly: false, required: true, selection: null, relation: null, relationField: null }];
const response = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
const services: MessageServices[] = [];
afterEach(() => { services.splice(0).forEach(service => service.dispose()); vi.useRealTimers(); document.body.replaceChildren(); });
function stateFor(warning: unknown): RecordState {
  return new RecordState(new RpcClient(async path => String(path).endsWith('onchange')
    ? response({ value: { name: 'Adjusted' }, warning }) : response([{ id: 7, name: 'Before' }]), () => {}), 'test.note', fields, {});
}
function messages(): MessageServices { const service = new MessageServices(document.body, error => { throw error; }); services.push(service); return service; }

/// (partial: admitted className/sticky warning response, applied values and notification lifetime; server production covered separately)
test('onchange forwards canonical className and sticky without reverting values', async () => {
  vi.useFakeTimers();
  const state = stateFor({ title: '<Check>', message: '<b>Changed</b>', type: 'notification', className: 'erp-review  erp-emphasis', sticky: true });
  await state.load(7); await state.onchange('name');
  expect(state.values['name']).toBe('Adjusted'); expect(state.dirty).toBe(true);
  expect(state.warningDetails).toEqual({ title: '<Check>', message: '<b>Changed</b>', type: 'notification', className: 'erp-review erp-emphasis', sticky: true });
  const service = messages(); service.warning(state.warningDetails);
  const node = document.querySelector<HTMLElement>('.client-notification')!;
  expect(node.classList.contains('erp-review')).toBe(true); expect(node.classList.contains('erp-emphasis')).toBe(true);
  expect(node.dataset['level']).toBe('warning'); expect(node.querySelector('b')).toBeNull(); expect(node.textContent).toContain('<b>Changed</b>');
  vi.advanceTimersByTime(60_000); expect(node.isConnected).toBe(true);
  node.querySelector<HTMLButtonElement>('[aria-label="Close notification"]')!.click(); expect(node.isConnected).toBe(false);
});

test('explicit sticky false survives admission and expires while omitted options preserve legacy shape', async () => {
  vi.useFakeTimers();
  const state = stateFor({ title: 'Review', message: 'Timed', type: 'notification', className: '', sticky: false });
  await state.load(7); await state.onchange('name');
  expect(state.warningDetails).toEqual({ title: 'Review', message: 'Timed', type: 'notification', className: '', sticky: false });
  messages().warning(state.warningDetails); vi.advanceTimersByTime(3999); expect(document.querySelector('.client-notification')).not.toBeNull();
  vi.advanceTimersByTime(1); expect(document.querySelector('.client-notification')).toBeNull();
  const legacy = stateFor({ message: 'Legacy' }); await legacy.load(7); await legacy.onchange('name');
  expect(legacy.warningDetails).toEqual({ title: 'Warning', message: 'Legacy', type: 'dialog' });
});

const badClasses: [unknown, string][] = [
  [null, 'expected text'], [3, 'expected text'],
  ['x'.repeat(513), 'exceeds 512 UTF-8 bytes'], ['é'.repeat(257), 'exceeds 512 UTF-8 bytes'],
  ['a b c d e f g h i', 'expected at most 8 tokens'], ['x'.repeat(65), 'token exceeds 64 bytes'],
  ['one one', "duplicate token 'one'"], ['one\ttwo', 'tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces'],
  ['<b>', 'tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces'],
  ['9bad', 'tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces'],
  ['é', 'tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces'],
];
test.each(badClasses)('invalid warning className %j refuses after value application and clears stale warning', async (className, reason) => {
  const state = stateFor({ message: 'New', type: 'notification', className }); await state.load(7);
  state.warning = 'Earlier'; state.warningDetails = { title: 'Old', message: 'Earlier', type: 'notification' };
  await expect(state.onchange('name')).rejects.toMatchObject({ kind: 'server', message: `Invalid onchange warning className: ${reason}` });
  expect(state.values['name']).toBe('Adjusted'); expect(state.dirty).toBe(true);
  expect(state.warning).toBe(''); expect(state.warningDetails).toBeUndefined();
});
test.each([null, 'true', 1])('wrong-type sticky %j is not substituted', async sticky => {
  const state = stateFor({ message: 'New', sticky }); await state.load(7);
  await expect(state.onchange('name')).rejects.toMatchObject({ kind: 'server', message: 'Onchange warning sticky must be boolean.' });
  expect(state.values['name']).toBe('Adjusted'); expect(state.warningDetails).toBeUndefined();
});
test('unknown warning members are refused without a successful feedback prefix', async () => {
  const state = stateFor({ message: 'New', class: 'typo' }); await state.load(7);
  await expect(state.onchange('name')).rejects.toMatchObject({ kind: 'server', message: 'Unknown onchange warning option: class.' });
  expect(state.values['name']).toBe('Adjusted'); expect(state.warningDetails).toBeUndefined();
});
test('notification classes participate in aggregate byte admission and invalid replacement leaves the old notice', () => {
  const service = new NotificationService(document.body, error => { throw error; });
  try {
    service.add('Old', { sticky: true });
    expect(() => service.add('Old', { className: 'bad\nclass' } as never)).toThrowError(expect.objectContaining({ kind: 'validation', message: 'Invalid notification className: tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces' }));
    expect(document.querySelectorAll('.client-notification')).toHaveLength(1);
    const close = service.add('x'.repeat(65_529), { className: 'hook' } as never);
    expect(() => service.add('Overflow')).toThrowError(expect.objectContaining({ kind: 'validation', message: 'Notification limit exceeded: 32 notices or 65536 UTF-8 bytes.' }));
    close();
  } finally { service.dispose(); }
});
