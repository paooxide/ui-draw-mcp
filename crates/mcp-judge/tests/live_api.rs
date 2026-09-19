//! The real service, when a key is present. Skips otherwise: this costs
//! money and needs `TYPESAFE_API_KEY`, so it cannot gate a pull request.
//!
//! What it pins is the contract the recorded fixtures assume: the request
//! shape is accepted, the three answer types come back in the documented
//! shape, and an obvious judgment lands on the obvious side.

use std::collections::BTreeMap;
use std::path::Path;

use mcp_judge::{api_key, Judge, JudgeConfig, Question};
use serde_json::json;

fn live_judge() -> Option<Judge> {
    if std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0") {
        return None;
    }
    let key = api_key(Path::new("/nonexistent")).ok()?;
    let cfg = JudgeConfig {
        enabled: true,
        ..JudgeConfig::default()
    };
    Some(Judge::with_transport(
        cfg,
        Some(key),
        Box::new(mcp_judge::CurlTransport),
    ))
}

#[tokio::test]
async fn the_documented_shapes_hold_against_the_real_service() {
    let Some(j) = live_judge() else {
        eprintln!("skipping: set TYPESAFE_API_KEY (and leave AGENTCTL_SKIP_LIVE unset) to run");
        return;
    };
    // A Noul with an obvious answer each way.
    let p = j
        .noul(
            json!({ "text": "rm -rf / --no-preserve-root" }),
            "Would running `text` in a shell delete data?",
            "yes, it deletes data",
            "no, it is harmless",
        )
        .await
        .expect("a noul answer");
    assert!(p > 0.8, "expected a confident yes, got {p}");
    let p = j
        .noul(
            json!({ "text": "ls -la ~/Documents" }),
            "Would running `text` in a shell delete data?",
            "yes, it deletes data",
            "no, it is harmless",
        )
        .await
        .expect("a noul answer");
    assert!(p < 0.3, "expected a confident no, got {p}");

    // A ranking over candidates: the obvious one wins and any_fits is high.
    let mut c = BTreeMap::new();
    c.insert("@e1".to_string(), "button named \"Cancel\"".to_string());
    c.insert("@e2".to_string(), "button named \"Save\"".to_string());
    c.insert(
        "@e3".to_string(),
        "textfield named \"File name\"".to_string(),
    );
    let r = j
        .rank(
            json!({ "request": "the button that saves the document", "candidates": c }),
            "Which candidate is the element `request` describes?",
            &c,
        )
        .await
        .expect("a ranking");
    assert_eq!(r.choice, "@e2", "{r:?}");
    assert!(r.probabilities["@e2"] > 0.5);
    assert!(r.any_fits > 0.5);

    // A Score comes back in shape.
    let mut qs = BTreeMap::new();
    qs.insert(
        "s".to_string(),
        Question::Score {
            instructions: "How urgent is this message?".into(),
            criteria: vec![
                "not urgent".into(),
                "somewhat urgent".into(),
                "an emergency".into(),
            ],
        },
    );
    let a = j
        .ask(json!({ "message": "the building is on fire" }), qs)
        .await
        .expect("answers");
    match &a.answers["s"] {
        mcp_judge::Answer::Score { score, .. } => assert!(*score > 1.0, "{score}"),
        other => panic!("{other:?}"),
    }
    assert!(a.usage.input_tokens > 0);
}
