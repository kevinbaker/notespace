//! HN comment trees to materialized paths.

use std::collections::{HashMap, HashSet};

use notespace_core::path::{Path, MAX_DEPTH};

use crate::hn::Item;

pub struct Placed<'a> {
    pub item: &'a Item,
    pub path: Path,
    /// Index into the returned slice, not a database id.
    pub parent: Option<usize>,
    /// True for the story's own text, which becomes the thread's opening post.
    pub is_opening: bool,
}

/// Flatten a story into posts, in path order.
///
/// Top-level comments are siblings of the opening post rather than replies to it: the story is
/// the thread, and its text is just the first thing said in it.
///
/// A reply deeper than `max_depth` attaches to the deepest ancestor that fits. Dropping it
/// instead would take its whole subtree with it.
///
/// `seen` spans the whole import, not one story. HN merges threads, and a merged comment is
/// returned under both stories -- so the same id really does arrive twice, and the second
/// arrival is dropped with its subtree, which is the same duplicate.
pub fn place<'a>(story: &'a Item, max_depth: usize, seen: &mut HashSet<i64>) -> Vec<Placed<'a>> {
    let max_depth = max_depth.clamp(1, MAX_DEPTH);
    let mut out: Vec<Placed> = Vec::with_capacity(story.descendants() + 1);
    let mut ordinals: HashMap<String, u32> = HashMap::new();

    let has_opening = !story.text.as_deref().unwrap_or("").is_empty();
    // LIFO: the opening post goes on last so it comes off first and takes ordinal 1.
    let mut stack: Vec<(&Item, Option<Path>, bool)> = Vec::new();
    for child in story.children.iter().rev() {
        stack.push((child, None, false));
    }
    if has_opening {
        stack.push((story, None, true));
    }

    while let Some((item, parent, is_opening)) = stack.pop() {
        if !seen.insert(item.id) {
            continue;
        }
        let parent = parent.and_then(|p| clamp(&p, max_depth));
        let key = parent
            .as_ref()
            .map_or(String::new(), |p| p.as_str().to_string());
        let ordinal = ordinals.entry(key.clone()).or_insert(0);
        *ordinal += 1;
        let path = match &parent {
            Some(p) => p.child(*ordinal),
            None => Path::root(*ordinal),
        };
        // Only a level wider than 1,048,575 siblings can fail here, and clamping has already
        // bounded the depth. Such a node is dropped with its subtree rather than aborting.
        let Ok(path) = path else { continue };

        out.push(Placed {
            item,
            // Filled in after the sort below, which moves every row.
            parent: None,
            path: path.clone(),
            is_opening,
        });
        if !is_opening {
            for child in item.children.iter().rev() {
                stack.push((child, Some(path.clone()), false));
            }
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    // Clamping already moved a too-deep reply under its new parent, so a row's path parent is
    // its real parent -- and path order guarantees that row was placed first.
    let index_of: HashMap<&str, usize> = out
        .iter()
        .enumerate()
        .map(|(i, p)| (p.path.as_str(), i))
        .collect();
    let parents: Vec<Option<usize>> = out
        .iter()
        .map(|p| {
            p.path
                .parent()
                .and_then(|q| index_of.get(q.as_str()).copied())
        })
        .collect();
    for (placed, parent) in out.iter_mut().zip(parents) {
        placed.parent = parent;
    }
    out
}

/// The deepest ancestor of `parent` that can still take a child within `max_depth` levels.
fn clamp(parent: &Path, max_depth: usize) -> Option<Path> {
    let deepest_parent = max_depth.checked_sub(2)?;
    if parent.depth() <= deepest_parent {
        return Some(parent.clone());
    }
    parent.ancestor_at_depth(deepest_parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, text: &str, children: Vec<Item>) -> Item {
        Item {
            id,
            author: Some(format!("u{id}")),
            created_at_i: Some(1_700_000_000 + id),
            title: None,
            url: None,
            text: Some(text.into()),
            points: None,
            hn_kind: None,
            children,
        }
    }

    fn story(text: &str, children: Vec<Item>) -> Item {
        let mut s = item(1, text, children);
        s.title = Some("A title".into());
        s
    }

    fn paths<'a>(placed: &'a [Placed<'a>]) -> Vec<&'a str> {
        placed.iter().map(|p| p.path.as_str()).collect()
    }

    #[test]
    fn story_text_is_the_first_post_and_comments_are_its_siblings() {
        let s = story("op", vec![item(2, "a", vec![]), item(3, "b", vec![])]);
        assert_eq!(
            paths(&place(&s, 8, &mut HashSet::new())),
            ["0001", "0002", "0003"]
        );
        assert!(place(&s, 8, &mut HashSet::new())[0].is_opening);
    }

    #[test]
    fn a_story_without_text_starts_at_the_first_comment() {
        let mut s = story("", vec![item(2, "a", vec![])]);
        s.text = None;
        let placed = place(&s, 8, &mut HashSet::new());
        assert_eq!(paths(&placed), ["0001"]);
        assert!(!placed[0].is_opening);
    }

    #[test]
    fn replies_nest_under_their_parent() {
        let s = story(
            "",
            vec![item(
                2,
                "a",
                vec![item(3, "a1", vec![item(4, "a1a", vec![])])],
            )],
        );
        assert_eq!(
            paths(&place(&s, 8, &mut HashSet::new())),
            ["0001", "0001.0001", "0001.0001.0001"]
        );
    }

    #[test]
    fn output_is_in_path_order_and_every_parent_precedes_its_child() {
        let s = story(
            "op",
            vec![
                item(2, "a", vec![item(3, "a1", vec![])]),
                item(4, "b", vec![item(5, "b1", vec![])]),
            ],
        );
        let placed = place(&s, 8, &mut HashSet::new());
        assert_eq!(
            paths(&placed),
            ["0001", "0002", "0002.0001", "0003", "0003.0001"]
        );
        for (i, p) in placed.iter().enumerate() {
            if let Some(parent) = p.parent {
                assert!(parent < i, "parent {parent} comes after child {i}");
            }
        }
    }

    #[test]
    fn parent_indexes_point_at_the_actual_parent() {
        let s = story("", vec![item(2, "a", vec![item(3, "a1", vec![])])]);
        let placed = place(&s, 8, &mut HashSet::new());
        assert_eq!(placed[0].parent, None);
        assert_eq!(placed[1].parent, Some(0));
        assert_eq!(placed[1].item.id, 3);
    }

    #[test]
    fn a_reply_past_the_depth_limit_attaches_to_the_deepest_ancestor_that_fits() {
        // Three levels allowed; the HN tree is four deep.
        let s = story(
            "",
            vec![item(
                2,
                "a",
                vec![item(3, "b", vec![item(4, "c", vec![item(5, "d", vec![])])])],
            )],
        );
        let placed = place(&s, 3, &mut HashSet::new());
        assert_eq!(
            paths(&placed),
            ["0001", "0001.0001", "0001.0001.0001", "0001.0001.0002"]
        );
        assert!(placed.iter().all(|p| p.path.depth() < 3));
    }

    #[test]
    fn nothing_is_lost_to_clamping() {
        let s = story(
            "",
            vec![item(
                2,
                "a",
                vec![item(3, "b", vec![item(4, "c", vec![item(5, "d", vec![])])])],
            )],
        );
        assert_eq!(place(&s, 2, &mut HashSet::new()).len(), 4);
        assert_eq!(place(&s, 1, &mut HashSet::new()).len(), 4);
    }

    #[test]
    fn a_chain_deeper_than_the_hard_ceiling_still_places() {
        let mut node = item(200, "deepest", vec![]);
        for id in (2..200).rev() {
            node = item(id, "x", vec![node]);
        }
        let s = story("", vec![node]);
        let placed = place(&s, MAX_DEPTH, &mut HashSet::new());
        assert_eq!(placed.len(), 199);
        assert!(placed.iter().all(|p| p.path.depth() < MAX_DEPTH));
    }

    #[test]
    fn an_id_already_imported_is_dropped_with_its_subtree() {
        let s = story(
            "",
            vec![
                item(2, "a", vec![item(3, "a1", vec![])]),
                item(4, "b", vec![]),
            ],
        );
        let mut seen = HashSet::new();
        assert_eq!(place(&s, 8, &mut seen).len(), 3);
        // The same story arriving again -- as a merged thread does -- adds nothing.
        assert!(place(&s, 8, &mut seen).is_empty());
    }

    #[test]
    fn one_duplicated_reply_does_not_cost_its_siblings() {
        let mut seen = HashSet::new();
        seen.insert(3);
        let s = story(
            "",
            vec![item(
                2,
                "a",
                vec![item(3, "dup", vec![]), item(5, "keep", vec![])],
            )],
        );
        let placed = place(&s, 8, &mut seen);
        assert_eq!(paths(&placed), ["0001", "0001.0001"]);
        assert_eq!(placed[1].item.id, 5);
    }

    #[test]
    fn paths_are_unique() {
        let s = story(
            "op",
            vec![
                item(2, "a", vec![item(3, "b", vec![item(4, "c", vec![])])]),
                item(5, "d", vec![]),
            ],
        );
        let placed = place(&s, 2, &mut HashSet::new());
        let mut seen: Vec<&str> = paths(&placed);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), placed.len());
    }
}
