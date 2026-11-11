//! The closure is computed over the strongly
//! connected components of the graph (iterative Tarjan, no recursion, so a chain of
//! [`crate::rows::MAX_GROUPS`] groups cannot exhaust the stack), and every cycle is listed in the
//! build report. A closure is a bit set over the groups (`n²/8` bytes per direction at most).

use std::cmp::Ordering;

use erp_fields::{Error, Result};
use erp_orm::{GroupId, GroupSet, QualifiedXmlId};
use rustc_hash::FxHashMap;

use crate::bits::BitSet;
use crate::graph;
use crate::rows::{PolicyRows, PrivilegeId};

pub const INTERNAL_USER_GROUP: &str = "base.group_user";

#[derive(Debug, Clone)]
struct GroupInfo {
    name: Box<str>,
    privilege: Option<(Option<i32>, Box<str>, PrivilegeId)>,
    sequence: Option<i32>,
    share: bool,
    xmlids: Box<[QualifiedXmlId]>,
}

/// The groups of a snapshot and their implications.
#[derive(Debug)]
pub struct GroupGraph {
    /// Group ids by index, ascending.
    ids: Vec<GroupId>,
    index: FxHashMap<GroupId, u32>,
    by_xmlid: FxHashMap<QualifiedXmlId, u32>,
    info: Vec<GroupInfo>,
    /// Direct implications (`implied_ids`) by index.
    implies: Vec<Vec<u32>>,
    /// Component of each group in the implication graph, and the closure of each component.
    component: Vec<u32>,
    closure: Vec<BitSet>,
    /// The same for the reversed graph (`all_implied_by_ids`).
    rev_component: Vec<u32>,
    rev_closure: Vec<BitSet>,
    cycles: Vec<Vec<GroupId>>,
    rank: Vec<u32>,
    internal: Option<u32>,
}

impl GroupGraph {
    /// The graph of `rows` (validated rows: every implication names known groups).
    ///
    /// # Errors
    ///
    /// `Programming` when an implication names a group missing from the rows, which
    /// [`crate::rows::PolicyRowsBuilder::build`] already refuses.
    pub fn build(rows: &PolicyRows) -> Result<Self> {
        let groups = rows.groups();
        let mut index: FxHashMap<GroupId, u32> = FxHashMap::default();
        let mut by_xmlid: FxHashMap<QualifiedXmlId, u32> = FxHashMap::default();
        let privileges: FxHashMap<PrivilegeId, (Option<i32>, &str)> = rows
            .privileges()
            .iter()
            .map(|p| (p.id(), (p.sequence(), p.name())))
            .collect();
        let mut info = Vec::with_capacity(groups.len());
        for (i, group) in groups.iter().enumerate() {
            let i = to_u32(i)?;
            index.insert(group.id(), i);
            for xmlid in group.xmlids() {
                by_xmlid.insert(xmlid.clone(), i);
            }
            let privilege = match group.privilege() {
                None => None,
                Some(id) => {
                    let Some((sequence, name)) = privileges.get(&id) else {
                        return Err(Error::programming(format!(
                            "group graph: group {} refers to the missing privilege {id}",
                            group.id()
                        )));
                    };
                    Some((*sequence, Box::<str>::from(*name), id))
                }
            };
            info.push(GroupInfo {
                name: group.name().into(),
                privilege,
                sequence: group.sequence(),
                share: group.share(),
                xmlids: group.xmlids().into(),
            });
        }
        let n = groups.len();
        let mut implies: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut implied_by: Vec<Vec<u32>> = vec![Vec::new(); n];
        for implication in rows.implied() {
            let (Some(&from), Some(&to)) = (
                index.get(&implication.group()),
                index.get(&implication.implies()),
            ) else {
                return Err(Error::programming(format!(
                    "group graph: the implication ({}, {}) names a missing group",
                    implication.group(),
                    implication.implies()
                )));
            };
            implies[from as usize].push(to);
            implied_by[to as usize].push(from);
        }
        let ids: Vec<GroupId> = groups.iter().map(|g| g.id()).collect();
        let forward = Closure::of(&implies)?;
        let backward = Closure::of(&implied_by)?;
        let cycles = forward
            .cycles
            .iter()
            .map(|members| members.iter().map(|&i| ids[i as usize]).collect())
            .collect();
        let mut order: Vec<u32> = (0..to_u32(n)?).collect();
        order.sort_by(|a, b| {
            group_order(
                &info[*a as usize],
                &info[*b as usize],
                ids[*a as usize],
                ids[*b as usize],
            )
        });
        let mut rank = vec![0; n];
        for (position, i) in order.iter().enumerate() {
            rank[*i as usize] = to_u32(position)?;
        }
        let internal = by_xmlid
            .get(&QualifiedXmlId::parse(INTERNAL_USER_GROUP)?)
            .copied();
        Ok(Self {
            ids,
            index,
            by_xmlid,
            info,
            implies,
            component: forward.component,
            closure: forward.closure,
            rev_component: backward.component,
            rev_closure: backward.closure,
            cycles,
            rank,
            internal,
        })
    }

    /// Number of groups.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether there is no group.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether `group` is in the snapshot.
    pub fn contains(&self, group: GroupId) -> bool {
        self.index.contains_key(&group)
    }

    /// The group an XML id names, if it is in the snapshot.
    pub fn by_xmlid(&self, xmlid: &QualifiedXmlId) -> Option<GroupId> {
        self.by_xmlid.get(xmlid).map(|i| self.ids[*i as usize])
    }

    /// The groups `group` implies directly (`implied_ids`), ascending.
    ///
    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn implied(&self, group: GroupId) -> Result<Vec<GroupId>> {
        let i = self.idx(group)?;
        let mut out: Vec<GroupId> = self.implies[i]
            .iter()
            .map(|j| self.ids[*j as usize])
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Repeats and order of `direct` do not
    /// matter.
    ///
    /// # Errors
    ///
    /// `Access` naming the first group of `direct` that is not in the snapshot (a deleted group):
    /// it is never dropped.
    pub fn closure(&self, direct: &[GroupId]) -> Result<GroupSet> {
        let bits = self.closure_bits(direct)?;
        Ok(GroupSet::new(bits.iter().map(|i| self.ids[i])))
    }

    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn implied_by(&self, group: GroupId) -> Result<Vec<GroupId>> {
        let i = self.idx(group)?;
        let component = self.rev_component[i] as usize;
        Ok(self.rev_closure[component]
            .iter()
            .map(|j| self.ids[j])
            .collect())
    }

    /// The implication cycles (strongly connected sets of two or more groups, or a group implying
    /// itself), each sorted, in the order of their smallest member.
    pub fn cycles(&self) -> &[Vec<GroupId>] {
        &self.cycles
    }

    /// # Errors
    ///
    /// `Programming` when the snapshot has no group with the XML id [`INTERNAL_USER_GROUP`]
    /// (internal and share users cannot be told apart).
    pub fn is_share(&self, groups: &GroupSet) -> Result<bool> {
        let Some(internal) = self.internal else {
            return Err(Error::programming(format!(
                "the policy has no group with the XML id {INTERNAL_USER_GROUP} (the internal user \
                 type), so internal users cannot be told apart from portal and public ones"
            )));
        };
        Ok(!groups.contains(self.ids[internal as usize]))
    }

    /// The share flag of `group` (`res.groups.share`).
    ///
    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn share_flag(&self, group: GroupId) -> Result<bool> {
        Ok(self.info[self.idx(group)?].share)
    }

    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn display_name(&self, group: GroupId) -> Result<String> {
        let info = &self.info[self.idx(group)?];
        Ok(match &info.privilege {
            Some((_, privilege, _)) => format!("{privilege} / {}", info.name),
            None => info.name.to_string(),
        })
    }

    /// The external ids of `group`, sorted.
    ///
    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn xmlids(&self, group: GroupId) -> Result<&[QualifiedXmlId]> {
        Ok(&self.info[self.idx(group)?].xmlids)
    }

    /// Names compare by code point, not by the database collation.
    ///
    /// # Errors
    ///
    /// `Programming` for a group that is not in the snapshot.
    pub fn sort(&self, groups: &mut [GroupId]) -> Result<()> {
        for group in groups.iter() {
            self.idx(*group)?;
        }
        groups.sort_by_key(|g| self.index.get(g).map(|i| self.rank[*i as usize]));
        Ok(())
    }

    pub(crate) fn idx(&self, group: GroupId) -> Result<usize> {
        self.index.get(&group).map(|i| *i as usize).ok_or_else(|| {
            Error::programming(format!("group {group} is not in the policy snapshot"))
        })
    }

    /// The closure of `direct` as a bit set over group indices.
    pub(crate) fn closure_bits(&self, direct: &[GroupId]) -> Result<BitSet> {
        let mut bits = BitSet::new(self.ids.len());
        for group in direct {
            let Some(i) = self.index.get(group) else {
                return Err(Error::access(format!(
                    "The group {group} does not exist (any more): the user's groups cannot be \
                     resolved."
                )));
            };
            let component = self.component[*i as usize] as usize;
            bits.union_with(&self.closure[component]);
        }
        Ok(bits)
    }

    /// `all_implied_by_ids` of one group as a bit set.
    pub(crate) fn implied_by_bits(&self, group: GroupId) -> Result<BitSet> {
        let i = self.idx(group)?;
        Ok(self.rev_closure[self.rev_component[i] as usize].clone())
    }

    /// Every group as a bit set.
    pub(crate) fn all_bits(&self) -> BitSet {
        BitSet::full(self.ids.len())
    }

    /// The group ids of a bit set, ascending.
    pub(crate) fn ids_of(&self, bits: &BitSet) -> Vec<GroupId> {
        bits.iter().map(|i| self.ids[i]).collect()
    }
}

fn to_u32(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| Error::programming(format!("group graph: {n} groups or more")))
}

fn group_order(a: &GroupInfo, b: &GroupInfo, a_id: GroupId, b_id: GroupId) -> Ordering {
    fn nulls_last<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
        match (a, b) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    let privilege = match (&a.privilege, &b.privilege) {
        (Some((sa, na, ia)), Some((sb, nb, ib))) => nulls_last(*sa, *sb)
            .then_with(|| na.cmp(nb))
            .then_with(|| ia.cmp(ib)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    privilege
        .then_with(|| nulls_last(a.sequence, b.sequence))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a_id.cmp(&b_id))
}

/// Per-component closures of a graph given by adjacency lists over `0..n`.
struct Closure {
    component: Vec<u32>,
    closure: Vec<BitSet>,
    /// Members of each cycle, sorted, in the order of their smallest member.
    cycles: Vec<Vec<u32>>,
}

impl Closure {
    fn of(adjacency: &[Vec<u32>]) -> Result<Self> {
        let n = adjacency.len();
        to_u32(n)?;
        let components = graph::components(adjacency);
        // components come in reverse topological order: the closures of every component a
        // member points to are complete when the component is reached
        let mut closure: Vec<BitSet> = Vec::with_capacity(components.members.len());
        for (c, members) in components.members.iter().enumerate() {
            let mut bits = BitSet::new(n);
            for &v in members {
                bits.insert(v as usize);
            }
            for &v in members {
                for &w in &adjacency[v as usize] {
                    let cw = components.of[w as usize] as usize;
                    if cw != c {
                        bits.union_with(&closure[cw]);
                    }
                }
            }
            closure.push(bits);
        }
        let cycles = components.cycles(adjacency);
        Ok(Self {
            component: components.of,
            closure,
            cycles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{GroupRow, Implication, PrivilegeRow};

    fn g(n: i64) -> GroupId {
        GroupId::new(n).unwrap()
    }

    fn graph(n: i64, edges: &[(i64, i64)]) -> GroupGraph {
        let rows = PolicyRows::builder(0)
            .groups((1..=n).map(|i| GroupRow::new(g(i), &format!("G{i}")).unwrap()))
            .implications(edges.iter().map(|(a, b)| Implication::new(g(*a), g(*b))))
            .build()
            .unwrap();
        GroupGraph::build(&rows).unwrap()
    }

    fn ids(set: &GroupSet) -> Vec<i64> {
        set.ids().iter().map(|g| g.get()).collect()
    }

    fn raw(groups: &[GroupId]) -> Vec<i64> {
        groups.iter().map(|g| g.get()).collect()
    }

    /// `_remove_group` are)
    #[test]
    fn closure_follows_implications_transitively_and_reflexively() {
        // C (3) implies A (1); D (4) implies A; E (5) implies C
        let graph = graph(5, &[(3, 1), (4, 1), (5, 3)]);
        assert_eq!(ids(&graph.closure(&[g(5)]).unwrap()), [1, 3, 5]);
        assert_eq!(ids(&graph.closure(&[g(2)]).unwrap()), [2]);
        assert_eq!(ids(&graph.closure(&[g(4), g(2), g(4)]).unwrap()), [1, 2, 4]);
        assert!(graph.closure(&[]).unwrap().is_empty());
        assert_eq!(raw(&graph.implied_by(g(1)).unwrap()), [1, 3, 4, 5]);
        assert_eq!(raw(&graph.implied_by(g(5)).unwrap()), [5]);
        assert_eq!(raw(&graph.implied(g(5)).unwrap()), [3]);
        assert!(graph.cycles().is_empty());
    }

    #[test]
    fn unknown_groups_are_errors_never_dropped() {
        let graph = graph(2, &[]);
        let err = graph.closure(&[g(1), g(9)]).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            err.message(),
            "The group 9 does not exist (any more): the user's groups cannot be resolved."
        );
        let err = graph.implied_by(g(9)).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(err.message(), "group 9 is not in the policy snapshot");
    }

    #[test]
    fn self_implication_is_a_cycle_of_one() {
        let graph = graph(2, &[(1, 1), (1, 2)]);
        assert_eq!(graph.cycles(), [vec![g(1)]]);
        assert_eq!(ids(&graph.closure(&[g(1)]).unwrap()), [1, 2]);
        assert_eq!(raw(&graph.implied_by(g(1)).unwrap()), [1]);
    }

    #[test]
    fn two_cycle_members_share_their_closure() {
        // 1 <-> 2, 2 -> 3
        let graph = graph(3, &[(1, 2), (2, 1), (2, 3)]);
        assert_eq!(graph.cycles(), [vec![g(1), g(2)]]);
        assert_eq!(ids(&graph.closure(&[g(1)]).unwrap()), [1, 2, 3]);
        assert_eq!(ids(&graph.closure(&[g(2)]).unwrap()), [1, 2, 3]);
        assert_eq!(raw(&graph.implied_by(g(3)).unwrap()), [1, 2, 3]);
        assert_eq!(raw(&graph.implied_by(g(1)).unwrap()), [1, 2]);
    }

    #[test]
    fn deep_cycle_with_a_tail_and_a_long_chain() {
        // tail 10 -> 9 -> … -> 6 enters the cycle 1 -> 2 -> 3 -> 4 -> 5 -> 1
        let mut edges: Vec<(i64, i64)> = (1..5).map(|i| (i, i + 1)).collect();
        edges.push((5, 1));
        edges.extend((7..=10).map(|i| (i, i - 1)));
        edges.push((6, 3));
        let gg = graph(10, &edges);
        assert_eq!(gg.cycles(), [vec![g(1), g(2), g(3), g(4), g(5)]]);
        assert_eq!(
            ids(&gg.closure(&[g(10)]).unwrap()),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(ids(&gg.closure(&[g(4)]).unwrap()), [1, 2, 3, 4, 5]);
        assert_eq!(
            raw(&gg.implied_by(g(1)).unwrap()),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(raw(&gg.implied_by(g(7)).unwrap()), [7, 8, 9, 10]);
        // a chain of the maximum number of groups: no recursion, so no stack overflow
        let n = i64::try_from(crate::rows::MAX_GROUPS).unwrap();
        let chain: Vec<(i64, i64)> = (1..n).map(|i| (i + 1, i)).collect();
        let long = graph(n, &chain);
        assert_eq!(
            long.closure(&[g(n)]).unwrap().len(),
            crate::rows::MAX_GROUPS
        );
        assert_eq!(
            long.implied_by(g(1)).unwrap().len(),
            crate::rows::MAX_GROUPS
        );
        assert!(long.cycles().is_empty());
    }

    #[test]
    fn display_names_order_and_share() {
        let p1 = crate::rows::PrivilegeId::new(1).unwrap();
        let p2 = crate::rows::PrivilegeId::new(2).unwrap();
        let rows = PolicyRows::builder(0)
            .privileges([
                PrivilegeRow::new(p1, "Sales").unwrap().with_sequence(20),
                PrivilegeRow::new(p2, "Accounting")
                    .unwrap()
                    .with_sequence(10),
            ])
            .groups([
                GroupRow::new(g(1), "User").unwrap().with_privilege(p1),
                GroupRow::new(g(2), "Manager")
                    .unwrap()
                    .with_privilege(p1)
                    .with_sequence(5),
                GroupRow::new(g(3), "Billing").unwrap().with_privilege(p2),
                GroupRow::new(g(4), "Zeta").unwrap(),
                GroupRow::new(g(5), "Alpha").unwrap(),
                GroupRow::new(g(6), "Internal User")
                    .unwrap()
                    .with_xmlid(QualifiedXmlId::parse(INTERNAL_USER_GROUP).unwrap()),
                GroupRow::new(g(7), "Portal").unwrap().with_share(true),
            ])
            .build()
            .unwrap();
        let gg = GroupGraph::build(&rows).unwrap();
        assert_eq!(gg.display_name(g(1)).unwrap(), "Sales / User");
        assert_eq!(gg.display_name(g(4)).unwrap(), "Zeta");
        let mut all: Vec<GroupId> = (1..=7).rev().map(g).collect();
        gg.sort(&mut all).unwrap();
        // privilege Accounting (seq 10) first, then Sales (seq 20) with sequence 5 before NULL,
        // then the groups without privilege by name
        assert_eq!(raw(&all), [3, 2, 1, 5, 6, 7, 4]);
        assert!(!gg.is_share(&gg.closure(&[g(6)]).unwrap()).unwrap());
        assert!(gg.is_share(&gg.closure(&[g(7)]).unwrap()).unwrap());
        assert!(gg.share_flag(g(7)).unwrap());
        assert_eq!(
            gg.by_xmlid(&QualifiedXmlId::parse(INTERNAL_USER_GROUP).unwrap()),
            Some(g(6))
        );
        let without_internal = graph(1, &[]);
        let err = without_internal.is_share(&GroupSet::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "the policy has no group with the XML id base.group_user (the internal user type), \
             so internal users cannot be told apart from portal and public ones"
        );
    }
}
