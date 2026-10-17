//! End-to-end mesh tests: real nodes talking WebSocket on localhost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::keys::{KeyStore, MemKeyStore};
use super::{Mesh, RpcHandler};
use crate::db::{Db, new_id};

const PASS: &str = "correct horse battery";

struct Node {
    mesh: Mesh,
    db: Arc<Db>,
    keys: Arc<MemKeyStore>,
    events: Arc<Mutex<Vec<(String, Value)>>>,
}

struct Echo;

#[async_trait::async_trait]
impl RpcHandler for Echo {
    async fn call(&self, from: String, method: String, params: Value) -> Result<Value> {
        Ok(json!({ "from": from, "method": method, "params": params }))
    }
}

fn node(name: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("pb-mesh-{name}-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
    let keys = Arc::new(MemKeyStore::default());
    let mesh = Mesh::new(db.clone(), dir, keys.clone()).unwrap();
    mesh.set_config(Some(name), Some("127.0.0.1:0"), None).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    mesh.set_ui(Arc::new(move |n: &str, p: Value| ev.lock().unwrap().push((n.to_owned(), p))));
    mesh.set_handler(Arc::new(Echo));
    Node { mesh, db, keys, events }
}

async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        if t0.elapsed() > Duration::from_secs(secs) {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn url(n: &Node) -> String {
    until("listener", 5, || n.mesh.bound_port().is_some()).await;
    format!("127.0.0.1:{}", n.mesh.bound_port().unwrap())
}

fn pending(n: &Node) -> Vec<Value> {
    n.mesh.status()["pending"].as_array().cloned().unwrap_or_default()
}

/// `joiner` asks to join through `via`; `via` approves after checking the codes match.
async fn admit(via: &Node, joiner: &Node) {
    joiner.mesh.join(&url(via).await, PASS, &joiner.mesh.name()).unwrap();
    until("join request", 10, || !pending(via).is_empty()).await;
    let req = pending(via)[0].clone();
    until("joiner code", 5, || joiner.mesh.status()["joining"]["code"].is_string()).await;
    assert_eq!(req["code"], joiner.mesh.status()["joining"]["code"], "both sides show the same code");
    via.mesh.approve(req["nodeId"].as_str().unwrap()).unwrap();
    until("membership", 10, || joiner.mesh.is_member()).await;
}

fn member_state(n: &Node, id: &str) -> String {
    n.mesh.status()["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["nodeId"] == id)
        .map(|m| m["state"].as_str().unwrap().to_owned())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_sync_relay_and_rpc() {
    let (a, b, c) = (node("alpha"), node("bravo"), node("charlie"));

    // A provider key on A before the mesh exists still reaches the others, sealed.
    let p = crate::db::Provider {
        id: "prov-a".into(),
        name: "Prov".into(),
        kind: crate::db::ProviderKind::Anthropic,
        base_url: "http://example".into(),
        has_key: false,
    };
    a.db.upsert_provider(&p, None).unwrap();
    a.keys.set("prov-a", "sk-alpha");

    a.mesh.create(PASS, "alpha").unwrap();
    assert!(a.mesh.is_member());
    let conv = a.db.create_conversation(None).unwrap();
    a.db.update_conversation(&conv.id, Some("made on alpha"), None, None).unwrap();

    admit(&a, &b).await;
    until("chat on bravo", 10, || b.db.get_conversation(&conv.id).unwrap().is_some()).await;
    until("key on bravo", 10, || b.keys.get("prov-a").as_deref() == Some("sk-alpha")).await;
    assert_eq!(b.db.list_providers().unwrap().len(), 1);

    // C joins through B. A never learns an address for C, so it has to go through B.
    admit(&b, &c).await;
    let cid = c.mesh.node_id().to_owned();
    until("chat on charlie", 10, || c.db.get_conversation(&conv.id).unwrap().is_some()).await;
    until("alpha reaches charlie via bravo", 20, || member_state(&a, &cid) == "relay").await;

    let r = a.mesh.rpc(&cid, "echo", json!({ "x": 1 })).await.unwrap();
    assert_eq!(r["from"], a.mesh.node_id());
    assert_eq!(r["params"]["x"], 1);

    // Edits flow both ways, live.
    c.db.update_conversation(&conv.id, Some("renamed on charlie"), None, None).unwrap();
    until("rename reaches alpha", 10, || {
        a.db.get_conversation(&conv.id).unwrap().unwrap().title == "renamed on charlie"
    })
    .await;
    assert!(a.events.lock().unwrap().iter().any(|(n, _)| n == "mesh://changed"));

    // Live events reach every member, relayed ones included.
    a.mesh.broadcast_event("code://event", json!({ "codeSessionId": "s1", "event": { "type": "notice", "message": "hi" } }));
    until("event on charlie", 10, || c.events.lock().unwrap().iter().any(|(n, p)| n == "code://event" && p["codeSessionId"] == "s1")).await;

    // Attachments are fetched from a peer on demand.
    let att = crate::attachments::ingest(&a.db, &a_dir(&a), "note.txt", b"hello from alpha").unwrap();
    until("attachment row on charlie", 10, || c.db.get_attachment(&att.id).unwrap().is_some()).await;
    let got = c.db.get_attachment(&att.id).unwrap().unwrap();
    let path = c.mesh.ensure_file(&got.sha256, &got.name).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"hello from alpha");

    // Removing a member cuts it off and rotates the data key.
    a.mesh.remove(&cid).unwrap();
    until("charlie stops", 10, || !c.mesh.running()).await;
    until("bravo gets the new key", 10, || {
        b.keys.get("mesh:data_key").is_some_and(|k| k.starts_with("2:"))
    })
    .await;
    assert_eq!(member_state(&b, &cid), "removed");
}

fn a_dir(n: &Node) -> std::path::PathBuf {
    // The attachment store sits next to the database in tests.
    let p: String = n.db.with_conn(|c| c.query_row("PRAGMA database_list", [], |r| r.get(2))).unwrap();
    std::path::Path::new(&p).parent().unwrap().to_path_buf()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_passphrase_fails_to_join() {
    let (a, d) = (node("alpha2"), node("delta"));
    a.mesh.create(PASS, "alpha2").unwrap();
    d.mesh.join(&url(&a).await, "not the passphrase", "delta").unwrap();
    until("join failure", 10, || d.mesh.status()["joining"]["status"] == "failed").await;
    assert!(pending(&a).is_empty());
    assert!(!d.mesh.is_member());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_streams_show_live_and_survive_a_lost_streamer() {
    let (a, b) = (node("alpha3"), node("bravo3"));
    a.mesh.create(PASS, "alpha3").unwrap();
    admit(&a, &b).await;
    let conv = a.db.create_conversation(None).unwrap();
    let u = a.db.insert_message(&conv.id, None, "user", "q", None, &[]).unwrap();
    let r = a.db.insert_message(&conv.id, Some(&u), "assistant", "", None, &[]).unwrap();
    until("reply row on bravo", 10, || b.db.list_messages(&conv.id).unwrap().len() == 2).await;

    a.mesh.chat_event(&conv.id, json!({ "type": "started", "userMessageId": u, "assistantMessageId": r }));
    a.mesh.chat_event(&conv.id, json!({ "type": "text", "text": "partial " }));
    a.mesh.chat_event(&conv.id, json!({ "type": "text", "text": "answer" }));
    until("stream on bravo", 10, || b.mesh.remote_streams().first().is_some_and(|s| s.text == "partial answer")).await;

    // Alpha vanishes mid-reply: bravo keeps what arrived and marks it interrupted.
    a.mesh.stop();
    until("interrupted reply saved", 30, || {
        b.db.list_messages(&conv.id).unwrap().last().is_some_and(|m| m.content == "partial answer" && m.error.is_some())
    })
    .await;
    assert!(b.mesh.remote_streams().is_empty());
}
