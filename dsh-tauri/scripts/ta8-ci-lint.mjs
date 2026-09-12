#!/usr/bin/env node
/**
 * TA8 任务二：tauri-release.yml CI workflow 静态断言套件（零依赖 node 脚本）
 *
 * 用途：在本地 / 任意 CI（含 Windows runner）零依赖跑一遍，静态锁定
 * .github/workflows/tauri-release.yml 的关键结构与门禁——防手改 workflow
 * 时误删某道防线（版本门禁/D3DCOMPILER/sha256 边车/delete-then-upload 等）
 * 而无人察觉。macOS 专属行为（mount-DMG 验证、签名断言）只能在 mac CI 真
 * 跑，本脚本至少静态断言这些步骤存在且顺序正确（任务一 macOS 项的本地替代）。
 *
 * 解析策略：YAML 子集按行锚点（workflow 缩进规整，不引 yaml 依赖）——
 * 按 `^  <job-id>:` 切 job 块，job 内按 `^      - name:` 切步骤。
 *
 * 用法：
 *   node dsh-tauri/scripts/ta8-ci-lint.mjs            # 检查真实 workflow
 *   node dsh-tauri/scripts/ta8-ci-lint.mjs --test     # 自检模式（内置正/负样本）
 * 退出码：0 全过 / 1 有失败或 workflow 存在结构性缺陷。
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

// ---------------------------------------------------------------------------
// 简易解析：job 块 + 步骤切分（子集足够：顶层 jobs: 下两空格缩进为 job id）
// ---------------------------------------------------------------------------
function parseJobs(yaml) {
  const lines = yaml.split('\n');
  const jobs = {};
  let cur = null;
  let inJobs = false;
  for (const line of lines) {
    if (/^jobs:\s*$/.test(line)) { inJobs = true; cur = null; continue; }
    const m = line.match(/^  ([A-Za-z0-9_-]+):\s*$/);
    if (inJobs && m) {
      cur = m[1];
      jobs[cur] = [];
    } else if (cur && /^      - name:/.test(line)) {
      jobs[cur].push({ name: line.replace(/^      - name:\s*/, '').trim(), block: [] });
    } else if (cur && jobs[cur].length) {
      jobs[cur][jobs[cur].length - 1].block.push(line);
    }
  }
  return jobs;
}
const stepText = (s) => s.block.join('\n');
const jobText = (steps) => steps.map(stepText).join('\n');
const jobHeader = (yaml, id) => {
  const i = yaml.indexOf(`\n  ${id}:`);
  return i < 0 ? '' : yaml.slice(i, i + 1200);
};

// ---------------------------------------------------------------------------
// 断言集：每条 = { id, desc, run(ctx) }
// ---------------------------------------------------------------------------
function buildChecks() {
  const checks = [];
  const C = (id, desc, run) => checks.push({ id, desc, run });
  const assert = (c, m) => { if (!c) throw new Error(m || 'assertion failed'); };

  C('JOBS-7', '7 个 job 存在（build×5 + publish + mirror-gitee）', ({ jobs }) => {
    for (const id of ['build-windows', 'build-windows-arm64', 'build-portable', 'build-linux', 'build-macos', 'publish', 'mirror-gitee']) {
      assert(jobs[id], `缺 job: ${id}`);
    }
    assert(Object.keys(jobs).length === 7, `job 数应恰为 7，实得 ${Object.keys(jobs).length}: ${Object.keys(jobs)}`);
  });

  C('DEPS', '依赖链：portable←windows；publish←5 build；mirror←publish', ({ yaml, jobs }) => {
    assert(/build-portable:[\s\S]*?needs: \[build-windows\]/.test(yaml.slice(yaml.indexOf('  build-portable:'), yaml.indexOf('  build-linux:'))), 'build-portable 须 needs build-windows');
    const pub = jobHeader(yaml, 'publish');
    for (const id of ['build-windows', 'build-windows-arm64', 'build-portable', 'build-linux', 'build-macos']) {
      assert(pub.includes(id), `publish needs 缺 ${id}`);
    }
    assert(/mirror-gitee:[\s\S]*?needs: \[publish\]/.test(yaml.slice(yaml.indexOf('  mirror-gitee:'))), 'mirror-gitee 须 needs publish');
  });

  C('GATE-REF', '版本门禁①：5 个 build job 解析 tag 并 checkout 对应 ref', ({ yaml }) => {
    const n = (yaml.match(/ref: \$\{\{ steps\.ver\.outputs\.ref \}\}/g) || []).length;
    assert(n >= 5, `checkout steps.ver.outputs.ref 须 ≥5 处（每个 build job），实得 ${n}`);
    assert((yaml.match(/echo "version=\$\{tag#v\}" >> "\$GITHUB_OUTPUT"/g) || []).length >= 5, 'ver 步骤 v 前缀剥离须在每个 build job');
  });

  C('GATE-CONF', '版本门禁②：tauri.conf.json version == tag 断言（每个 build job）', ({ jobs }) => {
    const n = Object.values(jobs).flat().filter((s) => s.name.includes('Assert tauri.conf.json version == tag version')).length;
    assert(n >= 4, `tauri.conf 版本断言步骤须 ≥4，实得 ${n}`);
  });

  C('GATE-GLOB', '版本门禁③：产物名 glob 含期望版本（NSIS/DMG/AppImage/deb 重命名断言）', ({ yaml, jobs }) => {
    const bw = jobText(jobs['build-windows']);
    assert(bw.includes('*_"${VERSION}"_x64-setup.exe'), 'win x64 须按 *_${VERSION}_x64-setup.exe glob 断言 NSIS 内嵌版本');
    assert(bw.includes('DSH-Desktop-Setup-${VERSION}-win-x64.exe'), 'win x64 重命名须含期望版本');
    const bm = jobText(jobs['build-macos']);
    assert(bm.includes('DSH-Desktop-${VERSION}-macos-arm64.dmg'), 'mac 重命名须含期望版本');
    const bl = jobText(jobs['build-linux']);
    assert(bl.includes('dsh-desktop_${VERSION}_amd64.deb'), 'deb 命名须含期望版本');
  });

  C('GATE-SUM', '版本门禁④：publish 汇总断言（版本入名 + 体积下限 + 边车齐全）', ({ jobs }) => {
    const s = jobs.publish.find((x) => x.name.includes('Verify assets'));
    assert(s, 'publish 缺 Verify assets 步骤');
    const t = stepText(s);
    assert(t.includes('*"$VERSION"*'), '汇总须断言每个资产文件名内嵌期望版本');
    assert(t.includes('52428800'), '汇总须有 50MB 体积下限');
    assert(t.includes('main_n') && t.includes('side_n'), '汇总须断言边车数 == 主资产数');
  });

  C('MAC-DMG', 'mac：hdiutil attach 挂 DMG + 内层 codesign --verify 验证在位', ({ jobs }) => {
    const t = jobText(jobs['build-macos']);
    assert(t.includes('hdiutil attach'), '缺 hdiutil attach（mount-DMG 验证）');
    assert(t.includes('codesign --verify --deep --strict'), '缺 DMG 内层签名验证');
    assert(t.includes('wc -c <'), '体积统计须用 wc -c（BSD stat 无 -c，全平台可移植）');
  });

  C('GH-REPO', 'GH_REPO 动态注入存在（create/upload/verify/mirror ≥3 处）', ({ yaml }) => {
    const n = (yaml.match(/GH_REPO: *\${{\s*github\.repository\s*}}/g) || []).length;
    assert(n >= 3, `GH_REPO 须为 github.repository 注入且 ≥3 处，实得 ${n}`);
  });

  C('D3D-SRC', 'D3DCOMPILER①：源 DLL 在位 + 体积 + PE 检查（构建前）', ({ jobs }) => {
    const s = jobs['build-windows'].find((x) => x.name.includes('Assert bundled D3DCOMPILER_47.dll'));
    assert(s, '缺源 DLL 断言步骤');
    const t = stepText(s);
    assert(t.includes('dsh-tauri/dlls/D3DCOMPILER_47.dll'), '须锚定源 DLL 路径');
    assert(t.includes('4000000'), '须有体积下限（~4.6MB）');
    assert(t.includes('--pe') && t.includes('--expect-machine x64'), '须有 PE 头/架构检查（check-imports.mjs --pe）');
  });

  C('D3D-NSIS', 'D3DCOMPILER②：7z 列 NSIS 安装包含 DLL（B1 门禁）', ({ jobs }) => {
    const s = jobs['build-windows'].find((x) => x.name.includes('inside NSIS installer'));
    assert(s, '缺 7z NSIS 内容断言步骤');
    assert(stepText(s).includes('7z l') && stepText(s).includes('D3DCOMPILER_47.dll'), '须 7z l + grep D3DCOMPILER_47.dll');
  });

  C('D3D-PORT', 'D3DCOMPILER③：便携包拷贝 DLL 到 exe 旁 + 双断言', ({ jobs }) => {
    const s = jobs['build-portable'].find((x) => x.name.includes('D3DCOMPILER_47.dll beside portable exe'));
    assert(s, '缺便携包 DLL 旁路步骤');
    const t = stepText(s);
    assert(t.includes('cp "dsh-tauri/dlls/D3DCOMPILER_47.dll"'), '须从源拷贝');
    assert(t.includes('DSH Desktop.exe'), '须断言便携主 exe 在位（productName 对齐）');
  });

  C('SHA-GEN', 'sha256：publish 统一生成边车 + 数量断言', ({ jobs }) => {
    const s = jobs.publish.find((x) => x.name.includes('Generate sha256 sidecars'));
    assert(s, '缺 Generate sha256 sidecars 步骤');
    const t = stepText(s);
    assert(t.includes('sha256sum "$f" > "$f.sha256"'), '须 sha256sum 生成边车');
    assert(t.includes('64') && t.includes('hex'), '须校验边车格式（64 位小写 hex）');
    assert(t.includes('side_count') && t.includes('main_count'), '须断言边车数 == 主资产数');
  });

  C('SHA-VERIFY', 'sha256：上传后远端边车数量核对 + 镜像含边车断言', ({ yaml, jobs }) => {
    const up = jobText(jobs.publish);
    assert(up.includes('remote_side') && up.includes('local_side'), '上传后须核对远端/本地边车数');
    const mirror = yaml.slice(yaml.indexOf('  mirror-gitee:'));
    assert(mirror.includes(`select(endswith(".sha256"))`), '镜像终检须按 .sha256 后缀统计');
    assert(mirror.includes('side_total') && mirror.includes('side_mirrored'), '镜像终检须比对边车总数');
  });

  C('IDEMPOTENT', 'delete-then-upload 幂等段（先 delete-asset 后 upload；镜像漂移删 release 重建）', ({ yaml, jobs }) => {
    const up = jobs.publish.find((x) => x.name.includes('Upload assets to Release'));
    assert(up, '缺 Upload assets to Release 步骤');
    const t = stepText(up);
    const d = t.indexOf('gh release delete-asset');
    const u = t.indexOf('gh release upload');
    assert(d >= 0 && u >= 0 && d < u, '须先 delete-asset 再 upload（幂等重跑）');
    assert(t.includes('if ! gh release view "$TAG"'), 'release 不存在才 create（元数据不重触）');
    const mirror = yaml.slice(yaml.indexOf('  mirror-gitee:'));
    assert(mirror.includes('-X DELETE'), '镜像须有同名漂移删除重建路径');
  });

  C('NAMING', 'asset 命名 pattern 与 tauri.conf productName/version 引用一致', ({ yaml, jobs, conf }) => {
    assert(conf && typeof conf === 'object', 'tauri.conf.json 须可解析');
    assert(typeof conf.productName === 'string' && conf.productName.length > 0, 'tauri.conf 缺 productName');
    assert(/^\d+\.\d+\.\d+/.test(conf.version || ''), `tauri.conf version 须 semver，实得 ${conf.version}`);
    // NSIS 产物名 = <productName>_<version>_x64-setup.exe（Tauri 空格保留）→
    // workflow glob *_${VERSION}_x64-setup.exe 与之兼容，且便携主 exe 名须逐字
    // 等于 productName（含空格）。
    const bw = jobText(jobs['build-windows']);
    assert(bw.includes('_"${VERSION}"_x64-setup.exe'), 'NSIS glob 须含版本段');
    const port = jobText(jobs['build-portable']);
    assert(port.includes(`${conf.productName}.exe`), `便携包主 exe 须名为 ${conf.productName}.exe（productName 对齐）`);
    assert(port.includes(`Compress-Archive -Path "portable/${conf.productName}"`), 'zip 须打包 productName 目录');
    // publish 收集口径与 5 类产物命名一致
    const collect = jobText(jobs.publish);
    for (const ext of ['*.exe', '*.zip', '*.AppImage', '*.deb', '*.dmg']) {
      assert(collect.includes(`-name "${ext}"`), `publish 收集须含 ${ext}`);
    }
  });

  return checks;
}

// ---------------------------------------------------------------------------
// 执行器
// ---------------------------------------------------------------------------
function lint(yaml, conf) {
  const jobs = parseJobs(yaml);
  const ctx = { yaml, jobs, conf };
  const results = buildChecks().map((c) => {
    try { c.run(ctx); return { ...c, ok: true }; }
    catch (e) { return { ...c, ok: false, err: e.message }; }
  });
  return results;
}

function report(results, label) {
  const failed = results.filter((r) => !r.ok);
  console.log(`\n== ${label} ==`);
  for (const r of results) console.log(`${r.ok ? 'ok  ' : 'FAIL'}  ${r.id.padEnd(10)} ${r.desc}${r.ok ? '' : `\n      ${r.err}`}`);
  console.log(`${results.length - failed.length}/${results.length} passed`);
  return failed.length === 0;
}

// --test 自检：正样本（真实 workflow）须全过；负样本（删掉关键段）须被检出
function selfTest() {
  const yaml = readFileSync(join(ROOT, '.github/workflows/tauri-release.yml'), 'utf8');
  const conf = JSON.parse(readFileSync(join(ROOT, 'dsh-tauri/src-tauri/src/app/tauri.conf.json'), 'utf8'));
  let ok = report(lint(yaml, conf), '自检-正样本（真实 workflow，须全过）');

  const replaceAll = (s, r) => (y) => y.split(s).join(r);
  const mutations = [
    ['删 delete-asset', (y) => y.split('gh release delete-asset "$TAG" "$name" --yes').join('echo skip')],
    ['删全部 tauri.conf 版本门禁', replaceAll('Assert tauri.conf.json version == tag version', 'noop step')],
    ['删 sha256 边车生成', replaceAll('Generate sha256 sidecars', 'noop step')],
    ['删 D3D NSIS 断言', replaceAll('7z l "$f" | grep -i "D3DCOMPILER_47.dll"', 'true')],
    ['删 publish job', replaceAll('  publish:', '  publish-removed:')],
    ['mac 换掉 wc -c', replaceAll('wc -c <', 'stat -c%s ')],
  ];
  for (const [name, mut] of mutations) {
    const r = lint(mut(yaml), conf);
    const caught = r.some((x) => !x.ok);
    console.log(`${caught ? 'ok  ' : 'FAIL'}  自检-负样本「${name}」被检出`);
    if (!caught) ok = false;
  }
  return ok;
}

if (process.argv.includes('--test')) {
  process.exit(selfTest() ? 0 : 1);
}

const yaml = readFileSync(join(ROOT, '.github/workflows/tauri-release.yml'), 'utf8');
const conf = JSON.parse(readFileSync(join(ROOT, 'dsh-tauri/src-tauri/src/app/tauri.conf.json'), 'utf8'));
process.exit(report(lint(yaml, conf), 'tauri-release.yml 静态断言') ? 0 : 1);
