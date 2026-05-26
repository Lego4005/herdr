#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const url = process.env.HERDR_DESKTOP_URL || 'http://127.0.0.1:62662/';
const chromePath =
  process.env.HERDR_CHROME_PATH || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const userDataDir = await mkdtemp(path.join(os.tmpdir(), 'herdr-desktop-smoke-'));
let browser;

try {
  browser = spawn(chromePath, [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    '--window-size=1518,1193',
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    'about:blank',
  ], { stdio: ['ignore', 'pipe', 'pipe'] });

  const browserWsUrl = await waitForDevtoolsUrl(browser);
  const browserHttp = browserWsUrl
    .replace(/^ws:/, 'http:')
    .replace(/\/devtools\/browser\/.*$/, '');
  const targetResponse = await fetch(`${browserHttp}/json/new?${encodeURIComponent(url)}`, {
    method: 'PUT',
  });
  const target = await targetResponse.json();
  const page = await connectCdp(target.webSocketDebuggerUrl);
  const consoleMessages = [];

  page.on('Runtime.consoleAPICalled', params => {
    const text = (params.args || [])
      .map(arg => arg.value ?? arg.description ?? '')
      .join(' ');
    consoleMessages.push(`${params.type}: ${text}`);
  });

  await page.send('Page.enable');
  await page.send('Runtime.enable');
  const loadEvent = page.waitForEvent('Page.loadEventFired', 10000);
  await page.send('Page.navigate', { url });
  await loadEvent;
  await waitForSelector(page, '[data-page="project"].active');
  await waitForSelector(page, '#missionChildren');
  await waitForSelector(page, '#waveGrid');
  await waitForSelector(page, '#projectBoard');
  await waitForSelector(page, '#researchPrompt');
  await waitForSelector(page, '#deckStartChildRight');
  await waitForFunction(
    page,
    `document.querySelector('#missionChildren')?.textContent.includes('Parent session')`,
  );
  await waitForFunction(
    page,
    `document.querySelector('#waveGrid')?.textContent.includes('Parent session')`,
  );

  const result = await evaluate(page, `(() => {
    const tabs = Array.from(document.querySelectorAll('.tab')).map(button => button.textContent.trim());
    const reviewTabs = Array.from(document.querySelectorAll('[data-review-tab]')).map(button => button.textContent.trim());
    const treeText = document.querySelector('#missionChildren')?.textContent || '';
    const gridText = document.querySelector('#waveGrid')?.textContent || '';
    const boardText = document.querySelector('#projectBoard')?.textContent || '';
    const html = document.documentElement.innerHTML;
    const treeToggle = document.querySelector('#treeQuickToggle');
    const detailsToggle = document.querySelector('#detailsQuickToggle');
    const commandToggle = document.querySelector('#commandQuickToggle');
    const commandDeck = document.querySelector('.pane-command-deck');
    const commandDeckStyleOnBoard = commandDeck ? window.getComputedStyle(commandDeck) : null;
    const commandTrayHiddenOnBoard = Boolean(commandDeckStyleOnBoard && commandDeckStyleOnBoard.display === 'none');
    const projectActiveOnLoad = document.querySelector('[data-page="project"]')?.classList.contains('active') || false;

    document.querySelector('[data-tab="research"]')?.click();
    const researchActive = document.querySelector('[data-page="research"]')?.classList.contains('active') || false;
    const researchPrompt = Boolean(document.querySelector('#researchPrompt'));
    const researchSources = Array.from(document.querySelectorAll('[data-research-source]')).map(input => input.dataset.researchSource);
    const researchModes = Array.from(document.querySelectorAll('[data-research-mode]')).map(button => button.dataset.researchMode);

    document.querySelector('[data-tab="panes"]')?.click();
    const workbenchActive = document.querySelector('[data-page="panes"]')?.classList.contains('active') || false;

    treeToggle?.click();
    const treeHidden = document.body.classList.contains('hide-tree');
    treeToggle?.click();
    const treeVisibleAgain = !document.body.classList.contains('hide-tree');
    detailsToggle?.click();
    const detailsVisible = !document.body.classList.contains('hide-inspector');
    commandToggle?.click();
    const commandVisible = document.body.classList.contains('show-command-deck');

    const handle = document.querySelector('#treeReopen')?.getBoundingClientRect();
    const title = document.querySelector('.terminal-title')?.getBoundingClientRect();
    const overlapsTerminalTitle = Boolean(handle && title &&
      handle.left < title.right &&
      handle.right > title.left &&
      handle.top < title.bottom &&
      handle.bottom > title.top);

    return {
      title: document.title,
      tabs,
      reviewTabs,
      treeText,
      gridText,
      boardText,
      projectActiveOnLoad,
      commandTrayHiddenOnBoard,
      researchActive,
      researchPrompt,
      researchSources,
      researchModes,
      workbenchActive,
      treeHidden,
      treeVisibleAgain,
      detailsVisible,
      commandVisible,
      overlapsTerminalTitle,
      hasExistingLaunchButton: Boolean(document.querySelector('#deckStartChildRight')),
      hasDuplicateLaunchButton: Boolean(document.querySelector('#deckCreateRight')),
      hasDuplicateInspectorEvidence: Boolean(document.querySelector('[data-inspector-tab="evidence"]')),
      hasDuplicateTreeRenderer: html.includes('function renderWorkroomTree('),
      hasDuplicateLaunchFunction: html.includes('function launchChildPaneFromDeck('),
    };
  })()`);

  await page.close();

  const failures = [];
  if (result.title !== 'Herdr Workroom Preview') failures.push(`unexpected title: ${result.title}`);
  if (result.tabs.join('|') !== 'Board|Research|Workbench|Review') {
    failures.push(`unexpected tabs: ${result.tabs.join(', ')}`);
  }
  if (!result.projectActiveOnLoad) failures.push('desktop did not open to Board');
  if (!result.commandTrayHiddenOnBoard) failures.push('command tray visible on Board');
  if (!result.boardText.includes('No child sessions yet') && !result.boardText.includes('Parent Review')) {
    failures.push('board missing project-home lane content');
  }
  if (!result.researchActive) failures.push('Research tab did not activate');
  if (!result.researchPrompt) failures.push('Research prompt missing');
  for (const mode of ['summary', 'code', 'design', 'research']) {
    if (!result.researchModes.includes(mode)) failures.push(`missing research mode: ${mode}`);
  }
  for (const source of ['px', 'git', 'recorder', 'sessions', 'foxchat']) {
    if (!result.researchSources.includes(source)) failures.push(`missing research source: ${source}`);
  }
  if (!result.workbenchActive) failures.push('Workbench tab did not activate');
  for (const tab of ['Overview', 'Evidence', 'Changes', 'Audit', 'Timeline']) {
    if (!result.reviewTabs.includes(tab)) failures.push(`missing review sidecar tab: ${tab}`);
  }
  if (!result.treeText.includes('Parent session')) failures.push('tree missing Parent session row');
  if (!result.gridText.includes('Parent session')) failures.push('grid missing Parent session pane');
  if (!result.treeHidden || !result.treeVisibleAgain) {
    failures.push('tree quick toggle did not hide and restore tree');
  }
  if (!result.detailsVisible) failures.push('details quick toggle did not open inspector drawer');
  if (!result.commandVisible) failures.push('command quick toggle did not open command deck');
  if (result.overlapsTerminalTitle) failures.push('tree edge handle overlaps terminal title');
  if (!result.hasExistingLaunchButton) failures.push('existing deckStartChildRight launch button missing');
  if (result.hasDuplicateLaunchButton) failures.push('duplicate deckCreateRight launch button exists');
  if (result.hasDuplicateInspectorEvidence) failures.push('duplicate evidence inspector tab exists');
  if (result.hasDuplicateTreeRenderer) failures.push('duplicate renderWorkroomTree implementation exists');
  if (result.hasDuplicateLaunchFunction) {
    failures.push('duplicate launchChildPaneFromDeck implementation exists');
  }

  const payload = { ok: failures.length === 0, failures, result, consoleMessages };
  console[failures.length ? 'error' : 'log'](JSON.stringify(payload, null, 2));
  process.exitCode = failures.length ? 1 : 0;
} catch (error) {
  console.error(JSON.stringify({ ok: false, failures: [error.message] }, null, 2));
  process.exitCode = 1;
} finally {
  await cleanup(browser, userDataDir);
}

function waitForDevtoolsUrl(process) {
  return new Promise((resolve, reject) => {
    let output = '';
    const timer = setTimeout(() => {
      reject(new Error(`Chrome did not expose a DevTools URL. Output: ${output}`));
    }, 10000);
    const onData = chunk => {
      output += chunk.toString();
      const match = output.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (match) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    };
    process.stdout.on('data', onData);
    process.stderr.on('data', onData);
    process.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    process.once('exit', code => {
      clearTimeout(timer);
      reject(new Error(`Chrome exited before DevTools was ready: ${code}. Output: ${output}`));
    });
  });
}

function connectCdp(wsUrl) {
  const socket = new WebSocket(wsUrl);
  let nextId = 1;
  const pending = new Map();
  const listeners = new Map();

  socket.addEventListener('message', event => {
    const message = JSON.parse(String(event.data));
    if (message.id && pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) reject(new Error(message.error.message || 'CDP command failed'));
      else resolve(message.result || {});
      return;
    }
    if (message.method && listeners.has(message.method)) {
      for (const handler of listeners.get(message.method)) {
        handler(message.params || {});
      }
    }
  });

  return new Promise((resolve, reject) => {
    socket.addEventListener('open', () => {
      resolve({
        send(method, params = {}) {
          const id = nextId++;
          socket.send(JSON.stringify({ id, method, params }));
          return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
        },
        on(method, handler) {
          const handlers = listeners.get(method) || [];
          handlers.push(handler);
          listeners.set(method, handlers);
        },
        waitForEvent(method, timeoutMs) {
          return new Promise((resolve, reject) => {
            const timer = setTimeout(
              () => reject(new Error(`Timed out waiting for ${method}`)),
              timeoutMs,
            );
            const handler = params => {
              clearTimeout(timer);
              resolve(params);
            };
            this.on(method, handler);
          });
        },
        close() {
          socket.close();
        },
      });
    });
    socket.addEventListener('error', reject);
  });
}

async function evaluate(page, expression) {
  const result = await page.send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.text || 'Runtime.evaluate failed');
  }
  return result.result?.value;
}

async function waitForSelector(page, selector, timeoutMs = 10000) {
  await waitForFunction(page, `Boolean(document.querySelector(${JSON.stringify(selector)}))`, timeoutMs);
}

async function waitForFunction(page, expression, timeoutMs = 10000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (await evaluate(page, `Boolean(${expression})`)) return;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`Timed out waiting for expression: ${expression}`);
}

async function cleanup(process, dir) {
  if (process && !process.killed) {
    process.kill('SIGTERM');
    await new Promise(resolve => process.once('exit', resolve));
  }
  await rm(dir, { recursive: true, force: true });
}
