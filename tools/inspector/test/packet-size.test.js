/**
 * 上下文页「这个包有多大」的回归。
 *
 * 这块度量原来在概览页，只读 `context_sample`——一个只存在于演示数据里的字段——
 * 所以真实项目上永远是空的，空态却写着「去编译一次这里就会显示」。
 * 现在它就画在编译按钮下面，和产生它的动作同一页。
 *
 * 跑：node --test test/packet-size.test.js
 */

'use strict';

const { test, beforeEach } = require('node:test');
const assert = require('node:assert');

const { install } = require('./fixtures/dom-stub.js');
install();

const app = require('../public/app.js');

const $ = (id) => document.getElementById(id);
const chartText = () => $('sizeChart').textContent;
const noteText = () => $('sizeNote').textContent;

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
  app.state.compile = null;
  app.state.status = {};
  app.state.raw = {};
  $('sizeChart').textContent = '';
  $('fWork').value = 'RECON-040';
  $('fGoal').value = '';
  $('fBudget').value = '16000';
  $('fIntent').value = '';
});

test('还没编译时，空态不承诺做不到的事', () => {
  app.renderPacketSize(null);

  assert.equal($('ctxBig').textContent, '—');
  assert.ok(chartText().includes('还没有编译'));
  // 旧文案说「显示这个项目自己的体积对比」——那个对比在真实项目上造不出来，
  // 不能再出现在空态里。
  assert.ok(!chartText().includes('体积对比'), `空态不该再提「体积对比」：${chartText()}`);
  assert.ok(chartText().includes('实测体积'), '应说明编译后显示的是这个包的实测值');
});

test('编译之后同一页就填上了，不用切页面', async () => {
  global.fetch = async () => ({ json: async () => compileResponse() });

  await app.doCompile();

  assert.equal($('ctxBig').textContent, '6,256');
  assert.ok($('ctxCap').textContent.includes('RECON-040'), '大数字旁边要写明是哪个工作项');
  assert.equal($('sizeSub').textContent, '用掉预算的 39%');

  const text = chartText();
  assert.ok(text.includes('必需内容') && text.includes('1,930 tokens'), text);
  assert.ok(text.includes('这次装进去的') && text.includes('6,256 tokens'), text);
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
  // 语料体积测不出来，不许出现这类条目
  assert.ok(!text.includes('读完整源码'), '真实项目不得出现无法测量的对比条');
});

test('有内容被省略时，注脚说出省了几块', async () => {
  const omitted = [
    { key: 'change:A:B', section: 'delta', reason: 'insufficient_budget_for_whole_chunk' },
    { key: 'change:C:D', section: 'delta', reason: 'insufficient_budget_for_whole_chunk' },
  ];
  global.fetch = async () => ({ json: async () => compileResponse({ omitted }) });
  await app.doCompile();

  assert.ok(noteText().includes('省略了 2 块'), noteText());
});

test('没有省略时也说清楚', async () => {
  global.fetch = async () => ({ json: async () => compileResponse() });
  await app.doCompile();
  assert.ok(noteText().includes('没有内容被省略'), noteText());
});

test('换一次编译，数字跟着换', async () => {
  global.fetch = async () => ({ json: async () => compileResponse() });
  await app.doCompile();
  assert.equal($('ctxBig').textContent, '6,256');

  global.fetch = async () => ({
    json: async () => compileResponse({ total: 900, required: 700, budget: 4000 }),
  });
  $('fBudget').value = '4000';
  await app.doCompile();
  assert.equal($('ctxBig').textContent, '900', '旧的数不该留在页面上');
  assert.ok(chartText().includes('4,000 tokens'));
  assert.ok(!chartText().includes('16,000 tokens'), '上一次的预算不该还在');
});
