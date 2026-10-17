//! Mesh data sync: an operation-based CRDT on top of SQLite.
//!
//! Every synced cell has a clock in `sync_clock`: the hybrid logical clock (`hlc`) of its last
//! write, the node that wrote it (`origin`) and that node's change counter (`seq`). SQLite
//! triggers keep the clocks current for local writes, so the rest of the app writes its tables as
//! usual. Peers swap version vectors (highest `seq` seen per origin) and send each other the cells
//! the other side lacks; a cell is overwritten only by a write with a newer `(hlc, origin)`
//! (last writer wins, per column). A deleted row leaves a tombstone clock (`col` = `-`), and a
//! delete beats concurrent edits of the same row.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct Table {
    pub name: &'static str,
    pub pk: &'static [&'static str],
    /// Synced columns other than the primary key. Columns left out stay local to each node.
    pub cols: &'static [&'static str],
}

pub const TABLES: &[Table] = &[
    Table { name: "providers", pk: &["id"], cols: &["name", "kind", "base_url", "created_at"] },
    Table {
        name: "models",
        pk: &["id"],
        cols: &["provider_id", "model_id", "display_name", "vision", "context_window", "params_json"],
    },
    Table {
        name: "conversations",
        pk: &["id"],
        cols: &["title", "model_ref", "system_prompt", "created_at", "updated_at", "pinned", "active_leaf_id"],
    },
    Table {
        name: "messages",
        pk: &["id"],
        cols: &[
            "conversation_id",
            "role",
            "content",
            "reasoning",
            "model_ref",
            "error",
            "created_at",
            "usage_json",
            "parent_id",
        ],
    },
    // `path` is local: each node keeps the file in its own data directory.
    Table { name: "attachments", pk: &["id"], cols: &["sha256", "name", "mime", "kind", "size", "created_at"] },
    Table { name: "message_attachments", pk: &["message_id", "attachment_id"], cols: &["ord"] },
    Table {
        name: "code_sessions",
        pk: &["id"],
        cols: &[
            "cwd",
            "acp_session_id",
            "title",
            "model_ref",
            "created_at",
            "updated_at",
            "usage_json",
            "pinned",
            "transcript_json",
            "activity",
            "host_node",
            "epoch",
        ],
    },
    Table { name: "mcp_servers", pk: &["id"], cols: &["name", "config_json", "enabled", "created_at"] },
    Table { name: "permission_rules", pk: &["id"], cols: &["cwd", "tool_kind", "pattern", "created_at"] },
    Table { name: "prompts", pk: &["id"], cols: &["name", "content", "created_at", "updated_at"] },
    Table {
        name: "mesh_members",
        pk: &["node_id"],
        cols: &[
            "name",
            "sign_pub",
            "noise_pub",
            "mesh_id",
            "approver",
            "admitted_at",
            "sig",
            "removed_by",
            "removed_at",
            "removed_sig",
        ],
    },
    Table { name: "mesh_secrets", pk: &["account"], cols: &["gen", "nonce", "ct", "digest"] },
];

pub fn table(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|t| t.name == name)
}

/// The tombstone pseudo-column.
pub const TOMBSTONE: &str = "-";

/// Next local HLC: wall-clock milliseconds in the high bits, a counter in the low 16.
const HLC_NEXT: &str = "max(hlc + 1, CAST(unixepoch('subsec') * 1000 AS INTEGER) * 65536)";

/// One synced cell write (or a row delete when `c` is [`TOMBSTONE`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Op {
    /// Table.
    pub t: String,
    /// Primary key: the value itself, or a JSON array for composite keys.
    pub k: String,
    /// Column.
    pub c: String,
    /// Value (null for tombstones).
    #[serde(default)]
    pub v: Value,
    pub h: i64,
    /// Origin node.
    pub o: String,
    pub s: i64,
}

pub type VersionVector = BTreeMap<String, i64>;

fn pk_expr(t: &Table, row: &str) -> String {
    if t.pk.len() == 1 {
        format!("{row}.{}", t.pk[0])
    } else {
        let cols: Vec<String> = t.pk.iter().map(|c| format!("{row}.{c}")).collect();
        format!("json_array({})", cols.join(", "))
    }
}

fn col_list(cols: &[&str]) -> String {
    cols.iter().map(|c| format!("SELECT '{c}' AS col")).collect::<Vec<_>>().join(" UNION ALL ")
}

/// Create the change-capture triggers (recreated on every start so they follow `TABLES`) and give
/// clocks to rows written before sync existed.
pub fn install(conn: &Connection) -> Result<()> {
    let upsert_clock = "ON CONFLICT (tbl, pk, col) DO UPDATE SET hlc = excluded.hlc, origin = excluded.origin, seq = excluded.seq";
    let mut sql = String::new();
    for t in TABLES {
        let n = t.name;
        let pk_new = pk_expr(t, "new");
        let pk_old = pk_expr(t, "old");
        let changed: Vec<String> = t.cols.iter().map(|c| format!("old.{c} IS NOT new.{c}")).collect();
        let changed_cols: Vec<String> = t
            .cols
            .iter()
            .map(|c| format!("SELECT '{c}' AS col WHERE old.{c} IS NOT new.{c}"))
            .collect();
        sql.push_str(&format!(
            "DROP TRIGGER IF EXISTS sync_ai_{n};
             DROP TRIGGER IF EXISTS sync_au_{n};
             DROP TRIGGER IF EXISTS sync_ad_{n};
             CREATE TRIGGER sync_ai_{n} AFTER INSERT ON {n}
             WHEN (SELECT applying FROM sync_meta) = 0
             BEGIN
               UPDATE sync_meta SET seq = seq + 1, hlc = {HLC_NEXT};
               INSERT INTO sync_clock (tbl, pk, col, hlc, origin, seq)
                 SELECT '{n}', {pk_new}, c.col, m.hlc, m.node_id, m.seq
                 FROM sync_meta m, ({cols}) c WHERE true {upsert_clock};
               DELETE FROM sync_clock WHERE tbl = '{n}' AND pk = {pk_new} AND col = '{TOMBSTONE}';
             END;
             CREATE TRIGGER sync_au_{n} AFTER UPDATE ON {n}
             WHEN (SELECT applying FROM sync_meta) = 0 AND ({any_changed})
             BEGIN
               UPDATE sync_meta SET seq = seq + 1, hlc = {HLC_NEXT};
               INSERT INTO sync_clock (tbl, pk, col, hlc, origin, seq)
                 SELECT '{n}', {pk_new}, c.col, m.hlc, m.node_id, m.seq
                 FROM sync_meta m, ({changed_cols}) c WHERE true {upsert_clock};
             END;
             CREATE TRIGGER sync_ad_{n} AFTER DELETE ON {n}
             WHEN (SELECT applying FROM sync_meta) = 0
             BEGIN
               UPDATE sync_meta SET seq = seq + 1, hlc = {HLC_NEXT};
               DELETE FROM sync_clock WHERE tbl = '{n}' AND pk = {pk_old};
               INSERT INTO sync_clock (tbl, pk, col, hlc, origin, seq)
                 SELECT '{n}', {pk_old}, '{TOMBSTONE}', m.hlc, m.node_id, m.seq FROM sync_meta m;
             END;
            ",
            cols = col_list(t.cols),
            any_changed = changed.join(" OR "),
            changed_cols = changed_cols.join(" UNION ALL "),
        ));
    }
    conn.execute_batch(&sql)?;
    backfill(conn)?;
    Ok(())
}

/// Clocks for rows that have none (data from before sync, or written with triggers off).
fn backfill(conn: &Connection) -> Result<()> {
    for t in TABLES {
        let pk = pk_expr(t, "t");
        let missing: Vec<String> = conn
            .prepare(&format!(
                "SELECT {pk} FROM {n} t WHERE NOT EXISTS
                   (SELECT 1 FROM sync_clock k WHERE k.tbl = '{n}' AND k.pk = {pk})",
                n = t.name
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if missing.is_empty() {
            continue;
        }
        conn.execute_batch("BEGIN")?;
        let res = (|| -> Result<()> {
            let mut ins = conn.prepare(
                "INSERT OR IGNORE INTO sync_clock (tbl, pk, col, hlc, origin, seq)
                 SELECT ?1, ?2, ?3, m.hlc, m.node_id, m.seq FROM sync_meta m",
            )?;
            for k in &missing {
                conn.execute(&format!("UPDATE sync_meta SET seq = seq + 1, hlc = {HLC_NEXT}"), [])?;
                for c in t.cols {
                    ins.execute(params![t.name, k, c])?;
                }
            }
            Ok(())
        })();
        match res {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }
    Ok(())
}

pub fn node_id(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT node_id FROM sync_meta", [], |r| r.get(0))?)
}

/// What this node has: the highest seq per origin, including its own.
pub fn version_vector(conn: &Connection) -> Result<VersionVector> {
    let mut vv: VersionVector = conn
        .prepare("SELECT origin, seq FROM sync_vv")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (me, seq): (String, i64) = conn.query_row("SELECT node_id, seq FROM sync_meta", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    vv.insert(me, seq);
    Ok(vv)
}

fn to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
}

fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Primary-key values for a WHERE clause.
fn pk_values(t: &Table, k: &str) -> Vec<SqlValue> {
    if t.pk.len() == 1 {
        return vec![SqlValue::Text(k.to_owned())];
    }
    match serde_json::from_str::<Vec<Value>>(k) {
        Ok(vals) => vals.iter().map(to_sql).collect(),
        Err(_) => vec![SqlValue::Null; t.pk.len()],
    }
}

fn pk_where(t: &Table) -> String {
    t.pk.iter().enumerate().map(|(i, c)| format!("{c} = ?{}", i + 1)).collect::<Vec<_>>().join(" AND ")
}

/// One batch of changes for a peer.
#[derive(Debug, Default)]
pub struct Batch {
    pub ops: Vec<Op>,
    /// More changes remain after this batch.
    pub more: bool,
    /// Highest seq per origin this batch covers, including changes with nothing left to send
    /// (cells of rows that are gone). The peer has everything up to here once it applies `ops`.
    pub upto: VersionVector,
}

/// Changes this node has that a peer with `vv` lacks, oldest first, up to about `limit` cells.
pub fn ops_since(conn: &Connection, vv: &VersionVector, limit: usize) -> Result<Batch> {
    let origins: Vec<String> = conn
        .prepare("SELECT DISTINCT origin FROM sync_clock")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // (hlc, origin, seq, tbl, pk, col)
    let mut clocks: Vec<(i64, String, i64, String, String, String)> = Vec::new();
    let mut more = false;
    for o in origins {
        let after = vv.get(&o).copied().unwrap_or(0);
        // Stop before the seq group that would pass the limit, so a group is never split.
        let cutoff: Option<i64> = conn
            .query_row(
                "SELECT seq FROM sync_clock WHERE origin = ?1 AND seq > ?2 ORDER BY seq LIMIT 1 OFFSET ?3",
                params![o, after, limit as i64],
                |r| r.get(0),
            )
            .optional()?;
        let upper = match cutoff {
            Some(c) => {
                more = true;
                let first: i64 = conn.query_row(
                    "SELECT min(seq) FROM sync_clock WHERE origin = ?1 AND seq > ?2",
                    params![o, after],
                    |r| r.get(0),
                )?;
                // One group larger than the limit still has to go out whole.
                if c == first { c + 1 } else { c }
            }
            None => i64::MAX,
        };
        let mut stmt = conn.prepare(
            "SELECT hlc, origin, seq, tbl, pk, col FROM sync_clock
             WHERE origin = ?1 AND seq > ?2 AND seq < ?3 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![o, after, upper], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;
        for row in rows {
            clocks.push(row?);
        }
    }
    clocks.sort();
    // Cap the merged list at a group boundary.
    if clocks.len() > limit {
        let mut end = limit;
        while end < clocks.len() && clocks[end].1 == clocks[end - 1].1 && clocks[end].2 == clocks[end - 1].2 {
            end += 1;
        }
        if end < clocks.len() {
            more = true;
        }
        clocks.truncate(end);
    }

    let mut upto = VersionVector::new();
    for c in &clocks {
        let e = upto.entry(c.1.clone()).or_insert(0);
        *e = (*e).max(c.2);
    }
    let mut rows: HashMap<(String, String), Option<HashMap<String, Value>>> = HashMap::new();
    let mut ops = Vec::with_capacity(clocks.len());
    for (h, o, s, tbl, pk, col) in clocks {
        let Some(t) = table(&tbl) else { continue };
        if col == TOMBSTONE {
            ops.push(Op { t: tbl, k: pk, c: col, v: Value::Null, h, o, s });
            continue;
        }
        let key = (tbl.clone(), pk.clone());
        if !rows.contains_key(&key) {
            let sql = format!("SELECT {} FROM {} WHERE {}", t.cols.join(", "), t.name, pk_where(t));
            let row = conn
                .query_row(&sql, params_from_iter(pk_values(t, &pk)), |r| {
                    let mut m = HashMap::new();
                    for (i, c) in t.cols.iter().enumerate() {
                        m.insert((*c).to_owned(), to_json(r.get_ref(i)?));
                    }
                    Ok(m)
                })
                .optional()?;
            rows.insert(key.clone(), row);
        }
        // A clock for a row that no longer exists (removed by a cascade) carries nothing.
        let Some(Some(row)) = rows.get(&key) else { continue };
        let mut v = row.get(&col).cloned().unwrap_or(Value::Null);
        if tbl == "mcp_servers" && col == "config_json" {
            v = super::secrets::redact_mcp_config(&v);
        }
        ops.push(Op { t: tbl, k: pk, c: col, v, h, o, s });
    }
    Ok(Batch { ops, more, upto })
}

/// After applying a batch: this node now has everything up to `upto`.
pub fn record_upto(conn: &Connection, upto: &VersionVector) -> Result<()> {
    let me = node_id(conn)?;
    for (o, s) in upto.iter().filter(|(o, _)| **o != me) {
        conn.execute(
            "INSERT INTO sync_vv (origin, seq) VALUES (?1, ?2)
             ON CONFLICT(origin) DO UPDATE SET seq = max(seq, excluded.seq)",
            params![o, s],
        )?;
    }
    Ok(())
}

/// Raise `vv` to `upto`.
pub fn merge(vv: &mut VersionVector, upto: &VersionVector) {
    for (o, s) in upto {
        let e = vv.entry(o.clone()).or_insert(0);
        *e = (*e).max(*s);
    }
}

/// Values for local-only columns when a synced row is first created on this node.
pub trait LocalDefaults {
    fn local_defaults(&self, table: &str, cells: &HashMap<String, Value>) -> Vec<(&'static str, Value)>;
}

impl LocalDefaults for () {
    fn local_defaults(&self, _: &str, _: &HashMap<String, Value>) -> Vec<(&'static str, Value)> {
        Vec::new()
    }
}

#[derive(Debug, Default)]
pub struct Applied {
    /// Rows written or deleted: (table, pk).
    pub rows: Vec<(String, String)>,
    /// Code sessions whose host changed: (session id, previous host, previous folder).
    pub host_changes: Vec<(String, Option<String>, Option<String>)>,
    pub tables: HashSet<String>,
    pub parked: usize,
}

enum RowOutcome {
    Unchanged,
    Changed,
    /// Can't apply yet (missing parent row or required column).
    Park,
}

/// Write remote changes. Unappliable ones are parked and retried after later batches.
pub fn apply(conn: &mut Connection, ops: &[Op], defaults: &dyn LocalDefaults) -> Result<Applied> {
    let mut out = apply_batch(conn, ops, defaults)?;
    if !out.rows.is_empty() {
        retry_parked(conn, defaults, &mut out)?;
    }
    Ok(out)
}

fn apply_batch(conn: &mut Connection, ops: &[Op], defaults: &dyn LocalDefaults) -> Result<Applied> {
    let me = node_id(conn)?;
    let tx = conn.transaction()?;
    tx.execute("UPDATE sync_meta SET applying = 1", [])?;
    let mut out = Applied::default();
    // Group by row, keeping first-seen order (the sender sorts by HLC, so parents come first).
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<&Op>> = HashMap::new();
    for op in ops {
        let key = (op.t.clone(), op.k.clone());
        groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        }).push(op);
    }
    for key in order {
        let group = &groups[&key];
        let Some(t) = table(&key.0) else { continue };
        tx.execute_batch("SAVEPOINT row")?;
        let prev_host: Option<(Option<String>, Option<String>)> =
            (t.name == "code_sessions" && group.iter().any(|o| o.c == "host_node")).then(|| {
                tx.query_row("SELECT host_node, cwd FROM code_sessions WHERE id = ?1", [&key.1], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()
                .ok()
                .flatten()
                .unwrap_or((None, None))
            });
        match apply_row(&tx, t, &key.1, group, defaults) {
            Ok(RowOutcome::Changed) => {
                tx.execute_batch("RELEASE row")?;
                if let Some((host, cwd)) = prev_host {
                    out.host_changes.push((key.1.clone(), host, cwd));
                }
                out.tables.insert(key.0.clone());
                out.rows.push(key);
            }
            Ok(RowOutcome::Unchanged) => tx.execute_batch("RELEASE row")?,
            Ok(RowOutcome::Park) | Err(_) => {
                tx.execute_batch("ROLLBACK TO row; RELEASE row")?;
                let parked: Vec<Op> = group.iter().map(|o| (*o).clone()).collect();
                tx.execute(
                    "INSERT INTO sync_parked (ops_json, created_at) VALUES (?1, ?2)",
                    params![serde_json::to_string(&parked)?, crate::db::now_ms()],
                )?;
                out.parked += 1;
            }
        }
    }
    let mut max_h = 0;
    let mut by_origin: HashMap<&str, i64> = HashMap::new();
    for op in ops {
        max_h = max_h.max(op.h);
        if op.o != me {
            let e = by_origin.entry(&op.o).or_insert(0);
            *e = (*e).max(op.s);
        }
    }
    for (o, s) in by_origin {
        tx.execute(
            "INSERT INTO sync_vv (origin, seq) VALUES (?1, ?2)
             ON CONFLICT(origin) DO UPDATE SET seq = max(seq, excluded.seq)",
            params![o, s],
        )?;
    }
    tx.execute("UPDATE sync_meta SET hlc = max(hlc, ?1), applying = 0", [max_h])?;
    tx.commit()?;
    Ok(out)
}

fn retry_parked(conn: &mut Connection, defaults: &dyn LocalDefaults, out: &mut Applied) -> Result<()> {
    let week_ago = crate::db::now_ms() - 7 * 86_400_000;
    conn.execute("DELETE FROM sync_parked WHERE created_at < ?1", [week_ago])?;
    // Each round may unblock more; stop once a round makes no progress.
    for _ in 0..8 {
        let parked: Vec<(i64, String)> = conn
            .prepare("SELECT id, ops_json FROM sync_parked ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if parked.is_empty() {
            break;
        }
        let ids: Vec<i64> = parked.iter().map(|p| p.0).collect();
        let ops: Vec<Op> = parked
            .iter()
            .flat_map(|(_, j)| serde_json::from_str::<Vec<Op>>(j).unwrap_or_default())
            .collect();
        conn.execute(
            &format!("DELETE FROM sync_parked WHERE id IN ({})", ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")),
            [],
        )?;
        let r = apply_batch(conn, &ops, defaults)?;
        let progressed = !r.rows.is_empty();
        out.tables.extend(r.tables);
        out.rows.extend(r.rows);
        out.host_changes.extend(r.host_changes);
        out.parked = r.parked;
        if !progressed {
            break;
        }
    }
    Ok(())
}

fn clock(conn: &Connection, t: &str, k: &str, c: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT hlc, origin FROM sync_clock WHERE tbl = ?1 AND pk = ?2 AND col = ?3",
            params![t, k, c],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn set_clock(conn: &Connection, op: &Op) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_clock (tbl, pk, col, hlc, origin, seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (tbl, pk, col) DO UPDATE SET hlc = excluded.hlc, origin = excluded.origin, seq = excluded.seq",
        params![op.t, op.k, op.c, op.h, op.o, op.s],
    )?;
    Ok(())
}

fn newer(op: &Op, local: &Option<(i64, String)>) -> bool {
    match local {
        None => true,
        Some((h, o)) => (op.h, op.o.as_str()) > (*h, o.as_str()),
    }
}

fn apply_row(conn: &Connection, t: &Table, k: &str, ops: &[&Op], defaults: &dyn LocalDefaults) -> Result<RowOutcome> {
    let pk_vals = pk_values(t, k);
    let exists = conn
        .query_row(&format!("SELECT 1 FROM {} WHERE {}", t.name, pk_where(t)), params_from_iter(pk_vals.iter()), |_| Ok(()))
        .optional()?
        .is_some();

    if let Some(del) = ops.iter().filter(|o| o.c == TOMBSTONE).max_by_key(|o| (o.h, o.o.clone())) {
        let had_tombstone = clock(conn, t.name, k, TOMBSTONE)?.is_some();
        if exists {
            conn.execute(&format!("DELETE FROM {} WHERE {}", t.name, pk_where(t)), params_from_iter(pk_vals.iter()))?;
        }
        conn.execute("DELETE FROM sync_clock WHERE tbl = ?1 AND pk = ?2", params![t.name, k])?;
        set_clock(conn, del)?;
        return Ok(if exists || !had_tombstone { RowOutcome::Changed } else { RowOutcome::Unchanged });
    }
    // Delete wins over concurrent edits.
    if clock(conn, t.name, k, TOMBSTONE)?.is_some() {
        return Ok(RowOutcome::Unchanged);
    }

    let mut latest: BTreeMap<&str, &Op> = BTreeMap::new();
    for op in ops {
        if !t.cols.contains(&op.c.as_str()) {
            continue;
        }
        match latest.get(op.c.as_str()) {
            Some(prev) if (prev.h, prev.o.as_str()) >= (op.h, op.o.as_str()) => {}
            _ => {
                latest.insert(op.c.as_str(), op);
            }
        }
    }
    let mut winners: Vec<&Op> = Vec::new();
    for op in latest.values() {
        if newer(op, &clock(conn, t.name, k, &op.c)?) {
            winners.push(op);
        }
    }
    if winners.is_empty() {
        return Ok(RowOutcome::Unchanged);
    }

    if exists {
        let sets: Vec<String> = winners.iter().enumerate().map(|(i, o)| format!("{} = ?{}", o.c, i + 1)).collect();
        let mut vals: Vec<SqlValue> = winners.iter().map(|o| to_sql(&o.v)).collect();
        let n = vals.len();
        let wh: Vec<String> = t.pk.iter().enumerate().map(|(i, c)| format!("{c} = ?{}", n + i + 1)).collect();
        vals.extend(pk_vals);
        conn.execute(
            &format!("UPDATE {} SET {} WHERE {}", t.name, sets.join(", "), wh.join(" AND ")),
            params_from_iter(vals.iter()),
        )?;
    } else {
        let cells: HashMap<String, Value> = winners.iter().map(|o| (o.c.clone(), o.v.clone())).collect();
        let mut cols: Vec<String> = t.pk.iter().map(|c| (*c).to_owned()).collect();
        let mut vals: Vec<SqlValue> = pk_vals;
        for o in &winners {
            cols.push(o.c.clone());
            vals.push(to_sql(&o.v));
        }
        for (c, v) in defaults.local_defaults(t.name, &cells) {
            cols.push(c.to_owned());
            vals.push(to_sql(&v));
        }
        let ph: Vec<String> = (1..=vals.len()).map(|i| format!("?{i}")).collect();
        let res = conn.execute(
            &format!("INSERT INTO {} ({}) VALUES ({})", t.name, cols.join(", "), ph.join(", ")),
            params_from_iter(vals.iter()),
        );
        if let Err(e) = res {
            if matches!(e, rusqlite::Error::SqliteFailure(ref f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation) {
                return Ok(RowOutcome::Park);
            }
            return Err(e.into());
        }
    }
    for op in winners {
        set_clock(conn, op)?;
    }
    Ok(RowOutcome::Changed)
}

/// Drop clocks of rows that are gone without a tombstone (removed by a cascade from a remote delete).
pub fn prune_stale_clocks(conn: &Connection) -> Result<()> {
    for t in TABLES {
        conn.execute(
            &format!(
                "DELETE FROM sync_clock WHERE tbl = '{n}' AND col <> '{TOMBSTONE}'
                 AND pk NOT IN (SELECT {pk} FROM {n} t)",
                n = t.name,
                pk = pk_expr(t, "t"),
            ),
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, new_id};

    fn temp_db() -> Db {
        let dir = std::env::temp_dir().join(format!("pb-sync-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(&dir.join("db.sqlite")).unwrap()
    }

    /// Send everything `from` has that `to` lacks.
    fn sync(from: &Db, to: &Db) -> Applied {
        let vv = to.with_conn(|c| version_vector(c)).unwrap();
        let mut total = Applied::default();
        let mut vv = vv;
        loop {
            let b = from.with_conn(|c| ops_since(c, &vv, 7)).unwrap();
            merge(&mut vv, &b.upto);
            let a = to.with_conn(|c| apply(c, &b.ops, &())).unwrap();
            to.with_conn(|c| record_upto(c, &b.upto)).unwrap();
            total.rows.extend(a.rows);
            total.parked = a.parked;
            if !b.more {
                break;
            }
        }
        total
    }

    fn both(a: &Db, b: &Db) {
        sync(a, b);
        sync(b, a);
    }

    #[test]
    fn conversation_and_messages_replicate() {
        let (a, b) = (temp_db(), temp_db());
        assert_ne!(a.node_id(), b.node_id());
        let c = a.create_conversation(None).unwrap();
        a.update_conversation(&c.id, Some("Hello mesh"), None, None).unwrap();
        let u = a.insert_message(&c.id, None, "user", "hi", None, &[]).unwrap();
        let r = a.insert_message(&c.id, Some(&u), "assistant", "", None, &[]).unwrap();
        a.finish_message(&r, "hello back", None, None, None).unwrap();
        sync(&a, &b);
        let conv = b.get_conversation(&c.id).unwrap().unwrap();
        assert_eq!(conv.title, "Hello mesh");
        let texts: Vec<String> = b.list_messages(&c.id).unwrap().into_iter().map(|m| m.content).collect();
        assert_eq!(texts, ["hi", "hello back"]);
        // Search works on replicated rows (FTS triggers still run while applying).
        assert_eq!(b.search_messages("hello", 5).unwrap().len(), 1);
        // Nothing echoes back as new.
        let back = sync(&b, &a);
        assert!(back.rows.is_empty(), "{:?}", back.rows);
    }

    #[test]
    fn concurrent_edits_converge_per_column() {
        let (a, b) = (temp_db(), temp_db());
        let c = a.create_conversation(None).unwrap();
        both(&a, &b);
        a.update_conversation(&c.id, Some("from A"), None, None).unwrap();
        b.set_conversation_pinned(&c.id, true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        b.update_conversation(&c.id, Some("from B, later"), None, None).unwrap();
        both(&a, &b);
        for db in [&a, &b] {
            let conv = db.get_conversation(&c.id).unwrap().unwrap();
            assert_eq!(conv.title, "from B, later");
            assert!(conv.pinned, "column edits on different nodes both survive");
        }
    }

    #[test]
    fn delete_beats_concurrent_edit() {
        let (a, b) = (temp_db(), temp_db());
        let c = a.create_conversation(None).unwrap();
        let u = a.insert_message(&c.id, None, "user", "q", None, &[]).unwrap();
        both(&a, &b);
        a.delete_conversation(&c.id).unwrap();
        b.update_conversation(&c.id, Some("renamed"), None, None).unwrap();
        b.insert_message(&c.id, Some(&u), "assistant", "late reply", None, &[]).unwrap();
        both(&a, &b);
        both(&a, &b);
        assert!(a.get_conversation(&c.id).unwrap().is_none());
        assert!(b.get_conversation(&c.id).unwrap().is_none());
        assert!(b.list_messages(&c.id).unwrap().is_empty());
    }

    #[test]
    fn child_before_parent_is_parked_then_applied() {
        let (a, b) = (temp_db(), temp_db());
        let c = a.create_conversation(None).unwrap();
        a.insert_message(&c.id, None, "user", "orphan for now", None, &[]).unwrap();
        let ops = a.with_conn(|x| ops_since(x, &VersionVector::new(), 1000)).unwrap().ops;
        let (conv_ops, msg_ops): (Vec<Op>, Vec<Op>) = ops.into_iter().partition(|o| o.t == "conversations");
        let r = b.with_conn(|x| apply(x, &msg_ops, &())).unwrap();
        assert_eq!(r.parked, 1);
        let r = b.with_conn(|x| apply(x, &conv_ops, &())).unwrap();
        assert_eq!(r.parked, 0);
        assert_eq!(b.list_messages(&c.id).unwrap().len(), 1);
    }

    #[test]
    fn changes_flow_through_a_middle_node() {
        let (a, b, c) = (temp_db(), temp_db(), temp_db());
        let conv = a.create_conversation(None).unwrap();
        sync(&a, &b);
        sync(&b, &c);
        c.update_conversation(&conv.id, Some("edited on C"), None, None).unwrap();
        sync(&c, &b);
        sync(&b, &a);
        assert_eq!(a.get_conversation(&conv.id).unwrap().unwrap().title, "edited on C");
        // A has everything C has, so C has nothing new for A.
        let vv = a.with_conn(|x| version_vector(x)).unwrap();
        let ops = c.with_conn(|x| ops_since(x, &vv, 1000)).unwrap().ops;
        assert!(ops.is_empty());
    }

    #[test]
    fn existing_rows_get_clocks() {
        let a = temp_db();
        let p = a.save_prompt(None, "p", "body").unwrap();
        a.with_conn(|c| c.execute("DELETE FROM sync_clock", [])).unwrap();
        a.with_conn(|c| install(c)).unwrap();
        let b = temp_db();
        sync(&a, &b);
        assert_eq!(b.list_prompts().unwrap()[0].id, p.id);
    }

    struct AttDefaults;
    impl LocalDefaults for AttDefaults {
        fn local_defaults(&self, table: &str, cells: &HashMap<String, Value>) -> Vec<(&'static str, Value)> {
            if table == "attachments" {
                vec![("path", Value::from(format!("/local/{}", cells["sha256"].as_str().unwrap())))]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn local_columns_get_local_values() {
        let (a, b) = (temp_db(), temp_db());
        let att = crate::db::Attachment {
            id: new_id(),
            sha256: "abc".into(),
            name: "x.txt".into(),
            mime: "text/plain".into(),
            kind: "text".into(),
            size: 3,
            path: "/home/a/x".into(),
        };
        a.insert_attachment(&att).unwrap();
        let ops = a.with_conn(|x| ops_since(x, &VersionVector::new(), 1000)).unwrap().ops;
        assert!(ops.iter().all(|o| o.c != "path"));
        b.with_conn(|x| apply(x, &ops, &AttDefaults)).unwrap();
        assert_eq!(b.get_attachment(&att.id).unwrap().unwrap().path, "/local/abc");
    }

    #[test]
    fn limits_never_split_a_change() {
        let (a, b) = (temp_db(), temp_db());
        for i in 0..5 {
            a.save_prompt(None, &format!("p{i}"), "x").unwrap();
        }
        let mut vv = b.with_conn(|c| version_vector(c)).unwrap();
        let mut batches = 0;
        loop {
            // Prompts have 4 synced columns; a limit of 3 must still send whole rows.
            let bt = a.with_conn(|c| ops_since(c, &vv, 3)).unwrap();
            assert_eq!(bt.ops.len() % 4, 0);
            merge(&mut vv, &bt.upto);
            b.with_conn(|c| apply(c, &bt.ops, &())).unwrap();
            batches += 1;
            if !bt.more {
                break;
            }
        }
        assert!(batches >= 5);
        assert_eq!(b.list_prompts().unwrap().len(), 5);
    }
}
