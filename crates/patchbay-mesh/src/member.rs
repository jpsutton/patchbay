//! Signed admission and removal records for mesh members.

use serde::{Deserialize, Serialize};

use crate::keys::{Identity, verify};

/// Role of a phone (or other client) that only drives other members' work.
pub const ROLE_REMOTE: &str = "remote";

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
    /// `None` for a full member, [`ROLE_REMOTE`] for a remote-only one. Signed with the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

impl Member {
    pub fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }

    pub fn is_remote(&self) -> bool {
        self.role.as_deref() == Some(ROLE_REMOTE)
    }

    fn admit_bytes(&self) -> Vec<u8> {
        let mut s = format!(
            "patchbay-admit\n{}\n{}\n{}\n{}\n{}\n{}",
            self.mesh_id, self.node_id, self.sign_pub, self.noise_pub, self.approver, self.admitted_at
        );
        // Full members' records predate roles; only a role adds a line, so their signatures hold.
        if let Some(r) = &self.role {
            s.push_str("\nrole=");
            s.push_str(r);
        }
        s.into_bytes()
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
    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        approver_id: &str,
        approver: &Identity,
        mesh_id: &str,
        node_id: &str,
        name: &str,
        sign_pub: &str,
        noise_pub: &str,
        role: Option<&str>,
    ) -> Self {
        let mut m = Member {
            node_id: node_id.to_owned(),
            name: name.to_owned(),
            sign_pub: sign_pub.to_owned(),
            noise_pub: noise_pub.to_owned(),
            mesh_id: mesh_id.to_owned(),
            approver: approver_id.to_owned(),
            admitted_at: now_ms(),
            sig: String::new(),
            removed_by: None,
            removed_at: None,
            removed_sig: None,
            role: role.map(str::to_owned),
        };
        m.sig = approver.sign(&m.admit_bytes());
        m
    }

    pub fn mark_removed(&mut self, by_id: &str, by: &Identity) {
        self.removed_by = Some(by_id.to_owned());
        self.removed_at = Some(now_ms());
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

/// Check `m` against records already trusted (`known`), without a database. `Ok(false)` = its
/// approver or remover isn't among them yet.
pub fn verify_against(known: &[Member], m: &Member) -> anyhow::Result<bool> {
    let key_of = |id: &str| known.iter().find(|x| x.node_id == id).map(|x| x.sign_pub.clone());
    let approver_key = if m.approver == m.node_id {
        let founder = known.iter().filter(|x| x.mesh_id == m.mesh_id).min_by_key(|x| x.admitted_at);
        match founder {
            Some(f) if f.node_id != m.node_id => anyhow::bail!("self-signed record for a non-founder"),
            _ => m.sign_pub.clone(),
        }
    } else {
        match key_of(&m.approver) {
            Some(k) => k,
            None => return Ok(false),
        }
    };
    if !m.admission_valid(&approver_key) {
        anyhow::bail!("bad admission signature for {}", m.node_id);
    }
    if let Some(by) = &m.removed_by {
        let key = if by == &m.node_id {
            m.sign_pub.clone()
        } else {
            match key_of(by) {
                Some(k) => k,
                None => return Ok(false),
            }
        };
        if !m.removal_valid(&key) {
            anyhow::bail!("bad removal signature for {}", m.node_id);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_is_signed() {
        let a = Identity::generate().unwrap();
        let mut m = Member::admit("a", &a, "mesh", "p", "phone", "s", "n", Some(ROLE_REMOTE));
        assert!(m.admission_valid(&a.sign_pub_hex()));
        m.role = None;
        assert!(!m.admission_valid(&a.sign_pub_hex()), "dropping the role must break the signature");
        let full = Member::admit("a", &a, "mesh", "d", "desk", "s", "n", None);
        assert!(full.admission_valid(&a.sign_pub_hex()));
        assert!(!full.is_remote());
    }
}
