// FTS5 index over conversation messages.
//
// Backs `/search` so the candidate list doesn't depend on a linear
// scan + fuzzy-match every keystroke. On startup we rebuild from
// `Store` (cheap; the JSON is in memory anyway). On mutations the
// REPL pokes the index: a new turn appends, `/delete` removes a
// conversation, agent runs replace the entire conversation's rows
// when their `AgentHistory` snapshot lands.
//
// System + tool turns are NOT indexed: the user almost always wants
// to find their own prompts or the assistant's prose, not the system
// preamble or a tool-result JSON blob.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::store::{Conversation, Store};

pub struct ConvIndex {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub conv_id: String,
    pub msg_idx: usize,
    pub role: String,
    pub snippet: String,
}

impl ConvIndex {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open conv index at {}", path.display()))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS conv_msgs USING fts5(
                conv_id UNINDEXED,
                msg_idx UNINDEXED,
                role UNINDEXED,
                content,
                tokenize='porter unicode61'
             );",
        )
        .context("init FTS schema")?;
        Ok(Self { conn })
    }

    /// Drop everything and re-index from the in-memory `Store`.
    /// O(messages) — fine even at thousands; sub-second on commodity
    /// hardware.
    pub fn rebuild_from(&self, store: &Store) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM conv_msgs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO conv_msgs (conv_id, msg_idx, role, content)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for conv in &store.conversations {
                for (mi, m) in conv.messages.iter().enumerate() {
                    if !indexable_role(&m.role) {
                        continue;
                    }
                    stmt.execute(params![
                        conv.id.as_str(),
                        mi as i64,
                        m.role.as_str(),
                        m.content.as_str()
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace every row for a single conversation. Used when the
    /// agent loop's snapshot rewrites the message vector.
    pub fn replace_conv(&self, conv: &Conversation) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM conv_msgs WHERE conv_id = ?1",
            params![conv.id.as_str()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO conv_msgs (conv_id, msg_idx, role, content)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (mi, m) in conv.messages.iter().enumerate() {
                if !indexable_role(&m.role) {
                    continue;
                }
                stmt.execute(params![
                    conv.id.as_str(),
                    mi as i64,
                    m.role.as_str(),
                    m.content.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop every row for a conversation.
    pub fn delete_conv(&self, conv_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM conv_msgs WHERE conv_id = ?1", params![conv_id])?;
        Ok(())
    }

    /// Append a single message's row (insert or replace by
    /// `(conv_id, msg_idx)`). System + tool turns are dropped.
    /// Kept around for fine-grained appends; the REPL currently
    /// uses `replace_conv` post-turn since it's easier than tracking
    /// which messages are new.
    #[allow(dead_code)]
    pub fn insert_message(
        &self,
        conv_id: &str,
        msg_idx: usize,
        role: &str,
        content: &str,
    ) -> Result<()> {
        // FTS5 has no `INSERT OR REPLACE` keyed on the
        // unindexed columns; emulate via DELETE + INSERT.
        self.conn.execute(
            "DELETE FROM conv_msgs WHERE conv_id = ?1 AND msg_idx = ?2",
            params![conv_id, msg_idx as i64],
        )?;
        if !indexable_role(role) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO conv_msgs (conv_id, msg_idx, role, content)
             VALUES (?1, ?2, ?3, ?4)",
            params![conv_id, msg_idx as i64, role, content],
        )?;
        Ok(())
    }

    /// Run the user's query against the FTS index. Returns hits
    /// ranked by FTS5's BM25 (the default `rank` ordering).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let fts_query = build_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT conv_id, msg_idx, role,
                    snippet(conv_msgs, 3, '<<', '>>', '…', 12)
             FROM conv_msgs
             WHERE conv_msgs MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |r| {
            Ok(Hit {
                conv_id: r.get(0)?,
                msg_idx: r.get::<_, i64>(1)? as usize,
                role: r.get(2)?,
                snippet: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }
        Ok(out)
    }
}

fn indexable_role(role: &str) -> bool {
    !matches!(role, "system" | "tool")
}

/// Translate a free-text user query into FTS5 MATCH syntax. Each
/// whitespace-separated token becomes a prefix-match against the
/// `content` column when it's a pure word, or a quoted phrase
/// otherwise (FTS5's quoted strings preserve punctuation). Empty
/// queries return an empty string — caller treats that as "no
/// match", not "match everything."
fn build_fts_query(q: &str) -> String {
    q.split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| {
            if t.chars().all(|c| c.is_alphanumeric() || c == '_') {
                // Prefix-search on plain word tokens so typing
                // `calig` matches `Caligula`.
                format!("{t}*")
            } else {
                let escaped = t.replace('"', "\"\"");
                format!("\"{escaped}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn build_fts_query_prefix_and_phrase() {
        assert_eq!(build_fts_query("calig"), "calig*");
        assert_eq!(build_fts_query("foo bar"), "foo* bar*");
        // Punctuation forces quoted phrase.
        let q = build_fts_query("a:b foo");
        assert!(q.contains("\"a:b\""));
        assert!(q.contains("foo*"));
    }

    #[test]
    fn build_fts_query_empty_returns_empty() {
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("   "), "");
    }

    #[test]
    fn insert_search_delete_roundtrip() {
        // We can build a ConvIndex on a temp file (no need for the
        // full Store machinery).
        let dir = TempDir::new().unwrap();
        let idx = ConvIndex::open(&dir.path().join("conv.db")).unwrap();
        idx.insert_message("c1", 0, "user", "what did caligula do?")
            .unwrap();
        idx.insert_message(
            "c1",
            1,
            "assistant",
            "Gaius Caesar Augustus Germanicus reigned briefly...",
        )
        .unwrap();
        idx.insert_message("c2", 0, "user", "tell me about smilodon")
            .unwrap();

        let hits = idx.search("caligula", 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.conv_id == "c1"));

        // System + tool turns are skipped at insert time.
        idx.insert_message("c1", 2, "system", "be terse").unwrap();
        idx.insert_message("c1", 3, "tool", "{\"ok\":true}")
            .unwrap();
        let hits = idx.search("terse", 10).unwrap();
        assert!(hits.is_empty(), "system turns shouldn't be indexed");

        // Delete a conversation -> its rows are gone.
        idx.delete_conv("c1").unwrap();
        let hits = idx.search("caligula", 10).unwrap();
        assert!(hits.is_empty(), "deleted conv should have no hits");
    }
}
