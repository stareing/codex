//! Host configuration summary for the site this computer is signed in to.
//!
//! After a sign-in and whenever `remote-control` starts, the CLI posts a short summary of the
//! computer (OS, hardware, installed tool versions, agent settings, MCP server names) to the
//! site's `/api/agent-devices/self/profile/`, authenticated with the device token. It never
//! contains environment variable values, paths, user names, file contents or keys; the shape below
//! is the whole contract (the site keeps only these fields). Reporting is best effort: failures
//! are a warning, never an error. Opt out with `--no-host-info` or `AGENT_HOST_INFO=0`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use codex_app_server_transport::SiteAccount;
use codex_core::config::Config;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_cli::CliConfigOverrides;
use regex_lite::Regex;
use serde::Serialize;

pub const HOST_INFO_ENV_VAR: &str = "AGENT_HOST_INFO";
const PROFILE_PATH: &str = "/api/agent-devices/self/profile/";
const TOOL_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const POST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_GPUS: usize = 8;
const MAX_MCP_SERVERS: usize = 64;
const MAX_TEXT_CHARS: usize = 128;
const MIB: u64 = 1024 * 1024;

/// Tools whose version is reported; the site accepts exactly these names.
pub const TOOLS: &[&str] = &[
    "git", "node", "npm", "pnpm", "yarn", "bun", "deno", "python", "pip", "uv", "conda", "cargo",
    "rustc", "go", "java", "dotnet", "docker", "podman", "gcc", "clang", "cmake", "make",
];

// Static patterns; `None` only if they failed to compile, which the tests rule out.
static VERSION_IN_TEXT: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"[0-9]+(?:\.[0-9]+)+[0-9A-Za-z.+_-]*").ok());
static VALID_VERSION: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[0-9A-Za-z][0-9A-Za-z.+_-]{0,47}$").ok());

/// Whether `version` has the shape the site accepts.
fn valid_version(version: &str) -> bool {
    VALID_VERSION
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(version))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostReport {
    pub os: OsSummary,
    pub hardware: Hardware,
    pub gpus: Vec<String>,
    pub client: ClientSummary,
    pub agent: AgentSettings,
    pub environment: Environment,
    pub tools: BTreeMap<String, String>,
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OsSummary {
    pub family: String,
    pub name: String,
    pub version: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Hardware {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    pub cpu_cores: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displays: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClientSummary {
    pub version: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Environment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// `true` when the user turned reporting off with the flag or `AGENT_HOST_INFO=0|false|off|no`.
pub fn opted_out(flag: bool) -> bool {
    flag || env_opts_out(std::env::var(HOST_INFO_ENV_VAR).ok().as_deref())
}

fn env_opts_out(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Free text from the system (CPU/GPU names, locale): control characters dropped, length capped.
fn clean_text(raw: &str) -> Option<String> {
    let text: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
    (!text.is_empty()).then_some(text)
}

/// The version number in a `--version` line, e.g. `git version 2.47.1` → `2.47.1`,
/// `go version go1.22.3 linux/amd64` → `1.22.3`, `v20.11.0` → `20.11.0`.
pub fn extract_version(output: &str) -> Option<String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let found = VERSION_IN_TEXT.as_ref()?.find(line)?.as_str();
    let found = found.trim_end_matches(['.', '-', '_', '+']);
    let found: String = found.chars().take(48).collect();
    valid_version(&found).then_some(found)
}

fn os_family() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

fn arch_name(arch: &str) -> String {
    match arch {
        "x86_64" | "aarch64" | "x86" | "arm" => arch.to_string(),
        other => other.chars().take(16).collect(),
    }
}

/// Maps a shell executable name or path to one of the names the site accepts.
pub fn shell_name(path: &str) -> Option<&'static str> {
    let base = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    match base {
        "bash" => Some("bash"),
        "zsh" => Some("zsh"),
        "fish" => Some("fish"),
        "sh" | "dash" | "ash" => Some("sh"),
        "powershell" => Some("powershell"),
        "pwsh" => Some("pwsh"),
        "cmd" => Some("cmd"),
        "nu" => Some("nu"),
        _ => None,
    }
}

fn current_shell() -> Option<&'static str> {
    if cfg!(windows) {
        // Commands run through PowerShell on Windows.
        return Some("powershell");
    }
    std::env::var("SHELL").ok().as_deref().and_then(shell_name)
}

fn approval_name(policy: AskForApproval) -> &'static str {
    match policy {
        AskForApproval::UnlessTrusted => "untrusted",
        AskForApproval::OnRequest | AskForApproval::Granular(_) => "on-request",
        AskForApproval::Never => "never",
    }
}

fn sandbox_name(policy: &SandboxPolicy) -> Option<&'static str> {
    match policy {
        SandboxPolicy::ReadOnly { .. } => Some("read-only"),
        SandboxPolicy::WorkspaceWrite { .. } => Some("workspace-write"),
        SandboxPolicy::DangerFullAccess => Some("danger-full-access"),
        SandboxPolicy::ExternalSandbox { .. } => None,
    }
}

fn effort_name(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => Some("none"),
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh => Some("xhigh"),
        _ => None,
    }
}

fn agent_settings(config: &Config) -> AgentSettings {
    AgentSettings {
        model: config.model.as_deref().and_then(clean_text),
        model_provider: clean_text(&config.model_provider_id),
        approval_policy: Some(approval_name(*config.permissions.approval_policy.get())),
        sandbox_mode: sandbox_name(&config.legacy_sandbox_policy()),
        reasoning_effort: config.model_reasoning_effort.as_ref().and_then(effort_name),
    }
}

fn mcp_server_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = config
        .mcp_servers
        .get()
        .keys()
        .filter_map(|name| clean_text(name))
        .collect();
    names.sort();
    names.truncate(MAX_MCP_SERVERS);
    names
}

/// Runs `program args…` with a deadline; its stdout (or stderr when stdout is empty) on success.
async fn run_probe(program: &Path, args: &[&str], deadline: Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(deadline, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() {
        Some(String::from_utf8_lossy(&output.stderr).into_owned())
    } else {
        Some(stdout)
    }
}

async fn run_named(program: &str, args: &[&str], deadline: Duration) -> Option<String> {
    let path = which::which(program).ok()?;
    run_probe(&path, args, deadline).await
}

async fn tool_versions() -> BTreeMap<String, String> {
    let probes = TOOLS.iter().map(|tool| async move {
        let output = run_named(tool, &["--version"], TOOL_TIMEOUT).await?;
        Some(((*tool).to_string(), extract_version(&output)?))
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// `model name` of the first processor in `/proc/cpuinfo`.
pub fn parse_cpuinfo_model(cpuinfo: &str) -> Option<String> {
    cpuinfo
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| matches!(key.trim(), "model name" | "Model" | "Hardware"))
        .and_then(|(_, value)| clean_text(value))
}

/// `MemTotal` of `/proc/meminfo`, in MiB.
pub fn parse_meminfo_total_mb(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

/// Display adapters in `lspci -mm` output (`slot "class" "vendor" "device" …`).
pub fn parse_lspci_gpus(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
            let [class, vendor, device, ..] = fields.as_slice() else {
                return None;
            };
            let is_display = class.contains("VGA")
                || class.contains("3D controller")
                || class.contains("Display controller");
            is_display.then(|| clean_text(&format!("{vendor} {device}")))?
        })
        .collect()
}

/// GPU names and the number of attached displays from `system_profiler SPDisplaysDataType -json`.
pub fn parse_macos_displays(json: &str) -> (Vec<String>, Option<u64>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return (Vec::new(), None);
    };
    let adapters = value["SPDisplaysDataType"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let gpus = adapters
        .iter()
        .filter_map(|adapter| adapter["sppci_model"].as_str().and_then(clean_text))
        .collect();
    let displays = adapters
        .iter()
        .map(|adapter| {
            adapter["spdisplays_ndrvs"]
                .as_array()
                .map_or(0, |screens| screens.len() as u64)
        })
        .sum();
    (gpus, Some(displays))
}

/// The JSON object printed by [`WINDOWS_PROBE`].
#[derive(Debug, Default, PartialEq, serde::Deserialize)]
pub struct WindowsProbe {
    cpu: Option<String>,
    memory: Option<u64>,
    #[serde(default, deserialize_with = "one_or_many")]
    gpus: Vec<String>,
    displays: Option<u64>,
    disk_total: Option<u64>,
    disk_free: Option<u64>,
}

/// PowerShell collapses one-element arrays into a scalar in `ConvertTo-Json`.
fn one_or_many<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<String>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<Option<String>>),
        Null(()),
    }
    Ok(match <OneOrMany as serde::Deserialize>::deserialize(de)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many.into_iter().flatten().collect(),
        OneOrMany::Null(()) => Vec::new(),
    })
}

pub fn parse_windows_probe(json: &str) -> Option<WindowsProbe> {
    serde_json::from_str(json.trim()).ok()
}

/// One CIM query for what the standard library cannot tell on Windows. `{DRIVE}` is replaced by
/// the validated drive letter of the current directory (or the line is dropped).
const WINDOWS_PROBE: &str = "$ErrorActionPreference='SilentlyContinue';\
$p=Get-CimInstance Win32_Processor|Select-Object -First 1;\
$c=Get-CimInstance Win32_ComputerSystem;\
$d=Get-CimInstance Win32_LogicalDisk -Filter \"DeviceID='{DRIVE}:'\";\
$m=@(Get-CimInstance -Namespace root\\wmi -ClassName WmiMonitorBasicDisplayParams).Count;\
@{cpu=$p.Name;memory=[uint64]$c.TotalPhysicalMemory;gpus=@(Get-CimInstance Win32_VideoController|ForEach-Object {$_.Name});\
displays=$m;disk_total=[uint64]$d.Size;disk_free=[uint64]$d.FreeSpace}|ConvertTo-Json -Compress";

fn drive_letter(path: &Path) -> Option<char> {
    let text = path.to_str()?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let mut chars = text.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

#[cfg(unix)]
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` is a valid out-pointer for statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it initialised `stats`.
    let stats = unsafe { stats.assume_init() };
    let block = u128::from(stats.f_frsize);
    let total = u64::try_from(u128::from(stats.f_blocks) * block).ok()?;
    let free = u64::try_from(u128::from(stats.f_bavail) * block).ok()?;
    Some((total, free))
}

#[cfg(not(unix))]
fn disk_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn kernel_release() -> Option<String> {
    use std::mem::MaybeUninit;
    let mut name = MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `name` is a valid out-pointer for uname.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: uname returned 0, so `release` holds a NUL-terminated string.
    let name = unsafe { name.assume_init() };
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    clean_text(&release.to_string_lossy())
}

#[cfg(not(unix))]
fn kernel_release() -> Option<String> {
    clean_text(&os_info::get().version().to_string())
}

fn work_dir() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

async fn hardware_and_gpus() -> (Hardware, Vec<String>) {
    let mut hardware = Hardware {
        cpu_cores: std::thread::available_parallelism().map_or(0, |n| n.get() as u64),
        ..Hardware::default()
    };
    if let Some((total, free)) = work_dir().as_deref().and_then(disk_space) {
        hardware.disk_total_mb = Some(total / MIB);
        hardware.disk_free_mb = Some(free / MIB);
    }
    let mut gpus = Vec::new();
    match os_family() {
        "linux" => {
            hardware.cpu = std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .as_deref()
                .and_then(parse_cpuinfo_model);
            hardware.memory_mb = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .as_deref()
                .and_then(parse_meminfo_total_mb);
            hardware.displays = Some(linux_connected_displays());
            let (lspci, nvidia) = tokio::join!(
                run_named("lspci", &["-mm"], PROBE_TIMEOUT),
                run_named(
                    "nvidia-smi",
                    &["--query-gpu=name", "--format=csv,noheader"],
                    PROBE_TIMEOUT
                ),
            );
            gpus = lspci.as_deref().map(parse_lspci_gpus).unwrap_or_default();
            if gpus.is_empty() {
                gpus = nvidia
                    .as_deref()
                    .map(|out| out.lines().filter_map(clean_text).collect())
                    .unwrap_or_default();
            }
        }
        "macos" => {
            let (cpu, memory, displays) = tokio::join!(
                run_named("sysctl", &["-n", "machdep.cpu.brand_string"], PROBE_TIMEOUT),
                run_named("sysctl", &["-n", "hw.memsize"], PROBE_TIMEOUT),
                run_named(
                    "system_profiler",
                    &["SPDisplaysDataType", "-json"],
                    PROBE_TIMEOUT
                ),
            );
            hardware.cpu = cpu.as_deref().and_then(clean_text);
            hardware.memory_mb = memory
                .and_then(|bytes| bytes.trim().parse::<u64>().ok())
                .map(|bytes| bytes / MIB);
            if let Some(json) = displays {
                let (names, count) = parse_macos_displays(&json);
                gpus = names;
                hardware.displays = count;
            }
        }
        _ => {
            let script = match work_dir().as_deref().and_then(drive_letter) {
                Some(letter) => WINDOWS_PROBE.replace("{DRIVE}", &letter.to_string()),
                None => WINDOWS_PROBE.replace("{DRIVE}", "C"),
            };
            let output = run_named(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &script],
                PROBE_TIMEOUT,
            )
            .await;
            if let Some(probe) = output.as_deref().and_then(parse_windows_probe) {
                hardware.cpu = probe.cpu.as_deref().and_then(clean_text);
                hardware.memory_mb = probe.memory.filter(|m| *m > 0).map(|m| m / MIB);
                hardware.displays = probe.displays;
                if hardware.disk_total_mb.is_none() {
                    hardware.disk_total_mb = probe.disk_total.filter(|d| *d > 0).map(|d| d / MIB);
                    hardware.disk_free_mb = probe.disk_free.map(|d| d / MIB);
                }
                gpus = probe.gpus.iter().filter_map(|g| clean_text(g)).collect();
            }
        }
    }
    gpus.dedup();
    gpus.truncate(MAX_GPUS);
    (hardware, gpus)
}

fn linux_connected_displays() -> u64 {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
        .count() as u64
}

fn os_summary() -> OsSummary {
    let info = os_info::get();
    OsSummary {
        family: os_family().to_string(),
        name: clean_text(&info.os_type().to_string()).unwrap_or_default(),
        version: clean_text(&info.version().to_string()).unwrap_or_default(),
        arch: arch_name(std::env::consts::ARCH),
        kernel: kernel_release(),
    }
}

fn environment() -> Environment {
    Environment {
        shell: current_shell(),
        locale: sys_locale::get_locale().as_deref().and_then(clean_text),
        timezone: iana_time_zone::get_timezone()
            .ok()
            .as_deref()
            .and_then(clean_text),
    }
}

/// Collects the summary; `config` supplies the agent settings and MCP server names.
pub async fn collect(config: Option<&Config>) -> HostReport {
    let ((hardware, gpus), tools) = tokio::join!(hardware_and_gpus(), tool_versions());
    HostReport {
        os: os_summary(),
        hardware,
        gpus,
        client: ClientSummary {
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: "cli".to_string(),
        },
        agent: config.map(agent_settings).unwrap_or_default(),
        environment: environment(),
        tools,
        mcp_servers: config.map(mcp_server_names).unwrap_or_default(),
    }
}

/// POSTs `report` to the site with the device token.
pub async fn send(site_url: &str, token: &str, report: &HostReport) -> anyhow::Result<()> {
    reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()
        .context("HTTP client")?
        .post(format!("{}{PROFILE_PATH}", site_url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(report)
        .send()
        .await
        .context("could not reach the site")?
        .error_for_status()
        .map_err(|err| {
            anyhow::anyhow!(
                "the site refused it: HTTP {}",
                err.status().map_or(0, |s| s.as_u16())
            )
        })?;
    Ok(())
}

async fn load_config(overrides: &CliConfigOverrides) -> Option<Config> {
    let overrides = overrides.parse_overrides().ok()?;
    Config::load_with_cli_overrides(overrides).await.ok()
}

/// After a sign-in or when remote control starts: collect and send, then tell the user. Never
/// fails; does nothing when opted out or not signed in to a site.
pub async fn report_for_saved_account(
    codex_home: &Path,
    overrides: &CliConfigOverrides,
    no_host_info: bool,
) {
    if opted_out(no_host_info) {
        return;
    }
    let Ok(Some(account)) = SiteAccount::load(codex_home) else {
        return;
    };
    let Ok(token) = std::fs::read_to_string(SiteAccount::token_path(codex_home)) else {
        return;
    };
    let config = load_config(overrides).await;
    let report = collect(config.as_ref()).await;
    match send(&account.site_url, token.trim(), &report).await {
        Ok(()) => eprintln!(
            "Reported a host configuration summary to {} (OS, hardware, tool versions, agent \
             settings; no secrets or file contents). Turn this off with --no-host-info or {HOST_INFO_ENV_VAR}=0.",
            account.site_url
        ),
        Err(err) => eprintln!("Warning: could not report the host configuration summary: {err:#}"),
    }
}

#[cfg(test)]
#[path = "host_info_tests.rs"]
mod tests;
