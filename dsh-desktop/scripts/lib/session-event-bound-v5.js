'use strict';

// ---------------------------------------------------------------------------
// session-event-bound-v5 —— M1 治本补丁的重锚版（按已安装 dsh 0.1.2-alpha.5 字节）。
//
// 历史：session-event-bound（v4，patch-adapters transformSessionEventBound）在
// 0.1.2-alpha.5 上游整体重写 Session events 保留机制后 7 锚点 5 失配，按
// 【休眠·已失效】退役（patch-adapters.js 注释明示"不得当作已修"）。
// 本模块按新架构重锚，靶：MutableSessionEventSource
//（@deepseek-ai/dsh-api-session-controller/lib/client.js，FLASH_PKG_REL 同文件）。
//
// 新架构下的无界增长机制（2026-09 实测字节）：
//   · append() 每事件 concat 一层节点，窗口无界增长（流式长会话 O(n) 常驻）；
//   · publish() 每 revision 生成 snapshot，entries 惰性 materialize 整窗新数组
//     （流式期间 O(n²) 分配，渲染进程内存线性堆积 → WebView2 OOM → 白屏）。
//
// 修复（v5 语义 = v4 精神在新字节上的重锚）：
//   · append 超过 SESSION_EVENT_BOUND(4000) → materialize 一次、按 turn/start
//     对齐裁头到 SESSION_EVENT_KEEP(2500)，window=leaf(裁后)，publish
//     kind:"replace"——所有派生视图自快照重建；host 会话日志是持久真相，
//     loadOlder 以窗首 seq 为 beforeSeq，裁头后依旧自洽；
//   · prepend（loadOlder/一键加载）超过 SESSION_EVENT_HARD_CAP(8000) 才紧急裁
//     到 5000（K22 精神：正常翻页保持增量 prepend，不把上滚读者拉回底部）。
//
// 幂等 marker：'dsh-desktop compat: bounded session event retention (v5)'。
// 锚点失配 → anchor-missing 原样返回，绝不改写（与全仓补丁同安全语义）。
// ---------------------------------------------------------------------------

const MARKER = 'dsh-desktop compat: bounded session event retention (v5)';

const CONSTANTS_ANCHOR = '\t\tvar MutableSessionEventSource = class {';
const CONSTANTS_INJECT = [
  '\t\t/** ' + MARKER + ' — hard cap on the in-memory raw window. */',
  '\t\tconst SESSION_EVENT_BOUND = 4000;',
  '\t\tconst SESSION_EVENT_KEEP = 2500;',
  '\t\tconst SESSION_EVENT_HARD_CAP = 8000;',
  '\t\tvar MutableSessionEventSource = class {',
].join('\n');

const APPEND_ANCHOR = [
  '\t\t\tappend(entry) {',
  '\t\t\t\tconst entries = [entry];',
  '\t\t\t\tthis.window = concat(this.window, leaf(entries));',
  '\t\t\t\tthis.publish(this.snapshot.hasMore, {',
  '\t\t\t\t\tkind: "append",',
  '\t\t\t\t\tentries',
  '\t\t\t\t});',
  '\t\t\t}',
].join('\n');

const APPEND_NEW = [
  '\t\t\tappend(entry) {',
  '\t\t\t\tconst entries = [entry];',
  '\t\t\t\tthis.window = concat(this.window, leaf(entries));',
  '\t\t\t\t// ' + MARKER + '.',
  '\t\t\t\tif (this.trimSessionWindow()) {',
  '\t\t\t\t\tconst kept = materialize(this.window);',
  '\t\t\t\t\tthis.publish(true, { kind: "replace", entries: kept });',
  '\t\t\t\t\treturn;',
  '\t\t\t\t}',
  '\t\t\t\tthis.publish(this.snapshot.hasMore, {',
  '\t\t\t\t\tkind: "append",',
  '\t\t\t\t\tentries',
  '\t\t\t\t});',
  '\t\t\t}',
  '\t\t\t/**',
  '\t\t\t * ' + MARKER + ' — keep the in-memory raw window bounded so long-lived',
  '\t\t\t * streaming sessions cannot grow the renderer without limit (WebView2 OOM,',
  '\t\t\t * white screen). The host session log stays the durable truth: trimming',
  '\t\t\t * drops the oldest slice (turn/start aligned) and the replace publication',
  '\t\t\t * makes every derived view rebuild from the bounded window; loadOlder pages',
  '\t\t\t * with beforeSeq = window head, which stays self-consistent after a trim.',
  '\t\t\t * @param bound - trim threshold (default SESSION_EVENT_BOUND).',
  '\t\t\t * @param keep - retained tail size after trim (default SESSION_EVENT_KEEP).',
  '\t\t\t * @returns whether a trim happened.',
  '\t\t\t */',
  '\t\t\ttrimSessionWindow(bound = SESSION_EVENT_BOUND, keep = SESSION_EVENT_KEEP) {',
  '\t\t\t\tif (this.window.length <= bound) return false;',
  '\t\t\t\tconst all = materialize(this.window);',
  '\t\t\t\tlet cut = all.length - keep;',
  '\t\t\t\tif (cut < 1) return false;',
  '\t\t\t\tfor (let i = cut; i < all.length; i++) {',
  '\t\t\t\t\tif (all[i]?.event?.type === "turn/start") { cut = i; break; }',
  '\t\t\t\t}',
  '\t\t\t\tthis.window = leaf(all.slice(cut));',
  '\t\t\t\treturn true;',
  '\t\t\t}',
].join('\n');

const PREPEND_ANCHOR = [
  '\t\t\tprepend(entries, hasMore) {',
  '\t\t\t\tthis.window = concat(leaf(entries), this.window);',
  '\t\t\t\tthis.publish(hasMore, {',
  '\t\t\t\t\tkind: "prepend",',
  '\t\t\t\t\tentries',
  '\t\t\t\t});',
  '\t\t\t}',
].join('\n');

const PREPEND_NEW = [
  '\t\t\tprepend(entries, hasMore) {',
  '\t\t\t\tthis.window = concat(leaf(entries), this.window);',
  '\t\t\t\t// ' + MARKER + ' (K22 spirit): emergency trim only — a normal loadOlder page',
  '\t\t\t\t// stays an incremental prepend; only a window over the hard cap collapses',
  '\t\t\t\t// to a replace publication.',
  '\t\t\t\tif (this.trimSessionWindow(SESSION_EVENT_HARD_CAP, 5000)) {',
  '\t\t\t\t\tconst kept = materialize(this.window);',
  '\t\t\t\t\tthis.publish(true, { kind: "replace", entries: kept });',
  '\t\t\t\t\treturn;',
  '\t\t\t\t}',
  '\t\t\t\tthis.publish(hasMore, {',
  '\t\t\t\t\tkind: "prepend",',
  '\t\t\t\t\tentries',
  '\t\t\t\t});',
  '\t\t\t}',
].join('\n');

/**
 * 幂等变换（与全仓补丁同契约：already / anchor-missing / changed）。
 * @param {string} src
 * @param {string} [file]
 */
function transformSessionEventBoundV5(src, file) {
  if (src.includes(MARKER)) return { status: 'already' };
  const missing = [CONSTANTS_ANCHOR, APPEND_ANCHOR, PREPEND_ANCHOR].filter((a) => !src.includes(a));
  if (missing.length > 0) {
    return {
      status: 'anchor-missing',
      detail: '未找到 MutableSessionEventSource 有界保留锚点（版本可能已变更），跳过 ' + (file || '<unknown>'),
    };
  }
  let out = src;
  out = out.replace(CONSTANTS_ANCHOR, CONSTANTS_INJECT);
  out = out.replace(APPEND_ANCHOR, APPEND_NEW);
  out = out.replace(PREPEND_ANCHOR, PREPEND_NEW);
  return { status: 'changed', src: out };
}

module.exports = { transformSessionEventBoundV5, MARKER };
