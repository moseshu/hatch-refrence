//! People pages: one row per contact, keyed by lower-cased email.

use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    Vip,
    Normal,
    Low,
}

impl Importance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Importance::Vip => "vip",
            Importance::Normal => "normal",
            Importance::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Result<Importance> {
        match s.to_ascii_lowercase().as_str() {
            "vip" => Ok(Importance::Vip),
            "normal" => Ok(Importance::Normal),
            "low" => Ok(Importance::Low),
            _ => Err(Error::Invalid(format!("importance must be vip|normal|low, got {s}"))),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub email: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub importance: Importance,
    pub notes: String,
    pub last_seen: Option<Timestamp>,
    pub interactions: i64,
}

pub struct People<'a> {
    db: &'a Db,
}

fn norm(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

impl<'a> People<'a> {
    pub fn new(db: &'a Db) -> Self {
        People { db }
    }

    /// Record that we saw this person (e.g. as a mail sender). Creates the page if needed.
    pub fn touch(&self, email: &str, name: &str, now: &Timestamp) -> Result<()> {
        let email = norm(email);
        if email.is_empty() {
            return Ok(());
        }
        self.db
            .conn()
            .execute(
                "INSERT INTO people(email, name, aliases, importance, notes, last_seen, interactions)
                 VALUES(?1, ?2, '[]', 'normal', '', ?3, 1)
                 ON CONFLICT(email) DO UPDATE SET
                    interactions = interactions + 1,
                    last_seen = excluded.last_seen,
                    name = CASE WHEN people.name = '' THEN excluded.name ELSE people.name END",
                params![email, name, to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    pub fn upsert(&self, email: &str, name: &str, importance: Importance, notes: &str) -> Result<()> {
        let notes = hatch_core::secrets::redact(notes);
        self.db
            .conn()
            .execute(
                "INSERT INTO people(email, name, aliases, importance, notes, interactions)
                 VALUES(?1, ?2, '[]', ?3, ?4, 0)
                 ON CONFLICT(email) DO UPDATE SET name = excluded.name,
                    importance = excluded.importance, notes = excluded.notes",
                params![norm(email), name, importance.as_str(), notes],
            )
            .db()?;
        Ok(())
    }

    pub fn set_importance(&self, email: &str, importance: Importance) -> Result<()> {
        let n = self
            .db
            .conn()
            .execute(
                "UPDATE people SET importance = ?2 WHERE email = ?1",
                params![norm(email), importance.as_str()],
            )
            .db()?;
        if n == 0 {
            return Err(Error::NotFound(format!("person {email}")));
        }
        Ok(())
    }

    pub fn get(&self, email: &str) -> Result<Option<Person>> {
        self.db
            .conn()
            .query_row(
                "SELECT email, name, aliases, importance, notes, last_seen, interactions FROM people WHERE email = ?1",
                params![norm(email)],
                row_to_person,
            )
            .optional()
            .db()
    }

    pub fn importance_of(&self, email: &str) -> Result<Importance> {
        Ok(self.get(email)?.map(|p| p.importance).unwrap_or(Importance::Normal))
    }

    /// Known = we have a page for them (used by the information-flow rule).
    pub fn is_known(&self, email: &str) -> Result<bool> {
        Ok(self.get(email)?.is_some())
    }

    pub fn list(&self) -> Result<Vec<Person>> {
        let mut st = self
            .db
            .conn()
            .prepare(
                "SELECT email, name, aliases, importance, notes, last_seen, interactions FROM people
                 ORDER BY CASE importance WHEN 'vip' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END, interactions DESC",
            )
            .db()?;
        let rows = st.query_map([], row_to_person).db()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().db()
    }

    pub fn remove(&self, email: &str) -> Result<bool> {
        let n = self.db.conn().execute("DELETE FROM people WHERE email = ?1", params![norm(email)]).db()?;
        Ok(n > 0)
    }
}

fn row_to_person(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<Person> {
    let aliases: String = r.get(2)?;
    let imp: String = r.get(3)?;
    let last: Option<String> = r.get(5)?;
    Ok(Person {
        email: r.get(0)?,
        name: r.get(1)?,
        aliases: serde_json::from_str(&aliases).unwrap_or_default(),
        importance: Importance::parse(&imp).unwrap_or(Importance::Normal),
        notes: r.get(4)?,
        last_seen: last.and_then(|s| parse_rfc3339(&s).ok()),
        interactions: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_and_importance() {
        let db = Db::open_in_memory().unwrap();
        let p = People::new(&db);
        let now = hatch_core::time::now();
        p.touch("Boss@Corp.com", "王总", &now).unwrap();
        p.touch("boss@corp.com", "someone else", &now).unwrap();
        let boss = p.get("BOSS@corp.com").unwrap().unwrap();
        assert_eq!(boss.name, "王总");
        assert_eq!(boss.interactions, 2);
        p.set_importance("boss@corp.com", Importance::Vip).unwrap();
        assert_eq!(p.importance_of("boss@corp.com").unwrap(), Importance::Vip);
        assert_eq!(p.importance_of("nobody@x.com").unwrap(), Importance::Normal);
        assert!(p.set_importance("nobody@x.com", Importance::Low).is_err());
        assert!(p.is_known("boss@corp.com").unwrap());
        p.upsert("a@b.com", "A", Importance::Low, "密码：hunter22").unwrap();
        assert!(!p.get("a@b.com").unwrap().unwrap().notes.contains("hunter22"));
        assert_eq!(p.list().unwrap()[0].email, "boss@corp.com");
        assert!(p.remove("a@b.com").unwrap());
    }
}
