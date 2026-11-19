use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub worker: String,
    pub description: String,
    pub depends_on: Vec<String>,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskGraph(pub Vec<Task>);

impl TaskGraph {
    /// Each worker receives one independently resumable task per plan.
    /// Returns dependency indices in worker order in O((V + E) log V) time.
    pub fn dependencies(&self, workers: usize) -> Result<Vec<Vec<usize>>, String> {
        if !(1..=64).contains(&workers) || self.0.len() != workers {
            return Err("require exactly one task per worker, with at most 64 workers".into());
        }
        let names: BTreeMap<_, _> = (0..workers)
            .map(|index| (format!("worker_{}", index + 1), index))
            .collect();
        let mut ids = BTreeMap::new();
        let mut owners = BTreeSet::new();
        for task in &self.0 {
            let Some(&owner) = names.get(&task.worker) else {
                return Err(format!("unknown task owner {:?}", task.worker));
            };
            let valid_id = !task.id.is_empty()
                && task.id.len() <= 64
                && task
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
            if !valid_id || ids.insert(task.id.as_str(), owner).is_some() {
                return Err(format!("invalid or duplicate task ID {:?}", task.id));
            }
            if !owners.insert(owner) {
                return Err(format!("multiple tasks for {}", task.worker));
            }
            if !valid_text(&task.description, 4096)
                || !(1..=8).contains(&task.acceptance.len())
                || task.acceptance.iter().any(|item| !valid_text(item, 1024))
                || task.depends_on.len() >= workers
            {
                return Err(format!(
                    "invalid description, acceptance or dependencies for {}",
                    task.id
                ));
            }
        }
        let mut dependencies = vec![Vec::new(); workers];
        let mut outgoing = vec![Vec::new(); workers];
        for task in &self.0 {
            let index = ids[task.id.as_str()];
            let mut seen = BTreeSet::new();
            for dependency in &task.depends_on {
                let Some(&source) = ids.get(dependency.as_str()) else {
                    return Err(format!("task {} needs unknown task {dependency}", task.id));
                };
                if source == index || !seen.insert(source) {
                    return Err(format!("self or duplicate dependency in task {}", task.id));
                }
                dependencies[index].push(source);
                outgoing[source].push(index);
            }
        }
        let mut indegrees: Vec<_> = dependencies.iter().map(Vec::len).collect();
        let mut ready: VecDeque<_> = (0..workers)
            .filter(|&index| indegrees[index] == 0)
            .collect();
        let mut visited = 0;
        while let Some(index) = ready.pop_front() {
            visited += 1;
            for &dependent in &outgoing[index] {
                indegrees[dependent] -= 1;
                if indegrees[dependent] == 0 {
                    ready.push_back(dependent);
                }
            }
        }
        if visited != workers {
            return Err("task graph contains a cycle".into());
        }
        Ok(dependencies)
    }
}

fn valid_text(text: &str, max_bytes: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_graph_in_worker_order_and_refuses_cycles() {
        let task = |id: &str, worker: &str, depends_on: &[&str]| Task {
            id: id.into(),
            worker: worker.into(),
            description: "Implement".into(),
            depends_on: depends_on.iter().map(|id| (*id).into()).collect(),
            acceptance: vec!["Run tests".into()],
        };
        let mut graph = TaskGraph(vec![
            task("consumer", "worker_3", &["producer", "config"]),
            task("producer", "worker_1", &[]),
            task("config", "worker_2", &[]),
        ]);
        assert_eq!(
            graph.dependencies(3).unwrap(),
            vec![vec![], vec![], vec![0, 1]]
        );
        graph.0[1].depends_on.push("consumer".into());
        assert_eq!(
            graph.dependencies(3).unwrap_err(),
            "task graph contains a cycle"
        );
    }
}
