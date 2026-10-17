//! Mesh membership: signed admission and removal records, stored in the synced `mesh_members`.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::keys::{Identity, verify};
use crate::db::Db;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub node_id: String,
    pub name: String,
    pub sign_pub: String,
    pub noise_pub: String,
    pub mesh_id: String,
    pub approver: String,
    pub admitted_at: i64,
    pub sig: String,
    pub removed_by: Option<String>,
    pub removed_at: Option<i64>,
    pub removed_sig: Option<String>,
}

impl Member {
    pub fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }

    fn admit_bytes(&self) -> Vec<u8> {
        format!(
            "patchbay-admit\n{}\n{}\n{}\n{}\n{}\n{}",
            self.mesh_id, self.node_id, self.sign_pub, self.noise_pub, self.approver, self.admitted_at
        )
        .into_bytes()
    }

    fn remove_bytes(&self) -> Vec<u8> {
        format!(
            "patchbay-remove\n{}\n{}\n{}\n{}",
            self.mesh_id,
            self.node_id,
            self.removed_by.as_deref().unwrap_or_default(),
            self.removed_at.unwrap_or_default()
        )
        .into_bytes()
    }

    /// A new record for `node`, signed by `approver` (who is `node` itself when founding a mesh).
    pub fn admit(
        approver_id: &str,
        approver: &Identity,
        mesh_id: &str,
        node_id: &str,
        name: &str,
        sign_pub: &str,
        noise_pub: &str,
    ) -> Self {
        let mut m = Member {
            node_id: node_id.to_owned(),
            name: name.to_owned(),
            sign_pub: sign_pub.to_owned(),
            noise_pub: noise_pub.to_owned(),
            mesh_id: mesh_id.to_owned(),
            approver: approver_id.to_owned(),
            admitted_at: crate::db::now_ms(),
            sig: String::new(),
            removed_by: None,
            removed_at: None,
            removed_sig: None,
        };
        m.sig = approver.sign(&m.admit_bytes());
        m
    }

    pub fn mark_removed(&mut self, by_id: &str, by: &Identity) {
        self.removed_by = Some(by_id.to_owned());
        self.removed_at = Some(crate::db::now_ms());
        self.removed_sig = Some(by.sign(&self.remove_bytes()));
    }

    /// The admission is signed by `approver_sign_pub` (or self-signed for a founder).
    pub fn admission_valid(&self, approver_sign_pub: &str) -> bool {
        verify(approver_sign_pub, &self.admit_bytes(), &self.sig)
    }

    pub fn removal_valid(&self, remover_sign_pub: &str) -> bool {
        match &self.removed_sig {
            Some(sig) => verify(remover_sign_pub, &self.remove_bytes(), sig),
            None => false,
        }
    }
}

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
    })
}

const COLS: &str =
    "node_id, name, sign_pub, noise_pub, mesh_id, approver, admitted_at, sig, removed_by, removed_at, removed_sig";

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
                "INSERT INTO mesh_members ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(node_id) DO UPDATE SET name = ?2, sign_pub = ?3, noise_pub = ?4, mesh_id = ?5,
                   approver = ?6, admitted_at = ?7, sig = ?8, removed_by = ?9, removed_at = ?10,
                   removed_sig = ?11"
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
                m.removed_sig
            ],
        )?;
        Ok(())
    })
}

/// Check a record against the members this node already trusts. `Ok(false)` = its approver isn't
/// known here yet.
pub fn verify_record(db: &Db, m: &Member) -> Result<bool> {
    let approver_key = if m.approver == m.node_id {
        // Only a mesh's founder signs their own admission; the earliest record of a mesh is it.
        let founder = list(db)?.into_iter().filter(|x| x.mesh_id == m.mesh_id).min_by_key(|x| x.admitted_at);
        match founder {
            Some(f) if f.node_id != m.node_id => anyhow::bail!("self-signed record for a non-founder"),
            _ => m.sign_pub.clone(),
        }
    } else {
        match get(db, &m.approver)? {
            Some(a) => a.sign_pub,
            None => return Ok(false),
        }
    };
    if !m.admission_valid(&approver_key) {
        anyhow::bail!("bad admission signature for {}", m.node_id);
    }
    if let Some(by) = &m.removed_by {
        let key = if by == &m.node_id { m.sign_pub.clone() } else {
            match get(db, by)? {
                Some(r) => r.sign_pub,
                None => return Ok(false),
            }
        };
        if !m.removal_valid(&key) {
            anyhow::bail!("bad removal signature for {}", m.node_id);
        }
    }
    Ok(true)
}
