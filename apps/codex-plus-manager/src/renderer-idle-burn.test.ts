import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// 空转 CPU 的三道闸门。
//
// 背景:dispatcher 装不上时,scanLightweight 由 MutationObserver 驱动(200ms 去抖),
// 用户打字或流式输出时约 5 次/秒。每轮都试三个前缀、每个前缀都把全部 app asset
// fetch 一遍再跑正则 —— 上游实测空闲态 301 次请求/秒、渲染进程 CPU 44.7%。
//
// 前三条把源码切片出来**真跑**,而不是断言"源码里有某个字符串":
// 阈值逻辑写错(冷却判反、计数没清零、桶串了)源码扫描一个都看不出来。
// 数的是 fetch 次数 —— 那是 codexAppAssetUrlFromScriptText 里真正贵的那一步。

const RENDERER = new URL("../../../assets/inject/renderer-inject.js", import.meta.url);

const LOADER_START = "  const codexAppModuleFailures = new Map();";
const LOADER_END = "\n  async function loadOptionalCodexAppModule(";

async function readRenderer(): Promise<string> {
  return readFile(RENDERER, "utf8");
}

/** 切出负缓存状态 + 资产发现 + loadCodexAppModule,注入可控时钟与 DOM 后执行。
 *
 * 注意切片里带着**真实的** codexAppAssetUrl / codexAppAssetUrlFromScriptText,
 * 所以不能用同名参数去桩它们(会被内部定义遮蔽)。改为注入它们依赖的
 * document / performance / fetch —— 这样测的是真实的发现逻辑,只把 IO 换掉。
 *
 * 让所有候选都匹配不上(fetch 回空文本),对应线上 asset 改名后的状态:
 * loadCodexAppModule 抛「未找到 Codex App asset」,正是那条失败路径。
 */
function moduleLoaderRuntime(renderer: string) {
  const start = renderer.indexOf(LOADER_START);
  const end = renderer.indexOf(LOADER_END);
  assert.ok(start >= 0, "找不到负缓存状态声明 —— 改名了就更新 LOADER_START");
  assert.ok(end > start, "找不到 loadCodexAppModule 的结尾锚点 —— 更新 LOADER_END");
  const source = renderer.slice(start, end);

  let now = 1_000_000;
  let fetches = 0;

  const document = {
    scripts: [{ src: "https://codex.invalid/assets/app-0000.js" }],
    querySelectorAll: () => [] as unknown[],
  };
  const performance = { getEntriesByType: () => [] as unknown[] };
  const fetchStub = async () => {
    // 贵的那一步。线上它会把每个候选资产整篇拉下来再跑三条正则。
    fetches += 1;
    return { ok: true, text: async () => "" };
  };

  const factory = new Function(
    "Date",
    "document",
    "performance",
    "fetch",
    "codexServiceTierModulePromises",
    source + "\nreturn loadCodexAppModule;",
  ) as (...args: unknown[]) => (namePart: string) => Promise<unknown>;

  const loadCodexAppModule = factory(
    { now: () => now },
    document,
    performance,
    fetchStub,
    new Map(),
  );

  return {
    call: (namePart: string) => loadCodexAppModule(namePart),
    advance: (ms: number) => {
      now += ms;
    },
    get fetches() {
      return fetches;
    },
  };
}

test("冷却期内反复调用只扫描一次 —— 这是空转的根因", async () => {
  const runtime = moduleLoaderRuntime(await readRenderer());

  for (let i = 0; i < 20; i += 1) {
    await assert.rejects(runtime.call("setting-storage-"));
  }

  assert.equal(
    runtime.fetches,
    1,
    `30 秒冷却内 20 次调用应只扫一次,实际 ${runtime.fetches} 次 —— 负缓存没生效`,
  );
});

test("跨过冷却窗口会再试,但试满 8 次就彻底放弃", async () => {
  const runtime = moduleLoaderRuntime(await readRenderer());

  // 每轮推进 31 秒跨过冷却,跑 30 轮 —— 远超 8 次上限。
  for (let round = 0; round < 30; round += 1) {
    await assert.rejects(runtime.call("setting-storage-"));
    runtime.advance(31_000);
  }

  assert.equal(
    runtime.fetches,
    8,
    `应停在 8 次(codexAppModuleMaxAttempts),实际 ${runtime.fetches} 次 —— 放弃阈值没生效,资产会被无限重扫`,
  );
});

test("不同前缀各自计数,一个前缀放弃不牵连另一个", async () => {
  const runtime = moduleLoaderRuntime(await readRenderer());

  for (let round = 0; round < 10; round += 1) {
    await assert.rejects(runtime.call("setting-storage-"));
    runtime.advance(31_000);
  }
  const afterFirst = runtime.fetches;
  await assert.rejects(runtime.call("vscode-api-"));

  assert.equal(
    runtime.fetches,
    afterFirst + 1,
    "第二个前缀是独立的桶,不该被第一个的放弃状态挡住",
  );
});

// 下面两条守的是源码结构:它们涉及 window/DOM 与并发,切片执行成本太高,
// 而"守卫被误删"恰恰是最可能发生的回归。
test("dispatcher 补丁有 in-flight 去重与放弃开关", async () => {
  const renderer = await readRenderer();
  const start = renderer.indexOf("  function installCodexServiceTierDispatcherPatch() {");
  assert.ok(start >= 0, "找不到 installCodexServiceTierDispatcherPatch");
  const body = renderer.slice(start, renderer.indexOf("\n  function ", start + 10));

  assert.match(
    body,
    /if \(serviceTierDispatcherPatchDisabled\) return;/,
    "少了放弃开关 —— 连续失败会一直拖着渲染进程",
  );
  assert.match(
    body,
    /if \(serviceTierDispatcherPatchPromise\) return;/,
    "少了 in-flight 去重 —— scan 的频率会直接变成并发全量扫描的频率",
  );
  assert.match(
    body,
    /serviceTierDispatcherPatchMissCount === 1/,
    "少了「只报首次」—— 每轮 scan 都会发一条相同诊断",
  );
  assert.match(
    body,
    /finally \{\s*serviceTierDispatcherPatchPromise = null;/,
    "in-flight 标志必须在 finally 里清,否则失败一次就永久卡住",
  );
});

test("只动自己 UI 的变更不再排新 scan,且该早退排在 relevance 判定之前", async () => {
  const renderer = await readRenderer();
  const start = renderer.indexOf("  function shouldScheduleScan(mutations) {");
  assert.ok(start >= 0, "找不到 shouldScheduleScan");
  const body = renderer.slice(start, renderer.indexOf("\n  function ", start + 10));

  const selfOnly = body.indexOf("changedElements.every(isExtensionUiNode)");
  const relevance = body.indexOf("nodeSelfOrAncestorMatchesScanRelevance(target)");
  assert.ok(selfOnly >= 0, "少了「只动自己节点就不排 scan」的早退 —— 自喂循环会回来");
  assert.ok(relevance >= 0, "shouldScheduleScan 结构变了,这条守卫要跟着更新");
  assert.ok(
    selfOnly < relevance,
    "早退必须排在 relevance 判定之前:容器本身是 relevant,那一行会先 return true,早退就永远走不到",
  );
});
