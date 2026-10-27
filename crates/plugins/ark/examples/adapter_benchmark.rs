//! Reproducible HTTP adapter measurement. Run release `adapter_benchmark all BUNDLE`.
//! Each mode gets a fresh process and includes both client and provider guard scans.
#![allow(clippy::unwrap_used)]
#[path = "../tests/support/mod.rs"]
mod support;
use respan_gateway_plugin_ark::{Ark, Config};
use serde_json::{Value, json};
use std::{error::Error, path::Path, process::Command, sync::Arc, time::Instant};

const SHORT: &str = "Please summarize the meeting agenda and opening hours.";
fn long_input() -> String {
    "Ordinary technical documentation about library opening hours and meeting agendas. ".repeat(60)
}
fn rss_bytes() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|n| n * 1024)
}
fn percentiles(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |p: f64| {
        values[((values.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(values.len() - 1)]
    };
    json!({"samples":values.len(),"p50_ms":at(0.5),"p95_ms":at(0.95),"min_ms":values[0],"max_ms":values[values.len()-1],"sorted_samples_ms":values})
}
fn host(command: &str, args: &[&str]) -> Option<String> {
    let result = Command::new(command).args(args).output().ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_string())
}
fn peak_bytes(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        if cfg!(target_os = "macos") && line.contains("maximum resident set size") {
            return line.split_whitespace().next()?.parse().ok();
        }
        if line.contains("Maximum resident set size (kbytes)") {
            return line
                .rsplit(':')
                .next()?
                .trim()
                .parse::<u64>()
                .ok()
                .map(|n| n * 1024);
        }
    }
    None
}
fn measure_all(bundle: &str) -> Result<(), Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    let mut modes = serde_json::Map::new();
    for mode in ["baseline", "l1", "wolf"] {
        let output = Command::new("/usr/bin/time")
            .arg(if cfg!(target_os = "macos") {
                "-l"
            } else {
                "-v"
            })
            .arg(&exe)
            .arg(mode)
            .arg(bundle)
            .env("RAYON_NUM_THREADS", "1")
            .output()?;
        if !output.status.success() {
            return Err(
                format!("{mode} failed: {}", String::from_utf8_lossy(&output.stderr)).into(),
            );
        }
        let mut data: Value = serde_json::from_slice(&output.stdout)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        data["peak_rss_bytes"] = json!(peak_bytes(&stderr));
        data["peak_rss_source"] = json!(if cfg!(target_os = "macos") {
            "time -l maximum resident set size, bytes"
        } else {
            "time -v maximum resident set size, KiB converted to bytes"
        });
        modes.insert(mode.into(), data);
    }
    let counts = Command::new(&exe).arg("windows").arg(bundle).output()?;
    if !counts.status.success() {
        return Err(format!(
            "window count failed: {}",
            String::from_utf8_lossy(&counts.stderr)
        )
        .into());
    }
    let counts: Value = serde_json::from_slice(&counts.stdout)?;
    let manifest: Option<Value> =
        std::fs::read_to_string(Path::new(bundle).join("wolf.manifest.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
    let revision_file = std::fs::read_to_string(Path::new(bundle).join(".patronus-revision")).ok();
    let model_path = manifest
        .as_ref()
        .and_then(|m| m.get("model"))
        .and_then(|m| m.get("path"))
        .and_then(Value::as_str)
        .map(|path| Path::new(bundle).join(path))
        .or_else(|| {
            [
                "model_fp16.onnx",
                "onnx/onnx_fp16/model_fp16.onnx",
                "onnx/fp16/model_fp16.onnx",
            ]
            .into_iter()
            .map(|path| Path::new(bundle).join(path))
            .find(|path| path.is_file())
        });
    let model_file_bytes = model_path
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|m| m.len());
    let core_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../patronus-ark-standalone");
    let core_root = core_root.to_str().ok_or("invalid Core path")?;
    let source_state = json!({"adapter_commit":host("git",&["rev-parse","--short","HEAD"]),"adapter_dirty_files":host("git",&["status","--short"]),"core_commit":host("git",&["-C",core_root,"rev-parse","--short","HEAD"]),"core_dirty_files":host("git",&["-C",core_root,"status","--short"]),"rustc":host("rustc",&["--version"])});
    let model = json!({"bundle":bundle,"optional_manifest_present":manifest.is_some(),"model_file_bytes":model_file_bytes,"precision":"fp16","execution_provider":"cpu","decision_threshold":0.5,"manifest_revision":manifest.as_ref().and_then(|m|m.get("revision")),"asset_revision_file":revision_file,"model_blake3":manifest.as_ref().and_then(|m|m.get("model")).and_then(|m|m.get("blake3")),"tokenizer_blake3":manifest.as_ref().and_then(|m|m.get("tokenizer")).and_then(|m|m.get("blake3"))});

    let l1_delta = modes["l1"]["warm_rss_bytes"]
        .as_u64()
        .unwrap()
        .saturating_sub(modes["baseline"]["warm_rss_bytes"].as_u64().unwrap());
    let wolf_delta = modes["wolf"]["warm_rss_bytes"]
        .as_u64()
        .unwrap()
        .saturating_sub(modes["l1"]["warm_rss_bytes"].as_u64().unwrap());
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema_version":1,"host":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"kernel":host("uname",&["-r"]),"cpu":host("sysctl",&["-n","machdep.cpu.brand_string"]),"memory_bytes":host("sysctl",&["-n","hw.memsize"])},
            "profile":if cfg!(debug_assertions){"debug"}else{"release"},"provider":"mockllm FakeOpenAi over loopback HTTP","binary_wolf_feature":cfg!(feature="wolf"),
            "configured_active_scans":1,"ort_batch_size":1,"ort_intra_op_threads":1,"ort_inter_op_threads":1,"rayon_threads":1,"tokio_worker_threads":2,
            "model":model,"native_categories":["pii","dlp","injection"],"source_state":source_state,"input_windows":counts,"modes":modes,"approximate_memory_deltas":{"native_over_baseline_warm_bytes":l1_delta,"wolf_over_native_warm_bytes":wolf_delta},
            "notes":["Fresh process per mode; same release binary, optional ORT code linked in all modes.","Incoming request uses in-process respan_gateway_http::handle; the provider hop uses real loopback TCP/HTTP.","Each guarded request scans at client and provider edges, so Wolf windows run twice.","RSS deltas include allocators, tokenizer, sessions and model execution; they are not pure weight sizes.","HTTP timing includes JSON, scanner work, mockllm request/reply and complete response consumption.","p95 is nearest rank; long samples are deliberately small and do not establish a service SLO."]
        }))?
    );
    Ok(())
}
#[cfg(feature = "wolf")]
fn window_counts(bundle: &Path) -> Result<(), Box<dyn Error>> {
    use patronus_ark_core::ml::mmbert_tokenizer::MmbertPairTokenizer;
    let tokenizer = MmbertPairTokenizer::from_file(&bundle.join("tokenizer.mmbpe"))?;
    let count = |text: &str| -> Result<Value, Box<dyn Error>> {
        let mut tokens = 0usize;
        tokenizer
            .visit_tokens(text, |_| {
                tokens += 1;
                Ok(())
            })
            .map_err(std::io::Error::other)?;
        Ok(
            json!({"bytes":text.len(),"content_tokens":tokens,"windows_per_field_per_scan":tokens.div_ceil(patronus_ark_core::wolf::CONTENT_TOKENS).max(1),"content_window_tokens":patronus_ark_core::wolf::CONTENT_TOKENS,"overlap":0}),
        )
    };
    println!(
        "{}",
        json!({"short":count(SHORT)?,"long":count(&long_input())?})
    );
    Ok(())
}
#[cfg(not(feature = "wolf"))]
fn window_counts(_: &Path) -> Result<(), Box<dyn Error>> {
    Err("compile with --features wolf for model window counts".into())
}

async fn measure_mode(mode: &str, bundle: &Path) -> Result<(), Box<dyn Error>> {
    let baseline = rss_bytes();
    let gateway_start = Instant::now();
    let fake = mockllm::FakeOpenAi::start().await;
    let start = Instant::now();
    let guard = if mode == "baseline" {
        None
    } else {
        #[allow(unused_mut)]
        let mut config = Config {
            queue_timeout_ms: 90_000,
            scan_timeout_ms: 90_000,
            ..Config::default()
        };
        if mode == "wolf" {
            #[cfg(feature = "wolf")]
            {
                let mut wolf = patronus_ark_core::wolf::WolfConfig::new(bundle, 0.5);
                wolf.backend = patronus_ark_core::ExecutionBackend::Cpu;
                wolf.precision = patronus_ark_core::wolf::WolfPrecision::Fp16;
                config.scanner.wolf = Some(wolf);
            }
            #[cfg(not(feature = "wolf"))]
            {
                let _ = bundle;
                return Err("wolf feature required".into());
            }
        }
        Some(Ark::new(config).map_err(|e| format!("scanner startup failed: {e}"))?)
    };
    let scanner_startup_ms = start.elapsed().as_secs_f64() * 1000.0;
    let gateway = Arc::new(support::gateway(&fake.base_url(), guard.as_ref()));
    let gateway_startup_ms = gateway_start.elapsed().as_secs_f64() * 1000.0;
    let initialized = rss_bytes();
    let long = long_input();
    for text in [SHORT, long.as_str()] {
        fake.answer(mockllm::Answer::text("ok"));
        let (status, body) = support::send(&gateway, support::text_body(text)).await;
        if status != 200 {
            return Err(format!("warmup refused {status}: {body}").into());
        }
    }
    let warm = rss_bytes();
    let mut timings = serde_json::Map::new();
    for (label, text, samples) in [("short", SHORT, 12), ("long", long.as_str(), 3)] {
        let mut times = Vec::new();
        for _ in 0..samples {
            fake.answer(mockllm::Answer::text("ok"));
            let start = Instant::now();
            let (status, body) = support::send(&gateway, support::text_body(text)).await;
            if status != 200 {
                return Err(format!("measured request refused {status}: {body}").into());
            }
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        timings.insert(label.into(), percentiles(times));
    }
    let parallel_start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        fake.answer(mockllm::Answer::text("ok"));
        let gateway = Arc::clone(&gateway);
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let (status, body) = support::send(&gateway, support::text_body(SHORT)).await;
            (status, body, start.elapsed().as_secs_f64() * 1000.0)
        }));
    }
    let mut parallel = Vec::new();
    for handle in handles {
        let (status, body, ms) = handle.await?;
        if status != 200 {
            return Err(format!("parallel request refused {status}: {body}").into());
        }
        parallel.push(ms);
    }
    let parallel_elapsed_ms = parallel_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        json!({"mode":mode,"process_id":std::process::id(),"scanner_startup_ms":scanner_startup_ms,"gateway_startup_ms":gateway_startup_ms,"initial_rss_bytes":baseline,"initialized_rss_bytes":initialized,"warm_rss_bytes":warm,"final_rss_bytes":rss_bytes(),"timings":timings,"parallel_short":{"clients":4,"elapsed_ms":parallel_elapsed_ms,"latency":percentiles(parallel)},"provider_requests":fake.requests().len(),"guard_scan_passes_per_request":if mode=="baseline"{0}else{2}})
    );
    Ok(())
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: adapter_benchmark {all|baseline|l1|wolf|windows} LOCAL_BUNDLE")?;
    let bundle = args.next().unwrap_or_default();
    match mode.as_str() {
        "all" => measure_all(&bundle),
        "windows" => window_counts(Path::new(&bundle)),
        "baseline" | "l1" | "wolf" => measure_mode(&mode, Path::new(&bundle)).await,
        _ => Err("unknown benchmark mode".into()),
    }
}
