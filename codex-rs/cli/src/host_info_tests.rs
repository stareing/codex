use super::*;
use pretty_assertions::assert_eq;

#[test]
fn version_is_extracted_from_common_version_lines() {
    let cases = [
        ("git version 2.47.1\n", Some("2.47.1")),
        ("v20.11.0\n", Some("20.11.0")),
        ("Python 3.12.1", Some("3.12.1")),
        ("go version go1.22.3 linux/amd64", Some("1.22.3")),
        ("cargo 1.95.0 (abc123 2026-01-01)", Some("1.95.0")),
        ("Docker version 27.3.1, build ce12230", Some("27.3.1")),
        (
            "pip 24.0 from /usr/lib/python3.12/site-packages/pip (python 3.12)",
            Some("24.0"),
        ),
        (
            "gcc (Ubuntu 13.2.0-23ubuntu4) 13.2.0",
            Some("13.2.0-23ubuntu4"),
        ),
        ("\n\nGNU Make 4.3\nBuilt for x86_64", Some("4.3")),
        ("openjdk 21.0.2 2024-01-16", Some("21.0.2")),
        ("no digits here", None),
        ("build 7", None),
        ("", None),
    ];
    for (input, expected) in cases {
        assert_eq!(extract_version(input).as_deref(), expected, "{input:?}");
    }
    let long = format!("tool {}", "1.".repeat(40));
    let version = extract_version(&long).unwrap();
    assert!(version.len() <= 48 && valid_version(&version));
}

#[test]
fn version_patterns_compile() {
    assert!(VERSION_IN_TEXT.is_some() && VALID_VERSION.is_some());
}

#[test]
fn env_opt_out_values() {
    for value in ["0", "false", "OFF", " no "] {
        assert!(env_opts_out(Some(value)), "{value}");
    }
    for value in [None, Some("1"), Some(""), Some("yes")] {
        assert!(!env_opts_out(value), "{value:?}");
    }
    assert!(opted_out(true));
}

#[test]
fn shells_map_to_the_accepted_names() {
    assert_eq!(shell_name("/bin/bash"), Some("bash"));
    assert_eq!(shell_name("/usr/bin/zsh"), Some("zsh"));
    assert_eq!(shell_name("/bin/dash"), Some("sh"));
    assert_eq!(
        shell_name(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        Some("pwsh")
    );
    assert_eq!(shell_name("/opt/homebrew/bin/nu"), Some("nu"));
    assert_eq!(shell_name("/usr/bin/tcsh"), None);
}

#[test]
fn linux_sources_are_parsed() {
    let cpuinfo =
        "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Core(TM) i7  CPU\n";
    assert_eq!(
        parse_cpuinfo_model(cpuinfo).as_deref(),
        Some("Intel(R) Core(TM) i7 CPU")
    );
    assert_eq!(
        parse_meminfo_total_mb("MemTotal:       16384000 kB\nMemFree: 1 kB\n"),
        Some(16000)
    );
    let lspci = "00:02.0 \"VGA compatible controller\" \"Intel Corporation\" \"UHD Graphics 770\" -r0c \"\" \"\"\n\
                 01:00.0 \"3D controller\" \"NVIDIA Corporation\" \"AD104 [GeForce RTX 4070]\" -ra1 \"\" \"\"\n\
                 00:1f.3 \"Audio device\" \"Intel Corporation\" \"Sound\" \"\" \"\"\n";
    assert_eq!(
        parse_lspci_gpus(lspci),
        vec![
            "Intel Corporation UHD Graphics 770".to_string(),
            "NVIDIA Corporation AD104 [GeForce RTX 4070]".to_string(),
        ]
    );
}

#[test]
fn macos_and_windows_probes_are_parsed() {
    let json =
        r#"{"SPDisplaysDataType":[{"sppci_model":"Apple M3 Pro","spdisplays_ndrvs":[{},{}]}]}"#;
    assert_eq!(
        parse_macos_displays(json),
        (vec!["Apple M3 Pro".to_string()], Some(2))
    );
    assert_eq!(parse_macos_displays("not json"), (Vec::new(), None));

    let one = parse_windows_probe(
        r#"{"cpu":"AMD Ryzen 9","memory":34359738368,"gpus":"NVIDIA GeForce RTX 4090","displays":1,"disk_total":1000,"disk_free":10}"#,
    )
    .unwrap();
    assert_eq!(one.gpus, vec!["NVIDIA GeForce RTX 4090".to_string()]);
    assert_eq!(one.memory, Some(34359738368));
    let many = parse_windows_probe(r#"{"cpu":null,"gpus":["A",null,"B"],"displays":0}"#).unwrap();
    assert_eq!(many.gpus, vec!["A".to_string(), "B".to_string()]);
    let none = parse_windows_probe(r#"{"gpus":null}"#).unwrap();
    assert_eq!(none.gpus, Vec::<String>::new());
    assert_eq!(drive_letter(Path::new(r"d:\code")), Some('D'));
    assert_eq!(drive_letter(Path::new(r"\\?\C:\x")), Some('C'));
    assert_eq!(drive_letter(Path::new(r"\\server\share")), None);
}

#[test]
fn clean_text_drops_controls_and_caps_length() {
    assert_eq!(clean_text(" a\tb\u{7}c \n").as_deref(), Some("a b c"));
    assert_eq!(clean_text("  "), None);
    assert_eq!(clean_text(&"x".repeat(500)).unwrap().len(), MAX_TEXT_CHARS);
}

/// The JSON keys are the site's contract; nothing else may appear.
#[tokio::test]
async fn report_has_exactly_the_contract_shape() {
    let report = collect(None).await;
    let value = serde_json::to_value(&report).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "agent",
            "client",
            "environment",
            "gpus",
            "hardware",
            "mcp_servers",
            "os",
            "tools"
        ]
    );
    let allowed = |object: &serde_json::Value, names: &[&str]| {
        for key in object.as_object().unwrap().keys() {
            assert!(names.contains(&key.as_str()), "unexpected key {key}");
        }
    };
    allowed(
        &value["os"],
        &["family", "name", "version", "arch", "kernel"],
    );
    allowed(
        &value["hardware"],
        &[
            "cpu",
            "cpu_cores",
            "memory_mb",
            "disk_total_mb",
            "disk_free_mb",
            "displays",
        ],
    );
    allowed(&value["environment"], &["shell", "locale", "timezone"]);
    allowed(
        &value["agent"],
        &[
            "model",
            "model_provider",
            "approval_policy",
            "sandbox_mode",
            "reasoning_effort",
        ],
    );
    assert_eq!(value["client"]["kind"], "cli");
    assert!(["linux", "macos", "windows"].contains(&value["os"]["family"].as_str().unwrap()));
    assert!(value["gpus"].as_array().unwrap().len() <= MAX_GPUS);
    for (tool, version) in &report.tools {
        assert!(TOOLS.contains(&tool.as_str()), "{tool}");
        assert!(valid_version(version), "{tool} {version}");
    }
}

#[tokio::test]
async fn report_is_posted_with_the_device_token_and_skipped_when_opted_out() {
    let site = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let site_url = format!(
        "http://127.0.0.1:{}",
        site.server_addr().to_ip().unwrap().port()
    );
    let home = tempfile::TempDir::new().unwrap();
    SiteAccount {
        site_url: site_url.clone(),
        relay_url: "https://onigio.com/agent-relay/".into(),
        account_id: "u7".into(),
        device_id: 3,
        name: "pc".into(),
        model: None,
    }
    .save(home.path(), "oad_tok")
    .unwrap();

    // Opted out: nothing is sent.
    report_for_saved_account(home.path(), &CliConfigOverrides::default(), true).await;
    assert!(
        site.recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none()
    );

    let server = std::thread::spawn(move || {
        let mut request = site.recv().unwrap();
        let path = request.url().to_string();
        let auth = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .map(|h| h.value.to_string());
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        request
            .respond(tiny_http::Response::from_string("{}").with_status_code(200))
            .unwrap();
        (path, auth, body)
    });
    let report = collect(None).await;
    send(&site_url, "oad_tok", &report).await.unwrap();
    let (path, auth, body) = server.join().unwrap();
    assert_eq!(path, "/api/agent-devices/self/profile/");
    assert_eq!(auth.as_deref(), Some("Bearer oad_tok"));
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["client"]["kind"], "cli");
    assert!(!body.to_string().contains("oad_tok"));
}
