//! Mesh membership: signed admission and removal records, stored in the synced `mesh_members`.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

pub use patchbay_mesh::member::{Member, ROLE_REMOTE};

use crate::db::Db;

fn map(r: &rusqlite::Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        node_id: r.get(0)?,
        name: r.get(1)?,
        sign_pub: r.get(2)?,
        noise_pub: r.get(3)?,
        mesh_id: r.get(4)?,
        approver: r.get(5)?,
        admitted_at: r.get(6)?,
        sig: r.get(7)?,
        removed_by: r.get(8)?,
        removed_at: r.get(9)?,
        removed_sig: r.get(10)?,
        role: r.get(11)?,
    })
}

const COLS: &str =
    "node_id, name, sign_pub, noise_pub, mesh_id, approver, admitted_at, sig, removed_by, removed_at, removed_sig, role";

pub fn list(db: &Db) -> Result<Vec<Member>> {
    db.with_conn(|c| {
        let mut stmt = c.prepare(&format!("SELECT {COLS} FROM mesh_members ORDER BY admitted_at"))?;
        let rows = stmt.query_map([], map)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })
}

pub fn get(db: &Db, node_id: &str) -> Result<Option<Member>> {
    db.with_conn(|c| {
        Ok(c.query_row(&format!("SELECT {COLS} FROM mesh_members WHERE node_id = ?1"), [node_id], map)
            .optional()?)
    })
}

pub fn upsert(db: &Db, m: &Member) -> Result<()> {
    db.with_conn(|c| {
        c.execute(
            &format!(
                "INSERT INTO mesh_members ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(node_id) DO UPDATE SET name = ?2, sign_pub = ?3, noise_pub = ?4, mesh_id = ?5,
                   approver = ?6, admitted_at = ?7, sig = ?8, removed_by = ?9, removed_at = ?10,
                   removed_sig = ?11, role = ?12"
            ),
            params![
                m.node_id,
                m.name,
                m.sign_pub,
                m.noise_pub,
                m.mesh_id,
                m.approver,
                m.admitted_at,
                m.sig,
                m.removed_by,
                m.removed_at,
                m.removed_sig,
                m.role
            ],
        )?;
        Ok(())
    })
}

/// Check a record against the members this node already trusts. `Ok(false)` = its approver isn't
/// known here yet.
pub fn verify_record(db: &Db, m: &Member) -> Result<bool> {
    patchbay_mesh::member::verify_against(&list(db)?, m)
}
