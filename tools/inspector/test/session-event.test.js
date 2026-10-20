'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { install } = require('./fixtures/dom-stub.js');
const i18n = require('../public/i18n.js');

function loadApp() {
  install();
  delete require.cache[require.resolve('../public/app.js')];
  return require('../public/app.js');
}

test('session and event panels render CLI JSON and hide on Unsupported', () => {
  const app = loadApp();
  const $ = (id) => document.getElementById(id);

  app.applyListResponse('sessions', {
    ok: true,
    data: {
      sessions: [{
        id: 11,
        agent_id: 'lin',
        provider: 'cursor',
        model: 'composer',
        status: 'active',
        work_item_id: 1,
        last_checkpoint_id: 7,
        started_at: Date.now() - 60000,
      }],
    },
  }, 'sessions', 'session.list');
  app.applyListResponse('events', {
    ok: true,
    data: {
      events: [{
        id: 21,
        type: 'session_started',
        summary: 'session started',
        importance: 'high',
        session_id: 11,
        work_item_id: 1,
        created_at: Date.now() - 120000,
      }],
    },
  }, 'events', 'event.history');
  app.renderSessions();
  app.renderEvents();

  assert.equal($('sessionPanel').hidden, false);
  assert.ok($('sessionList').textContent.includes('lin'));
  assert.ok($('sessionList').textContent.includes('active'));
  assert.ok($('sessionList').textContent.includes('checkpoint=7'));
  assert.equal($('eventPanel').hidden, false);
  assert.ok($('eventList').textContent.includes('session started'));
  assert.ok($('eventList').textContent.includes('session_started'));

  app.applyListResponse('sessions', {
    ok: false,
    error: { code: 'Unsupported', message: 'operation is not implemented' },
  }, 'sessions', 'session.list');
  app.applyListResponse('events', {
    ok: false,
    error: { code: 'Unsupported', message: 'operation is not implemented' },
  }, 'events', 'event.history');
  app.renderSessions();
  app.renderEvents();
  assert.equal($('sessionPanel').hidden, true);
  assert.equal($('eventPanel').hidden, true);
  assert.ok(!($('sessionList').textContent.includes(i18n.t('ui.no_active_sessions'))));
});

test('session show expands checkpoint, open loops and claims from CLI JSON', () => {
  const app = loadApp();
  const $ = (id) => document.getElementById(id);
  app.applyListResponse('sessions', {
    ok: true,
    data: {
      sessions: [{
        id: 11,
        agent_id: 'lin',
        status: 'active',
        work_item_id: 1,
        last_checkpoint_id: 7,
        started_at: Date.now(),
      }],
    },
  }, 'sessions', 'session.list');
  app.applySessionShow(11, {
    ok: true,
    command: 'awr session show 11',
    data: {
      checkpoint: {
        id: 7,
        next_action: 'Wire the panel',
        open_loops: ['Confirm JSON'],
      },
      inherited_checkpoint: null,
      claims: [{ agent_id: 'lin', status: 'active', work_item_id: 1 }],
      checkpoint_saves: { incomplete_count: 1 },
    },
  });
  app.renderSessions();
  assert.equal($('sessionPanel').hidden, false);
  assert.equal($('sessionDetail').hidden, false);
  assert.ok($('sessionDetail').textContent.includes('Wire the panel'));
  assert.ok($('sessionDetail').textContent.includes('Confirm JSON'));
  assert.ok($('sessionDetail').textContent.includes('lin · active · work=1'));
  assert.ok($('sessionDetail').textContent.includes(i18n.t('ui.session_incomplete_saves', { p0: 1 })));
  assert.equal($('sessionList').find((n) => n.tagName === 'LI').getAttribute('aria-selected'), 'true');
});

test('session show uses inherited checkpoint when the current one is missing', () => {
  const app = loadApp();
  const $ = (id) => document.getElementById(id);
  app.applyListResponse('sessions', {
    ok: true,
    data: { sessions: [{ id: 12, agent_id: 'lin', status: 'active' }] },
  }, 'sessions', 'session.list');
  app.applySessionShow(12, {
    ok: true,
    data: {
      checkpoint: null,
      inherited_checkpoint: {
        id: 4,
        next_action: 'Resume the handoff',
        open_loops: [],
      },
      claims: [],
    },
  });
  app.renderSessions();
  assert.ok($('sessionDetail').textContent.includes('Resume the handoff'));
  assert.ok($('sessionDetail').textContent.includes(i18n.t('ui.session_inherited_checkpoint') + ' 4'));
  assert.ok($('sessionDetail').textContent.includes(i18n.t('ui.session_no_open_loops')));
  assert.ok($('sessionDetail').textContent.includes(i18n.t('ui.session_no_claims')));
});

test('session show Unsupported keeps the list visible', () => {
  const app = loadApp();
  const $ = (id) => document.getElementById(id);
  app.applyListResponse('sessions', {
    ok: true,
    data: { sessions: [{ id: 11, agent_id: 'lin', status: 'active' }] },
  }, 'sessions', 'session.list');
  app.applySessionShow(11, {
    ok: false,
    command: 'awr session show 11',
    error: { code: 'Unsupported', message: 'operation is not implemented' },
  });
  app.renderSessions();
  assert.equal($('sessionPanel').hidden, false);
  assert.ok($('sessionList').textContent.includes('lin'));
  assert.equal($('sessionDetail').hidden, false);
  assert.ok($('sessionDetail').textContent.includes('Unsupported'));
});

test('empty session and event lists stay visible', () => {
  const app = loadApp();
  const $ = (id) => document.getElementById(id);
  app.applyListResponse('sessions', { ok: true, data: { sessions: [] } }, 'sessions', 'session.list');
  app.applyListResponse('events', { ok: true, data: { events: [] } }, 'events', 'event.history');
  app.renderSessions();
  app.renderEvents();
  assert.equal($('sessionPanel').hidden, false);
  assert.ok($('sessionList').textContent.includes(i18n.t('ui.no_active_sessions')));
  assert.equal($('eventPanel').hidden, false);
  assert.ok($('eventList').textContent.includes(i18n.t('ui.no_recent_events')));
});
