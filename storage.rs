use crate::config::{Profile, ProfileId, TabId};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

pub struct Database(Connection);
#[derive(Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub tab_id: TabId,
    pub profile_id: ProfileId,
    pub action: String,
    pub result: String,
    pub error_code: Option<String>,
}
impl Database {
    pub fn save_macro(&self, id: ProfileId, file: &crate::macros::MacroFile) -> anyhow::Result<()> {
        self.0.execute_batch(
            "CREATE TABLE IF NOT EXISTS macros (profile_id TEXT PRIMARY KEY,payload TEXT NOT NULL)",
        )?;
        self.0.execute("INSERT INTO macros(profile_id,payload) VALUES (?1,?2) ON CONFLICT(profile_id) DO UPDATE SET payload=excluded.payload",params![id.to_string(),serde_json::to_string(file)?])?;
        Ok(())
    }
    pub fn load_macro(&self, id: ProfileId) -> anyhow::Result<crate::macros::MacroFile> {
        use rusqlite::OptionalExtension;
        self.0.execute_batch(
            "CREATE TABLE IF NOT EXISTS macros (profile_id TEXT PRIMARY KEY,payload TEXT NOT NULL)",
        )?;
        let text: Option<String> = self
            .0
            .query_row(
                "SELECT payload FROM macros WHERE profile_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        match text {
            Some(text) => crate::macros::MacroFile::from_json(&text),
            None => Ok(crate::macros::MacroFile::default()),
        }
    }
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch("PRAGMA journal_mode=WAL;  CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, payload TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), tab_id TEXT NOT NULL, profile_id TEXT NOT NULL, action TEXT NOT NULL, result TEXT NOT NULL, error_code TEXT); CREATE INDEX IF NOT EXISTS event_tab ON events(tab_id,id);")?;
        Ok(Self(c))
    }
    pub fn profiles(&self) -> anyhow::Result<Vec<Profile>> {
        let mut s = self
            .0
            .prepare("SELECT payload FROM profiles ORDER BY rowid")?;
        let rows = s.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn save(&self, p: &Profile) -> anyhow::Result<()> {
        self.0.execute("INSERT INTO profiles(id,payload) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![p.id.to_string(), serde_json::to_string(p)?])?;
        Ok(())
    }
    pub fn log(
        &self,
        tab: TabId,
        profile: ProfileId,
        action: &str,
        result: &str,
        code: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.execute("INSERT INTO events(tab_id,profile_id,action,result,error_code) VALUES (?1,?2,?3,?4,?5)", params![tab.to_string(),profile.to_string(),action,result,code])?;
        tracing::info!(tab_id=%tab, profile_id=%profile, action, result, error_code=code.unwrap_or(""));
        // Keep disk usage bounded. No addresses, selectors or page data enter logs.
        if self.0.last_insert_rowid() % 500 == 0 {
            self.0.execute(
                "DELETE FROM events WHERE id < (SELECT MAX(id)-10000 FROM events)",
                [],
            )?;
        }
        Ok(())
    }
    pub fn logs(&self, tab: TabId) -> anyhow::Result<Vec<LogEntry>> {
        let mut s = self.0.prepare("SELECT timestamp,profile_id,action,result,error_code FROM events WHERE tab_id=?1 ORDER BY id DESC LIMIT 80")?;
        let rows = s.query_map([tab.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.map(|r| {
            let (timestamp, p, action, result, error_code) = r?;
            Ok(LogEntry {
                timestamp,
                tab_id: tab,
                profile_id: p.parse()?,
                action,
                result,
                error_code,
            })
        })
        .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistence_roundtrip_and_separate_logs() {
        let path = std::env::temp_dir().join(format!("tabpilot-{}.db", uuid::Uuid::new_v4()));
        let p = Profile::new(1);
        let other = Profile::new(2);
        {
            let db = Database::open(&path).unwrap();
            db.save(&p).unwrap();
            db.log(p.tab_id, p.id, "CLICK", "success", None).unwrap();
            assert!(db.logs(other.tab_id).unwrap().is_empty());
        }
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.profiles().unwrap()[0].id, p.id);
            assert_eq!(db.logs(p.tab_id).unwrap().len(), 1);
        }
        let _ = std::fs::remove_file(path);
    }
}
