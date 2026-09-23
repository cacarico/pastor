//! Speaks the herdr socket protocol on stdin/stdout. Used by transport tests and for
//! running pastor end to end without a real herdr: put
//! `command = ["target/debug/fake-herdr"]` on a flock machine.
use pastor::herdr::{AgentStatus, fake::FakeHerdr};

#[tokio::main]
async fn main() {
    let fake = FakeHerdr::new();
    if let Some(ms) = std::env::var("FAKE_HERDR_AUTO_DONE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let watcher = fake.clone();
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                for a in watcher.agents() {
                    if a.agent_status == AgentStatus::Working
                        && seen.insert((a.pane_id.clone(), a.state_change_seq))
                    {
                        let w = watcher.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                            w.set_status(
                                &a.pane_id,
                                AgentStatus::Idle,
                                Some(a.completion_seq.unwrap_or(0) + 1),
                            );
                        });
                    }
                }
            }
        });
    }
    fake.serve(Box::new(tokio::io::stdin()), Box::new(tokio::io::stdout()))
        .await;
}
