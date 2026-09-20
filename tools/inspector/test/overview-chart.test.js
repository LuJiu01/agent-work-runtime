/**
 * 概览页「上下文体积」那张图的回归。
 *
 * 它曾经只读 `context_sample`——一个只存在于演示数据里的字段——所以在真实项目上
 * 永远是空的，空态却写着「去编译一次这里就会显示」。这些用例守住两点：
 * 编译之后图真的会填上，以及空态不再承诺做不到的事。
 *
 * 跑：node --test test/overview-chart.test.js
 */

'use strict';

const { test, beforeEach } = require('node:test');
const assert = require('node:assert');

const { install } = require('./fixtures/dom-stub.js');
install();

const app = require('../public/app.js');

const $ = (id) => document.getElementById(id);
const chartText = () => $('cmpChart').textContent;
const noteText = () => document.querySelector('[data-note="contextChart"]').textContent;

/** 一份真实形状的编译响应。 */
function compileResponse({ total = 6256, required = 1930, budget = 16000, omitted = [] } = {}) {
  return {
    ok: true,
    command: 'awr context compile --work RECON-040',
    data: {
      ok: true,
      project_revision: 16,
      completeness: { complete: true, status: 'CONTEXT COMPLETE', issues: [], evidence_gaps: [] },
      work_context: {
        rendered_context: '# 包的正文',
        token_estimate: total,
        required_tokens: required,
        token_budget: budget,
        selected_chunks: [{ key: 'rules/a', section: 'rules', required: true }],
        omitted_chunks: omitted,
      },
    },
  };
}

beforeEach(() => {
  app.state.mode = 'live';
  app.state.lastCompile = null;
  app.state.compile = null;
  app.state.status = { contextSample: null };
  app.state.raw = {};
  $('cmpChart').textContent = '';
  $('fWork').value = 'RECON-040';
  $('fGoal').value = '';
  $('fBudget').value = '16000';
  $('fIntent').value = '';
});

test('还没编译时，空态不承诺做不到的事', () => {
  app.renderContextChart(app.state.status);

  assert.equal($('heroBig').textContent, '—');
  assert.ok(chartText().includes('还没有编译记录'));
  // 旧文案说「显示这个项目自己的体积对比」——那个对比在真实项目上造不出来，
  // 不能再出现在空态里。
  assert.ok(!chartText().includes('体积对比'), `空态不该再提「体积对比」：${chartText()}`);
  assert.ok(chartText().includes('实测体积'), '应说明编译后显示的是那次编译的实测值');
});

test('编译之后概览那张图会填上（这正是原来的 bug）', async () => {
  global.fetch = async () => ({ json: async () => compileResponse() });

  await app.doCompile();

  // doCompile 会自己把概览那张图重画一遍，不用切页面
  assert.equal($('heroBig').textContent, '6,256');
  assert.ok($('heroCap').textContent.includes('RECON-040'), '大数字旁边要写明是哪个工作项');
  assert.equal($('cmpSub').textContent, '本项目实测');

  const text = chartText();
  assert.ok(text.includes('必需内容') && text.includes('1,930 tokens'), text);
  assert.ok(text.includes('这次编译装进去的') && text.includes('6,256 tokens'), text);
  assert.ok(text.includes('预算上限') && text.includes('16,000 tokens'), text);
});

test('三条都来自 AWR，不做推算', async () => {
  global.fetch = async () => ({
    json: async () => compileResponse({ total: 4998, required: 1561, budget: 5000 }),
  });
  await app.doCompile();

  const text = chartText();
  assert.ok(text.includes('1,561 tokens'), 'required 要原样取自 required_tokens');
  assert.ok(text.includes('4,998 tokens'), 'total 要原样取自 token_estimate');
  assert.ok(text.includes('5,000 tokens'), 'budget 要原样取自 token_budget');
  // 演示数据里那个「读完整源码」的对比，真实项目上不该出现
  assert.ok(!text.includes('读完整源码'), '真实项目不得出现无法测量的对比条');
});

test('有内容被省略时，注脚说出省了几块', async () => {
  const omitted = [
    { key: 'change:A:B', section: 'delta', reason: 'insufficient_budget_for_whole_chunk' },
    { key: 'change:C:D', section: 'delta', reason: 'insufficient_budget_for_whole_chunk' },
  ];
  global.fetch = async () => ({ json: async () => compileResponse({ omitted }) });
  await app.doCompile();

  assert.ok($('cmpNote').textContent.includes('省略了 2 块'), $('cmpNote').textContent);
});

test('演示模式仍走公开 benchmark 那三条，并标注来源', () => {
  app.state.mode = 'demo';
  app.renderContextChart({
    contextSample: {
      full_corpus_tokens: 18955,
      json_dump_tokens: 12748,
      compiled_tokens: 4998,
      note: '公开 benchmark 值，不是本项目实测。',
    },
  });

  const text = chartText();
  assert.ok(text.includes('读完整源码') && text.includes('18,955 tokens'), text);
  assert.ok(text.includes('AWR 编译包'), text);
  assert.equal($('heroBig').textContent, '−73.6%');
  assert.equal($('cmpSub').textContent, '公开基准值');
  assert.ok($('cmpNote').textContent.includes('不是本项目实测'), '必须标注这不是本项目的数');
});

test('说明文字跟着当前显示的那张图走', async () => {
  // 空态：讲编译后会看到什么
  app.renderContextChart(app.state.status);
  assert.ok(noteText().includes('编译一次之后'), noteText());

  // 实测：讲这三条各是什么
  global.fetch = async () => ({ json: async () => compileResponse() });
  await app.doCompile();
  assert.ok(noteText().includes('必需内容是不能省的部分'), noteText());
  assert.ok(!noteText().includes('不用 AWR'), '实测图不该沿用 benchmark 的说法');
});
