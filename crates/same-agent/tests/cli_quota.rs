//! Reads the usage limits from the real `claude` and `codex` CLIs. They need
//! logged-in CLIs (with a plan, not an API key), so they are ignored by
//! default: `cargo test -p same-agent --test cli_quota -- --ignored`
//! (no tokens are spent: no turn is run).

use same_agent::{AgentManager, ExecPreference};
use same_core::config::default_providers;
use same_core::{EventBus, QuotaReport};

async fn fetch(id: &str) -> QuotaReport {
    let provider = default_providers()
        .into_iter()
        .find(|p| p.id == id)
        .expect("default provider");
    let manager = AgentManager::new(EventBus::default(), tokio::runtime::Handle::current());
    let cwd = std::env::temp_dir();
    let (_target, res) = manager
        .fetch_quota(&provider, ExecPreference::Auto, &cwd)
        .await;
    res.expect("fetch quota")
}

fn assert_plan_limits(report: QuotaReport) {
    let QuotaReport::Available(snap) = report else {
        panic!("expected plan limits, got {report:?}");
    };
    assert!(snap.plan.is_some(), "{snap:?}");
    assert!(!snap.windows.is_empty(), "{snap:?}");
    for w in &snap.windows {
        assert!((0.0..=100.0).contains(&w.used_percent), "{w:?}");
    }
    eprintln!("{snap:#?}");
}

#[tokio::test]
#[ignore]
async fn claude_quota() {
    assert_plan_limits(fetch("claude").await);
}

#[tokio::test]
#[ignore]
async fn codex_quota() {
    assert_plan_limits(fetch("codex").await);
}

/// Token statistics from the real local history (Claude Code, Codex) and
/// `opencode stats`: `cargo test -p same-agent --test cli_quota -- --ignored stats`
async fn stats(id: &str, days: u32) -> same_core::TokenStats {
    let provider = default_providers()
        .into_iter()
        .find(|p| p.id == id)
        .expect("default provider");
    let manager = AgentManager::new(EventBus::default(), tokio::runtime::Handle::current());
    let started = std::time::Instant::now();
    let (_target, res) = manager
        .fetch_stats(&provider, ExecPreference::Auto, &std::env::temp_dir(), days)
        .await;
    let same_core::StatsReport::Available(stats) = res.expect("fetch stats") else {
        panic!("expected stats");
    };
    assert_eq!(stats.days.len(), days as usize);
    let day_sum: u64 = stats.days.iter().map(|(_, c)| c.total()).sum();
    assert_eq!(day_sum, stats.total.total());
    eprintln!(
        "{id} {days}d in {:?}: total {:?}, top models {:?}, cost {:?}",
        started.elapsed(),
        stats.total,
        stats.models.iter().take(3).collect::<Vec<_>>(),
        stats.cost_usd
    );
    stats
}

#[tokio::test]
#[ignore]
async fn stats_claude() {
    stats("claude", 30).await;
}

#[tokio::test]
#[ignore]
async fn stats_codex() {
    stats("codex", 30).await;
}

#[tokio::test]
#[ignore]
async fn stats_opencode() {
    let s = stats("opencode", 7).await;
    let model_sum: u64 = s.models.iter().map(|(_, c)| c.total()).sum();
    assert_eq!(model_sum, s.total.total());
}
