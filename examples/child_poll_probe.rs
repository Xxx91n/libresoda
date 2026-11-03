//! 票 #28 计时探针：外部签名器的快路径上，走执行器比裸 spawn 贵多少。
//!
//! 同一条最小签名器各跑两遍：一遍走 `CommandSignature::sign`（写 + 排空 + 有上限等待，
//! `src/soda/child.rs`），一遍走裸 `spawn + write_all + wait_with_output`（票 #23 之前库本来的
//! 做法，也是这条快路径的下界）。两列之差 = 执行器整条路的开销：三次工作线程的 spawn、
//! 库里 `sign()` 自己那次 JSON 编码，以及轮询粒度的那半拍 —— 跟 #23 复核留下的那个差值同形
//! （那两条数也各自含 spawn 与编码），所以能对照着看；但它不是「轮询粒度单独占多少」，
//! 那一格要读 `POLL_INTERVAL_*` 的退避序列。
//!
//! **只记录，不判分**：这张表里不设阈值、不判成败，只让读的人自己看量级。口径与「为什么不做成
//! 门禁」记在 `docs/PORTING.md`「计时凭证」一条；`.github/workflows/ci.yml` 在 Test 之后跑一次。
//!
//! ```text
//! CHILD_POLL_ROUNDS  可选；计时轮数，缺省 100（非法按 100，实际取 1..=2000）
//! ```
//!
//! 跑法（离线、不碰网络；签名器是 `sh` + `cat`）：
//!
//! ```text
//! cargo run --example child_poll_probe
//! ```

use libresoda::soda::signature::{
    sign_request, CommandSignature, SignRequest, SignResponse, SignatureProvider,
};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

/// 最小签名器：按契约先把 stdin 读干净，再交出一份合法回包。
///
/// 它自己只要几毫秒，正好落在轮询最敏感的那段区间里。
const SIGNER_SCRIPT: &str = "cat >/dev/null; printf '{\"msToken\":\"x\",\"aBogus\":\"y\"}'";

/// 不计入统计的预热轮数：头几趟 spawn 摊上了冷启动成本。
const WARM_UP_ROUNDS: usize = 5;

/// 默认计时轮数。
const DEFAULT_ROUNDS: usize = 100;

/// 轮数上限：`CHILD_POLL_ROUNDS` 是人给的数，不能直接拿去当容量用（AGENTS.md 内存安全铁律 1）；
/// 上限同时保证两列跑完仍落在 CI 那步 `timeout 300` 之内。
const MAX_ROUNDS: usize = 2_000;

/// 一组样本的分位（单位：微秒）。
struct Stats {
    min: u64,
    p50: u64,
    p90: u64,
    mean: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rounds = std::env::var("CHILD_POLL_ROUNDS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_ROUNDS)
        .clamp(1, MAX_ROUNDS);

    let request = sign_request("https://api.qishui.com/x", "POST", &"x".repeat(512));
    // 两条路径喂同一份字节。注意库里 `sign()` 内部还会再编码一次（signature.rs），那一次算在
    // 差值里 —— 这张表量的是「走执行器一共贵多少」，不是轮询粒度单独贵多少。
    let payload = serde_json::to_vec(&request)?;
    let provider = CommandSignature::new("sh").args(["-c", SIGNER_SCRIPT]);

    for _ in 0..WARM_UP_ROUNDS {
        sign_through_library(&provider, &request)?;
        sign_through_bare(&payload)?;
    }
    let mut library = Vec::with_capacity(rounds);
    let mut bare = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        // 同一趟里 A/B 交替：机器漂移不会只砸中其中一列。
        library.push(sign_through_library(&provider, &request)?);
        bare.push(sign_through_bare(&payload)?);
    }
    library.sort_unstable();
    bare.sort_unstable();
    let library = stats(&library);
    let bare = stats(&bare);

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let bytes = payload.len();
    println!("platform: {os} {arch} | rounds: {rounds} | payload: {bytes} bytes");
    println!("{:<34}{:>10}{:>10}{:>10}{:>10}", "path (ms)", "min", "p50", "p90", "mean");
    print_row("CommandSignature::sign", &library);
    print_row("bare spawn+wait_with_output", &bare);
    let diff = (library.p50 as f64 - bare.p50 as f64) / 1_000.0;
    println!("p50 diff (library - bare): {diff:+.3} ms");
    println!("记录用：数字随机器变，这里不设阈值、不判成败。");
    Ok(())
}

/// 走库：`CommandSignature::sign`，也就是票 #23 那套「写 + 排空 + 有上限轮询」。
fn sign_through_library(
    provider: &CommandSignature,
    request: &SignRequest,
) -> Result<u64, Box<dyn std::error::Error>> {
    let started = Instant::now();
    provider
        .sign(request)
        .map_err(|err| format!("library sign: {err}"))?;
    Ok(elapsed_us(started))
}

/// 走裸路径：与票 #23 之前的实现同形 —— spawn、写满 stdin、关掉写端、`wait_with_output`。
fn sign_through_bare(payload: &[u8]) -> Result<u64, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut child = Command::new("sh")
        .args(["-c", SIGNER_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(payload)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    let status = output.status;
    if !status.success() {
        return Err(format!("probe signer exited with {status}").into());
    }
    let parsed: SignResponse = serde_json::from_slice(&output.stdout)?;
    if parsed.is_empty() {
        return Err("probe signer answered with an empty signature".into());
    }
    Ok(elapsed_us(started))
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros() as u64
}

/// 对已排序的样本取分位：p50 取中间那格，p90 取秩 `0.9 * last` 向下取整的那格。
fn stats(sorted: &[u64]) -> Stats {
    let last = sorted.len() - 1;
    let total = sorted.iter().sum::<u64>();
    Stats {
        min: sorted[0],
        p50: sorted[last / 2],
        p90: sorted[last * 9 / 10],
        mean: total / sorted.len() as u64,
    }
}

fn print_row(label: &str, stats: &Stats) {
    let cells = [stats.min, stats.p50, stats.p90, stats.mean];
    let mut row = format!("{label:<34}");
    for value in cells {
        row.push_str(&format!("{:>10.3}", value as f64 / 1_000.0));
    }
    println!("{row}");
}
