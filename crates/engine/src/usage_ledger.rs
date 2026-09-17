//! Local billing ledger. The message receipt and total commit together so
//! replay after a restart cannot charge the same message twice.
use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, params};
use zeron_proto::UsageTotals;

#[derive(Default)]
pub(crate) struct UsageLedger {
    totals: HashMap<String, UsageTotals>,
    receipts: std::collections::HashSet<(String, String)>,
    db: Option<Connection>,
}

impl UsageLedger {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS usage_totals (
                 chat TEXT PRIMARY KEY, input INTEGER NOT NULL, output INTEGER NOT NULL,
                 cost REAL);
             CREATE TABLE IF NOT EXISTS cost_receipts (
                 chat TEXT NOT NULL, message TEXT NOT NULL, PRIMARY KEY (chat, message));",
        )?;
        let totals = {
            let mut stmt = db.prepare("SELECT chat, input, output, cost FROM usage_totals")?;
            stmt.query_map([], |row| Ok((row.get(0)?, UsageTotals {
                input_tokens: row.get(1)?, output_tokens: row.get(2)?, cost_usd: row.get(3)?,
            })))?.collect::<rusqlite::Result<HashMap<_, _>>>()?
        };
        Ok(Self { totals, receipts: Default::default(), db: Some(db) })
    }

    pub fn get(&self, chat: &str) -> Option<UsageTotals> {
        self.totals.get(chat).copied()
    }

    pub fn add_tokens(&mut self, chat: &str, input: u64, output: u64) -> rusqlite::Result<()> {
        let mut total = self.get(chat).unwrap_or_default();
        total.add(input, output);
        // SQLite stores signed counters. Clamp only at the persistence boundary.
        total.input_tokens = total.input_tokens.min(i64::MAX as u64);
        total.output_tokens = total.output_tokens.min(i64::MAX as u64);
        if let Some(db) = &self.db {
            save_total(db, chat, total)?;
        }
        self.totals.insert(chat.into(), total);
        Ok(())
    }

    pub fn add_cost(&mut self, chat: &str, message: &str, usd: f64) -> rusqlite::Result<()> {
        if message.is_empty() || !usd.is_finite() || usd < 0.0 {
            return Ok(());
        }
        let mut total = self.get(chat).unwrap_or_default();
        let cost = total.cost_usd.unwrap_or(0.0) + usd;
        if !cost.is_finite() { return Ok(()); }
        total.cost_usd = Some(cost);
        if let Some(db) = &mut self.db {
            let tx = db.transaction()?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO cost_receipts (chat, message) VALUES (?1, ?2)",
                params![chat, message],
            )?;
            if inserted == 0 { return Ok(()); }
            save_total(&tx, chat, total)?;
            tx.commit()?;
        } else if !self.receipts.insert((chat.into(), message.into())) {
            return Ok(());
        }
        self.totals.insert(chat.into(), total);
        Ok(())
    }
}

fn save_total(db: &Connection, chat: &str, total: UsageTotals) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO usage_totals (chat, input, output, cost) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(chat) DO UPDATE SET input=excluded.input, output=excluded.output, cost=excluded.cost",
        params![chat, total.input_tokens, total.output_tokens, total.cost_usd],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_and_tokens_survive_restart_and_replayed_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        {
            let mut ledger = UsageLedger::open(&path).unwrap();
            ledger.add_tokens("chat", 30, 4).unwrap();
            ledger.add_cost("chat", "pi/session/one", 0.25).unwrap();
            ledger.add_cost("chat", "pi/session/one", 0.25).unwrap();
        }
        let mut ledger = UsageLedger::open(&path).unwrap();
        ledger.add_cost("chat", "pi/session/one", 0.25).unwrap();
        ledger.add_cost("chat", "opencode/session/two", 0.5).unwrap();
        let total = ledger.get("chat").unwrap();
        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 4);
        assert_eq!(total.cost_usd, Some(0.75));
        assert_eq!(ledger.get("unrelated"), None);
    }

    #[test]
    fn unknown_free_invalid_and_distinct_chats_are_different() {
        let mut ledger = UsageLedger::default();
        ledger.add_tokens("chat", 10, 2).unwrap();
        assert_eq!(ledger.get("chat").unwrap().cost_usd, None);
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            ledger.add_cost("chat", "bad", invalid).unwrap();
        }
        assert_eq!(ledger.get("chat").unwrap().cost_usd, None);
        ledger.add_cost("chat", "free", 0.0).unwrap();
        assert_eq!(ledger.get("chat").unwrap().cost_usd, Some(0.0));
        ledger.add_cost("other", "free", 0.3).unwrap();
        assert_eq!(ledger.get("other").unwrap().cost_usd, Some(0.3));
    }
}
