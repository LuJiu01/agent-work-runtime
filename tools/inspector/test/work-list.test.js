"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
require("./fixtures/dom-stub.js").install();
const app = require("../public/app.js");
global.fetch = async () => ({ json: async () => ({ ok: false, error: { code: "fixture", message: "fixture" } }) });
function fixture(total = 17, returned = total, revision = 7) {
  const ready = Array.from({ length: total }, (_, i) => ({ key: `R${i}`, title: `Ready ${i}` }));
  return [{ project_revision: 7, ready_count: total, current_total: 2, blocked_count: 1, waiting_count: 0,
    current: [{key:"C1"}, {key:"C2"}], ready: ready.slice(0,5), waiting: [], blocked: [{key:"B1"}],
    omissions: {ready: Math.max(0,total-5)} },
    { project_revision: revision, ready_total: total, ready: ready.slice(0,returned), blocked_sample: [{key:"DO-NOT-MERGE"}] }];
}
function render(status, filter = "all") {
  app.state.mode = "live"; app.state.status = status; app.state.workFilter = filter; app.state.selectedWork = null;
  app.state.workDetail = {}; app.renderWork();
}
test("17 ready items replace the five-item summary and render all twenty queue items", () => {
  const status = app.normStatus(...fixture()); render(status);
  assert.equal(status.queues.ready.omitted, 0);
  assert.equal(document.getElementById("workRows").children.length, 20);
  assert.match(document.getElementById("workFilters").textContent, /当前队列 20/);
  render(status, "ready");
  assert.equal(document.getElementById("workRows").children.length, 17);
  assert.equal(document.getElementById("workSub").textContent, "17 项");
  assert.equal(status.queues.blocked.items[0].key, "B1");
});
test("bounded lists disclose missing rows instead of calling them complete", () => {
  const status = app.normStatus(...fixture(117,100)); render(status, "ready");
  assert.equal(status.queues.ready.total, 117);
  assert.equal(document.getElementById("workRows").children.length, 100);
  assert.match(document.getElementById("workSub").textContent, /另有 17 项未加载/);
  assert.match(document.getElementById("workEmpty").textContent, /尚未完整加载/);
});
test("different revisions never merge queue membership from another snapshot", () => {
  const status = app.normStatus(...fixture(17,17,8)); render(status, "ready");
  assert.equal(status.queues.ready.items.length, 5);
  assert.match(document.getElementById("workSub").textContent, /另有 12 项未加载/);
});
test("failed ready request preserves summary and discloses omitted rows", () => {
  const status = app.normStatus(fixture()[0], null); render(status, "ready");
  assert.equal(status.queues.ready.items.length, 5);
  assert.equal(status.queues.ready.total, 17);
  assert.match(document.getElementById("workSub").textContent, /另有 12 项未加载/);
});

function pageResponse(offset, total = 17, queue = "ready") {
  return { ok: true, data: { ...fixture(total)[0], page: {queue, offset, limit:10, total,
    has_more:offset+10<total, items:Array.from({length:Math.max(0,Math.min(10,total-offset))},(_,i)=>({key:`R${offset+i}`,queue}))} } };
}
function pagination() {
  app.state.mode="live"; app.state.workPagination=true; app.state.workFilter="ready";
  app.state.workPageSize=10; app.state.workOffset=0; app.state.status=app.normStatus(...fixture());
}
test("server pages show ten then seven rows, not a slice of the summary", async () => {
  pagination(); const urls=[];
  global.fetch=async url=>({json:async()=>{urls.push(url);return pageResponse(Number(new URL(url,"http://local").searchParams.get("offset")));}});
  await app.loadWorkPage();
  assert.equal(document.getElementById("workRows").children.length,10);
  assert.match(document.getElementById("workPagination").textContent,/第 1 \/ 2 页，共 17 项/);
  app.state.workOffset=10; await app.loadWorkPage();
  assert.equal(document.getElementById("workRows").children.length,7);
  assert.equal(app.state.workPage.items[0].key,"R10");
  assert.ok(urls.some(url=>url.includes("offset=10&limit=10")));
});
test("late page responses cannot overwrite a newly selected queue", async () => {
  pagination(); let resolveFirst;
  global.fetch=url=>url.startsWith("/api/work-page") ? new Promise(resolve=>{resolveFirst=resolve;}) : Promise.resolve({json:async()=>({ok:false,error:{code:"fixture",message:"fixture"}})});
  const first=app.loadWorkPage();
  app.state.workFilter="blocked";
  global.fetch=async()=>({json:async()=>pageResponse(0,1,"blocked")});
  await app.loadWorkPage();
  resolveFirst({json:async()=>pageResponse(0)}); await first;
  assert.equal(app.state.workPage.queue,"blocked");
  assert.equal(document.getElementById("workRows").children.length,1);
});
test("removed last page moves back to the last available page", async () => {
  pagination(); app.state.workOffset=20;
  global.fetch=async url=>({json:async()=>pageResponse(Number(new URL(url,"http://local").searchParams.get("offset")),17)});
  await app.loadWorkPage(); assert.equal(app.state.workOffset,10);
  assert.equal(document.getElementById("workRows").children.length,7);
});
test("page failure clears old rows and does not pretend the queue is empty", async () => {
  pagination(); global.fetch=async()=>({json:async()=>({ok:false,error:{code:"Offline",message:"Disconnected"}})});
  await app.loadWorkPage(); assert.equal(app.state.workPage,null);
  assert.equal(document.getElementById("workRows").children.length,0);
  assert.match(document.getElementById("workEmpty").textContent,/Disconnected/);
});
