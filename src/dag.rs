use crate::structs::State;
use crate::Result;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;

// Contains all the dependency and state of a particular vertex in a DAG
#[derive(Clone, Debug)]
pub struct Vertex<T> {
    pub id: T,
    children: HashSet<usize>,
    parents: HashSet<usize>,
    pub state: State,
    parents_outstanding: usize,
}

impl<T> Vertex<T> {
    fn new(id: T) -> Self {
        Vertex {
            id,
            children: HashSet::new(),
            parents: HashSet::new(),
            state: State::Queued,
            parents_outstanding: 0,
        }
    }
}

// A visitable [directed-acyclic graph](https://en.wikipedia.org/wiki/Directed_acyclic_graph) structure
// with user-defined keys.
#[derive(Debug, Default)]
pub struct DAG<T: Hash + PartialEq + Eq + Clone + Debug> {
    pub vertices: Vec<Vertex<T>>,
    keymap: HashMap<T, usize>,
    ready: HashSet<usize>,
    visiting: HashSet<usize>,
}

impl<T> DAG<T>
where
    T: Hash + PartialEq + Eq + Clone + Debug,
{
    // Creates a new, empty DAG.
    #[must_use]
    pub fn new() -> Self {
        DAG {
            vertices: Vec::new(),
            keymap: HashMap::new(),
            ready: HashSet::new(),
            visiting: HashSet::new(),
        }
    }

    // Returns a copy of a vertex structure identified by `key`, if it exists in the DAG.
    pub fn get_vertex(&self, key: &T) -> Option<Vertex<T>> {
        self.keymap.get(key).map(|idx| self.vertices[*idx].clone())
    }

    /// Adds a new vertex identified by key
    ///
    /// # Errors
    ///
    /// Will return `Err` if a vertex with ID `key` already exists in
    /// the dag
    pub fn add_vertex(&mut self, key: T) -> Result<()> {
        if self.keymap.contains_key(&key) {
            Err(anyhow!("DAG already contains a vertex with key {:?}", key))
        } else {
            let idx = self.vertices.len();
            self.keymap.insert(key.clone(), idx);
            self.vertices.push(Vertex::new(key));
            self.ready.insert(idx);
            Ok(())
        }
    }

    /// Adds new vertices with IDs in `keys`
    ///
    /// # Errors
    ///
    /// Will return `Err` if a vertex with ID in `keys` already exists
    /// in the dag
    pub fn add_vertices(&mut self, keys: &[T]) -> Result<()> {
        for key in keys.iter() {
            self.add_vertex(key.clone())?;
        }
        Ok(())
    }

    /// Clears the traversal state of the DAG, and preps it to run again
    pub fn reset(&mut self) {
        // Update dependency counts
        for (i, v) in self.vertices.iter_mut().enumerate() {
            v.parents_outstanding = v.parents.len();
            if v.parents_outstanding == 0 {
                self.ready.insert(i);
            }
        }
    }

    /// Returns the number of vertices in the DAG
    pub fn len(&mut self) -> usize {
        self.vertices.len()
    }

    /// True if the DAG has no vertices
    pub fn is_empty(&mut self) -> bool {
        self.vertices.is_empty()
    }

    /// Updates the traversal state of an individual vertex, either
    /// queuing it again, or removing it from running.
    ///
    /// # Errors
    ///
    /// Will return `Err` if attempting an invalid transition.
    pub fn set_vertex_state(&mut self, key: &T, state: State) -> Result<()> {
        let idx = *self.keymap.get(key).ok_or_else(|| anyhow!("No such key"))?;
        let cur_state = self.vertices[idx].state;

        if cur_state == state {
            return Ok(());
        }

        match (cur_state, state) {
            (_, State::Completed) => {
                self.ready.remove(&idx);
                self.visiting.remove(&idx);
                self.complete_visit(key, false)?;
            }
            (State::Errored | State::Killed, State::Queued) => {
                self.ready.insert(idx);
            }
            (_, State::Errored | State::Killed) => {
                self.ready.remove(&idx);
                self.visiting.remove(&idx);
                self.complete_visit(key, true)?;
            }
            (_, _) => {
                return Err(anyhow!(
                    "Unsupported transition from {:?} to {:?}",
                    cur_state,
                    state
                ));
            }
        }
        self.vertices[idx].state = state;
        Ok(())
    }

    /// Add an edge from the vertex identified by `src` to the one
    /// identified by `dst`.
    ///
    /// # Errors
    ///
    /// Will return `Err` if adding the edge would create a cycle
    pub fn add_edge(&mut self, src_key: &T, dst_key: &T) -> Result<()> {
        if self.has_path(dst_key, src_key)? {
            return Err(anyhow!("Adding edge would result in a cycle"));
        }
        let src = *self
            .keymap
            .get(src_key)
            .ok_or_else(|| anyhow!("No such key"))?;
        let dst = *self
            .keymap
            .get(dst_key)
            .ok_or_else(|| anyhow!("No such key"))?;
        self.vertices[src].children.insert(dst);
        self.vertices[dst].parents.insert(src);
        match self.vertices[src].state {
            State::Completed => {}
            _ => {
                self.vertices[dst].parents_outstanding += 1;
            }
        }
        if self.vertices[dst].parents_outstanding == 0 {
            self.ready.insert(dst);
        } else {
            self.ready.take(&dst);
        }
        Ok(())
    }

    /// Returns true if there is a path in the DAG between `src_key`
    /// and `dst_key`
    ///
    /// # Errors
    ///
    /// Will return `Err` if either `src_key` or `dst_key` don't identify
    /// a vertex in the DAG.
    pub fn has_path(&self, src_key: &T, dst_key: &T) -> Result<bool> {
        let src = *self
            .keymap
            .get(src_key)
            .ok_or_else(|| anyhow!("No such key"))?;
        let dst = *self
            .keymap
            .get(dst_key)
            .ok_or_else(|| anyhow!("No such key"))?;
        let mut seen = HashSet::<usize>::new();
        Ok(self._has_path(src, dst, &mut seen))
    }

    /// DFS for a path between `src` and `dst`
    fn _has_path(&self, src: usize, dst: usize, seen: &mut HashSet<usize>) -> bool {
        if src == dst {
            return true;
        }
        if seen.contains(&src) {
            return false;
        }
        if self.vertices[src].children.contains(&dst) {
            return true;
        }
        seen.insert(src);
        for child in &self.vertices[src].children {
            if self._has_path(*child, dst, seen) {
                return true;
            }
        }
        false
    }

    /// Returns the next ID in the traversal, or `None` if no vertices
    /// are ready to be visited.
    /// The vertex will move from the `Queued` state to the `Running`
    /// state.
    pub fn visit_next(&mut self) -> Option<T> {
        if let Some(id) = self.ready.iter().next() {
            let idx = *id;
            self.vertices[idx].state = State::Running;
            self.ready.take(&idx);
            self.visiting.insert(idx);
            Some(self.vertices[idx].id.clone())
        } else {
            None
        }
    }

    /// Transitions the vertex `key` from `Running` to either `Completed`
    /// (if `errored` is `false`), or `Errored`.
    ///
    /// # Errors
    ///
    /// Will return `Err` if `key` doesn't identify a vertex in the DAG,
    /// or if `key` wasn't being visited.
    pub fn complete_visit(&mut self, key: &T, errored: bool) -> Result<()> {
        let idx = *self.keymap.get(key).ok_or_else(|| anyhow!("No such key"))?;
        if !self.visiting.contains(&idx) {
            return Err(anyhow!("Not currently visiting {:?}", key));
        }
        self.visiting.take(&idx);
        let state = &self.vertices[idx].state;
        if *state == State::Completed {
            return Ok(());
        }

        if errored {
            self.vertices[idx].state = State::Errored;
        } else {
            self.vertices[idx].state = State::Completed;
            let children = self.vertices[idx].children.clone();
            for child in &children {
                self.vertices[*child].parents_outstanding -= 1;
                if self.vertices[*child].parents_outstanding == 0 {
                    self.ready.insert(*child);
                }
            }
        }
        Ok(())
    }

    /// Is there any progress still to be had
    #[must_use]
    pub fn can_progress(&self) -> bool {
        !(self.ready.is_empty() && self.visiting.is_empty())
    }

    /// Has everything been successfully visited
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.visiting.is_empty() && self.ready.is_empty()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn dag_construction() {
        let mut dag = DAG::<usize>::new();

        dag.add_vertices(&[0, 1, 2])
            .expect("Unable to add vertices");
        assert_eq!(dag.len(), 3);

        dag.add_vertices(&[3, 4, 5, 6, 7, 8, 9])
            .expect("Unable to add vertices");
        assert_eq!(dag.len(), 10);

        // Unable to add an existing vertex
        assert!(dag.add_vertices(&[3]).is_err());
    }

    #[test]
    fn dag_cycle_detection() {
        let mut dag = DAG::<usize>::new();
        dag.add_vertices(&[0, 1, 2])
            .expect("Unable to add vertices");
        dag.add_edge(&0, &1).unwrap();
        assert!(dag.add_edge(&1, &2).is_ok());
        assert!(dag.add_edge(&2, &0).is_err());
    }

    #[test]
    fn dag_traversal_order() {
        let mut dag = DAG::new();
        dag.add_vertices(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();

        /*
           0 ---------------------\
           1 ------------ \        \           /-----> 8
           2 ---- 3 ---- > 5 -----> 6 -----> 7
           4 -------------------------------/  \-----> 9
        */
        let edges = [
            (0usize, 6usize),
            (1, 5),
            (2, 3),
            (3, 5),
            (5, 6),
            (6, 7),
            (7, 8),
            (8, 9),
            (7, 9),
            (4, 7),
        ];

        for (src, dst) in &edges {
            dag.add_edge(src, dst).unwrap();
        }
        dag.reset();

        let mut visit_order: Vec<usize> = Vec::new();
        visit_order.resize(dag.len(), 0);
        let mut i: usize = 0;
        while let Some(id) = dag.visit_next() {
            dag.complete_visit(&id, false)
                .expect("Unable to complete visit");
            assert_eq!(visit_order[id], 0);
            visit_order[id] = i;
            i += 1;
        }

        // All vertices visited
        assert!(dag.is_complete());
        assert_eq!(visit_order.len(), dag.len());
        for (src, dst) in &edges {
            assert!(visit_order[*src] < visit_order[*dst]);
        }
    }

    #[test]
    fn dag_additions_during_traversal() {
        let mut dag = DAG::new();
        dag.add_vertices(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();

        /*
           0 ---------------------\
           1 ------------ \        \           /-----> 8
           2 ---- 3 ---- > 5 -----> 6 -----> 7
           4 -------------------------------/  \-----> 9
        */
        let edges = [
            (0usize, 6usize),
            (1, 5),
            (2, 3),
            (3, 5),
            (5, 6),
            (6, 7),
            (7, 8),
            (8, 9),
            (7, 9),
            (4, 7),
        ];

        for (src, dst) in &edges {
            dag.add_edge(src, dst).unwrap();
        }
        dag.reset();

        // At the visit of item 5, we'll add in 3 new vertices with these
        // extra edges:
        let extra_vertices = vec![10, 11, 12];
        let n_extra_vertices: usize = extra_vertices.len();
        let extra_edges = [
            // Adding on to the end
            (7usize, 10usize),
            (10, 8),
            (10, 9),
            // Adding into the middle
            (5, 11),
            (6, 11),
            // Adding in a dependency that's already been visited
            (4, 12),
        ];

        let mut visit_order: Vec<usize> = Vec::new();
        visit_order.resize(dag.len() + n_extra_vertices, 0);
        let mut i: usize = 0;
        loop {
            if i == 5 {
                dag.add_vertices(&extra_vertices)
                    .expect("unable to add vertices");
                for (src, dst) in &extra_edges {
                    dag.add_edge(src, dst).unwrap();
                }
            }

            match dag.visit_next() {
                Some(id) => {
                    dag.complete_visit(&id, false)
                        .expect("unable to complete visit");
                    assert_eq!(visit_order[id], 0);
                    visit_order[id] = i;
                }
                None => break,
            }
            i += 1;
        }

        // All vertices visited
        assert_eq!(visit_order.len(), dag.len());
        for (src, dst) in &edges {
            assert!(visit_order[*src] < visit_order[*dst]);
        }
        for (src, dst) in &extra_edges {
            assert!(visit_order[*src] < visit_order[*dst]);
        }
    }
}
