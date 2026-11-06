//! `.github/workflows/dependabot-close.yml` closes Dependabot's pull requests
//! in the public repo: its updates are merged in the private copy and arrive
//! with the sync. It runs on `pull_request_target`, so what it may do is
//! pinned down here: only in cacarico/pastor, only for Dependabot's pull
//! requests, with a token that can touch pull requests and nothing else, and
//! never with the pull request's code checked out.
use std::path::Path;

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/dependabot-close.yml");
    std::fs::read_to_string(path).unwrap()
}

/// Lines that are not comments or blank, so the prose at the top can name
/// what the workflow must not do.
fn code(wf: &str) -> Vec<&str> {
    wf.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn runs_on_pull_request_target_opened_and_reopened_only() {
    let wf = workflow();
    let code = code(&wf).join("\n");
    assert!(
        code.contains("on:\n  pull_request_target:\n    types: [opened, reopened]\n"),
        "{code}"
    );
    for other in ["push:", "pull_request:", "workflow_dispatch:", "schedule:"] {
        assert!(!code.contains(&format!("\n  {other}")), "{other} in {code}");
    }
}

#[test]
fn only_in_the_public_repo_and_only_for_dependabot() {
    let wf = workflow();
    let ifs: Vec<&str> = code(&wf)
        .into_iter()
        .filter(|l| l.trim_start().starts_with("if:"))
        .collect();
    assert_eq!(ifs.len(), 1, "{ifs:?}");
    let gate = ifs[0];
    assert!(
        gate.contains("github.repository == 'cacarico/pastor'"),
        "{gate}"
    );
    assert!(
        gate.contains("github.event.pull_request.user.login == 'dependabot[bot]'"),
        "{gate}"
    );
    assert!(gate.contains("&&") && !gate.contains("||"), "{gate}");
}

#[test]
fn token_can_write_pull_requests_and_nothing_else() {
    let wf = workflow();
    let code = code(&wf);
    let starts: Vec<usize> = code
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with("permissions:"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(starts.len(), 1, "one permissions block: {code:?}");
    let i = starts[0];
    assert_eq!(code[i], "permissions:", "top level, not inline");
    assert_eq!(code[i + 1], "  pull-requests: write");
    assert!(
        !code[i + 2].starts_with("  "),
        "only pull-requests: {code:?}"
    );
}

#[test]
fn never_checks_out_or_runs_the_pull_requests_code() {
    let wf = workflow();
    let code = code(&wf).join("\n");
    assert!(!code.contains("uses:"), "no actions at all: {code}");
    assert!(
        !code.contains("head.sha") && !code.contains("head.ref"),
        "{code}"
    );
    assert!(!code.contains("secrets."), "{code}");
}

#[test]
fn closes_the_pull_request_with_a_comment_about_the_sync() {
    let wf = workflow();
    let code = code(&wf).join("\n");
    assert!(code.contains("gh pr close \"$PR_URL\" --comment"), "{code}");
    assert!(code.contains("sync"), "{code}");
    assert!(
        code.contains("PR_URL: ${{ github.event.pull_request.html_url }}"),
        "the URL goes in through env, never spliced into the script: {code}"
    );
}
