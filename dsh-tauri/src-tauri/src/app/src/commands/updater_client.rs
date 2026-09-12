//! 客户端更新引擎：GitHub/Gitee 双源「检测 + 下载」（UI 与安装动作归 U2/menu.rs）。
//!
//! # 公共契约（U2 按此调用，签名不得漂移）
//! ```text
//! pub enum UpdateSource { GitHub, Gitee }
//! pub struct ReleaseAsset { pub name: String, pub url: String, pub size: u64 }   // size=0 表示未知（Gitee 无该字段）
//! pub struct UpdateAvailable { pub current: String, pub next: String, pub notes: String,
//!                              pub asset: ReleaseAsset, pub source: UpdateSource }
//! pub enum CheckOutcome { UpToDate, Available(UpdateAvailable) }
//! pub enum UpdaterError { Offline(String), SourceUnreachable(String), BadManifest(String),
//!                         HashMismatch{expected:String,actual:String}, Download(String), Io(String) }
//! pub async fn check_latest(app_version: &str) -> Result<CheckOutcome, UpdaterError>;
//! pub fn pick_asset(assets: &[ReleaseAsset]) -> Option<ReleaseAsset>;
//! pub fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering;
//! pub async fn download_to_temp(asset: &ReleaseAsset, progress: impl FnMut(u64,u64),
//!                               sha256: Option<&str>) -> Result<std::path::PathBuf, UpdaterError>;
//! ```
//!
//! # 事件契约（lib.rs 启动 hook 发出，U2 垫片消费）
//! 事件名 **`client-update-available`**，载荷 = `UpdateAvailable` 的 JSON 形态：
//! `{"current":"0.5.2","next":"0.5.3","notes":"...","asset":{"name":..,"url":..,"size":..},"source":"github"|"gitee"}`
//! （`source` 序列化为小写 `"github"`/`"gitee"`）。启动早期 webview 可能尚未挂
//! 监听而错过该事件——垫片可经菜单 `check-client-update` 通道主动再查兜底。
//!
//! # 双源策略
//! - 元数据端点：GitHub `api.github.com/repos/myYangyunfan/dsh_desktop/releases/latest`
//!   （免 token，60/h）与 Gitee `gitee.com/api/v5/repos/my-yang-yunfan/dsh_desktop/releases/latest`；
//! - 两源并发探测（每源 8s 超时），都不通 → [`UpdaterError::Offline`]；
//! - 都通：取平台资产齐全者优先；均齐全 prefer Gitee（国内快）——但 tag 分歧时
//!   取严格更新者（Gitee 镜像同步滞后不得让用户停在旧版）；
//! - 单通：用通的；该源无本平台资产 → [`UpdaterError::BadManifest`]；
//! - Gitee 100MB/文件限：大资产（mac dmg/linux 包）常缺席 Gitee——资产级回落
//!   由「无平台资产者出局」自然达成（v0.5.2 实测：Gitee 仅 win 三件套）。
//!
//! # 完整性（哈希是硬需求）
//! 下载 sha256 来源优先级：调用方显式参数 > GitHub 资产 `digest` 字段
//! （`sha256:<hex>`，v0.5.2 起全部资产携带，经 check_latest 进程内缓存）>
//! `<资产名>.sha256` 边车资产（URL 推导 `asset.url + ".sha256"`，404 视为无）。
//! 有哈希必校验，不匹配硬失败 [`UpdaterError::HashMismatch`]；无哈希时兜底：
//! 元数据 size（>0 时）必须与落盘字节数一致（截断检测）+ Windows 安装器下限
//! >50MB（防损坏页/HTML 错误页装成 exe）。
//!
//! 与 menu.rs 的 `compare_versions`（内核 npm 版本比对，Electron 移植语义）互相
//! 独立；本模块 `cmp_semver` 是更强的 semver 实现（prerelease 链、build 元数据
//! 忽略、脏输入容错），供客户端更新链专用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// 契约类型
// ---------------------------------------------------------------------------

/// 更新元数据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateSource {
    GitHub,
    Gitee,
}

impl std::fmt::Display for UpdateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UpdateSource::GitHub => "GitHub",
            UpdateSource::Gitee => "Gitee",
        })
    }
}

/// 发行版资产（GitHub/Gitee 归一形态；Gitee 不提供 size → 0 = 未知）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// 单源 `/releases/latest` 归一结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRelease {
    pub tag: String,
    pub notes: String,
    pub assets: Vec<ReleaseAsset>,
}

/// 检测到可用更新（tag 严格大于本地版本时才产生）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UpdateAvailable {
    pub current: String,
    pub next: String,
    pub notes: String,
    pub asset: ReleaseAsset,
    pub source: UpdateSource,
}

/// `check_latest` 结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum CheckOutcome {
    UpToDate,
    Available(UpdateAvailable),
}

/// 更新引擎错误（`to_bridge`/`From` 转 [`bridge::BridgeError`] 供命令层用）。
#[derive(Debug, Clone, PartialEq)]
pub enum UpdaterError {
    /// 两源均不可达（离线/DNS 失败/全超时）。
    Offline(String),
    /// 单源不可达（HTTP 非 2xx / 连接失败）——另一源可能仍可用。
    SourceUnreachable(String),
    /// 可达源的响应不可解析为本平台可用的发行版信息。
    BadManifest(String),
    /// sha256 校验失败（硬失败，不得安装）。
    HashMismatch { expected: String, actual: String },
    /// 下载过程失败（网络中断/截断/大小校验不过）。
    Download(String),
    Io(String),
}

impl std::fmt::Display for UpdaterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdaterError::Offline(m) => write!(f, "离线（两源均不可达）：{m}"),
            UpdaterError::SourceUnreachable(m) => write!(f, "更新源不可达：{m}"),
            UpdaterError::BadManifest(m) => write!(f, "发行版信息异常：{m}"),
            UpdaterError::HashMismatch { expected, actual } => {
                write!(f, "sha256 校验失败：期望 {expected}，实际 {actual}")
            }
            UpdaterError::Download(m) => write!(f, "下载失败：{m}"),
            UpdaterError::Io(m) => write!(f, "io：{m}"),
        }
    }
}

impl std::error::Error for UpdaterError {}

impl UpdaterError {
    /// 转壳统一错误（错误码映射见实现；code 是稳定契约）。
    pub fn to_bridge(&self) -> bridge::BridgeError {
        self.clone().into()
    }
}

impl From<UpdaterError> for bridge::BridgeError {
    fn from(e: UpdaterError) -> Self {
        use bridge::codes;
        match e {
            UpdaterError::Offline(m) => bridge::BridgeError::new(codes::UPDATER_NETWORK, format!("更新检查离线：{m}")),
            UpdaterError::SourceUnreachable(m) => {
                bridge::BridgeError::new(codes::UPDATER_NETWORK, format!("更新源不可达：{m}"))
            }
            UpdaterError::BadManifest(m) => {
                bridge::BridgeError::new(codes::UPDATER_NETWORK, format!("发行版信息异常：{m}"))
            }
            // 哈希与 minisign 签名同属「产物完整性校验失败，fail-closed」域。
            UpdaterError::HashMismatch { expected, actual } => {
                bridge::BridgeError::new(codes::UPDATER_SIGNATURE, format!("安装包 sha256 校验失败：期望 {expected}，实际 {actual}"))
            }
            UpdaterError::Download(m) => bridge::BridgeError::new(codes::UPDATER_NETWORK, format!("更新下载失败：{m}")),
            UpdaterError::Io(m) => bridge::BridgeError::internal(format!("更新链 io：{m}")),
        }
    }
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// GitHub API（免 token，60 请求/h；v0.5.2 实测形状：tag_name + assets[].{name,browser_download_url,size,digest}）。
const GITHUB_LATEST: &str = "https://api.github.com/repos/solilk115-arch/dsh_desktop/releases/latest";
/// Gitee API v5（资产仅 {name,browser_download_url}，无 size；100MB/文件限）。
const GITEE_LATEST: &str = "https://gitee.com/api/v5/repos/solilk115-arch/dsh_desktop/releases/latest";
/// 单源元数据超时（探测与边车查询共用；spec：8s/源）。
const META_TIMEOUT: Duration = Duration::from_secs(8);
/// 下载：无总超时（大文件），连接 10s + 单次读 60s（静默死链防护）。
const DL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DL_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// 无哈希时的 Windows 安装器大小下限（spec：setup exe > 50MB）。
const SETUP_SIZE_FLOOR: u64 = 50 * 1024 * 1024;
/// notes 上限（发布说明全文可能很大，事件载荷/菜单展示截断；8KB 足覆盖要点）。
const NOTES_CAP: usize = 8 * 1024;

/// 装 rustls ring provider 为进程默认（幂等；tauri-plugin-updater 的
/// updater.rs:448 同款动作——reqwest 走 rustls-no-provider 特性时必须有
/// 进程级 provider，否则首次 TLS 握手 panic。先到先装，后到者 Err 忽略）。
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 元数据查询客户端（8s 总超时；UA 必带——GitHub API 无 UA 直接 403）。
fn meta_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ensure_crypto_provider();
        reqwest::Client::builder()
            .timeout(META_TIMEOUT)
            .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest meta client 构建")
    })
}

/// 下载客户端（无总超时；连接/读超时防死挂）。
fn dl_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ensure_crypto_provider();
        reqwest::Client::builder()
            .connect_timeout(DL_CONNECT_TIMEOUT)
            .read_timeout(DL_READ_TIMEOUT)
            .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest download client 构建")
    })
}

/// GitHub 资产 digest（`sha256:<hex>`）进程内缓存：check_latest 拉元数据时填充，
/// download_to_temp 消费——不改契约结构体也能让哈希校验今天就生效。
static SHA256_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn sha256_cache() -> &'static Mutex<HashMap<String, String>> {
    SHA256_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_sha256(name: &str, sha: String) {
    if let Ok(mut m) = sha256_cache().lock() {
        m.insert(name.to_string(), sha);
    }
}

fn cached_sha256(name: &str) -> Option<String> {
    sha256_cache().lock().ok()?.get(name).cloned()
}

/// 另一源同名资产的 URL 旁路缓存（V2 P1-1 修复）：digest 缓存存的是 **GitHub**
/// 的哈希，而 resolve 在双源同 tag 时 **prefer Gitee URL** 下载——镜像若重传过
/// 逐字节不同的文件，哈希必然 mismatch。此时用同源哈希硬卡死等于把「镜像漂移」
/// 误报成「产物被篡改」并断掉唯一更新路。本缓存记录另一源的同名资产 URL，
/// [`download_to_temp`] 在 HashMismatch 时自动换源重试一次：换源后同一 digest
/// 通过 = 镜像漂移（用权威源文件）；仍失败 = 真篡改，硬失败不变（fail-closed）。
static ALT_URL_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn cache_alt_url(name: &str, url: String) {
    if url.is_empty() {
        return;
    }
    if let Ok(mut m) = ALT_URL_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        m.insert(name.to_string(), url);
    }
}

fn cached_alt_url(name: &str) -> Option<String> {
    ALT_URL_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().ok()?.get(name).cloned()
}

/// RV8 P0-1 跨源哈希锚定：Gitee 单源可达（GitHub API 不可达——国内常态）时
/// digest 缓存为空、Gitee 资产无边车，50MB 下限**不是认证**——被劫持的镜像
/// 可塞任意大恶意安装器。锚定方案：为每个资产登记 **GitHub 侧**边车 URL
/// （`github.com/<repo>/releases/download/<tag>/<name>.sha256`——从 GitHub
/// release CDN 直取，API 不可达时 CDN 常仍可达，且与 Gitee 资产**不同源**，
/// 镜像劫持者无法伪造）。下载链兜底序追加：本源边车 → GitHub 边车（跨源
/// 锚）→ 仍无锚且主源是 Gitee → **fail-closed**（拒绝下载，提示手动更新）。
/// GitHub 自家源无锚时维持 size/50MB 下限（同源信任 = GitHub HTTPS）。
static CROSS_ANCHOR_URL_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

const GITHUB_REPO_PATH: &str = "solilk115-arch/dsh_desktop";

fn github_sidecar_url(tag: &str, name: &str) -> String {
    format!("https://github.com/{GITHUB_REPO_PATH}/releases/download/{tag}/{name}.sha256")
}

fn cache_cross_anchor(name: &str, url: String) {
    if url.is_empty() {
        return;
    }
    if let Ok(mut m) = CROSS_ANCHOR_URL_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        m.insert(name.to_string(), url);
    }
}

fn cached_cross_anchor(name: &str) -> Option<String> {
    CROSS_ANCHOR_URL_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().ok()?.get(name).cloned()
}

// ---------------------------------------------------------------------------
// Gitee 分片资产（.partN）—— GitHub 不可达用户的新版安装包唯一来路
// ---------------------------------------------------------------------------

/// 分片下载计划（进程内缓存，check_latest 登记、download_to_temp 消费）。
///
/// 背景：Gitee 单附件 100MB 上限，v0.6.x 起 Windows 安装器 ~110MB——
/// mirror-gitee job 对超限资产按 `<原名>.part1/.part2/…` 分片镜像（每片
/// ≤80MiB）；`<原名>.sha256` 边车是小文件照常整体镜像，锚的是**完整文件**
/// 的哈希。壳侧在此登记分片计划：某源只有分片形态时合成 name=原名的
/// 资产（url=part1，供展示），真实下载逐片顺序拼接后整文件哈希校验。
/// 实爆场景（v0.6.2 多用户）：GitHub 不可达 + Gitee 缺整资产 →
/// 「已查 ["Gitee"]，均无 windows/x86_64 可用安装包」永久卡死无法更新。
#[derive(Debug, Clone, PartialEq)]
pub struct PartsPlan {
    /// 分片下载 URL（part1..partN 顺序，逐片追加即原文）。
    pub urls: Vec<String>,
    /// 同 release 内 `<原名>.sha256` 边车资产的 URL（完整文件哈希锚；None=无边车）。
    pub sidecar_url: Option<String>,
    /// 元数据 size 合计（Gitee 资产无 size → 0 = 未知）。
    pub total_size: u64,
}

static PARTS_CACHE: OnceLock<Mutex<HashMap<String, PartsPlan>>> = OnceLock::new();

fn cache_parts(name: &str, plan: PartsPlan) {
    if let Ok(mut m) = PARTS_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        m.insert(name.to_string(), plan);
    }
}

fn cached_parts(name: &str) -> Option<PartsPlan> {
    PARTS_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().ok()?.get(name).cloned()
}

/// `<原名>.partN` → (`原名`, N)。N 非数字/0 → None（`.part` 是壳侧下载临时
/// 后缀，不得与分片命名混淆；`x.part1.sha256` 这类尾巴解析失败自然出局）。
fn parse_part_name(name: &str) -> Option<(&str, u32)> {
    let (base, idx) = name.rsplit_once(".part")?;
    if base.is_empty() {
        return None;
    }
    let idx = idx.parse::<u32>().ok()?;
    (idx > 0).then_some((base, idx))
}

/// 从单源资产清单里找本平台的分片套装：base 名过 [`asset_rank`]、分片号
/// 连续 1..=N。返回（合成整资产[name=base, url=part1, size=合计]，计划）。
/// 多套命中时按 base 的 rank 取最优（与整资产挑选同口径）。
fn find_part_set(os: &str, arch: &str, assets: &[ReleaseAsset]) -> Option<(ReleaseAsset, PartsPlan)> {
    let mut groups: HashMap<&str, Vec<(u32, &ReleaseAsset)>> = HashMap::new();
    for a in assets {
        if let Some((base, idx)) = parse_part_name(&a.name) {
            groups.entry(base).or_default().push((idx, a));
        }
    }
    let mut best: Option<(u8, &str, Vec<(u32, &ReleaseAsset)>)> = None;
    for (base, mut parts) in groups {
        let Some(rank) = asset_rank(os, arch, base) else { continue };
        parts.sort_by_key(|(i, _)| *i);
        // 分片号必须从 1 起连续：缺片拼接必坏，宁可当作没有分片。
        let contiguous = parts.iter().enumerate().all(|(k, (i, _))| *i == k as u32 + 1);
        if !contiguous {
            continue;
        }
        let better = best
            .as_ref()
            .map(|(r, _, _)| rank < *r)
            .unwrap_or(true);
        if better {
            best = Some((rank, base, parts));
        }
    }
    let (_, base, parts) = best?;
    let urls: Vec<String> = parts.iter().map(|(_, a)| a.url.clone()).collect();
    let total_size: u64 = parts.iter().map(|(_, a)| a.size).sum();
    let sidecar_url = assets
        .iter()
        .find(|a| a.name == format!("{base}.sha256"))
        .map(|a| a.url.clone());
    let synth = ReleaseAsset { name: base.to_string(), url: urls[0].clone(), size: total_size };
    Some((synth, PartsPlan { urls, sidecar_url, total_size }))
}

/// 清扫陈旧 `dsh-update-*` 临时目录（V2 P2-2）：崩溃/强杀中断的下载目录以
/// pid+nanos 命名永不复用，会永久残留（每次约 70MB）。下载前按 TTL（24h，
/// 远大于任何正常下载窗口）清一遍，best-effort 忽略一切错误。
fn sweep_stale_update_dirs() {
    const TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("dsh-update-") {
            continue;
        }
        let expired = std::fs::metadata(e.path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > TTL);
        if expired {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

// ---------------------------------------------------------------------------
// 版本比较（cmp_semver：比 menu.rs compare_versions 强——支持 prerelease 链）
// ---------------------------------------------------------------------------

/// 去掉单个前导 v/V（"v0.5.2" → "0.5.2"；"vv" 只剥一层——脏输入容错）。
fn strip_v(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t)
}

/// 解析 core 版本段（忽略 `+build` 元数据与 `-prerelease`）。非数字段按 0 计
/// （"garbage"/"" → 0；容错优先，语义稳定不 panic）。
fn core_segments(v: &str) -> Vec<u64> {
    v.split('.').map(|seg| seg.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect()
}

/// prerelease 标识符比较（semver 规则）：纯数字按数值、数值 < 字母数字、
/// 字母数字按 ASCII 字典序；空 prerelease（"" / "-" 尾巴）视为无 prerelease。
fn cmp_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ia: Vec<&str> = if a.is_empty() { vec![] } else { a.split('.').collect() };
    let ib: Vec<&str> = if b.is_empty() { vec![] } else { b.split('.').collect() };
    for i in 0..ia.len().max(ib.len()) {
        match (ia.get(i), ib.get(i)) {
            (None, Some(_)) => return Ordering::Less, // 前缀相同，更长 prerelease 更大：rc < rc.1
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let (nx, ny) = (x.parse::<u64>().ok(), y.parse::<u64>().ok());
                match (nx, ny) {
                    (Some(p), Some(q)) => match p.cmp(&q) {
                        Ordering::Equal => continue,
                        o => return o,
                    },
                    (Some(_), None) => return Ordering::Less, // 数值标识符 < 字母数字
                    (None, Some(_)) => return Ordering::Greater,
                    (None, None) => match x.cmp(y) {
                        Ordering::Equal => continue,
                        o => return o,
                    },
                }
            }
            (None, None) => unreachable!(),
        }
    }
    Ordering::Equal
}

/// semver 语义比较（客户端更新链专用，独立于 menu.rs 的 compare_versions）：
/// - 忽略前导 v 与 `+build` 元数据；核心段按数值（缺段 = 0：`1.0` == `1.0.0`）；
/// - **prerelease 链**：`0.5.2 < 0.5.3-rc.1 < 0.5.3-rc.2 < 0.5.3`（无后缀 > 有后缀）；
/// - 脏输入（空串/垃圾/空段）容错：垃圾段按 0 计，`""`/`"garbage"` 永远小于
///   任何真实版本（不会把垃圾判成可更新）。
pub fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ca, pa) = split_core_pre(strip_v(a));
    let (cb, pb) = split_core_pre(strip_v(b));
    let (sa, sb) = (core_segments(ca), core_segments(cb));
    for i in 0..sa.len().max(sb.len()) {
        let x = sa.get(i).copied().unwrap_or(0);
        let y = sb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    // 核心相等：无 prerelease > 有 prerelease；都有 → semver 标识符规则。
    match (pa.is_empty(), pb.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => cmp_prerelease(pa, pb),
    }
}

/// `0.5.3-rc.1+build` → (`0.5.3`, `rc.1`)：先剥 `+build`，再在首个 `-` 处分 core/pre。
fn split_core_pre(v: &str) -> (&str, &str) {
    let no_build = v.split_once('+').map(|(c, _)| c).unwrap_or(v);
    match no_build.split_once('-') {
        Some((core, pre)) => (core, pre),
        None => (no_build, ""),
    }
}

// ---------------------------------------------------------------------------
// 资产挑选（per OS+arch）
// ---------------------------------------------------------------------------

/// 本平台（编译期 `std::env::consts`）资产挑选——契约入口（U2/菜单链消费；
/// 引擎内部走 pick_asset_platform 参数化路径，故非测试构建暂无生产调用方）。
/// 平台矩阵（v0.5.2 实际产物命名）：
/// - windows x64/arm64 → `DSH-Desktop-Setup-<v>-win-{x64,arm64}.exe`（跳过 Portable zip）；
/// - macos arm64 → `DSH-Desktop-<v>-macos-arm64.dmg`（x64 mac 无产物则 None，不误装 arm64）；
/// - linux x64 → AppImage 优先，deb 兜底。
#[allow(dead_code)] // 公共契约面：签名稳定供 UI 链调用，测试矩阵已全覆盖
pub fn pick_asset(assets: &[ReleaseAsset]) -> Option<ReleaseAsset> {
    pick_asset_platform(std::env::consts::OS, std::env::consts::ARCH, assets)
}

/// 按显式 (os, arch) 挑选（矩阵单测/诊断用；与 `pick_asset` 同规则）。
/// 返回匹配资产，多个命中时按（安装器优先级 rank, 名字字典序）取最优。
pub fn pick_asset_platform(os: &str, arch: &str, assets: &[ReleaseAsset]) -> Option<ReleaseAsset> {
    assets
        .iter()
        .filter_map(|a| {
            let rank = asset_rank(os, arch, &a.name)?;
            Some((rank, a))
        })
        .min_by_key(|(rank, a)| (*rank, a.name.clone()))
        .map(|(_, a)| a.clone())
}

/// 资产 → 安装器优先级（0 最优；None = 不适配本平台/非安装产物）。
/// 排除项：`.sha256` 边车、`.blockmap`/`.sig`（tauri updater 残留）、源码包
/// （Gitee 的 `vX.Y.Z.zip`/`.tar.gz`——扩展名白名单天然排除）。
fn asset_rank(os: &str, arch: &str, name: &str) -> Option<u8> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".sha256") || n.ends_with(".blockmap") || n.ends_with(".sig") {
        return None;
    }
    let arch_ok = |tags: &[&str]| tags.iter().any(|t| n.contains(t));
    match os {
        // Windows：Setup exe（NSIS）；Portable zip 是免安装形态，自动更新链不用。
        "windows" => {
            if !n.ends_with(".exe") || !n.contains("setup") || n.contains("portable") {
                return None;
            }
            match arch {
                "x86_64" if arch_ok(&["win-x64", "win-x86_64", "win_amd64", "win64"]) => Some(0),
                "aarch64" if arch_ok(&["win-arm64", "win-aarch64"]) => Some(0),
                _ => None,
            }
        }
        "macos" => {
            if !n.ends_with(".dmg") {
                return None;
            }
            match arch {
                "aarch64" if arch_ok(&["macos-arm64", "darwin-arm64", "mac-arm64", "arm64"]) => Some(0),
                "x86_64" if arch_ok(&["macos-x64", "x64_mac", "x64-macos", "darwin-x64", "macos-amd64", "macos-x86_64"]) => Some(0),
                _ => None,
            }
        }
        "linux" => {
            let x64 = arch == "x86_64" && arch_ok(&["linux-x64", "x86_64", "amd64"]);
            let arm64 = arch == "aarch64" && arch_ok(&["linux-arm64", "aarch64", "arm64"]);
            if !x64 && !arm64 {
                return None;
            }
            if n.ends_with(".appimage") {
                Some(0)
            } else if n.ends_with(".deb") {
                Some(1)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 响应解析（GitHub/Gitee 两形状；Gitee 资产缺 size/digest → serde default 容忍）
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RawRelease {
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(serde::Deserialize)]
struct RawAsset {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    browser_download_url: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    digest: Option<String>,
}

/// 解析单源 `/releases/latest` 文档 → (归一发行版, [资产名→sha256])。
/// 字段缺失/类型错乱 → [`UpdaterError::BadManifest`]；资产缺 name/url 的条目跳过。
fn parse_release_doc(doc: &str) -> Result<(RemoteRelease, Vec<(String, String)>), UpdaterError> {
    let raw: RawRelease =
        serde_json::from_str(doc).map_err(|e| UpdaterError::BadManifest(format!("JSON 解析失败：{e}")))?;
    let tag = raw.tag_name.filter(|t| !t.trim().is_empty()).ok_or_else(|| UpdaterError::BadManifest("响应缺 tag_name".into()))?;
    let mut assets = Vec::with_capacity(raw.assets.len());
    let mut digests = Vec::new();
    for a in raw.assets {
        let (Some(name), Some(url)) = (a.name, a.browser_download_url) else { continue };
        if let Some(sha) = a.digest.as_deref().and_then(parse_digest) {
            digests.push((name.clone(), sha));
        }
        assets.push(ReleaseAsset { name, url, size: a.size.unwrap_or(0) });
    }
    Ok((RemoteRelease { tag, notes: raw.body.unwrap_or_default(), assets }, digests))
}

/// `sha256:<64hex>` → `<hex>`（小写）；其余形态 None。
fn parse_digest(v: &str) -> Option<String> {
    let hex = v.strip_prefix("sha256:")?;
    hex_ok(hex).then(|| hex.to_ascii_lowercase())
}

/// 边车内容解析：兼容 `<hex>`、`<hex>  <文件名>`、`sha256:<hex>` 形态；非 64hex → None。
fn parse_sha256_sidecar(text: &str) -> Option<String> {
    let tok = text.split_whitespace().next()?;
    let hex = tok.strip_prefix("sha256:").unwrap_or(tok);
    hex_ok(hex).then(|| hex.to_ascii_lowercase())
}

fn hex_ok(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// 双源检测
// ---------------------------------------------------------------------------

/// 单源拉取（8s 超时；digest 进缓存）。HTTP 非 2xx → SourceUnreachable
/// （GitHub 403 限流也归此类——另一源可能仍可用）。
pub async fn fetch_release(source: UpdateSource) -> Result<RemoteRelease, UpdaterError> {
    let (url, accept) = match source {
        UpdateSource::GitHub => (GITHUB_LATEST, "application/vnd.github+json"),
        UpdateSource::Gitee => (GITEE_LATEST, "application/json"),
    };
    let mut req = meta_client().get(url).header(reqwest::header::ACCEPT, accept);
    if matches!(source, UpdateSource::GitHub) {
        // GitHub 官方建议的稳定 API 版本头（Gitee 不发）。
        req = req.header("x-github-api-version", "2022-11-28");
    }
    let resp = req
        .send()
        .await
        .map_err(|e| UpdaterError::SourceUnreachable(format!("{source}: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdaterError::SourceUnreachable(format!("{source}: HTTP {}", resp.status())));
    }
    let doc = resp.text().await.map_err(|e| UpdaterError::SourceUnreachable(format!("{source}: 读体失败：{e}")))?;
    let (release, digests) = parse_release_doc(&doc)?;
    for (name, sha) in digests {
        cache_sha256(&name, sha);
    }
    Ok(release)
}

/// 检查客户端更新（契约入口）：双源并发探测（各 8s）→ 平台资产齐全者优先 →
/// tag 严格大于本地才报 [`CheckOutcome::Available`]（防降级：Equal/Less → UpToDate）。
pub async fn check_latest(app_version: &str) -> Result<CheckOutcome, UpdaterError> {
    // 并发探测两源（各 8s 超时）；源标签随任务携带，成败顺序无关。
    let probe = |source| async move { fetch_release(source).await.map(|r| (source, r)) };
    let gh = tauri::async_runtime::spawn(probe(UpdateSource::GitHub));
    let gitee = tauri::async_runtime::spawn(probe(UpdateSource::Gitee));
    let mut cands: Vec<(UpdateSource, RemoteRelease)> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for h in [gh, gitee] {
        match h.await {
            Ok(Ok(pair)) => cands.push(pair),
            Ok(Err(e)) => errs.push(e.to_string()),
            Err(e) => errs.push(format!("探测任务失败：{e}")),
        }
    }
    resolve_outcome(app_version, std::env::consts::OS, std::env::consts::ARCH, cands, errs)
}

/// 检测决策（纯函数，单测主场）：
/// 1. 两源全灭 → Offline；
/// 2. 有平台资产者出局筛选：Gitee 排前（国内快），但两源都有资产且 tag 分歧时
///    取严格更新者（镜像滞后不打折）；
/// 3. 候选全无平台资产 → BadManifest（含「唯一可达源缺本平台资产」场景）；
/// 4. 选定源 tag ≤ 本地 → UpToDate（防降级）。
fn resolve_outcome(
    app_version: &str,
    os: &str,
    arch: &str,
    mut cands: Vec<(UpdateSource, RemoteRelease)>,
    errs: Vec<String>,
) -> Result<CheckOutcome, UpdaterError> {
    if cands.is_empty() {
        return Err(UpdaterError::Offline(errs.join("；")));
    }
    // Gitee 优先序（稳定排序不破坏同源内次序）。
    cands.sort_by_key(|(s, _)| if matches!(s, UpdateSource::Gitee) { 0u8 } else { 1 });
    if cands.len() == 2 {
        // 两源都通：tag 分歧时取严格更新者（Gitee 镜像同步滞后不得让用户停在旧版）；
        // tag 相等保持 Gitee 在前（国内快）。
        if cmp_semver(strip_v(&cands[1].1.tag), strip_v(&cands[0].1.tag)) == std::cmp::Ordering::Greater {
            cands.swap(0, 1);
        }
    }
    let mut chosen: Option<(UpdateSource, RemoteRelease, ReleaseAsset)> = None;
    let mut scanned: Vec<String> = Vec::new();
    for (source, release) in &cands {
        scanned.push(source.to_string());
        if let Some(asset) = pick_asset_platform(os, arch, &release.assets) {
            chosen = Some((*source, release.clone(), asset));
            break;
        }
    }
    if chosen.is_none() {
        // 分片回落（Gitee 100MB 限镜像形态）：某源无整资产但有本平台
        // `.partN` 套装时合成整资产并登记分片计划，下载链逐片拼接后整文件
        // 哈希校验——GitHub 不可达的国内单源用户由此不再卡死在旧版。
        for (source, release) in &cands {
            if let Some((synth, plan)) = find_part_set(os, arch, &release.assets) {
                cache_parts(&synth.name, plan);
                // 跨源锚照常登记（GitHub CDN 可达时的额外哈希兑底）。
                cache_cross_anchor(&synth.name, github_sidecar_url(&release.tag, &synth.name));
                chosen = Some((*source, release.clone(), synth));
                break;
            }
        }
    }
    let Some((source, release, asset)) = chosen else {
        return Err(UpdaterError::BadManifest(format!(
            "已查 {scanned:?}，均无 {os}/{arch} 可用安装包（整资产与 .partN 分片形态均未发现）——请到 GitHub Releases 或 Gitee Releases（gitee.com/my-yang-yunfan/dsh_desktop）手动下载安装包；Gitee 分片资产可连同 .merge.bat 一并下载后双击合并"
        )));
    };
    // V2 P1-1：登记另一源的同名资产 URL（HashMismatch 时换源重试用）。
    // 仅双源都在场时有意义；单源场景缓存缺席 → mismatch 直接硬失败（正确）。
    for (other_source, other_release) in &cands {
        if other_source == &source {
            continue;
        }
        if let Some(other_asset) = other_release.assets.iter().find(|a| a.name == asset.name) {
            cache_alt_url(&asset.name, other_asset.url.clone());
        }
    }
    // RV8 P0-1：登记 GitHub 侧边车 URL 作为跨源哈希锚（Gitee 单源场景的
    // 唯一可信锚——GitHub CDN 与 Gitee 资产不同源，镜像劫持者无法伪造；
    // tag 漂移时该 URL 404 → 锚缺席 → Gitee 下载 fail-closed）。
    cache_cross_anchor(&asset.name, github_sidecar_url(&release.tag, &asset.name));
    let next = strip_v(&release.tag).to_string();
    if cmp_semver(&next, app_version) != std::cmp::Ordering::Greater {
        return Ok(CheckOutcome::UpToDate);
    }
    Ok(CheckOutcome::Available(UpdateAvailable {
        current: app_version.to_string(),
        next,
        notes: cap_notes(&release.notes),
        asset,
        source,
    }))
}

/// notes 截断（事件载荷/菜单展示上限；按 char 边界截，防切碎中文）。
fn cap_notes(s: &str) -> String {
    if s.len() <= NOTES_CAP {
        return s.to_string();
    }
    let mut end = NOTES_CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（已截断）", &s[..end])
}

// ---------------------------------------------------------------------------
// 下载
// ---------------------------------------------------------------------------

/// 流式下载到系统临时目录（`<temp>/dsh-update-<pid>-<nanos>/<资产名>`）。
///
/// - 进度回调 `(已收字节, 总字节)`，总字节 = Content-Length 或元数据 size，
///   两者皆缺时 total = 0（未知，调用方需容忍）；
/// - `sha256 = Some` 强制校验；None 时依次尝试 digest 缓存（check_latest 填充）
///   与 `<url>.sha256` 边车（404 = 无边车，不阻断——v0.5.2 尚无边车，向前兼容）；
/// - 无任何哈希时兜底：元数据 size（>0）必须吻合 + Windows 安装器 >50MB 下限；
/// - 失败清理临时目录（不留半截 .part 冒充完整包）；成功返回最终文件路径
///   （安装动作由 U2 执行；断点续传不支持，失败重下）。
pub async fn download_to_temp(
    asset: &ReleaseAsset,
    // FnMut（RV9 P1 节流回调需要内部可变性保存上次发射状态）。
    mut progress: impl FnMut(u64, u64),
    sha256: Option<&str>,
) -> Result<PathBuf, UpdaterError> {
    // V2 P2-3：调用方显式传入的哈希形态非法（非 64 hex）必须硬失败——静默降级
    // 到边车/size 兜底会让校验强度悄悄变弱，违背「显式参数最高优先」的契约。
    let expected: Option<String> = match sha256 {
        Some(s) => {
            let lower = s.to_ascii_lowercase();
            if !hex_ok(&lower) {
                return Err(UpdaterError::BadManifest(format!("显式 sha256 形态非法（需 64 位 hex）: {s}")));
            }
            Some(lower)
        }
        None => match cached_sha256(&asset.name) {
            Some(s) => Some(s),
            None => {
                // 哈希锚优先序（digest 缓存之后）：边车（分片套装用同 release
                // 镜像的整文件边车；整资产用 url 推导）→ 跨源 GitHub 边车锚。
                let mut anchors: Vec<String> = Vec::new();
                match cached_parts(&asset.name).and_then(|p| p.sidecar_url) {
                    Some(u) => anchors.push(u),
                    None => anchors.push(format!("{}.sha256", asset.url)),
                }
                if let Some(anchor) = cached_cross_anchor(&asset.name) {
                    if !anchors.contains(&anchor) {
                        anchors.push(anchor);
                    }
                }
                let mut resolved = None;
                for u in &anchors {
                    if let Some(s) = fetch_sidecar_url_sha256(u).await {
                        resolved = Some(s);
                        break;
                    }
                }
                resolved
            }
        },
    };
    // RV8 P0-1 fail-closed：Gitee 源且全链无任何哈希锚 → 拒绝下载（50MB
    // 下限不是认证——镜像劫持可塞任意大恶意安装器；提示手动更新）。
    // GitHub 源维持 size/50MB 下限兜底（同源信任 = GitHub HTTPS，且其
    // API 正常时必有 digest）。
    if expected.is_none() && asset.url.contains("gitee.com") {
        return Err(UpdaterError::Download(
            "Gitee 源下载缺少可用的哈希锚点（GitHub digest/边车均不可达）——为防镜像投毒已拒绝自动下载；请稍后重试或到 GitHub Releases 手动下载".into(),
        ));
    }
    sweep_stale_update_dirs();
    let dir = std::env::temp_dir().join(format!("dsh-update-{}-{}", std::process::id(), unique_suffix()));
    std::fs::create_dir_all(&dir).map_err(|e| UpdaterError::Io(e.to_string()))?;
    let safe = sanitize_filename(&asset.name);
    let part = dir.join(format!("{safe}.part"));
    let final_path = dir.join(&safe);
    // 分片套装（Gitee 超限镜像形态）：逐片顺序拼接为整文件后校验——另一源
    // 无同名分片可换，不走换源重试；整资产维持原单请求路径（含 HashMismatch
    // 换源重试，见 stream_direct_with_alt_retry）。
    let outcome = if let Some(plan) = cached_parts(&asset.name) {
        stream_parts_to_file(&plan, &asset.name, &part, &mut progress, expected.as_deref()).await
    } else {
        stream_direct_with_alt_retry(asset, &part, progress, expected.as_deref()).await
    };
    match outcome {
        Ok(()) => {
            // V2 P2-1：rename 失败也必须清理整个临时目录（不留 70MB 半截 .part）。
            if let Err(e) = std::fs::rename(&part, &final_path) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(UpdaterError::Io(e.to_string()));
            }
            Ok(final_path)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// 流式落盘 + 哈希/大小校验（`.part` 先行，校验全过才由调用方 rename）。
/// RV8 P1：下载前 URL 门禁——https 且 host ∈ 更新源白名单（或回环地址，
/// 供本地测试；非 https 的远程地址直接拒绝，防未来哈希路径被绕过后立即
/// 变成任意/file:// URL 下载器）。
fn assert_download_url_allowed(url: &str) -> Result<(), UpdaterError> {
    let parsed = url::Url::parse(url).map_err(|_| UpdaterError::Download(format!("下载 URL 非法：{url}")))?;
    let host = parsed.host_str().unwrap_or("");
    let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
    let host_ok = host == "github.com" || host == "gitee.com" || host == "objects.githubusercontent.com";
    let scheme_ok = parsed.scheme() == "https" || (is_loopback && parsed.scheme() == "http");
    if scheme_ok && (host_ok || is_loopback) {
        Ok(())
    } else {
        Err(UpdaterError::Download(format!("下载 URL 不在更新源白名单（https + github/gitee）：{url}")))
    }
}

async fn stream_to_file(
    asset: &ReleaseAsset,
    part: &Path,
    progress: &mut impl FnMut(u64, u64),
    expected_sha: Option<&str>,
) -> Result<(), UpdaterError> {
    assert_download_url_allowed(&asset.url)?;
    let mut resp = dl_client()
        .get(&asset.url)
        .send()
        .await
        .map_err(|e| UpdaterError::Download(format!("请求失败：{e}")))?;
    if !resp.status().is_success() {
        return Err(UpdaterError::Download(format!("HTTP {}", resp.status())));
    }
    let total = resp.content_length().unwrap_or(asset.size);
    let mut file = std::fs::File::create(part).map_err(|e| UpdaterError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| UpdaterError::Download(format!("流中断：{e}")))? {
        std::io::Write::write_all(&mut file, &chunk).map_err(|e| UpdaterError::Io(e.to_string()))?;
        hasher.update(&chunk);
        received += chunk.len() as u64;
        progress(received, total);
    }
    // 截断检测：元数据 size（>0 = 已知）必须与落盘一致。
    if asset.size > 0 && received != asset.size {
        return Err(UpdaterError::Download(format!("下载不完整：收到 {received} B，元数据标注 {} B", asset.size)));
    }
    if let Some(exp) = expected_sha {
        let actual = format!("{:x}", hasher.finalize());
        if actual != exp {
            return Err(UpdaterError::HashMismatch { expected: exp.to_string(), actual });
        }
    } else if is_setup_exe(&asset.name) && received < SETUP_SIZE_FLOOR {
        // 无哈希兜底（向前兼容 v0.5.2 无边车期）：损坏页/错误页装不成安装器。
        return Err(UpdaterError::Download(format!(
            "安装包疑似损坏：{received} B 低于下限 {SETUP_SIZE_FLOOR} B（且无 sha256 可校验）"
        )));
    }
    Ok(())
}

/// 整资产单请求下载 + HashMismatch 换源重试（V2 P1-1 / TA9-1 语义原为
/// download_to_temp 内联 match，分片路径拆出后收口为独立函数）。
async fn stream_direct_with_alt_retry(
    asset: &ReleaseAsset,
    part: &Path,
    mut progress: impl FnMut(u64, u64),
    expected_sha: Option<&str>,
) -> Result<(), UpdaterError> {
    match stream_to_file(asset, part, &mut progress, expected_sha).await {
        Err(err @ UpdaterError::HashMismatch { .. }) => {
            // digest 缓存是 GitHub 的哈希而本源（Gitee）文件可能镜像漂移
            // ——换另一源同名资产重试一次；同 digest 通过 = 漂移（用权威源文件），
            // 仍失败 = 真篡改，原样硬失败（fail-closed 不变）。
            match cached_alt_url(&asset.name) {
                Some(alt_url) if alt_url != asset.url => {
                    let _ = std::fs::remove_file(part);
                    let mut alt = asset.clone();
                    alt.url = alt_url;
                    // 另一源的元数据 size 未知（本结构体只带首选源的）→ 置 0 走
                    // 「未知」语义（跳过截断核对）；哈希校验不受影响仍强制。
                    alt.size = 0;
                    match stream_to_file(&alt, part, &mut progress, expected_sha).await {
                        // TA9-1：双源均不符时带逐路摘要——单看一条 HashMismatch
                        // 无法区分「镜像漂移（换源救不回=全面投毒）」与单源故障，
                        // 支持排障需要两路期望/实际对照。（产出值而非提前 return：
                        // 外层 outcome 分支负责临时目录清理。）
                        Err(alt_err @ UpdaterError::HashMismatch { .. }) => {
                            if let (UpdaterError::HashMismatch { expected: e1, actual: a1 },
                                    UpdaterError::HashMismatch { expected: _, actual: a2 }) = (&err, &alt_err) {
                                Err(UpdaterError::HashMismatch {
                                    expected: format!(
                                        "{e1}（双源均不符，疑似全面投毒/发布事故——主源实际={a1}；换源后实际={a2}）"
                                    ),
                                    actual: a2.clone(),
                                })
                            } else {
                                Err(alt_err)
                            }
                        }
                        other => other,
                    }
                }
                _ => Err(err),
            }
        }
        other => other,
    }
}

/// 分片下载（Gitee 100MB 限镜像形态）：逐片顺序追加写同一输出文件
/// （part1..N 字节序拼接 = 原文件），全部写完后对整文件做哈希/大小校验。
/// 单片失败即整体失败（与整资产同语义：不支持断点续传，失败重下）；
/// progress 跨片累计（已收字节, 元数据合计——Gitee 无 size → total=0 未知）。
async fn stream_parts_to_file(
    plan: &PartsPlan,
    name: &str,
    out: &Path,
    progress: &mut impl FnMut(u64, u64),
    expected_sha: Option<&str>,
) -> Result<(), UpdaterError> {
    let mut file = std::fs::File::create(out).map_err(|e| UpdaterError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    for (i, url) in plan.urls.iter().enumerate() {
        assert_download_url_allowed(url)?;
        let mut resp = dl_client()
            .get(url)
            .send()
            .await
            .map_err(|e| UpdaterError::Download(format!("分片 {} 请求失败：{e}", i + 1)))?;
        if !resp.status().is_success() {
            return Err(UpdaterError::Download(format!("分片 {} HTTP {}", i + 1, resp.status())));
        }
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| UpdaterError::Download(format!("分片 {} 流中断：{e}", i + 1)))?
        {
            std::io::Write::write_all(&mut file, &chunk).map_err(|e| UpdaterError::Io(e.to_string()))?;
            hasher.update(&chunk);
            received += chunk.len() as u64;
            progress(received, plan.total_size);
        }
    }
    // 截断检测：元数据合计（>0 = 已知）必须与拼接后一致（GitHub 分片有 size；
    // Gitee 无 → 跳过，完整性由哈希独挑）。
    if plan.total_size > 0 && received != plan.total_size {
        return Err(UpdaterError::Download(format!(
            "分片拼接不完整：收到 {received} B，元数据合计 {} B",
            plan.total_size
        )));
    }
    if let Some(exp) = expected_sha {
        let actual = format!("{:x}", hasher.finalize());
        if actual != exp {
            return Err(UpdaterError::HashMismatch { expected: exp.to_string(), actual });
        }
    } else if is_setup_exe(name) && received < SETUP_SIZE_FLOOR {
        // 无哈希兑底（与整资产同口径；Gitee 源已被上游 fail-closed 拦住，
        // 此分支实际只覆盖 GitHub 分片无边车且无 digest 的罕见态）。
        return Err(UpdaterError::Download(format!(
            "安装包疑似损坏：分片拼接后 {received} B 低于下限 {SETUP_SIZE_FLOOR} B（且无 sha256 可校验）"
        )));
    }
    Ok(())
}

/// 尝试取边车哈希：`GET <asset.url>.sha256`，非 200/不可解析 → None（不阻断）。
async fn fetch_sidecar_sha256(asset_url: &str) -> Option<String> {
    fetch_sidecar_url_sha256(&format!("{asset_url}.sha256")).await
}

/// 取**最终形态**边车 URL 的哈希（调用方已拼好 `.sha256` 后缀——分片套装的
/// 同源边车资产 URL 与跨源 GitHub 锚都属此类；历史上把它们再喂给
/// [`fetch_sidecar_sha256`] 会二次追加后缀必 404，交叉锚形同虚设）。
async fn fetch_sidecar_url_sha256(sidecar_url: &str) -> Option<String> {
    let resp = meta_client().get(sidecar_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    parse_sha256_sidecar(&resp.text().await.ok()?)
}

fn is_setup_exe(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".exe") && n.contains("setup") && !n.contains("portable")
}

/// 文件名净化：白名单外字符替换为 `_`（资产名来自远端，防路径穿越/非法字符）。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// 临时目录随机后缀（无 rand 依赖：原子计数 + 纳秒）。
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("{nanos:08x}-{n}")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub `/releases/latest` 真实响应脱敏样本（2026-08-21 v0.5.2 实抓：
    /// 去 author/uploader 等个人信息，保留 tag/assets 形状与真实 size/digest）。
    const GH_FIXTURE: &str = r##"{
  "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/853366",
  "html_url": "https://github.com/myYangyunfan/dsh_desktop/releases/tag/v0.5.2",
  "id": 853366,
  "tag_name": "v0.5.2",
  "target_commitish": "main",
  "name": "DSH Desktop v0.5.2",
  "draft": false,
  "immutable": false,
  "prerelease": false,
  "created_at": "2026-08-21T20:50:11Z",
  "published_at": "2026-08-21T20:55:00Z",
  "body": "# DSH Desktop v0.5.2\n\n修复：频繁重启/白屏根治；余额功能收口。",
  "assets": [
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/1",
      "id": 1,
      "name": "DSH-Desktop-0.5.2-linux-x64.AppImage",
      "content_type": "application/vnd.appimage",
      "state": "uploaded",
      "size": 190437880,
      "digest": "sha256:67ebea6c38aec3e2992edb4a30e9f3eb84bff0148dfdc539b2826082dbbb99d0",
      "download_count": 1,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-0.5.2-linux-x64.AppImage"
    },
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/2",
      "id": 2,
      "name": "DSH-Desktop-0.5.2-macos-arm64.dmg",
      "content_type": "application/x-apple-diskimage",
      "state": "uploaded",
      "size": 119223466,
      "digest": "sha256:ed71832e4c27b7e7cf65691510179faf4727c15c87a5e6c5752c2a3fb1606455",
      "download_count": 2,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-0.5.2-macos-arm64.dmg"
    },
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/3",
      "id": 3,
      "name": "DSH-Desktop-Portable-0.5.2-win-x64.zip",
      "content_type": "application/zip",
      "state": "uploaded",
      "size": 102477786,
      "digest": "sha256:d43024918a61d02755035d7a9945a3dd266453976393de4b5f111c031bf4f94f",
      "download_count": 3,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Portable-0.5.2-win-x64.zip"
    },
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/4",
      "id": 4,
      "name": "DSH-Desktop-Setup-0.5.2-win-arm64.exe",
      "content_type": "application/x-msdownload",
      "state": "uploaded",
      "size": 68534272,
      "digest": "sha256:ee2a964de870d07c043f9e29380d15b3a10b90af2170272b2a74545f049e36e6",
      "download_count": 4,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Setup-0.5.2-win-arm64.exe"
    },
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/5",
      "id": 5,
      "name": "DSH-Desktop-Setup-0.5.2-win-x64.exe",
      "content_type": "application/x-msdownload",
      "state": "uploaded",
      "size": 72272680,
      "digest": "sha256:8e062808478cf7bcc311b10414095f634ce5566d7686bf8498803898995e9646",
      "download_count": 5,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Setup-0.5.2-win-x64.exe"
    },
    {
      "url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/releases/assets/6",
      "id": 6,
      "name": "dsh-desktop_0.5.2_amd64.deb",
      "content_type": "application/vnd.debian.binary-package",
      "state": "uploaded",
      "size": 122865758,
      "digest": "sha256:07830f02473945f7904fdbe83fafebd0bcef05ed1fbcfaa4c3c4faa7f2da7859",
      "download_count": 6,
      "browser_download_url": "https://github.com/myYangyunfan/dsh_desktop/releases/download/v0.5.2/dsh-desktop_0.5.2_amd64.deb"
    }
  ],
  "tarball_url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/tarball/v0.5.2",
  "zipball_url": "https://api.github.com/repos/myYangyunfan/dsh_desktop/zipball/v0.5.2"
}"##;

    /// Gitee `/releases/latest` 真实响应脱敏样本（同日实抓）：资产只有
    /// name/browser_download_url（**无 size、无 digest**），且 100MB 限导致
    /// mac dmg / linux 包缺席，另带源码归档——回落与排除逻辑的实弹靶场。
    const GITEE_FIXTURE: &str = r##"{
  "id": 853366,
  "tag_name": "v0.5.2",
  "target_commitish": "main",
  "prerelease": false,
  "name": "DSH Desktop v0.5.2",
  "body": "# DSH Desktop v0.5.2\n\n修复：频繁重启/白屏根治；余额功能收口。",
  "created_at": "2026-08-21T20:50:11Z",
  "assets": [
    {
      "browser_download_url": "https://gitee.com/my-yang-yunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Setup-0.5.2-win-x64.exe",
      "name": "DSH-Desktop-Setup-0.5.2-win-x64.exe"
    },
    {
      "browser_download_url": "https://gitee.com/my-yang-yunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Setup-0.5.2-win-arm64.exe",
      "name": "DSH-Desktop-Setup-0.5.2-win-arm64.exe"
    },
    {
      "browser_download_url": "https://gitee.com/my-yang-yunfan/dsh_desktop/releases/download/v0.5.2/DSH-Desktop-Portable-0.5.2-win-x64.zip",
      "name": "DSH-Desktop-Portable-0.5.2-win-x64.zip"
    },
    {
      "browser_download_url": "https://gitee.com/my-yang-yunfan/dsh_desktop/archive/refs/tags/v0.5.2.zip",
      "name": "v0.5.2.zip"
    },
    {
      "browser_download_url": "https://gitee.com/my-yang-yunfan/dsh_desktop/archive/refs/tags/v0.5.2.tar.gz",
      "name": "v0.5.2.tar.gz"
    }
  ]
}"##;

    fn gh_release() -> RemoteRelease {
        parse_release_doc(GH_FIXTURE).unwrap().0
    }

    fn gitee_release() -> RemoteRelease {
        parse_release_doc(GITEE_FIXTURE).unwrap().0
    }

    fn asset(name: &str, url: &str, size: u64) -> ReleaseAsset {
        ReleaseAsset { name: name.into(), url: url.into(), size }
    }

    // ---- 1. cmp_semver 矩阵 ----

    #[test]
    fn cmp_semver_core_matrix() {
        use std::cmp::Ordering::*;
        // 基本 + v 前缀。
        assert_eq!(cmp_semver("0.5.2", "0.5.2"), Equal);
        assert_eq!(cmp_semver("v0.5.2", "0.5.2"), Equal);
        assert_eq!(cmp_semver("0.5.3", "v0.5.2"), Greater);
        assert_eq!(cmp_semver("0.5.2", "0.5.3"), Less);
        // 跨段进位（非字符串比较）。
        assert_eq!(cmp_semver("0.6.0", "0.5.9"), Greater);
        assert_eq!(cmp_semver("0.10.0", "0.9.9"), Greater);
        // 段数不齐：缺段 = 0。
        assert_eq!(cmp_semver("1.0", "1.0.0"), Equal);
        assert_eq!(cmp_semver("1.0.1", "1.0"), Greater);
        // build 元数据忽略。
        assert_eq!(cmp_semver("0.5.3+build.7", "0.5.3"), Equal);
    }

    #[test]
    fn cmp_semver_prerelease_chain() {
        use std::cmp::Ordering::*;
        // spec 链：0.5.2 < 0.5.3-rc.1 < 0.5.3-rc.2 < 0.5.3。
        assert_eq!(cmp_semver("0.5.3-rc.1", "0.5.2"), Greater);
        assert_eq!(cmp_semver("0.5.3-rc.1", "0.5.3-rc.2"), Less);
        assert_eq!(cmp_semver("0.5.3-rc.2", "0.5.3"), Less);
        assert_eq!(cmp_semver("0.5.3", "0.5.3-rc.2"), Greater);
        // rc.N 数值序（非字典序：rc.10 > rc.9）。
        assert_eq!(cmp_semver("0.5.3-rc.10", "0.5.3-rc.9"), Greater);
        // 后缀字典序：alpha < beta < rc。
        assert_eq!(cmp_semver("0.5.3-alpha", "0.5.3-beta"), Less);
        assert_eq!(cmp_semver("0.5.3-beta", "0.5.3-rc"), Less);
        // 前缀相同更长更大：rc < rc.1。
        assert_eq!(cmp_semver("0.5.3-rc", "0.5.3-rc.1"), Less);
        // 数值标识符 < 字母数字标识符（semver 规则）。
        assert_eq!(cmp_semver("0.5.3-1", "0.5.3-rc"), Less);
        // 高段 rc 仍大于低段正式版（真实升级路径 0.5.2 → 0.5.3-rc.1 不漏报）。
        assert_eq!(cmp_semver("0.5.3-rc.1", "0.5.2"), Greater);
        assert_eq!(cmp_semver("0.6.0-rc.1", "0.5.9"), Greater);
    }

    #[test]
    fn cmp_semver_dirty_inputs_tolerated() {
        use std::cmp::Ordering::*;
        // 空串/垃圾：彼此相等，恒小于真实版本（不误报可更新、不 panic）。
        assert_eq!(cmp_semver("", ""), Equal);
        assert_eq!(cmp_semver("garbage", "garbage"), Equal);
        assert_eq!(cmp_semver("", "0.1.0"), Less);
        assert_eq!(cmp_semver("garbage", "0.1.0"), Less);
        assert_eq!(cmp_semver("0.1.0", ""), Greater);
        // 空段/混合段按 0 容错。
        assert_eq!(cmp_semver("0..1", "0.0.1"), Equal);
        assert_eq!(cmp_semver("0.x.1", "0.0.1"), Equal);
        // 空白容错 + 大小写 V。
        assert_eq!(cmp_semver(" 0.5.2 ", "V0.5.2"), Equal);
        // 空 prerelease（尾杠）视为无。
        assert_eq!(cmp_semver("0.5.3-", "0.5.3"), Equal);
    }

    // ---- 2. pick_asset 平台矩阵 ----

    #[test]
    fn pick_asset_windows_matrix() {
        let assets = gh_release().assets;
        // x64：Setup exe（不是 Portable zip）。
        let got = pick_asset_platform("windows", "x86_64", &assets).expect("win-x64 必有 Setup");
        assert_eq!(got.name, "DSH-Desktop-Setup-0.5.2-win-x64.exe");
        assert_eq!(got.size, 72272680);
        // arm64：Setup exe。
        let got = pick_asset_platform("windows", "aarch64", &assets).expect("win-arm64 必有 Setup");
        assert_eq!(got.name, "DSH-Desktop-Setup-0.5.2-win-arm64.exe");
        // Gitee 缺 size 形态同样可挑（size=0 未知）。
        let gitee = gitee_release().assets;
        let got = pick_asset_platform("windows", "x86_64", &gitee).expect("Gitee win-x64 Setup");
        assert_eq!(got.name, "DSH-Desktop-Setup-0.5.2-win-x64.exe");
        assert_eq!(got.size, 0, "Gitee 无 size 字段 → 0=未知");
    }

    #[test]
    fn pick_asset_windows_portable_and_sidecar_excluded() {
        let only_portable = [asset("DSH-Desktop-Portable-0.5.2-win-x64.zip", "https://x/Portable.zip", 1)];
        assert_eq!(pick_asset_platform("windows", "x86_64", &only_portable), None, "Portable zip 不得入选");
        let sidecars = [
            asset("DSH-Desktop-Setup-0.5.2-win-x64.exe.sha256", "https://x/s.sha256", 64),
            asset("DSH-Desktop-Setup-0.5.2-win-x64.exe.blockmap", "https://x/s.blockmap", 64),
        ];
        assert_eq!(pick_asset_platform("windows", "x86_64", &sidecars), None, "边车/blockmap 不得入选");
    }

    #[test]
    fn pick_asset_macos_matrix() {
        let assets = gh_release().assets;
        // arm64：dmg。
        let got = pick_asset_platform("macos", "aarch64", &assets).expect("macos-arm64 必有 dmg");
        assert_eq!(got.name, "DSH-Desktop-0.5.2-macos-arm64.dmg");
        // x64 mac：v0.5.2 无产物 → None（不误装 arm64 镜像）。
        assert_eq!(pick_asset_platform("macos", "x86_64", &assets), None, "无 x64 mac dmg 应返回 None");
        // Gitee 无 mac 资产（100MB 限）→ None（资产级回落由双源选择层负责）。
        assert_eq!(pick_asset_platform("macos", "aarch64", &gitee_release().assets), None);
        // x64_mac 命名形态可识别（tauri 官方 triplet 后缀）。
        let x64_mac = [asset("DSH-Desktop-0.5.2-x64_mac.dmg", "https://x/m.dmg", 1)];
        assert!(pick_asset_platform("macos", "x86_64", &x64_mac).is_some(), "x64_mac 命名应命中");
    }

    #[test]
    fn pick_asset_linux_appimage_preferred_over_deb() {
        let assets = gh_release().assets;
        let got = pick_asset_platform("linux", "x86_64", &assets).expect("linux x64 必有 AppImage");
        assert_eq!(got.name, "DSH-Desktop-0.5.2-linux-x64.AppImage", "AppImage 优先");
        // 仅 deb 时兜底入选。
        let only_deb = [asset("dsh-desktop_0.5.2_amd64.deb", "https://x/d.deb", 1)];
        let got = pick_asset_platform("linux", "x86_64", &only_deb).expect("无 AppImage 时 deb 兜底");
        assert_eq!(got.name, "dsh-desktop_0.5.2_amd64.deb");
        // arm64 linux 命名形态。
        let arm = [asset("DSH-Desktop-0.5.2-linux-arm64.AppImage", "https://x/a.AppImage", 1)];
        assert!(pick_asset_platform("linux", "aarch64", &arm).is_some());
        // 源码归档（Gitee）不得入选。
        let src = [asset("v0.5.2.zip", "https://x/s.zip", 1), asset("v0.5.2.tar.gz", "https://x/s.tgz", 1)];
        assert_eq!(pick_asset_platform("linux", "x86_64", &src), None, "源码归档排除");
    }

    #[test]
    fn pick_asset_empty_and_unknown_platform() {
        assert_eq!(pick_asset_platform("windows", "x86_64", &[]), None, "空列表 → None");
        assert_eq!(pick_asset_platform("freebsd", "x86_64", &gh_release().assets), None, "未知 OS → None");
    }

    /// 契约入口 pick_asset（编译期本平台）与平台矩阵一致（fixtures 含全平台产物，
    /// 三大平台构建上都能拿到本平台安装器）。
    #[test]
    fn pick_asset_contract_entry_matches_current_platform() {
        let got = pick_asset(&gh_release().assets).expect("本平台必有安装器");
        let os = std::env::consts::OS;
        match os {
            "windows" => assert!(got.name.contains("Setup") && got.name.ends_with(".exe"), "{}", got.name),
            "macos" => assert!(got.name.ends_with(".dmg"), "{}", got.name),
            "linux" => assert!(got.name.ends_with(".AppImage") || got.name.ends_with(".deb"), "{}", got.name),
            _ => {}
        }
    }

    // ---- 3. 双源选择逻辑（resolve_outcome 纯函数，不真发网）----

    #[test]
    fn resolve_both_reachable_both_complete_prefers_gitee() {
        let out = resolve_outcome(
            "0.5.1",
            "windows",
            "x86_64",
            vec![(UpdateSource::GitHub, gh_release()), (UpdateSource::Gitee, gitee_release())],
            vec![],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!("0.5.1 → 0.5.2 应报可用") };
        assert_eq!(u.source, UpdateSource::Gitee, "双源齐全 prefer Gitee（国内快）");
        assert_eq!(u.next, "0.5.2");
        assert_eq!(u.current, "0.5.1");
        assert_eq!(u.asset.name, "DSH-Desktop-Setup-0.5.2-win-x64.exe");
        assert!(u.notes.contains("v0.5.2"));
    }

    #[test]
    fn resolve_gitee_lagging_tag_loses_to_newer_github() {
        // Gitee 镜像滞后（v0.5.1）而 GitHub 已发 v0.5.2：不得因 prefer Gitee 停旧版。
        let mut lagging = gitee_release();
        lagging.tag = "v0.5.1".into();
        for a in &mut lagging.assets {
            a.name = a.name.replace("0.5.2", "0.5.1");
        }
        let out = resolve_outcome(
            "0.5.1",
            "windows",
            "x86_64",
            vec![(UpdateSource::GitHub, gh_release()), (UpdateSource::Gitee, lagging)],
            vec![],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!("应报可用") };
        assert_eq!((u.source, u.next.as_str()), (UpdateSource::GitHub, "0.5.2"), "tag 分歧取严格更新者");
    }

    #[test]
    fn resolve_single_source_reachable_uses_it() {
        let out = resolve_outcome(
            "0.4.0",
            "windows",
            "x86_64",
            vec![(UpdateSource::GitHub, gh_release())],
            vec!["Gitee: HTTP 502".into()],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!("单通 GitHub 应可用") };
        assert_eq!(u.source, UpdateSource::GitHub);
        assert_eq!(u.asset.url.contains("github.com"), true);
    }

    // ── Gitee 分片（.partN）：GitHub 不可达 + Gitee 缺整资产的实爆回归 ──

    /// v0.6.2 实爆形态：Gitee 只有 win 安装包的 .part1/.part2 + 边车
    /// （整资产超 100MB 限被 mirror-gitee 跳过），GitHub 不可达。
    fn gitee_parts_only_release() -> RemoteRelease {
        RemoteRelease {
            tag: "v0.6.3".into(),
            notes: "parts".into(),
            assets: vec![
                ReleaseAsset {
                    name: "DSH-Desktop-Setup-0.6.3-win-x64.exe.part1".into(),
                    url: "https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-x64.exe.part1".into(),
                    size: 0,
                },
                ReleaseAsset {
                    name: "DSH-Desktop-Setup-0.6.3-win-x64.exe.part2".into(),
                    url: "https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-x64.exe.part2".into(),
                    size: 0,
                },
                ReleaseAsset {
                    name: "DSH-Desktop-Setup-0.6.3-win-x64.exe.sha256".into(),
                    url: "https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-x64.exe.sha256".into(),
                    size: 96,
                },
                ReleaseAsset {
                    name: "DSH-Desktop-Setup-0.6.3-win-arm64.exe".into(),
                    url: "https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-arm64.exe".into(),
                    size: 0,
                },
            ],
        }
    }

    #[test]
    fn parse_part_name_shapes() {
        assert_eq!(parse_part_name("a.exe.part1"), Some(("a.exe", 1)));
        assert_eq!(parse_part_name("a.exe.part12"), Some(("a.exe", 12)));
        assert_eq!(parse_part_name("a.exe.part0"), None, "0 号片不存在（从 1 起）");
        assert_eq!(parse_part_name("a.exe.part"), None, "无片号");
        assert_eq!(parse_part_name("a.partx"), None, "片号非数字");
        assert_eq!(parse_part_name(".part1"), None, "空 base");
        assert_eq!(parse_part_name("a.exe"), None, "非分片名");
        assert_eq!(parse_part_name("a.exe.part1.sha256"), None, "边车尾巴不出局成片");
    }

    #[test]
    fn find_part_set_prefers_platform_ranked_contiguous() {
        let rel = gitee_parts_only_release();
        let (synth, plan) = find_part_set("windows", "x86_64", &rel.assets).expect("应命中 x64 分片套");
        assert_eq!(synth.name, "DSH-Desktop-Setup-0.6.3-win-x64.exe");
        assert_eq!(synth.url, plan.urls[0], "合成资产 url 指向 part1");
        assert_eq!(plan.urls.len(), 2);
        assert!(plan.urls[0].ends_with(".part1") && plan.urls[1].ends_with(".part2"));
        assert_eq!(synth.size, 0, "Gitee 资产无 size → 合计 0（未知语义）");
        assert_eq!(
            plan.sidecar_url.as_deref(),
            Some("https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-x64.exe.sha256"),
            "整文件边车随镜像在场，作为分片哈希锚"
        );
        // arm64 只有整资产（非分片形态）——find_part_set 不命中，交给
        // 直连挑选路径（pick_asset_platform），两路互不越界。
        assert_eq!(find_part_set("windows", "aarch64", &rel.assets), None, "arm64 整资产不属分片路径");
        // 缺片（part1+part3）不成套。
        let broken = RemoteRelease {
            assets: vec![
                rel.assets[0].clone(),
                ReleaseAsset {
                    name: "DSH-Desktop-Setup-0.6.3-win-x64.exe.part3".into(),
                    url: "https://gitee.com/r/v0.6.3/DSH-Desktop-Setup-0.6.3-win-x64.exe.part3".into(),
                    size: 0,
                },
            ],
            ..rel.clone()
        };
        assert_eq!(find_part_set("windows", "x86_64", &broken.assets), None, "缺片不成套");
    }

    #[test]
    fn resolve_gitee_parts_only_falls_back_to_part_set() {
        // GitHub 不可达（实爆：api.github.com 被墙/超时），Gitee 只有分片。
        let out = resolve_outcome(
            "0.6.2",
            "windows",
            "x86_64",
            vec![(UpdateSource::Gitee, gitee_parts_only_release())],
            vec!["GitHub: socket hang up".into()],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!("分片形态应可用") };
        assert_eq!(u.source, UpdateSource::Gitee);
        assert_eq!(u.next, "0.6.3");
        assert_eq!(u.asset.name, "DSH-Desktop-Setup-0.6.3-win-x64.exe");
        // 分片计划已登记，下载链按 urls 逐片拼接。
        let plan = cached_parts(&u.asset.name).expect("分片计划应已登记");
        assert_eq!(plan.urls.len(), 2);
        assert!(plan.sidecar_url.is_some());
        // 跨源锚也登记（GitHub CDN 可达时的额外兑底）。
        assert!(cached_cross_anchor(&u.asset.name).is_some());
    }

    #[test]
    fn resolve_direct_asset_wins_over_parts() {
        // 双源都在场：GitHub 整资产 + Gitee 分片 → 直连整资产优先（现行为不变）。
        let gh = gh_release(); // v0.5.2 含 win-x64 整资产
        let gitee_parts = RemoteRelease { tag: "v0.5.2".into(), ..gitee_parts_only_release_v052() };
        let out = resolve_outcome(
            "0.5.1",
            "windows",
            "x86_64",
            vec![(UpdateSource::Gitee, gitee_parts), (UpdateSource::GitHub, gh)],
            vec![],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!() };
        assert_eq!(u.source, UpdateSource::GitHub, "整资产源优先于分片源");
        assert_eq!(u.asset.url.contains("github.com"), true);
    }

    fn gitee_parts_only_release_v052() -> RemoteRelease {
        let mut rel = gitee_parts_only_release();
        rel.tag = "v0.5.2".into();
        for a in &mut rel.assets {
            a.name = a.name.replace("0.6.3", "0.5.2");
            a.url = a.url.replace("0.6.3", "0.5.2");
        }
        rel
    }

    #[test]
    fn resolve_all_unreachable_is_offline() {
        let err = resolve_outcome("0.5.1", "windows", "x86_64", vec![], vec!["GitHub: 超时".into(), "Gitee: DNS 失败".into()])
            .unwrap_err();
        assert!(matches!(err, UpdaterError::Offline(ref m) if m.contains("GitHub") && m.contains("Gitee")), "{err:?}");
    }

    #[test]
    fn resolve_asset_fallback_gitee_missing_platform_asset() {
        // mac 平台：Gitee（100MB 限）无 dmg → 回落 GitHub；Gitee 排前也不影响出局。
        let out = resolve_outcome(
            "0.5.1",
            "macos",
            "aarch64",
            vec![(UpdateSource::GitHub, gh_release()), (UpdateSource::Gitee, gitee_release())],
            vec![],
        )
        .unwrap();
        let CheckOutcome::Available(u) = out else { panic!("mac 应从 GitHub 拿到 dmg") };
        assert_eq!(u.source, UpdateSource::GitHub, "Gitee 缺 mac 资产 → 资产级回落 GitHub");
        assert_eq!(u.asset.name, "DSH-Desktop-0.5.2-macos-arm64.dmg");
    }

    #[test]
    fn resolve_no_platform_asset_anywhere_is_bad_manifest() {
        // 唯一可达源（Gitee）无 mac 资产 → BadManifest（诚实报缺，不谎报 UpToDate）。
        let err = resolve_outcome(
            "0.5.1",
            "macos",
            "aarch64",
            vec![(UpdateSource::Gitee, gitee_release())],
            vec!["GitHub: 超时".into()],
        )
        .unwrap_err();
        assert!(matches!(err, UpdaterError::BadManifest(_)), "{err:?}");
    }

    #[test]
    fn resolve_downgrade_equal_and_prerelease_rules() {
        // 本地更高/相等/同版 prerelease 远端：一律 UpToDate（防降级；rc 不自动升 rc+）。
        for remote in ["v0.5.2", "v0.5.1", "v0.5.3-rc.1"] {
            let mut rel = gh_release();
            rel.tag = remote.into();
            let out = resolve_outcome("0.5.3", "windows", "x86_64", vec![(UpdateSource::GitHub, rel)], vec![]).unwrap();
            assert!(matches!(out, CheckOutcome::UpToDate), "本地 0.5.3 vs 远端 {remote} 不得报可更新");
        }
        // 本地 rc、远端正式版：应报可用（0.5.3-rc.1 → 0.5.3）。
        let mut rel = gh_release();
        rel.tag = "v0.5.3".into();
        let out = resolve_outcome("0.5.3-rc.1", "windows", "x86_64", vec![(UpdateSource::GitHub, rel)], vec![]).unwrap();
        assert!(matches!(out, CheckOutcome::Available(u) if u.next == "0.5.3"), "rc → 正式版应报可用");
    }

    // ---- 4. JSON 解析固件 ----

    #[test]
    fn parse_github_fixture_shape() {
        let (rel, digests) = parse_release_doc(GH_FIXTURE).unwrap();
        assert_eq!(rel.tag, "v0.5.2");
        assert_eq!(rel.assets.len(), 6);
        assert!(rel.notes.contains("余额"));
        let exe = rel.assets.iter().find(|a| a.name == "DSH-Desktop-Setup-0.5.2-win-x64.exe").unwrap();
        assert_eq!(exe.size, 72272680);
        assert!(exe.url.starts_with("https://github.com/"));
        // digest 提取：全部 6 资产各一条，形态 64hex 小写。
        assert_eq!(digests.len(), 6);
        let (_, sha) = digests.iter().find(|(n, _)| n == "DSH-Desktop-Setup-0.5.2-win-x64.exe").unwrap();
        assert_eq!(sha, "8e062808478cf7bcc311b10414095f634ce5566d7686bf8498803898995e9646");
    }

    #[test]
    fn parse_gitee_fixture_shape_without_size_digest() {
        let (rel, digests) = parse_release_doc(GITEE_FIXTURE).unwrap();
        assert_eq!(rel.tag, "v0.5.2");
        assert_eq!(rel.assets.len(), 5, "3 安装器 + 2 源码归档");
        assert!(digests.is_empty(), "Gitee 资产无 digest 字段");
        assert!(rel.assets.iter().all(|a| a.size == 0), "Gitee 无 size → 0=未知");
        assert!(rel.assets.iter().all(|a| a.url.starts_with("https://gitee.com/")));
    }

    #[test]
    fn parse_bad_docs_are_bad_manifest_not_panic() {
        for doc in ["", "not json", r#"{"assets":[]}"#, r#"{"tag_name":""}"#, r#"{"tag_name":null}"#] {
            let err = parse_release_doc(doc).unwrap_err();
            assert!(matches!(err, UpdaterError::BadManifest(_)), "{doc} → BadManifest，得 {err:?}");
        }
        // body=null（GitHub 可能）不炸：notes 空串。
        let (rel, _) = parse_release_doc(r#"{"tag_name":"v1.0.0","body":null,"assets":[{"name":"a","browser_download_url":"u"}]}"#).unwrap();
        assert_eq!((rel.tag.as_str(), rel.notes.as_str(), rel.assets.len()), ("v1.0.0", "", 1));
        // 资产缺 name/url 条目跳过不炸。
        let (rel, _) = parse_release_doc(r#"{"tag_name":"v1.0.0","assets":[{"name":"a"},{"browser_download_url":"u"},{"name":"b","browser_download_url":"v"}]}"#).unwrap();
        assert_eq!(rel.assets.len(), 1);
        assert_eq!(rel.assets[0].name, "b");
    }

    #[test]
    fn sha256_text_parsers() {
        // 边车三形态。
        assert_eq!(parse_sha256_sidecar("a".repeat(64).as_str()), Some("a".repeat(64)));
        assert_eq!(
            parse_sha256_sidecar(&format!("{}  DSH-Desktop-Setup-0.5.2-win-x64.exe\n", "b".repeat(64))),
            Some("b".repeat(64))
        );
        assert_eq!(parse_sha256_sidecar(&format!("sha256:{}\n", "c".repeat(64))), Some("c".repeat(64)));
        // 大写十六进制归一。
        assert_eq!(parse_sha256_sidecar(&"D".repeat(64)), Some("d".repeat(64)));
        // 垃圾 → None。
        assert_eq!(parse_sha256_sidecar(""), None);
        assert_eq!(parse_sha256_sidecar("deadbeef"), None);
        assert_eq!(parse_sha256_sidecar(&"z".repeat(64)), None);
        // digest 字段形态。
        assert_eq!(parse_digest(&format!("sha256:{}", "e".repeat(64))), Some("e".repeat(64)));
        assert_eq!(parse_digest(&format!("sha512:{}", "f".repeat(64))), None);
        assert_eq!(parse_digest(&format!("sha256:{}", "g".repeat(63))), None);
    }

    #[test]
    fn to_bridge_error_code_mapping() {
        use bridge::codes;
        assert_eq!(UpdaterError::Offline("x".into()).to_bridge().code, codes::UPDATER_NETWORK);
        assert_eq!(UpdaterError::SourceUnreachable("x".into()).to_bridge().code, codes::UPDATER_NETWORK);
        assert_eq!(UpdaterError::BadManifest("x".into()).to_bridge().code, codes::UPDATER_NETWORK);
        assert_eq!(
            UpdaterError::HashMismatch { expected: "a".into(), actual: "b".into() }.to_bridge().code,
            codes::UPDATER_SIGNATURE
        );
        assert_eq!(UpdaterError::Download("x".into()).to_bridge().code, codes::UPDATER_NETWORK);
        assert_eq!(UpdaterError::Io("x".into()).to_bridge().code, codes::INTERNAL);
        // From 自动转换（命令层 `?` 直接可用）。
        let b: bridge::BridgeError = UpdaterError::Offline("x".into()).into();
        assert!(b.message.contains("离线"));
    }

    #[test]
    fn notes_cap_keeps_char_boundary() {
        assert_eq!(cap_notes("短").len(), 3);
        let long = "汉".repeat(5000); // 15000 bytes > 8KB
        let capped = cap_notes(&long);
        assert!(capped.len() < 15000 && capped.contains("（已截断）"));
        // 保留前缀必须在原串 char 边界上（不切碎多字节字符）。
        let kept = capped.strip_suffix("\n…（已截断）").expect("截断标记后缀");
        assert!(long.starts_with(kept) && long.is_char_boundary(kept.len()), "截断点必须落在 char 边界");
        assert!(kept.len() > NOTES_CAP - 16, "截断点应贴近上限（{}/{}）", kept.len(), NOTES_CAP);
    }

    #[test]
    fn sanitize_filename_whitelist() {
        assert_eq!(sanitize_filename("DSH-Desktop-Setup-0.5.2-win-x64.exe"), "DSH-Desktop-Setup-0.5.2-win-x64.exe");
        assert_eq!(sanitize_filename("../evil/../name.exe"), ".._evil_.._name.exe");
        assert_eq!(sanitize_filename("a b:c*d"), "a_b_c_d");
    }

    // ---- 下载链（本地 127.0.0.1 迷你 HTTP 服务，真走 reqwest 栈）----

    /// 路径路由迷你 HTTP 服务：每连接一请求，按 path 前缀匹配响应后关闭。
    /// 返回基 URL（`http://127.0.0.1:<port>/`）。
    fn spawn_http_server(routes: Vec<(String, u16, Vec<u8>)>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..32 {
                let Ok((mut sock, _)) = listener.accept() else { return };
                let mut buf = [0u8; 2048];
                let mut head = Vec::new();
                // 读到请求头末尾（GET 无 body）。
                loop {
                    let n = match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let line = String::from_utf8_lossy(&head);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| path == *p)
                    .map(|(_, s, b)| (*s, b.clone()))
                    .unwrap_or((404, Vec::new()));
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.write_all(&body);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    /// 在系统 temp 的 dsh-update-* 目录里查找名字含 marker 的文件（失败清理断言用）。
    fn find_temp_file(marker: &str) -> Option<std::path::PathBuf> {
        std::fs::read_dir(std::env::temp_dir())
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("dsh-update-"))
            .flat_map(|e| std::fs::read_dir(e.path()).ok().into_iter().flatten().flatten())
            .map(|f| f.path())
            .find(|p| p.file_name().map(|n| n.to_string_lossy().contains(marker)).unwrap_or(false))
    }

    #[test]
    fn download_streams_verifies_and_reports_progress() {
        let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let base = spawn_http_server(vec![("/pkg.exe".into(), 200, body.clone())]);
        let ast = asset("DSH-Desktop-Setup-9.9.9-win-x64.exe", &format!("{base}/pkg.exe"), body.len() as u64);
        let seen: std::sync::Arc<Mutex<Vec<(u64, u64)>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let rec = seen.clone();
        let path = tauri::async_runtime::block_on(download_to_temp(
            &ast,
            move |done, total| rec.lock().unwrap().push((done, total)),
            Some(&sha256_hex(&body)),
        ))
        .expect("显式 sha256 正确应成功");
        assert_eq!(std::fs::read(&path).unwrap(), body, "落盘内容逐字节一致");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with("DSH-Desktop-Setup-9.9.9-win-x64.exe"), "{name}");
        let part = path.parent().unwrap().join(format!("{name}.part"));
        assert!(!part.exists(), "part 已 rename 消费");
        let events = seen.lock().unwrap().clone();
        assert!(!events.is_empty(), "进度必须回调");
        assert_eq!(*events.last().unwrap(), (body.len() as u64, body.len() as u64), "末次进度 = (总, 总)");
        assert!(events.windows(2).all(|w| w[0].0 <= w[1].0), "进度单调不减");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn download_hash_mismatch_hard_fails_and_cleans_up() {
        let body = b"payload-that-is-not-the-hashed-one".to_vec();
        let base = spawn_http_server(vec![("/pkg.exe".into(), 200, body.clone())]);
        let ast = asset("mismatch-probe-9.9.9.exe", &format!("{base}/pkg.exe"), body.len() as u64);
        let wrong = sha256_hex(b"something-else-entirely");
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, Some(&wrong))).unwrap_err();
        match err {
            UpdaterError::HashMismatch { expected, actual } => {
                assert_eq!(expected, wrong);
                assert_eq!(actual, sha256_hex(&body));
            }
            other => panic!("应硬失败 HashMismatch，得 {other:?}"),
        }
        // 失败不留半截包。
        assert!(find_temp_file("mismatch-probe-9.9.9.exe").is_none(), "失败必须清理临时目录");
    }

    /// V2 P1-1：digest 缓存（GitHub 哈希）× Gitee 镜像漂移 → 换源重试一次必须
    /// 用权威源文件救回；换源后同 digest 通过 = 漂移而非篡改。
    #[test]
    fn download_hash_mismatch_falls_back_to_alt_source() {
        let good = b"authoritative-github-bytes".to_vec();
        let drifted = b"gitee-mirror-reuploaded-different-bytes".to_vec();
        let base = spawn_http_server(vec![
            ("/mirror/pkg.exe".into(), 200, drifted.clone()), // 首选源（Gitee 形态）文件漂移
            ("/origin/pkg.exe".into(), 200, good.clone()),    // 另一源（GitHub）权威文件
        ]);
        let name = "altsrc-probe-Setup-9.9.9-win-x64.exe";
        let ast = asset(name, &format!("{base}/mirror/pkg.exe"), drifted.len() as u64);
        cache_sha256(name, sha256_hex(&good));
        cache_alt_url(name, format!("{base}/origin/pkg.exe"));
        let got = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).expect("换源重试应成功");
        let bytes = std::fs::read(&got).expect("落盘文件可读");
        assert_eq!(bytes, good, "必须落盘权威源字节（镜像漂移救回）");
        let _ = std::fs::remove_dir_all(got.parent().unwrap());
    }

    /// V2 P1-1 反向：换源后同 digest 仍不过 = 真篡改，原样硬失败。
    #[test]
    fn download_hash_mismatch_alt_source_tampered_still_hard_fails() {
        let bad1 = b"tampered-variant-a".to_vec();
        let bad2 = b"tampered-variant-b".to_vec();
        let base = spawn_http_server(vec![
            ("/m/pkg.exe".into(), 200, bad1.clone()),
            ("/o/pkg.exe".into(), 200, bad2.clone()),
        ]);
        let name = "altsrc-tamper-9.9.9.exe";
        let ast = asset(name, &format!("{base}/m/pkg.exe"), bad1.len() as u64);
        let digest = sha256_hex(b"the-real-release-bytes");
        cache_sha256(name, digest.clone());
        cache_alt_url(name, format!("{base}/o/pkg.exe"));
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).unwrap_err();
        assert!(matches!(err, UpdaterError::HashMismatch { .. }), "双源皆不符 digest 应硬失败: {err:?}");
        assert!(find_temp_file("altsrc-tamper").is_none());
    }

    /// V2 P2-3：显式哈希形态非法（带 sha256: 前缀）必须硬失败，不得静默降级。
    #[test]
    fn download_invalid_explicit_sha256_hard_fails() {
        let body = b"any".to_vec();
        let base = spawn_http_server(vec![("/pkg.exe".into(), 200, body)]);
        let ast = asset("badhash-probe-9.9.9.exe", &format!("{base}/pkg.exe"), 3);
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, Some("sha256:deadbeef"))).unwrap_err();
        assert!(matches!(err, UpdaterError::BadManifest(ref m) if m.contains("非法")), "{err:?}");
        assert!(find_temp_file("badhash-probe").is_none());
    }

    #[test]
    fn download_truncation_against_metadata_size_fails() {
        let body = b"short-body".to_vec();
        let base = spawn_http_server(vec![("/pkg.bin".into(), 200, body.clone())]);
        // 元数据 size 虚标 +10 → 截断检测（name 非 setup，隔离 floor 分支）。
        let ast = asset("trunc-probe-9.9.9.bin", &format!("{base}/pkg.bin"), body.len() as u64 + 10);
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).unwrap_err();
        assert!(matches!(err, UpdaterError::Download(ref m) if m.contains("元数据")), "{err:?}");
        assert!(find_temp_file("trunc-probe").is_none());
    }

    #[test]
    fn download_floor_guard_without_any_hash() {
        let body = b"tiny".to_vec();
        let base = spawn_http_server(vec![("/setup.exe".into(), 200, body.clone())]);
        // 无显式 sha、无 digest 缓存（名字唯一）、无边车（404）→ 触发 >50MB 下限。
        let ast = asset("floor-probe-Setup-9.9.9-win-x64.exe", &format!("{base}/setup.exe"), body.len() as u64);
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).unwrap_err();
        assert!(matches!(err, UpdaterError::Download(ref m) if m.contains("下限")), "{err:?}");
        assert!(find_temp_file("floor-probe").is_none());
    }

    #[test]
    fn download_sidecar_sha256_enforced_and_beats_floor() {
        let body = b"tiny-but-authenticated".to_vec();
        let sidecar = format!("{}  tiny-setup.exe\n", sha256_hex(&body));
        let base = spawn_http_server(vec![
            ("/tiny-setup.exe.sha256".into(), 200, sidecar.into_bytes()),
            ("/tiny-setup.exe".into(), 200, body.clone()),
        ]);
        // setup 名 + 微小体积：若无边车哈希会被 floor 拒——边车命中则放行（证明边车被真消费）。
        let ast = asset("tiny-setup.exe", &format!("{base}/tiny-setup.exe"), body.len() as u64);
        let path = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).expect("边车哈希校验通过");
        assert_eq!(std::fs::read(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        // 边车内容与实际不符 → HashMismatch。
        let bad_sidecar = format!("{}  tiny-setup.exe\n", sha256_hex(b"not-the-body"));
        let base2 = spawn_http_server(vec![
            ("/tiny-setup.exe.sha256".into(), 200, bad_sidecar.into_bytes()),
            ("/tiny-setup.exe".into(), 200, body.clone()),
        ]);
        let ast2 = asset("tiny-setup.exe", &format!("{base2}/tiny-setup.exe"), body.len() as u64);
        let err = tauri::async_runtime::block_on(download_to_temp(&ast2, |_, _| {}, None)).unwrap_err();
        assert!(matches!(err, UpdaterError::HashMismatch { .. }), "{err:?}");
        assert!(find_temp_file("tiny-setup.exe").is_none());
    }

    #[test]
    fn download_http_error_fails() {
        let base = spawn_http_server(vec![]);
        let ast = asset("gone-probe-9.9.9.exe", &format!("{base}/gone.exe"), 128);
        let err = tauri::async_runtime::block_on(download_to_temp(&ast, |_, _| {}, None)).unwrap_err();
        assert!(matches!(err, UpdaterError::Download(ref m) if m.contains("404")), "{err:?}");
        assert!(find_temp_file("gone-probe").is_none());
    }

    // ---- 5. 实网冒烟（只读；cargo test -- --ignored）----

    /// 实网冒烟：真打两源 API（只读 GET，不下载资产），断言 v0.5.2 形状与平台资产。
    #[test]
    #[ignore = "实网冒烟（只读）：需外网，cargo test updater_client -- --ignored --nocapture"]
    fn check_latest_live() {
        for (source, expect_all_platforms) in [(UpdateSource::GitHub, true), (UpdateSource::Gitee, false)] {
            let rel = tauri::async_runtime::block_on(fetch_release(source))
                .unwrap_or_else(|e| panic!("{source} latest 不可达：{e}"));
            println!("[live] {source}: tag={} assets={}", rel.tag, rel.assets.len());
            assert_eq!(rel.tag, "v0.5.2", "{source} tag 形态漂移：{}", rel.tag);
            // GitHub 应三平台齐全；Gitee 至少 win 双架构 Setup。
            assert!(pick_asset_platform("windows", "x86_64", &rel.assets).is_some(), "{source} 缺 win-x64 Setup");
            assert!(pick_asset_platform("windows", "aarch64", &rel.assets).is_some(), "{source} 缺 win-arm64 Setup");
            if expect_all_platforms {
                assert!(pick_asset_platform("macos", "aarch64", &rel.assets).is_some(), "GitHub 缺 mac dmg");
                assert!(pick_asset_platform("linux", "x86_64", &rel.assets).is_some(), "GitHub 缺 linux AppImage");
            }
        }
        // 端到端：本地 0.5.2 → 双源 latest 0.5.2 → UpToDate（未来发新版则 Available 且 next 严格更大）。
        let cur = env!("CARGO_PKG_VERSION").to_string();
        match tauri::async_runtime::block_on(check_latest(&cur)).expect("live check_latest 不应 Err") {
            CheckOutcome::UpToDate => println!("[live] 本地 {cur} 已是最新（远端 v0.5.2）"),
            CheckOutcome::Available(u) => {
                println!("[live] 检测到更新 {cur} -> {}（{} / {}）", u.next, u.source, u.asset.name);
                assert_eq!(cmp_semver(&u.next, &cur), std::cmp::Ordering::Greater, "远端已发新版，必须严格大于本地");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TA1 测试加固门（用户批准的最小 cfg(test)] 门）：把私有下载门禁
// assert_download_url_allowed 以 pub(crate) 包装暴露给 crate 内单元测试
// （ta1_property_unit）。纯追加测试专用，非测试构建零影响。
// ---------------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod ta1_url_gate {
    pub(crate) fn assert_download_url_allowed(url: &str) -> Result<(), super::UpdaterError> {
        super::assert_download_url_allowed(url)
    }
}
