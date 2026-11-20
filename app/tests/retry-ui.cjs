// Optional browser regression: install Playwright and its Chromium browser, then
// run `node --test app/tests/retry-ui.cjs`. Set ZH3_BROWSER_CHANNEL=chrome to
// use an installed Chrome. Only the simulated IPC channel below receives clicks.
const assert = require('node:assert/strict');
const path = require('node:path');
const { before, after, test } = require('node:test');
const { chromium } = require('playwright');

const assets = {
  '/index.html': ['text/html; charset=utf-8', 'ui/index.html'],
  '/zh3-mark.svg': ['image/svg+xml', 'assets/zh3-icon.svg'],
  '/zh3-front.svg': ['image/svg+xml', 'ui/zh3-front.svg'],
};
const device = {
  usb: 'example-usb', container: 'example-container', driver: '6.16.0.15392',
  ks: 'example-ks', endpoint_id: 'example-output', stream: 'output-a',
  base_ref: 'factory-output', active_ref: 'factory-output',
};
const pending = {
  state: 'connected', snapshot: {device, mix: {rate: 44100}, volume: 40, muted: false},
  profile: {mode: 'factory', phase: 'pending', can_write: false, can_restore: true},
  factory_target: {usb: device.usb, container: device.container, token: 'attachment-a'},
  target_token: 'format-a', can_retry: true, retry_token: 'pending-operation-a',
  restart_gate: {kind: 'ready', panel_running: false},
};

let browser;
before(async () => {
  browser = await chromium.launch({channel: process.env.ZH3_BROWSER_CHANNEL});
});
after(async () => { await browser?.close(); });

async function openPanel(t) {
  const page = await browser.newPage();
  t.after(() => page.close());
  await page.addInitScript(() => {
    window.messages = [];
    window.ipc = {postMessage: data => window.messages.push(JSON.parse(data))};
  });
  await page.route('**/*', route => {
    const url = new URL(route.request().url());
    const asset = url.origin === 'https://zh3.test' && assets[url.pathname];
    return asset ? route.fulfill({
      contentType: asset[0], path: path.join(__dirname, '..', asset[1]),
    }) : route.abort();
  });
  await page.goto('https://zh3.test/index.html');
  return {
    element: id => page.locator('#' + id),
    update: frame => page.evaluate(data => window.zh3Update(data), frame),
    commands: () => page.evaluate(() => window.messages.filter(c => c.action !== 'ready')),
  };
}

for (const [label, frame] of [
  ['playback profile', {...pending, profile: {...pending.profile, mode: 'range', half_ms: 7}}],
  ['all playback profiles', pending],
  ['all playback profiles without an endpoint', {
    ...pending, state: 'recovery', snapshot: null, device: {...device, endpoint_id: ''},
  }],
]) {
  test('retry identifies the current pending operation: ' + label, async t => {
    const {element, update, commands} = await openPanel(t);
    await update({...frame, retry_token: undefined});
    assert(await element('retry').isHidden());
    await update(frame);
    await element('retry').click();
    await update({...frame, retry_token: 'replacement-operation'});
    assert(await element('dialog').isHidden());
    assert.deepEqual(await commands(), []);

    await update(frame);
    await element('retry').click();
    await element('cancel').click();
    assert.deepEqual(await commands(), []);
    await element('retry').click();
    await update({...frame, snapshot: frame.snapshot && {...frame.snapshot, volume: 0, muted: true}});
    assert(await element('dialog').isVisible());
    await element('confirm').click();
    assert.deepEqual(await commands(), [{
      action: 'retry', usb: device.usb, container: device.container,
      endpoint_id: frame.snapshot ? device.endpoint_id : '',
      target_token: frame.target_token, retry_token: frame.retry_token,
    }]);
  });
}

test('failed factory verification remains visible alongside restart advice', async t => {
  const {element, update} = await openPanel(t);
  const diagnosis = '全设备原厂恢复尚未完成，或播放配置发生变化。请再次确认恢复原厂。';
  const advice = '请从系统托盘退出 Fosi 面板。';
  await update({
    ...pending, can_retry: false, message: diagnosis,
    restart_gate: {kind: 'panel_open', panel_running: true, message: advice},
  });
  const text = await element('message').innerText();
  assert(text.includes(diagnosis));
  assert(text.includes(advice));
  assert(await element('retry').isHidden());
  assert(await element('restore').isEnabled());
});
