//! OCR 插件模型下载（`plugins/ocr/` 四件套，见 AGENTS.md 3.8 节）。
//!
//! - 每文件独立任务：行按钮 下载 → 暂停/取消 → 继续；顶部标题行 `取消` 停掉
//!   全部在途任务；取消删 `.part`（行按钮回到初始"下载"），暂停保留 `.part`
//!   （点继续按已下字节 `Range` 续传，服务端不支持则从头下）；
//! - 四文件与官方 `default_models.yaml`（v3.9.2，2026-09-06 实测核对）同源、
//!   哈希互认：`det.onnx` ← PP-OCRv6 det small、`rec.onnx` ← PP-OCRv6 rec small、
//!   `keys.txt` ← `ppocrv6_dict.txt`（须配套，错配由 `rapid.rs` 类别校验熔断）；
//! - `onnxruntime.dll` ← 微软 ORT v1.28.1 win-x64 发布包（与 `ort =2.0.0-rc.13`
//!   配对），zip 内按文件名模糊定位，不依赖固定内层路径；
//! - 手动下载走设置页帮助窗（项目地址 + 文件直链可复制 + 改名对照 + 校验说明）；
//! - 本模块跨平台（`reqwest::blocking` + `sha2` + `zip` 皆纯逻辑），WSL2 可单测；
//!   任务编排与行按钮由设置页组织（`Settings::model_dl`），见 `ui/settings.rs`。
//!
//! 下载失败不抛崩溃：工作线程发 `DlEvent::JobEnd{ok:false}`，行内提示换手动源
//! （帮助窗）或重试；哈希 mismatch 删 `.part`，下次从零重下（不断续传坏块）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::rapid::{DET_MODEL_FILE, KEYS_FILE, REC_MODEL_FILE, RUNTIME_LIB_FILE};

/// 后台下载线程发往设置页的事件（`mpsc`，设置页每帧 `try_recv` 轮询）。
#[derive(Debug)]
pub enum DlEvent {
    /// 某文件进度（`total` 为 `None` 表示服务端未给长度，只能看已下字节）。
    Progress {
        index: usize,
        done: u64,
        total: Option<u64>,
    },
    /// 某文件完成（含校验通过；设置页据此刷新状态行）。
    FileDone {
        index: usize,
    },
    /// 某文件已暂停（`.part` 保留，`done` 为已下字节，点继续可断点续传）。
    Paused {
        index: usize,
        done: u64,
    },
    /// 单文件任务结束（成功/失败/用户取消；`cancelled` 为用户主动取消，
    /// 此时 `.part` 已删除，行按钮回到初始"下载"）。
    JobEnd {
        index: usize,
        ok: bool,
        msg: String,
        cancelled: bool,
    },
}

/// 单文件下载任务的外部控制（暂停/取消；设置页按钮写，工作线程每块检查）。
#[derive(Debug, Default)]
pub struct JobControl {
    /// 暂停（线程刷盘后退出，保留 `.part`，发 `Paused`）。
    pub pause: AtomicBool,
    /// 取消（线程删除 `.part` 后退出，发 `JobEnd{cancelled:true}`）。
    pub cancel: AtomicBool,
}

/// 单个插件文件的下载描述。
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    /// 存入 `plugins/ocr/` 的本地文件名（`rapid.rs` 四常量）。
    pub local: &'static str,
    /// 上游原始文件名（帮助页改名对照用）。
    pub origin: &'static str,
    /// 文件说明（设置页行标签用）。
    pub desc: &'static str,
    /// 直接下载地址（`None` 表示需从压缩包解出，见 `zip_url`）。
    pub url: Option<&'static str>,
    /// 期望 SHA256（小写 hex；`None` 表示只校验非空，如字典/DLL）。
    pub sha256: Option<&'static str>,
    /// 压缩包下载地址（仅 DLL 有：ORT 官方发布包，内含 `onnxruntime.dll`）。
    pub zip_url: Option<&'static str>,
    /// 压缩包内定位：取第一个以该后缀结尾的文件条目（不依赖内层目录）。
    pub zip_entry_suffix: Option<&'static str>,
}

/// det.onnx ← PP-OCRv6 det small（约 9.9MB）。
pub const DET_URL: &str = "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/det/PP-OCRv6_det_small.onnx";
/// det.onnx 期望哈希（官方 `default_models.yaml`，2026-09-06 核对）。
pub const DET_SHA256: &str =
    "090f04abcd9d9a7498bc4ebf677e4cb9bdce1fe4197ddb7e529f1ef44e1ff94f";
/// rec.onnx ← PP-OCRv6 rec small（约 21MB）。
pub const REC_URL: &str = "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx";
/// rec.onnx 期望哈希（同上）。
pub const REC_SHA256: &str =
    "6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884";
/// keys.txt ← ppocrv6_dict.txt（须与 rec 配套；类别数由 `rapid.rs` 加载时校验）。
pub const DICT_URL: &str = "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt";
/// onnxruntime.dll ← 微软 ORT v1.28.1 win-x64 发布包（约 78MB，`ort 2.0.0-rc.13` 配对）。
pub const ORT_ZIP_URL: &str =
    "https://github.com/microsoft/onnxruntime/releases/download/v1.28.1/onnxruntime-win-x64-1.28.1.zip";

/// 四件套下载表（顺序即设置页展示顺序）。
pub fn model_files() -> [ModelFile; 4] {
    [
        ModelFile {
            local: DET_MODEL_FILE,
            origin: "PP-OCRv6_det_small.onnx",
            desc: "检测模型（约 10MB）",
            url: Some(DET_URL),
            sha256: Some(DET_SHA256),
            zip_url: None,
            zip_entry_suffix: None,
        },
        ModelFile {
            local: REC_MODEL_FILE,
            origin: "PP-OCRv6_rec_small.onnx",
            desc: "识别模型（约 21MB）",
            url: Some(REC_URL),
            sha256: Some(REC_SHA256),
            zip_url: None,
            zip_entry_suffix: None,
        },
        ModelFile {
            local: KEYS_FILE,
            origin: "ppocrv6_dict.txt",
            desc: "识别字典（须配套）",
            url: Some(DICT_URL),
            sha256: None,
            zip_url: None,
            zip_entry_suffix: None,
        },
        ModelFile {
            local: RUNTIME_LIB_FILE,
            origin: "onnxruntime.dll（在发布包 zip 内）",
            desc: "推理运行时（约 78MB 包）",
            url: None,
            sha256: None,
            zip_url: Some(ORT_ZIP_URL),
            zip_entry_suffix: Some("onnxruntime.dll"),
        },
    ]
}

/// RapidOCR 项目地址（帮助页手动下载用）。
pub const RAPIDOCR_REPO_URL: &str = "https://github.com/RapidAI/RapidOCR";
/// 模型托管页（ModelScope，可网页手动下载）。
pub const MODELSCOPE_PAGE_URL: &str = "https://www.modelscope.cn/models/RapidAI/RapidOCR";
/// ONNX Runtime 发布页（手动下载 DLL 用，选 v1.28.1 win-x64 包）。
pub const ORT_RELEASES_URL: &str = "https://github.com/microsoft/onnxruntime/releases";

/// 单文件下载任务入口（阻塞，调用方跑后台线程；结束必发 `JobEnd`）。
///
/// - 已就绪文件直接报完成（`FileDone` + 成功 `JobEnd`），不重复下载；
/// - 其余走 `.part` 断点续传（`Range`，服务端不支持则从头下）；
/// - det/rec 下完比 SHA256（mismatch 删 `.part` 报错，下次从零重下）；
/// - DLL 下 zip → 按后缀定位条目解出 → 删 zip；
/// - 暂停/取消由 `ctl` 触发（见 [`JobControl`]）；失败 `msg` 含手动下载指引。
pub fn run_file_job(dir: PathBuf, index: usize, tx: Sender<DlEvent>, ctl: Arc<JobControl>) {
    let files = model_files();
    let Some(file) = files.get(index) else {
        let _ = tx.send(DlEvent::JobEnd {
            index,
            ok: false,
            msg: format!("内部错误：文件序号 {index} 越界"),
            cancelled: false,
        });
        return;
    };
    if file_ready(&dir, file) {
        let _ = tx.send(DlEvent::FileDone { index });
        let _ = tx.send(DlEvent::JobEnd {
            index,
            ok: true,
            msg: String::from("文件已就绪"),
            cancelled: false,
        });
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let _ = tx.send(DlEvent::JobEnd {
            index,
            ok: false,
            msg: format!("{} 创建插件目录失败：{e:#}", file.local),
            cancelled: false,
        });
        return;
    }
    let client = match reqwest::blocking::Client::builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(30))
        .user_agent("PrismaSnap/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(DlEvent::JobEnd {
                index,
                ok: false,
                msg: format!("{} 网络初始化失败：{e:#}", file.local),
                cancelled: false,
            });
            return;
        }
    };
    let outcome = if let Some(url) = file.url {
        let part = part_path(&dir, file.local);
        match fetch_resumable(&client, url, &part, index, &tx, &ctl) {
            Ok(FetchOutcome::Completed) => verify_and_commit(&dir, file, &part).map(|_| ()),
            Ok(FetchOutcome::PausedAt(done)) => {
                let _ = tx.send(DlEvent::Paused { index, done });
                return;
            }
            Ok(FetchOutcome::Cancelled) => {
                let _ = std::fs::remove_file(&part);
                let _ = tx.send(DlEvent::JobEnd {
                    index,
                    ok: false,
                    msg: String::new(),
                    cancelled: true,
                });
                return;
            }
            Err(e) => Err(e),
        }
    } else if let Some(zip_url) = file.zip_url {
        let suffix = file.zip_entry_suffix.unwrap_or(file.local);
        match fetch_and_extract_dll(&client, zip_url, &dir, file.local, suffix, index, &tx, &ctl) {
            Ok(FetchOutcome::Completed) => Ok(()),
            Ok(FetchOutcome::PausedAt(done)) => {
                let _ = tx.send(DlEvent::Paused { index, done });
                return;
            }
            Ok(FetchOutcome::Cancelled) => {
                let _ = tx.send(DlEvent::JobEnd {
                    index,
                    ok: false,
                    msg: String::new(),
                    cancelled: true,
                });
                return;
            }
            Err(e) => Err(e),
        }
    } else {
        Err(anyhow::anyhow!("{} 无下载地址，请手动下载", file.local))
    };
    match outcome {
        Ok(()) => {
            let _ = tx.send(DlEvent::FileDone { index });
            let _ = tx.send(DlEvent::JobEnd {
                index,
                ok: true,
                msg: format!("{} 下载完成", file.local),
                cancelled: false,
            });
        }
        Err(e) => {
            let _ = tx.send(DlEvent::JobEnd {
                index,
                ok: false,
                msg: format!(
                    "{} 下载失败：{e:#}。可点帮助手动下载放入，或稍后重试",
                    file.local
                ),
                cancelled: false,
            });
        }
    }
}

/// 拉取结果（`fetch_resumable` 单次返回）。
enum FetchOutcome {
    /// 到尾（调用方继续校验/解包）。
    Completed,
    /// 用户暂停（`.part` 保留，`done` 为已下字节）。
    PausedAt(u64),
    /// 用户取消（`.part` 已删除）。
    Cancelled,
}

/// 断点续传拉取（分块读 + 进度回传 + 暂停/取消检查，内存占用恒定）。
///
/// - `.part` 已有字节则带 `Range` 续传；服务端回 200（忽略 Range）则截断重下，
///   回 416（范围越界，常为已下完）直接按完成处理；
/// - 暂停：刷盘后返回 `PausedAt`（保留 `.part`，点继续时按大小续传）；
/// - 取消：删除 `.part` 后返回 `Cancelled`。
fn fetch_resumable(
    client: &reqwest::blocking::Client,
    url: &str,
    part: &Path,
    index: usize,
    tx: &Sender<DlEvent>,
    ctl: &JobControl,
) -> anyhow::Result<FetchOutcome> {
    use reqwest::StatusCode;
    let existing = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut req = client.get(url);
    if existing > 0 {
        req = req.header("Range", format!("bytes={existing}-"));
    }
    let mut resp = req.send()?;
    let status = resp.status();
    if status != StatusCode::OK
        && status != StatusCode::PARTIAL_CONTENT
        && status != StatusCode::RANGE_NOT_SATISFIABLE
    {
        anyhow::bail!("服务端返回 {status}");
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // 已下字节超出服务端范围（多为 .part 恰好完整）：按完成走，调用方验哈希
        return Ok(FetchOutcome::Completed);
    }
    // 206 = 续传生效（content_length 只是剩余量）；200 = 从头下
    let resumed = status == StatusCode::PARTIAL_CONTENT && existing > 0;
    let base = if resumed { existing } else { 0 };
    let total = resp.content_length().map(|n| n + base);
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!resumed)
        .append(resumed)
        .open(part)?;
    let mut buf = [0u8; 65536];
    let mut done = base;
    loop {
        if ctl.cancel.load(Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(part);
            return Ok(FetchOutcome::Cancelled);
        }
        if ctl.pause.load(Ordering::Relaxed) {
            let _ = out.flush();
            return Ok(FetchOutcome::PausedAt(done));
        }
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        let _ = tx.send(DlEvent::Progress { index, done, total });
    }
    out.flush()?;
    Ok(FetchOutcome::Completed)
}

/// 单个文件在本地的状态（设置页行展示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// 已存在且校验通过（哈希文件比哈希，字典/DLL 比非空）。
    Ready,
    /// 缺失或校验失败，需要下载。
    Missing,
}

impl FileState {
    /// 状态文字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "已就绪",
            Self::Missing => "缺失",
        }
    }
}

/// 计算文件 SHA256（小写 hex；大文件分块读，内存占用恒定）。
///
/// # Errors
/// 文件打不开或读取失败时返回错误。
pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 单个文件是否就绪（存在 + 非空 + 哈希匹配，如有期望哈希）。
pub fn file_ready(dir: &Path, file: &ModelFile) -> bool {
    let path = dir.join(file.local);
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    match file.sha256 {
        Some(expect) => sha256_file(&path).is_ok_and(|h| h == expect),
        None => true,
    }
}

/// 四件套本地状态（设置页每次绘制时现查，下载完成后自动变绿）。
pub fn file_states(dir: &Path) -> [FileState; 4] {
    model_files().map(|f| {
        if file_ready(dir, &f) {
            FileState::Ready
        } else {
            FileState::Missing
        }
    })
}

/// 是否四齐（`RapidOcrEngine::is_available` 的文件侧条件）。
pub fn all_ready(dir: &Path) -> bool {
    file_states(dir).iter().all(|s| *s == FileState::Ready)
}

/// 校验（哈希）通过后改名为正式文件。
fn verify_and_commit(dir: &Path, file: &ModelFile, part: &Path) -> anyhow::Result<()> {
    if let Some(expect) = file.sha256 {
        let actual = sha256_file(part)?;
        if actual != expect {
            let _ = std::fs::remove_file(part);
            anyhow::bail!(
                "{} 校验失败（下载内容与官方不一致，可能源站异常），已删除半截文件",
                file.local
            );
        }
    }
    let target = dir.join(file.local);
    if target.exists() {
        std::fs::remove_file(&target)?;
    }
    std::fs::rename(part, &target)?;
    // 字典无哈希：只要求非空（类别配套由 rapid.rs 加载时熔断，绝不乱码）
    if file.sha256.is_none() && std::fs::metadata(&target)?.len() == 0 {
        anyhow::bail!("{} 下载为空，请重试或手动下载", file.local);
    }
    Ok(())
}

/// 未完成下载的临时文件（同目录 `.part`，暂停保留、取消删除、下次覆盖重下）。
fn part_path(dir: &Path, local: &str) -> PathBuf {
    dir.join(format!("{local}.part"))
}

/// 下载 ORT 发布包（断点续传）→ 按后缀定位 DLL 条目解出 → 删包。
///
/// 返回拉取结果（暂停/取消直接透出，调用方转事件；完成则继续解包校验）。
fn fetch_and_extract_dll(
    client: &reqwest::blocking::Client,
    zip_url: &str,
    dir: &Path,
    local: &str,
    suffix: &str,
    index: usize,
    tx: &Sender<DlEvent>,
    ctl: &JobControl,
) -> anyhow::Result<FetchOutcome> {
    let part = dir.join("ort_package.zip.part");
    match fetch_resumable(client, zip_url, &part, index, tx, ctl)? {
        FetchOutcome::Completed => {}
        other => return Ok(other),
    }
    let target = dir.join(local);
    let result = extract_zip_suffix(&part, suffix, &target);
    let _ = std::fs::remove_file(&part);
    result?;
    if std::fs::metadata(&target)?.len() < 1024 * 1024 {
        anyhow::bail!("{local} 解出文件过小，发布包可能异常，请重试或手动下载");
    }
    Ok(FetchOutcome::Completed)
}

/// 从 zip 里找出第一个以 `suffix` 结尾的文件条目并解到 `target`。
fn extract_zip_suffix(zip_path: &Path, suffix: &str, target: &Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(f)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if name.to_lowercase().ends_with(&suffix.to_lowercase()) {
            let mut out = std::fs::File::create(target)?;
            std::io::copy(&mut entry, &mut out)?;
            out.flush()?;
            return Ok(());
        }
    }
    anyhow::bail!("压缩包里找不到 {suffix}，发布包结构可能变化，请手动下载")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_table_is_sane() {
        let files = model_files();
        // 本地名互异且与 rapid.rs 探测常量一致
        let mut locals: Vec<_> = files.iter().map(|f| f.local).collect();
        locals.sort_unstable();
        locals.dedup();
        assert_eq!(locals.len(), 4);
        assert!(locals.contains(&DET_MODEL_FILE));
        assert!(locals.contains(&REC_MODEL_FILE));
        assert!(locals.contains(&KEYS_FILE));
        assert!(locals.contains(&RUNTIME_LIB_FILE));
        // 自动源 URL 全为 https 且互异
        let mut urls = Vec::new();
        for f in &files {
            if let Some(u) = f.url {
                assert!(u.starts_with("https://"), "{u}");
                urls.push(u);
            }
            if let Some(z) = f.zip_url {
                assert!(z.starts_with("https://"), "{z}");
                urls.push(z);
            }
        }
        assert_eq!(urls.len(), 4);
        // det/rec 哈希为 64 位 hex（与 AGENTS.md 3.8 节一致）
        for f in files.iter().filter(|f| f.sha256.is_some()) {
            let h = f.sha256.unwrap();
            assert_eq!(h.len(), 64, "{}", f.local);
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "{}", f.local);
        }
        // DLL 走 zip 包（微软发布包，条目按后缀定位）
        let dll = files.iter().find(|f| f.local == RUNTIME_LIB_FILE).unwrap();
        assert!(dll.url.is_none() && dll.zip_url.is_some());
    }

    #[test]
    fn sha256_of_known_content() {
        let dir = std::env::temp_dir().join("prismsnap_dl_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("abc.bin");
        std::fs::write(&p, b"abc").unwrap();
        // "abc" 的 SHA256 公开值
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_dir_reports_all_missing() {
        let dir = std::env::temp_dir().join("prismsnap_dl_nope_12345");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(file_states(&dir).iter().all(|s| *s == FileState::Missing));
        assert!(!all_ready(&dir));
    }

    #[test]
    fn ready_file_passes_and_bad_hash_fails() {
        let dir = std::env::temp_dir().join("prismsnap_dl_ready");
        let _ = std::fs::create_dir_all(&dir);
        // 无哈希文件（字典）：非空即就绪
        let dict = ModelFile {
            local: "t_keys.txt",
            origin: "x",
            desc: "x",
            url: None,
            sha256: None,
            zip_url: None,
            zip_entry_suffix: None,
        };
        std::fs::write(dir.join(dict.local), b"a\nb\n").unwrap();
        assert!(file_ready(&dir, &dict));
        // 有哈希文件：内容对不上即缺失
        let hashed = ModelFile { local: "t_det.onnx", sha256: Some(DET_SHA256), ..dict };
        std::fs::write(dir.join(hashed.local), b"nope").unwrap();
        assert!(!file_ready(&dir, &hashed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_finds_dll_by_suffix_in_nested_dirs() {
        // 内存造一个带内层目录的 deflate zip（模拟 ORT 发布包结构），
        // 验证发布构建的 `default-features = false + deflate` 真够用。
        let dir = std::env::temp_dir().join("prismsnap_dl_zip");
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("pkg.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("ort-1.28.1/lib/onnxruntime.dll", opts).unwrap();
            w.write_all(b"fake-dll-bytes").unwrap();
            w.start_file("ort-1.28.1/include/x.h", opts).unwrap();
            w.write_all(b"h").unwrap();
            w.finish().unwrap();
        }
        let target = dir.join("onnxruntime.dll");
        extract_zip_suffix(&zip_path, "onnxruntime.dll", &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-dll-bytes");
        // 后缀对不上时明确报错（发布包结构变化可感知，不静默）
        assert!(extract_zip_suffix(&zip_path, "nope.dll", &target).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实网络下载（默认忽略，手动跑：`cargo test dl_network -- --ignored`）。
    #[test]
    #[ignore]
    fn dl_network_dict_downloads() {
        let dir = std::env::temp_dir().join("prismsnap_dl_net");
        let _ = std::fs::create_dir_all(&dir);
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let part = part_path(&dir, "net_keys.txt");
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctl = JobControl::default();
        let out = fetch_resumable(&client, DICT_URL, &part, 0, &tx, &ctl).unwrap();
        assert!(matches!(out, FetchOutcome::Completed));
        assert!(std::fs::metadata(&part).unwrap().len() > 0);
        let _ = std::fs::remove_file(&part);
    }
}
