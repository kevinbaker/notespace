//! The Hacker News item, as `scripts/hn-fetch.mjs` writes it.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub id: i64,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub created_at_i: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub points: Option<i64>,
    /// `ask`, `show`, `job`, `poll` or `story`, decided by the fetcher from HN's own tags.
    #[serde(default)]
    pub hn_kind: Option<String>,
    #[serde(default)]
    pub children: Vec<Item>,
}

impl Item {
    pub fn created_at(&self) -> i64 {
        self.created_at_i.unwrap_or(0)
    }

    /// A dead or deleted comment: HN returns the node so the tree keeps its shape, but strips
    /// the author and the body.
    pub fn is_tombstone(&self) -> bool {
        self.author.is_none() || self.text.as_deref().unwrap_or("").is_empty()
    }

    pub fn descendants(&self) -> usize {
        self.children.iter().map(|c| 1 + c.descendants()).sum()
    }
}
