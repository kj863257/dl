use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{ArgAction, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use libdl::{
    DisplayLanguage, DlClient, DownloadKind, DownloadOptions, DownloadPhase, DownloadProgress, DownloadSource,
    TorrentInput,
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang { Zh, En }

impl Lang {
    fn detect() -> Self {
        if let Ok(value) = std::env::var("DL_LANG") {
            if value.to_ascii_lowercase().starts_with("en") { return Self::En; }
            if value.to_ascii_lowercase().starts_with("zh") { return Self::Zh; }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
            let mut buffer = [0u16; 85];
            let len = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
            if len > 0 {
                let locale = String::from_utf16_lossy(&buffer[..len as usize]);
                return if locale.to_ascii_lowercase().starts_with("zh") { Self::Zh } else { Self::En };
            }
        }
        for key in ["LC_ALL", "LANG", "LANGUAGE"] {
            if let Ok(value) = std::env::var(key) {
                return if value.to_ascii_lowercase().starts_with("zh") { Self::Zh } else { Self::En };
            }
        }
        Self::En
    }

    fn from_arg(value: Option<&str>) -> Result<Self, String> {
        match value.map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("auto") => Ok(Self::detect()),
            Some("zh") | Some("zh-cn") | Some("中文") => Ok(Self::Zh),
            Some("en") | Some("en-us") | Some("english") => Ok(Self::En),
            Some(other) => Err(format!("不支持的语言：{other}（可选 auto、zh、en）")),
        }
    }
}

fn text(lang: Lang, zh: &str, en: &str) -> String { if lang == Lang::Zh { zh } else { en }.to_string() }

#[derive(Debug, Clone, Parser)]
#[command(
    name = "dl",
    version,
    about = "轻量级 HTTP 与种子下载加速工具",
    help_template = "{about}\n\n用法：{usage}\n\n参数：\n{positionals}\n\n选项：\n{options}",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    /// 要下载的 URL、磁力链接或 .torrent 文件
    source: String,

    /// HTTP 下载的输出文件，或种子下载的输出目录
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// HTTP 分段下载的并发工作线程数量
    #[arg(short = 'j', long)]
    connections: Option<usize>,

    /// HTTP 分块大小，支持纯字节数或 K、M、G 后缀
    #[arg(long, default_value = "2M", value_parser = parse_size)]
    chunk_size: u64,

    /// 禁用可恢复下载的内嵌元数据
    #[arg(long)]
    no_resume: bool,

    /// 覆盖已有的输出路径
    #[arg(long)]
    overwrite: bool,

    /// 将 HTTP(S) 地址视为种子地址，而非普通 HTTP 文件
    #[arg(long)]
    torrent: bool,

    /// HTTP/HTTPS 代理地址
    #[arg(long)]
    proxy: Option<String>,

    /// 最大下载速度，例如 5M、500K
    #[arg(long, value_parser = parse_size)]
    limit_rate: Option<u64>,

    /// 附加请求头，可重复指定，例如 --header "Referer: https://example.com"
    #[arg(long = "header", value_parser = parse_header)]
    headers: Vec<(String, String)>,

    /// Cookie 内容，例如 "session=abc; token=xyz"
    #[arg(long)]
    cookie: Option<String>,

    /// Bearer 令牌，自动生成 Authorization 请求头
    #[arg(long)]
    bearer_token: Option<String>,

    /// HTTP 基本认证，格式为 用户名:密码
    #[arg(long)]
    basic_auth: Option<String>,

    /// 以 JSON 输出结果，适合脚本和 AI 调用
    #[arg(long)]
    json: bool,

    /// 输出语言：auto、zh 或 en
    #[arg(long, default_value = "auto")]
    lang: String,

    /// 显示帮助信息
    #[arg(short = 'h', long, action = ArgAction::Help)]
    help: Option<bool>,

    /// 显示版本信息
    #[arg(short = 'V', long, action = ArgAction::Version)]
    version: Option<bool>,
}

#[tokio::main]
async fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.is_empty() {
        print_missing_source(Lang::detect());
        std::process::exit(2);
    }
    if raw_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help(language_from_args(&raw_args));
        return;
    }
    let lang = language_from_args(&raw_args);
    let cli = match Cli::try_parse_from(std::iter::once("dl.exe".to_string()).chain(raw_args.clone())) {
        Ok(cli) => cli,
        Err(error) => {
            if raw_args.iter().any(|arg| arg == "--json") {
                println!("{}", serde_json::json!({"ok": false, "error": parse_error_message(&error, lang), "language": if lang == Lang::Zh { "zh" } else { "en" }}));
            } else {
                print_parse_error(&error, lang);
            }
            std::process::exit(error.exit_code());
        }
    };
    let json = cli.json;
    let lang = Lang::from_arg(Some(&cli.lang)).unwrap_or(lang);
    if let Err(error) = run(cli, lang).await {
        if json {
            println!("{}", serde_json::json!({"ok": false, "error": display_error(&error, lang), "language": if lang == Lang::Zh { "zh" } else { "en" }}));
        } else {
            eprintln!("{}：{}", text(lang, "错误", "Error"), display_error(&error, lang));
        }
        std::process::exit(1);
    }
}

fn display_error(error: &libdl::DlError, lang: Lang) -> String {
    use libdl::DlError;
    match error {
        DlError::Io(detail) => format!("{}：{detail}", text(lang, "输入输出错误", "I/O error")),
        DlError::Http(detail) => format!("{}：{detail}", text(lang, "HTTP 错误", "HTTP error")),
        DlError::InvalidHeader(detail) => format!("{}：{detail}", text(lang, "请求头无效", "Invalid HTTP header")),
        DlError::InvalidResponse(detail) => format!("{}：{detail}", text(lang, "无效的 HTTP 响应", "Invalid HTTP response")),
        DlError::RateLimited { message, .. } => format!("{}：{message}", text(lang, "请求被限速", "Rate limited")),
        DlError::ServerError(detail) => format!("{}：{detail}", text(lang, "服务器错误", "Server error")),
        DlError::RangesUnsupported => text(lang, "服务器不支持可恢复的分段下载", "Server does not support resumable range downloads"),
        DlError::InvalidState(detail) => format!("{}：{detail}", text(lang, "下载状态无效", "Download state is invalid")),
        DlError::Serialization(detail) => format!("{}：{detail}", text(lang, "序列化错误", "Serialization error")),
        DlError::Torrent(detail) => format!("{}：{detail}", text(lang, "种子错误", "Torrent error")),
        DlError::Join(detail) => format!("{}：{detail}", text(lang, "工作线程任务失败", "Worker task failed")),
    }
}

async fn run(cli: Cli, lang: Lang) -> libdl::Result<()> {
    let source = classify_source(&cli);
    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| default_output_path(&source, &cli.source));

    let mut selected_files = None;

    if let DownloadSource::Torrent(ref torrent_input) = source {
        println!("{}", text(lang, "正在解析种子元数据", "Resolving torrent metadata..."));
        let files = libdl::list_torrent_files(torrent_input.clone(), &output).await?;
        if files.is_empty() {
            println!("{}", text(lang, "种子中没有找到文件", "No files found in torrent."));
        } else if files.len() > 1 {
            selected_files = select_torrent_files_interactive(&files)?;
        }
    }

    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let render_task = (!cli.json).then(|| tokio::spawn(render_progress(progress_rx, lang)));

    let mut headers = cli.headers.clone();
    if let Some(cookie) = &cli.cookie { headers.push(("Cookie".to_string(), cookie.clone())); }
    if let Some(token) = &cli.bearer_token { headers.push(("Authorization".to_string(), format!("Bearer {token}"))); }
    if let Some(auth) = &cli.basic_auth {
        use base64::Engine;
        headers.push(("Authorization".to_string(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(auth))));
    }

    let options = DownloadOptions {
        connections: cli.connections,
        chunk_size: cli.chunk_size,
        resume: !cli.no_resume,
        overwrite: cli.overwrite,
        only_files: selected_files,
        rate_limit: cli.limit_rate,
        headers,
        proxy: cli.proxy.clone(),
        language: if lang == Lang::Zh { DisplayLanguage::Zh } else { DisplayLanguage::En },
        progress: (!cli.json).then_some(progress_tx),
        ..DownloadOptions::default()
    };

    let client = DlClient::new(options);
    let result = client.download(source, &output).await;
    drop(client);

    if let Some(task) = render_task { let _ = task.await; }
    let summary = result?;
    if cli.json {
        println!("{}", serde_json::json!({"ok": true, "kind": format!("{:?}", summary.kind), "source": summary.source, "output": summary.output_path, "total_bytes": summary.total_bytes, "downloaded_bytes": summary.downloaded_bytes, "resumed": summary.resumed, "language": if lang == Lang::Zh { "zh" } else { "en" }}));
    } else {
        println!("{}", text(lang, &format!("已下载 {} 字节，保存至 {}", summary.downloaded_bytes, summary.output_path.display()), &format!("Downloaded {} bytes to {}", summary.downloaded_bytes, summary.output_path.display())));
    }
    Ok(())
}

fn select_torrent_files_interactive(files: &[(usize, String, u64)]) -> libdl::Result<Option<Vec<usize>>> {
    use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};

    let theme = ColorfulTheme::default();

    let choose = Confirm::with_theme(&theme)
        .with_prompt("此种子包含多个文件，是否选择要下载的文件？")
        .default(false)
        .interact()
        .map_err(|err| libdl::DlError::Torrent(format!("交互提示失败：{err}")))?;

    if !choose {
        return Ok(None);
    }

    let items: Vec<String> = files
        .iter()
        .map(|(_, name, size)| format!("{} ({})", name, format_bytes(*size)))
        .collect();

    let defaults = vec![true; files.len()];

    loop {
        let selections = MultiSelect::with_theme(&theme)
            .with_prompt("选择要下载的文件（按空格键切换，按回车键确认）")
            .items(&items)
            .defaults(&defaults)
            .interact()
            .map_err(|err| libdl::DlError::Torrent(format!("交互选择失败：{err}")))?;

        if selections.is_empty() {
            let cancel = Confirm::with_theme(&theme)
                .with_prompt("未选择任何文件，是否取消下载？")
                .default(true)
                .interact()
                .map_err(|err| libdl::DlError::Torrent(format!("交互取消提示失败：{err}")))?;

            if cancel {
                return Err(libdl::DlError::Torrent("用户取消了下载".to_string()));
            } else {
                continue;
            }
        }

        let mut indices: Vec<usize> = selections.into_iter().map(|idx| files[idx].0).collect();
        indices.sort_unstable();
        indices.dedup();
        return Ok(Some(indices));
    }
}

fn classify_source(cli: &Cli) -> DownloadSource {
    if cli.torrent
        || cli.source.starts_with("magnet:")
        || (!cli.source.starts_with("http://")
            && !cli.source.starts_with("https://")
            && cli.source.ends_with(".torrent"))
    {
        DownloadSource::Torrent(TorrentInput::from_source(cli.source.clone()))
    } else {
        DownloadSource::Http(cli.source.clone())
    }
}

fn default_output_path(source: &DownloadSource, raw_source: &str) -> PathBuf {
    match source {
        DownloadSource::Torrent(_) => PathBuf::from("."),
        DownloadSource::Http(_) => url::Url::parse(raw_source)
            .ok()
            .and_then(|url| {
                url.path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .filter(|segment| !segment.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("下载文件.bin")),
    }
}

struct SpeedTracker {
    history: VecDeque<(Instant, u64)>,
    window_duration: Duration,
}

impl SpeedTracker {
    fn new(window_duration: Duration) -> Self {
        Self {
            history: VecDeque::new(),
            window_duration,
        }
    }

    fn add_sample(&mut self, bytes: u64) {
        let now = Instant::now();
        self.history.push_back((now, bytes));
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        let threshold = now.checked_sub(self.window_duration).unwrap_or(now);
        while self.history.len() > 1 && self.history[0].0 < threshold {
            self.history.pop_front();
        }
    }

    fn current_speed(&mut self) -> f64 {
        let now = Instant::now();
        self.prune(now);

        if self.history.len() < 2 {
            return 0.0;
        }

        let oldest = self.history.front().unwrap();
        let youngest = self.history.back().unwrap();

        // If the last update was too long ago, we've stalled/stopped
        if now.duration_since(youngest.0) > Duration::from_secs(2) {
            return 0.0;
        }

        let duration = youngest.0.duration_since(oldest.0);
        if duration.as_secs_f64() < 0.01 {
            return 0.0;
        }

        let bytes = youngest.1.saturating_sub(oldest.1);
        bytes as f64 / duration.as_secs_f64()
    }
}

async fn render_progress(mut progress_rx: libdl::ProgressReceiver, lang: Lang) {
    let multi = MultiProgress::new();
    let overall = multi.add(ProgressBar::new_spinner());
    let status = multi.add(ProgressBar::new_spinner());

    let mut tracker = SpeedTracker::new(Duration::from_secs(3));

    overall.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    status.set_style(ProgressStyle::with_template("{msg}").unwrap());

    let mut last_update = Instant::now();
    let mut latest_progress: Option<DownloadProgress> = None;
    let mut needs_render = false;
    let mut last_phase = None;
    const UI_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

    while let Some(progress) = progress_rx.recv().await {
        if progress.phase == DownloadPhase::Downloading {
            tracker.add_sample(progress.downloaded_bytes);
        }

        let is_complete = progress.phase == DownloadPhase::Complete;
        let phase_changed = last_phase.as_ref() != Some(&progress.phase);
        last_phase = Some(progress.phase.clone());
        latest_progress = Some(progress);
        needs_render = true;

        let now = Instant::now();
        if is_complete || phase_changed || now.duration_since(last_update) >= UI_UPDATE_INTERVAL {
            if let Some(ref p) = latest_progress {
                let speed = tracker.current_speed();
                let speed_str = format!("{}/s", format_bytes(speed as u64));
                let eta_str = if p.phase == DownloadPhase::Complete {
                    String::new()
                } else if speed < 1.0 {
                    "--:--:--".to_string()
                } else {
                    let total = p.total_bytes.unwrap_or(p.downloaded_bytes);
                    if total <= p.downloaded_bytes {
                        "0s".to_string()
                    } else {
                        let remaining = total - p.downloaded_bytes;
                        let eta_secs = remaining as f64 / speed;
                        format_duration(Duration::from_secs_f64(eta_secs))
                    }
                };

                let speed_and_eta = if eta_str.is_empty() {
                    speed_str
                } else {
                    format!("{speed_str} {eta_str}")
                };

                update_overall(&overall, p, speed_and_eta);
                status.set_message(status_message(p, lang));
                needs_render = false;
                last_update = now;

                if is_complete {
                    overall.finish();
                    status.finish_and_clear();
                }
            }
        }
    }

    if needs_render {
        if let Some(ref p) = latest_progress {
            let speed = tracker.current_speed();
            let speed_str = format!("{}/s", format_bytes(speed as u64));
            let eta_str = if p.phase == DownloadPhase::Complete {
                String::new()
            } else if speed < 1.0 {
                "--:--:--".to_string()
            } else {
                let total = p.total_bytes.unwrap_or(p.downloaded_bytes);
                if total <= p.downloaded_bytes {
                    "0s".to_string()
                } else {
                    let remaining = total - p.downloaded_bytes;
                    let eta_secs = remaining as f64 / speed;
                    format_duration(Duration::from_secs_f64(eta_secs))
                }
            };

            let speed_and_eta = if eta_str.is_empty() {
                speed_str
            } else {
                format!("{speed_str} {eta_str}")
            };

            update_overall(&overall, p, speed_and_eta);
            status.set_message(status_message(p, lang));
            if p.phase == DownloadPhase::Complete {
                overall.finish();
                status.finish_and_clear();
            }
        }
    }

    overall.finish_and_clear();
    status.finish_and_clear();
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        return "0 秒".to_string();
    }
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{} 小时 {} 分钟", hours, minutes)
    } else if minutes > 0 {
        format!("{} 分钟 {} 秒", minutes, seconds)
    } else {
        format!("{} 秒", seconds)
    }
}

fn update_overall(overall: &ProgressBar, progress: &DownloadProgress, speed_and_eta: String) {
    if let Some(total) = progress.total_bytes {
        overall.set_length(total);
    }
    overall.set_position(progress.downloaded_bytes);

    if progress.total_bytes.is_none() {
        overall.set_message(format!("{} | {}", format_bytes(progress.downloaded_bytes), speed_and_eta));
    } else {
        overall.set_message(speed_and_eta);
    }
}

fn status_message(progress: &DownloadProgress, lang: Lang) -> String {
    let kind = match progress.kind {
        DownloadKind::Http => "HTTP",
        DownloadKind::Torrent => if lang == Lang::Zh { "种子" } else { "torrent" },
    };
    let phase = match progress.phase {
        DownloadPhase::Probing => if lang == Lang::Zh { "探测中" } else { "probing" },
        DownloadPhase::Downloading => if lang == Lang::Zh { "下载中" } else { "downloading" },
        DownloadPhase::PersistingState => if lang == Lang::Zh { "保存状态" } else { "saving state" },
        DownloadPhase::Finalizing => if lang == Lang::Zh { "整理文件" } else { "finalizing" },
        DownloadPhase::Complete => if lang == Lang::Zh { "已完成" } else { "complete" },
    };

    match (progress.completed_chunks, progress.total_chunks) {
        (Some(completed), Some(total)) => if lang == Lang::Zh {
            format!("{kind}：{phase} | 工作线程={} | 分块={completed}/{total} | {}", progress.active_workers, progress.output_path.display())
        } else {
            format!("{kind}: {phase} | workers={} | chunks={completed}/{total} | {}", progress.active_workers, progress.output_path.display())
        },
        _ => if lang == Lang::Zh {
            format!("{kind}：{phase} | 工作线程={} | {}", progress.active_workers, progress.output_path.display())
        } else {
            format!("{kind}: {phase} | workers={} | {}", progress.active_workers, progress.output_path.display())
        },
    }
}

fn parse_size(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("大小不能为空".to_string());
    }

    let (number, multiplier) = match trimmed.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&trimmed[..trimmed.len() - 1], 1024),
        Some(b'm') | Some(b'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };

    number
        .parse::<u64>()
        .map(|value| value.saturating_mul(multiplier))
        .map_err(|error| format!("大小格式无效 `{input}`：{error}"))
}

fn language_from_args(args: &[String]) -> Lang {
    let requested = args.iter().enumerate().find_map(|(index, arg)| {
        arg.strip_prefix("--lang=").map(str::to_string).or_else(|| (arg == "--lang").then(|| args.get(index + 1).cloned()).flatten())
    });
    Lang::from_arg(requested.as_deref()).unwrap_or_else(|_| Lang::detect())
}

fn print_help(lang: Lang) {
    let help = if lang == Lang::Zh {
        r#"轻量级 HTTP 与种子下载加速工具

用法：dl.exe [选项] <来源>

参数：
  <来源>                          要下载的 URL、磁力链接或 .torrent 文件

选项：
  -o, --output <输出>             HTTP 下载的输出文件，或种子下载的输出目录
  -j, --connections <数量>        HTTP 分段下载的并发工作线程数量
      --chunk-size <大小>         HTTP 分块大小，支持纯字节数或 K、M、G 后缀 [默认值：2M]
      --limit-rate <大小>         最大下载速度，例如 5M、500K
      --proxy <地址>              HTTP/HTTPS 代理地址
      --header <名称: 值>         附加请求头，可重复指定
      --cookie <内容>             Cookie 内容
      --bearer-token <令牌>       Bearer 令牌
      --basic-auth <用户:密码>    HTTP 基本认证
      --json                      以 JSON 输出结果，适合脚本和 AI 调用
      --lang <语言>               输出语言：auto、zh 或 en [默认值：auto]
      --no-resume                 禁用可恢复下载的内嵌元数据
      --overwrite                 覆盖已有的输出路径
      --torrent                   将 HTTP(S) 地址视为种子地址，而非普通 HTTP 文件
  -h, --help                      显示帮助信息
  -V, --version                   显示版本信息"#
    } else {
        r#"A lightweight HTTP and torrent download accelerator

Usage: dl.exe [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>                        URL, magnet link, or .torrent file to download

Options:
  -o, --output <OUTPUT>            Output file for HTTP downloads, or output directory for torrents
  -j, --connections <COUNT>        Number of concurrent HTTP range workers
      --chunk-size <SIZE>          HTTP chunk size with optional K, M, or G suffix [default: 2M]
      --limit-rate <SIZE>          Maximum download speed, for example 5M or 500K
      --proxy <URL>                HTTP/HTTPS proxy URL
      --header <NAME: VALUE>       Additional HTTP header; can be specified more than once
      --cookie <VALUE>             Cookie header value
      --bearer-token <TOKEN>       Bearer token for Authorization
      --basic-auth <USER:PASSWORD> HTTP Basic authentication
      --json                       Output the final result as JSON for scripts and AI callers
      --lang <LANG>                Output language: auto, zh, or en [default: auto]
      --no-resume                  Disable resumable inline metadata
      --overwrite                  Replace an existing output path
      --torrent                    Treat an HTTP(S) URL as a torrent URL
  -h, --help                       Print help
  -V, --version                    Print version"#
    };
    println!("{help}");
}

fn print_missing_source(lang: Lang) {
    if lang == Lang::Zh {
        eprintln!("错误：缺少必填参数 <来源>\n\n用法：dl.exe [选项] <来源>\n\n使用 --help 查看帮助信息");
    } else {
        eprintln!("error: the following required arguments were not provided:\n  <SOURCE>\n\nUsage: dl.exe [OPTIONS] <SOURCE>\n\nFor more information, try '--help'.");
    }
}

fn print_parse_error(error: &clap::Error, lang: Lang) {
    eprintln!("{}", parse_error_message(error, lang));
}

fn parse_error_message(error: &clap::Error, lang: Lang) -> String {
    use clap::error::ErrorKind;
    match error.kind() {
        ErrorKind::MissingRequiredArgument => if lang == Lang::Zh {
            "错误：缺少必填参数 <来源>\n\n用法：dl.exe [选项] <来源>\n\n使用 --help 查看帮助信息".to_string()
        } else {
            "error: the following required arguments were not provided:\n  <SOURCE>\n\nUsage: dl.exe [OPTIONS] <SOURCE>\n\nFor more information, try '--help'.".to_string()
        },
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            if lang == Lang::Zh {
                "错误：参数值无效\n\n用法：dl.exe [选项] <来源>\n\n使用 --help 查看帮助信息".to_string()
            } else {
                "error: invalid argument value\n\nUsage: dl.exe [OPTIONS] <SOURCE>\n\nFor more information, try '--help'.".to_string()
            }
        }
        ErrorKind::UnknownArgument => {
            if lang == Lang::Zh {
                "错误：包含未知选项\n\n使用 --help 查看帮助信息".to_string()
            } else {
                "error: unexpected argument\n\nFor more information, try '--help'.".to_string()
            }
        }
        _ => {
            if lang == Lang::Zh {
                "错误：命令参数无效\n\n使用 --help 查看帮助信息".to_string()
            } else {
                "error: invalid command line arguments\n\nFor more information, try '--help'.".to_string()
            }
        }
    }
}

fn parse_header(input: &str) -> Result<(String, String), String> {
    let (name, value) = input.split_once(':').ok_or_else(|| "请求头格式应为 名称: 值".to_string())?;
    if name.trim().is_empty() || value.trim().is_empty() { return Err("请求头名称和值不能为空".to_string()); }
    Ok((name.trim().to_string(), value.trim().to_string()))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    format!("{size:.1} {}", UNITS[unit])
}
