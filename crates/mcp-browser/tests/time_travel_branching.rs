//! Tests for Phase 4: Deterministic Time-Travel & Branching Execution
//! Task 4.1: Ephemeral Browser Context Forking & Speculative Branching
//! Task 4.2: Instant State Checkpointing & Rollback ($T_{-1}$)

mod common;

use mcp_browser::{BrowserBackend, CdpBackend, NavPolicy, CHROME_BINS};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn skip_live() -> bool {
    std::env::var_os("AGENTCTL_SKIP_LIVE").is_some_and(|v| v != "0")
}

fn have_chrome() -> bool {
    !skip_live() && CHROME_BINS.iter().any(|p| std::path::Path::new(p).exists())
}

async fn tab() -> Option<(CdpBackend, String)> {
    if !have_chrome() {
        return None;
    }
    let b = CdpBackend::new(NavPolicy::new(&[], true));
    b.connect(None, Some(json!({ "headless": true, "port": 0 })))
        .await
        .ok()?;
    let tabs = b.tabs(1, "list", None, None).await.ok()?;
    let target = tabs
        .get("tabs")?
        .as_array()?
        .first()?
        .get("target_id")?
        .as_str()?
        .to_string();
    Some((b, target))
}

async fn serve_html(content: String) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                res = listener.accept() => {
                    if let Ok((mut stream, _)) = res {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            content.len(),
                            content
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.flush().await;
                    }
                }
            }
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), tx)
}

async fn eval_js(b: &CdpBackend, target: &str, expr: &str) -> Value {
    let res = b.eval(target, expr).await.expect("eval failed");
    res.get("result").cloned().unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_speculative_branch_isolation_and_commit() {
    let Some((b, parent_tab)) = tab().await else {
        return;
    };

    let html = r#"<!DOCTYPE html>
<html>
<head><title>Speculative Branching Test</title></head>
<body>
  <h1>Order Checkout</h1>
  <input id="customer_name" value="Alice">
  <input id="discount_code" value="NONE">
  <div id="status">pending</div>
  <script>
    localStorage.setItem('cart_status', 'initial_parent');
  </script>
</body>
</html>"#;
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&parent_tab, "goto", Some(&url))
        .await
        .expect("navigate failed");
    common::wait_until(
        "the page script to set its storage",
        std::time::Duration::from_secs(5),
        || async {
            b.eval(&parent_tab, "localStorage.getItem('cart_status')")
                .await
                .map(|v| v["result"] == "initial_parent")
                .unwrap_or(false)
        },
    )
    .await;

    // 1. Fork context into speculative branch "attempt_promo"
    let branch_res = b
        .branch_create(&parent_tab, "attempt_promo")
        .await
        .expect("branch_create failed");
    assert_eq!(branch_res.get("created"), Some(&json!(true)));
    assert_eq!(branch_res.get("branch_id"), Some(&json!("attempt_promo")));
    let branch_tab = branch_res
        .get("branch_target_id")
        .and_then(Value::as_str)
        .expect("branch_target_id")
        .to_string();

    // 2. Perform actions exclusively inside branch target
    b.eval(
        &branch_tab,
        r#"
        document.getElementById('customer_name').value = 'Alice Cooper';
        document.getElementById('discount_code').value = 'PROMO50';
        document.getElementById('status').innerText = 'discount_applied';
        localStorage.setItem('cart_status', 'branch_promo_applied');
        "#,
    )
    .await
    .expect("eval in branch failed");

    // 3. Verify State Isolation (AC 4.1.1): Parent tab remains completely unchanged!
    let parent_val = eval_js(
        &b,
        &parent_tab,
        "({ name: document.getElementById('customer_name').value, status: localStorage.getItem('cart_status') })",
    )
    .await;
    assert_eq!(
        parent_val.get("name"),
        Some(&json!("Alice")),
        "Parent input must remain untouched during speculative branch execution"
    );
    assert_eq!(
        parent_val.get("status"),
        Some(&json!("initial_parent")),
        "Parent storage must remain untouched during speculative branch execution"
    );

    // 4. Commit winning branch
    let commit_res = b
        .branch_commit("attempt_promo")
        .await
        .expect("branch_commit failed");
    assert_eq!(commit_res.get("committed"), Some(&json!(true)));

    // 5. Verify parent tab received the committed state
    let final_parent_val = eval_js(
        &b,
        &parent_tab,
        "({ status: localStorage.getItem('cart_status') })",
    )
    .await;
    assert_eq!(
        final_parent_val.get("status"),
        Some(&json!("branch_promo_applied")),
        "Parent tab should mirror branch state upon commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_speculative_branch_discard_and_teardown() {
    let Some((b, parent_tab)) = tab().await else {
        return;
    };

    let html = "<html><body><h1>Discard Test</h1></body></html>";
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&parent_tab, "goto", Some(&url))
        .await
        .expect("navigate failed");

    // 1. Create speculative branch
    let res = b
        .branch_create(&parent_tab, "failed_branch")
        .await
        .expect("branch_create failed");
    assert_eq!(res.get("created"), Some(&json!(true)));

    // 2. Branch should be visible in branch_list
    let list_res = b
        .branch_list(Some(&parent_tab))
        .await
        .expect("branch_list failed");
    let branches = list_res
        .get("branches")
        .and_then(Value::as_array)
        .expect("branches array");
    assert!(branches
        .iter()
        .any(|x| x.get("branch_id") == Some(&json!("failed_branch"))));

    // 3. Discard branch (AC 4.1.2: Clean Teardown)
    let discard_res = b
        .branch_discard("failed_branch")
        .await
        .expect("branch_discard failed");
    assert_eq!(discard_res.get("discarded"), Some(&json!(true)));

    // 4. Discarding already discarded branch should fail
    let second_discard = b.branch_discard("failed_branch").await;
    assert!(second_discard.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_speculative_branching_5_branches() {
    let Some((b, parent_tab)) = tab().await else {
        return;
    };

    let html = r#"<!DOCTYPE html>
<html>
<head><title>Multi-Branch Test</title></head>
<body>
  <h1>Search Flight Options</h1>
  <div id="choice">none</div>
</body>
</html>"#;
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&parent_tab, "goto", Some(&url))
        .await
        .expect("navigate failed");

    // Fork 5 concurrent speculative branches
    let mut branch_tabs = Vec::new();
    for i in 1..=5 {
        let b_id = format!("flight_branch_{i}");
        let res = b
            .branch_create(&parent_tab, &b_id)
            .await
            .expect("branch_create failed");
        let tid = res
            .get("branch_target_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        branch_tabs.push((b_id, tid));
    }

    assert_eq!(branch_tabs.len(), 5);

    // Mutate branch 5 with winning selection
    let (win_id, win_tab) = &branch_tabs[4];
    b.eval(
        win_tab,
        r#"localStorage.setItem('selected_flight', 'FLIGHT_5_BEST_PRICE');"#,
    )
    .await
    .expect("eval failed");

    // Discard first 4 branches
    for (b_id, _) in &branch_tabs[0..4] {
        b.branch_discard(b_id).await.expect("discard failed");
    }

    // Commit winning branch 5
    b.branch_commit(win_id).await.expect("commit failed");

    // Verify parent tab has winning flight state
    let check = eval_js(&b, &parent_tab, "localStorage.getItem('selected_flight')").await;
    assert_eq!(check, json!("FLIGHT_5_BEST_PRICE"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_instant_checkpoint_save_and_fast_rollback() {
    let Some((b, target)) = tab().await else {
        return;
    };

    let html = r#"<!DOCTYPE html>
<html>
<head><title>Form State Recovery</title></head>
<body>
  <h1>Registration Form</h1>
  <form id="reg">
    <input type="text" id="fullname" name="fullname" value="Initial Name">
    <input type="email" id="email" name="email" value="test@example.com">
    <input type="checkbox" id="newsletter" name="newsletter" checked>
    <select id="country" name="country">
      <option value="US">United States</option>
      <option value="UK" selected>United Kingdom</option>
      <option value="CA">Canada</option>
    </select>
    <textarea id="notes" name="notes">Original Note</textarea>
  </form>
  <script>
    localStorage.setItem('user_session', 'sess_valid_123');
    sessionStorage.setItem('temp_stage', 'step_two');
  </script>
</body>
</html>"#;
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&target, "goto", Some(&url))
        .await
        .expect("navigate failed");
    common::wait_until(
        "the page script to set its storage",
        std::time::Duration::from_secs(5),
        || async {
            b.eval(&target, "localStorage.getItem('user_session')")
                .await
                .map(|v| v["result"] == "sess_valid_123")
                .unwrap_or(false)
        },
    )
    .await;

    // 1. Save checkpoint T-1 (Tag: "step_2_filled")
    let save_res = b
        .checkpoint_save(&target, Some("step_2_filled"))
        .await
        .expect("checkpoint_save failed");
    assert_eq!(save_res.get("saved"), Some(&json!(true)));
    assert_eq!(save_res.get("tag"), Some(&json!("step_2_filled")));
    let inputs_captured = save_res
        .get("inputs_captured")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        inputs_captured >= 5,
        "Should capture at least 5 form elements"
    );

    // 2. Corrupt / Mutate form state with invalid data causing failure
    b.eval(
        &target,
        r#"
        document.getElementById('fullname').value = 'Corrupted Name';
        document.getElementById('email').value = 'invalid-email';
        document.getElementById('newsletter').checked = false;
        document.getElementById('country').selectedIndex = 0;
        document.getElementById('notes').value = 'Wiped notes';
        localStorage.setItem('user_session', 'broken_token');
        sessionStorage.clear();
        "#,
    )
    .await
    .expect("eval corruption failed");

    // Verify form was actually corrupted
    let corrupt_name = eval_js(&b, &target, "document.getElementById('fullname').value").await;
    assert_eq!(corrupt_name, json!("Corrupted Name"));

    // 3. Rollback to checkpoint ($T_{-1}$) and verify sub-300ms latency (AC 4.2.1)
    let start_rb = std::time::Instant::now();
    let rb_res = b
        .checkpoint_rollback(&target, Some("step_2_filled"))
        .await
        .expect("checkpoint_rollback failed");
    let rb_elapsed_ms = start_rb.elapsed().as_millis();

    assert_eq!(rb_res.get("rolled_back"), Some(&json!(true)));
    assert!(
        rb_elapsed_ms < 300,
        "Rollback latency should be < 300ms (took {}ms)",
        rb_elapsed_ms
    );

    // 4. Verify Form State Recovery (AC 4.2.2)
    let restored_state = eval_js(
        &b,
        &target,
        r#"({
            fullname: document.getElementById('fullname').value,
            email: document.getElementById('email').value,
            newsletter: document.getElementById('newsletter').checked,
            country: document.getElementById('country').value,
            notes: document.getElementById('notes').value,
            session: localStorage.getItem('user_session'),
            temp: sessionStorage.getItem('temp_stage')
        })"#,
    )
    .await;

    assert_eq!(restored_state.get("fullname"), Some(&json!("Initial Name")));
    assert_eq!(
        restored_state.get("email"),
        Some(&json!("test@example.com"))
    );
    assert_eq!(restored_state.get("newsletter"), Some(&json!(true)));
    assert_eq!(restored_state.get("country"), Some(&json!("UK")));
    assert_eq!(restored_state.get("notes"), Some(&json!("Original Note")));
    assert_eq!(
        restored_state.get("session"),
        Some(&json!("sess_valid_123"))
    );
    assert_eq!(restored_state.get("temp"), Some(&json!("step_two")));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_checkpoint_t_minus_1_default_and_delete() {
    let Some((b, target)) = tab().await else {
        return;
    };

    let html = "<html><body><input id='val' value='1'></body></html>";
    let (url, _shutdown) = serve_html(html.to_string()).await;
    b.navigate(&target, "goto", Some(&url))
        .await
        .expect("navigate failed");

    // Save checkpoint 1
    b.checkpoint_save(&target, Some("cp_1"))
        .await
        .expect("save 1");

    // Update value & Save checkpoint 2
    b.eval(&target, "document.getElementById('val').value = '2'")
        .await
        .unwrap();
    b.checkpoint_save(&target, Some("cp_2"))
        .await
        .expect("save 2");

    // Update value to 3
    b.eval(&target, "document.getElementById('val').value = '3'")
        .await
        .unwrap();

    // Rollback without specifying tag (defaults to latest / T-1)
    b.checkpoint_rollback(&target, None)
        .await
        .expect("rollback default failed");
    let val_after = eval_js(&b, &target, "document.getElementById('val').value").await;
    assert_eq!(val_after, json!("2"));

    // Checkpoint list & delete
    let list = b.checkpoint_list(Some(&target)).await.expect("list failed");
    let count = list.get("count").and_then(Value::as_u64).unwrap_or(0);
    assert_eq!(count, 2);

    let del = b
        .checkpoint_delete(&target, Some("cp_1"))
        .await
        .expect("delete failed");
    assert_eq!(del.get("deleted"), Some(&json!(1)));

    let list_after = b.checkpoint_list(Some(&target)).await.expect("list failed");
    assert_eq!(list_after.get("count"), Some(&json!(1)));
}
