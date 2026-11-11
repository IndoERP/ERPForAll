//! Strongly connected components (iterative Tarjan): the group graph's closures and the `access`
//! cycles of the rules. No recursion, so the depth of a graph never reaches the stack.

/// The strongly connected components of a graph over `0..n`.
#[derive(Debug)]
pub(crate) struct Components {
    /// Component of each node.
    pub(crate) of: Vec<u32>,
    /// Members of each component, in reverse topological order: every component reachable from
    /// a component comes before it.
    pub(crate) members: Vec<Vec<u32>>,
}

impl Components {
    /// The components that are cycles (two or more nodes, or one node with an edge to itself),
    /// each sorted, in the order of their smallest member.
    pub(crate) fn cycles(&self, adjacency: &[Vec<u32>]) -> Vec<Vec<u32>> {
        let mut cycles: Vec<Vec<u32>> = self
            .members
            .iter()
            .filter(|group| match group.as_slice() {
                [single] => adjacency[*single as usize].contains(single),
                _ => true,
            })
            .map(|group| {
                let mut group = group.clone();
                group.sort_unstable();
                group
            })
            .collect();
        cycles.sort();
        cycles
    }
}

/// Tarjan's algorithm over `adjacency` (node `i`'s successors are `adjacency[i]`, every one below
/// `adjacency.len()`, which must fit in a `u32`).
pub(crate) fn components(adjacency: &[Vec<u32>]) -> Components {
    const UNSEEN: u32 = u32::MAX;
    let n = adjacency.len();
    let mut order = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut of = vec![UNSEEN; n];
    let mut members: Vec<Vec<u32>> = Vec::new();
    let mut next = 0u32;
    // explicit call stack: (node, index of its next edge)
    let mut calls: Vec<(u32, usize)> = Vec::new();
    for root in 0..n {
        if order[root] != UNSEEN {
            continue;
        }
        let root = root as u32;
        order[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        calls.push((root, 0));
        while let Some(&mut (v, ref mut edge)) = calls.last_mut() {
            let vi = v as usize;
            if let Some(&w) = adjacency[vi].get(*edge) {
                *edge += 1;
                let wi = w as usize;
                if order[wi] == UNSEEN {
                    order[wi] = next;
                    low[wi] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[wi] = true;
                    calls.push((w, 0));
                } else if on_stack[wi] {
                    low[vi] = low[vi].min(order[wi]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                let pi = parent as usize;
                low[pi] = low[pi].min(low[vi]);
            }
            if low[vi] == order[vi] {
                let c = members.len() as u32;
                let mut group = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w as usize] = false;
                    of[w as usize] = c;
                    group.push(w);
                    if w == v {
                        break;
                    }
                }
                members.push(group);
            }
        }
    }
    Components { of, members }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_come_in_reverse_topological_order() {
        // 0 -> 1 -> 2 -> 1, 2 -> 3, 4 alone with a self-loop
        let adjacency = vec![vec![1], vec![2], vec![1, 3], vec![], vec![4]];
        let c = components(&adjacency);
        assert_eq!(c.of[1], c.of[2]);
        assert_ne!(c.of[0], c.of[1]);
        // 3 is reachable from {1, 2}, which is reachable from 0
        assert!(c.of[3] < c.of[1] && c.of[1] < c.of[0]);
        assert_eq!(c.cycles(&adjacency), [vec![1, 2], vec![4]]);
        assert!(components(&[]).members.is_empty());
    }
}
